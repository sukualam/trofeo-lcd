//! Baca suhu paket CPU dan power draw (Watt) langsung dari hardware.
//!
//! - **Windows**: driver PawnIO + baca MSR/SMN langsung (AMD Zen 1–4 lewat
//!   modul resmi `AMDFamily17.bin`, di-embed saat compile). Lihat `mod imp`
//!   di bawah untuk detail register.
//! - **Linux**: sysfs standar kernel.
//!   - **Suhu**: modul kernel bawaan `k10temp` (built-in, semua CPU Zen).
//!   - **Power — Zen 4+ (Ryzen 7000/8000/9000, termasuk 7500F)**: RAPL
//!     powercap `/sys/class/powercap/intel-rapl:N/energy_uj` (zona
//!     "package"). Nama drivernya `intel_rapl_msr` (config `CONFIG_INTEL_RAPL`,
//!     otomatis aktif di semua distro modern) — awalnya untuk Intel, tapi
//!     sejak dukungan AMD masuk (patches dari Google/AMD) driver ini membaca
//!     MSR RAPL AMD (`MSR_PKG_ENERGY_STAT`/`0xC001_029B`, register yang sama
//!     dengan jalur Windows) sehingga angkanya setara. Ini PEMBACAAN UTAMA
//!     untuk Zen 4+, karena `amd_energy` sudah dihapus dari mainline dan
//!     `zenpower`/`zenpower3` TIDAK mendukung Zen 4 (SVI3, bukan SVI2).
//!   - **Power — Zen 1-3**: `zenpower`/`zenpower3` (driver komunitas
//!     out-of-tree, AUR: `zenpower3-dkms`), baca power dari telemetri SVI2
//!     VRM. Lihat komentar di `mod imp` (Linux) untuk detail kenapa dan
//!     bagaimana sumber-sumber ini dibedakan (`PowerSource`).
//! - Platform lain (macOS, dst.): semua sensor `None`, program tetap jalan.
//!
//! # Cakupan CPU
//! Hanya **AMD Ryzen (Zen 1 s/d Zen 4)** — di KEDUA platform. CPU Intel
//! tidak di-cover: `CpuSensor::new()` tetap berhasil, tapi semua sensor
//! `None` (baris info menampilkan N/A, program tidak crash).
//!
//! # Catatan akurasi suhu (Windows)
//! Formula decode suhu (layout bit + kondisi offset 49°C) di-port dari
//! `Amd17Cpu.cs` milik LibreHardwareMonitor. Bandingkan bacaan pertama
//! dengan Ryzen Master/HWiNFO untuk konfirmasi di CPU spesifik kamu.

// ---------------------------------------------------------------------------
// Windows: PawnIO + MSR/SMN langsung
// ---------------------------------------------------------------------------
#[cfg(windows)]
mod imp {
    use crate::pawnio::PawnIo;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Threading::{
        CreateMutexW, ReleaseMutex, WaitForSingleObject, INFINITE,
    };
    use windows::core::PCWSTR;

    /// Modul PawnIO resmi untuk AMD Family 17h–1Ah, di-embed saat compile.
    /// Diunduh dari release `AMDFamily17.bin` di repo PawnIO.Modules.
    pub(super) static AMD_MODULE: &[u8] = include_bytes!("resources/AMDFamily17.bin");

    // --- Alamat register ---

    /// MSR AMD: energy unit (Joule per LSB counter energi).
    pub(super) const MSR_PWR_UNIT: u64 = 0xC001_0299;
    /// MSR AMD: counter energi paket CPU kumulatif (32-bit efektif, wrap-around).
    pub(super) const MSR_PKG_ENERGY_STAT: u64 = 0xC001_029B;
    /// Offset register SMN `THM_TCON_CUR_TMP` — suhu paket CPU.
    pub(super) const SMN_THM_TCON_CUR_TMP: u64 = 0x5980_0;

    /// Named mutex yang harus dipegang sebelum akses SMN (indirect PCI config
    /// space), sesuai dokumentasi modul AMD PawnIO — mencegah race dengan tool
    /// lain (HWiNFO, Ryzen Master, dst.) yang juga akses jalur yang sama.
    const PCI_MUTEX: &str = "Global\\Access_PCI";

    /// RAII guard untuk named Win32 mutex: acquire saat dibuat, release+close
    /// otomatis saat di-drop. Dipakai untuk serialisasi akses SMN.
    pub(super) struct MutexGuard(HANDLE);

    impl MutexGuard {
        pub(super) fn acquire(name: &str) -> Option<Self> {
            let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            // SAFETY: wide adalah null-terminated UTF-16 valid; null attr = default.
            let handle = unsafe {
                CreateMutexW(
                    None,  // default security attributes
                    false, // bukan owner awal
                    PCWSTR(wide.as_ptr()),
                )
            }
            .ok()?;

            // WAIT_FAILED = 0xFFFF_FFFF — semua nilai lain (termasuk
            // WAIT_ABANDONED = 0x80) berarti kita sekarang memegang mutex.
            let wait_result = unsafe { WaitForSingleObject(handle, INFINITE) };
            if wait_result.0 == 0xFFFF_FFFF {
                unsafe { let _ = CloseHandle(handle); }
                return None;
            }
            Some(MutexGuard(handle))
        }

        pub(super) fn acquire_pci() -> Option<Self> {
            Self::acquire(PCI_MUTEX)
        }
    }

    impl Drop for MutexGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = ReleaseMutex(self.0);
                let _ = CloseHandle(self.0);
            }
        }
    }

    /// Decode energy unit dari `MSR_PWR_UNIT`: bit `[12:8]` adalah `ESU`,
    /// dan unit = 0.5^ESU Joule per LSB (sesuai dokumentasi AMD RAPL).
    pub(super) fn read_energy_unit(pawnio: &PawnIo) -> Option<f64> {
        let raw = pawnio
            .execute("ioctl_read_msr", &[MSR_PWR_UNIT], 1)?
            .first()
            .copied()?;
        let esu = (raw >> 8) & 0x1F;
        Some(0.5_f64.powi(esu as i32))
    }
}

// ---------------------------------------------------------------------------
// Linux: sysfs — tidak butuh root, cukup baca file teks biasa di /sys.
//
// Suhu: k10temp (built-in kernel, semua CPU Zen didukung).
//
// Power — tiga sumber, yang klasifikasi & cara hitungnya dijelaskan lengkap
// di enum `PowerSource` di bawah:
//
// 1. RAPL powercap (/sys/class/powercap/intel-rapl:N/energy_uj, zona
//    "package") — jalur UTAMA untuk Zen 4+ (Ryzen 7000/8000/9000, termasuk
//    7500F). Driver kernel MAINLINE `intel_rapl_msr` (dari config
//    CONFIG_INTEL_RAPL) awalnya untuk Intel, tapi sejak dukungan AMD masuk
//    (patches dari Google sejak ~Linux 5.8, lalu diperluas), driver ini
//    membaca MSR RAPL AMD — `MSR_PKG_ENERGY_STAT`, register 0xC001_029B
//    yang SAMA persis dengan jalur Windows proyek ini. Artinya di Zen 4+
//    (yang pakai SVI3, bukan SVI2) power KEMBALI bisa terbaca di Linux lewat
//    jalur RAPL, setara angka Windows.
//
// 2. amd_energy (hwmon RAPL) — SEBENARNYA modul ini DIHAPUS TOTAL dari
//    kernel Linux mainstream sejak versi 5.13 (April 2021) karena sengketa
//    AMD vs maintainer hwmon soal mitigasi celah keamanan Platypus. Kode di
//    bawah tetap MENCOBA-nya dulu (untuk jaga-jaga kalau ada distro/kernel
//    custom yang masih membawanya), lalu fallback ke sumber lain.
//
// 3. zenpower/zenpower3 (hwmon SVI2) — driver komunitas out-of-tree (AUR:
//    `zenpower3-dkms`). Hanya mendukung Zen 1-3; di Zen 4+ yang sudah pindah
//    ke telemetri SVI3, driver ini TIDAK bekerja (bukan soal konfigurasi,
//    memang tidak didukung). Format datanya beda: sudah berupa WATT SESAAT
//    (`powerN_input`), bukan counter energi kumulatif — jadi tidak perlu
//    dihitung selisih dari waktu ke waktu.
// ---------------------------------------------------------------------------
#[cfg(target_os = "linux")]
mod imp {
    use std::fs;
    use std::path::{Path, PathBuf};

    /// Cari folder `/sys/class/hwmon/hwmonN` yang isi file `name`-nya persis
    /// cocok dengan nama driver (mis. "k10temp", "amd_energy", "zenpower").
    pub(super) fn find_hwmon_dir(driver_name: &str) -> Option<PathBuf> {
        let entries = fs::read_dir("/sys/class/hwmon").ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            if let Ok(name) = fs::read_to_string(path.join("name")) {
                if name.trim() == driver_name {
                    return Some(path);
                }
            }
        }
        None
    }

    /// Sumber pembacaan power CPU — bentuk data tiap sumber beda semantiknya:
    #[derive(Clone)]
    pub(super) enum PowerSource {
        /// **RAPL powercap** — jalur UTAMA di Zen 4+ (Ryzen 7000/8000/9000,
        /// termasuk Ryzen 7500F). Driver kernel mainline `intel_rapl_msr`
        /// (dibuat pertama untuk Intel, tapi sejak Linux ~5.8+. sudah
        /// membaca RAPL AMD juga lewat patches dari Google/AMD) mengekspos
        /// counter energi paket sebagai `/sys/class/powercap/intel-rapl:N/
        /// energy_uj` (zona "package-N", satuan microjoule kumulatif) —
        /// HARUS dihitung selisih dari waktu ke waktu untuk dapat Watt.
        /// Angkanya setara jalur MSR Windows (`MSR_PKG_ENERGY_STAT`), karena
        /// memang membaca register yang sama.
        Rapl(PathBuf),
        /// amd_energy (RAPL hwmon, kalau kebetulan ada di kernel custom) —
        /// `energyN_input` kumulatif dalam microjoule, HARUS dihitung selisih
        /// dari waktu ke waktu untuk dapat satuan Watt (lihat `calc_power_watts`).
        Energy(PathBuf),
        /// zenpower — satu atau lebih `powerN_input` yang SUDAH dalam
        /// microwatt SESAAT (bukan kumulatif), tinggal dibaca & dijumlahkan
        /// langsung. zenpower biasa expose lebih dari satu rail (mis. "SVI2
        /// Core" + "SVI2 SoC" terpisah) — dijumlahkan semua supaya dapat
        /// estimasi total yang paling dekat dengan power draw paket CPU
        /// (pendekatan lewat VRM telemetry, bukan RAPL — jangan berharap
        /// akurasi identik dengan Windows/amd_energy, tapi cukup untuk
        /// ditampilkan sebagai indikator).
        Instant(Vec<PathBuf>),
    }

    /// Di dalam hwmon `amd_energy`, cari file `energyN_input` yang label-nya
    /// mengandung "ocket" (mis. label "Esocket0" = total energi 1 soket CPU)
    /// — itu yang paling dekat dengan "power paket CPU" di jalur Windows.
    /// Kalau tidak ketemu (versi driver lain / label beda), fallback ke
    /// `energy1_input` apa adanya.
    pub(super) fn find_socket_energy_input(hwmon_dir: &Path) -> Option<PathBuf> {
        if let Ok(entries) = fs::read_dir(hwmon_dir) {
            for entry in entries.flatten() {
                let file_name = entry.file_name();
                let file_name = file_name.to_string_lossy();
                if let Some(idx) = file_name
                    .strip_prefix("energy")
                    .and_then(|s| s.strip_suffix("_label"))
                {
                    if let Ok(label) = fs::read_to_string(entry.path()) {
                        if label.to_lowercase().contains("ocket") {
                            return Some(hwmon_dir.join(format!("energy{idx}_input")));
                        }
                    }
                }
            }
        }
        let fallback = hwmon_dir.join("energy1_input");
        fallback.exists().then_some(fallback)
    }

    /// Cari zona "package" pertama di `/sys/class/powercap` (mis.
    /// `intel-rapl:0`, yang `${name}`-nya berawalan "package") dan kembalikan
    /// path file `energy_uj`-nya.
    ///
    /// Inilah sumber power UTAMA untuk CPU Zen 4+ di kernel mainline modern:
    /// driver `intel_rapl_msr` (bagian dari `CONFIG_INTEL_RAPL`, umumnya
    /// sudah built-in/modprobe otomatis di semua distro) membaca MSR RAPL AMD
    /// (`MSR_PKG_ENERGY_STAT`/`0xC001_029B`, register yang SAMA dengan jalur
    /// Windows proyek ini) dan mengeksposnya sebagai zona paket powercap.
    ///
    /// Zona dalam (mis. `intel-rapl:0:0` yang `${name}`-nya "core") sengaja
    /// di-skip: kita mau power SELURUH paket, bukan cuma bagian core.
    pub(super) fn find_powercap_package_energy() -> Option<PathBuf> {
        let entries = fs::read_dir("/sys/class/powercap").ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            // Node control-type (mis. "intel-rapl") tidak punya energy_uj —
            // yang punya adalah zona (mis. "intel-rapl:0"). Cek-before-skor
            // supaya control-type ikut ter-skip otomatis.
            let energy = path.join("energy_uj");
            if !energy.exists() {
                continue;
            }
            let name = fs::read_to_string(path.join("name")).unwrap_or_default();
            if !name.trim().starts_with("package") {
                continue;
            }
            return Some(energy);
        }
        None
    }

    /// Semua file `powerN_input` di dalam satu folder hwmon (dipakai untuk
    /// `zenpower`, yang bisa expose lebih dari satu rail power terpisah).
    pub(super) fn find_all_power_inputs(hwmon_dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Ok(entries) = fs::read_dir(hwmon_dir) {
            for entry in entries.flatten() {
                let file_name = entry.file_name();
                let file_name = file_name.to_string_lossy();
                if file_name.starts_with("power") && file_name.ends_with("_input") {
                    out.push(entry.path());
                }
            }
        }
        out.sort();
        out
    }

    pub(super) fn read_u64(path: &Path) -> Option<u64> {
        fs::read_to_string(path).ok()?.trim().parse().ok()
    }
}

// ---------------------------------------------------------------------------

#[cfg(windows)]
struct Inner {
    pawnio: crate::pawnio::PawnIo,
    energy_unit_joules: Option<f64>,
}

/// State internal Linux: path sysfs yang sudah ditemukan sekali di `new()`
/// (tidak perlu scan ulang `/sys/class/hwmon` tiap sample).
#[cfg(target_os = "linux")]
struct Inner {
    temp_path: Option<std::path::PathBuf>,
    power_source: Option<imp::PowerSource>,
}

#[cfg(windows)]
type PlatformInner = Inner;
#[cfg(target_os = "linux")]
type PlatformInner = Inner;
#[cfg(not(any(windows, target_os = "linux")))]
type PlatformInner = ();

/// Monitor suhu + power draw CPU (AMD Ryzen).
///
/// Di platform/CPU yang tidak didukung, semua method gracefully return
/// `None`/`0` — program tetap jalan normal dengan data itu ditampilkan
/// sebagai "N/A".
pub struct CpuSensor {
    inner: Option<PlatformInner>,
}

impl CpuSensor {
    /// Inisialisasi sensor. Kalau sensor tidak ditemukan/tidak didukung,
    /// cetak peringatan ke stderr dan lanjut (bukan exit).
    pub fn new() -> Self {
        #[cfg(windows)]
        {
            use crate::pawnio::PawnIo;
            use imp::{AMD_MODULE, read_energy_unit};

            let pawnio = PawnIo::open_with_module(AMD_MODULE);
            if pawnio.is_none() {
                eprintln!(
                    "PERINGATAN: sensor suhu/power CPU tidak tersedia. \
                     Pastikan:\n  1. Driver PawnIO terpasang: winget install namazso.PawnIO\n  \
                     2. CPU adalah AMD Ryzen (Zen1–Zen4 / Family 17h–1Ah)\n  \
                     Baris info akan menampilkan N/A untuk suhu & power."
                );
            }
            let energy_unit_joules = pawnio.as_ref().and_then(read_energy_unit);
            return CpuSensor {
                inner: pawnio.map(|pw| Inner { pawnio: pw, energy_unit_joules }),
            };
        }

        #[cfg(target_os = "linux")]
        {
            let temp_path = imp::find_hwmon_dir("k10temp")
                .or_else(|| imp::find_hwmon_dir("zenpower"))
                .map(|d| d.join("temp1_input"));

            // Prioritas sumber power (urutan penting — lihat penjelasan di
            // `imp::PowerSource`):
            //   1. RAPL powercap — satu-satunya yang bekerja di Zen 4+
            //      (Ryzen 7000/8000/9000, termasuk 7500F): driver mainline
            //      intel_rapl_msr yang membaca MSR RAPL AMD.
            //   2. amd_energy hwmon — kernel custom lama yang masih membawa
            //      modul ini (sudah dihapus dari mainline sejak 5.13).
            //   3. zenpower/zenpower3 — HANYA CPU Zen 1-3 (SVI2); di Zen 4+
            //      yang pakai SVI3, driver ini tidak akan pernah bekerja
            //      (bukan masalah konfigurasi, tapi memang tidak mendukung).
            let power_source = imp::find_powercap_package_energy()
                .map(imp::PowerSource::Rapl)
                .or_else(|| {
                    imp::find_hwmon_dir("amd_energy")
                        .and_then(|d| imp::find_socket_energy_input(&d))
                        .map(imp::PowerSource::Energy)
                })
                .or_else(|| {
                    let zp_dir = imp::find_hwmon_dir("zenpower")?;
                    let inputs = imp::find_all_power_inputs(&zp_dir);
                    (!inputs.is_empty()).then_some(imp::PowerSource::Instant(inputs))
                });

            if temp_path.is_none() {
                eprintln!(
                    "PERINGATAN: sensor suhu CPU tidak ditemukan (butuh modul \
                     kernel 'k10temp', biasanya sudah built-in — cek: \
                     ls /sys/class/hwmon/*/name | xargs grep -l k10temp 2>/dev/null, \
                     atau 'sudo modprobe k10temp'). Suhu CPU akan N/A."
                );
            }
            if power_source.is_none() {
                eprintln!(
                    "PERINGATAN: sensor power CPU tidak ditemukan (tidak ada sumber RAPL \
                     terbaca).\n  \
                     Untuk Zen 4+ (Ryzen 7000/7500F dst) jalur yang dipakai program ini \
                     adalah RAPL powercap (/sys/class/powercap/intel-rapl:N/energy_uj) — \
                     driver kernel 'intel_rapl_msr'. Kalau ini Peringatan muncul padahal \
                     CPU-nya Zen 4+, cek:\n  \
                     1. modul RAPL aktif: 'sudo modprobe intel_rapl_msr' (biasanya \
                     otomatis), lalu cek energi kebaca: \
                     'ls /sys/class/powercap/*/energy_uj'\n  \
                     2. file-nya bisa dibaca user biasa (default hanya root; kalau \'cat ...\' \
                     bilang Permission denied, pasang udev rule chmod 0444 di \
                     /etc/udev/rules.d/, lihat README).\n  \
                     Catatan: 'zenpower3' (SVI2) cuma jalan untuk Zen 1-3, TIDAK \
                     mendukung Zen 4+ yang pakai SVI3; 'amd_energy' sudah dihapus dari \
                     kernel mainline sejak 5.13. Power CPU akan N/A sampai ada sumber yang \
                     terbaca."
                );
            }
            return CpuSensor { inner: Some(Inner { temp_path, power_source }) };
        }

        #[cfg(not(any(windows, target_os = "linux")))]
        CpuSensor { inner: None }
    }

    /// Suhu paket CPU dalam °C. `None` kalau sensor tidak tersedia.
    pub fn get_temp_c(&self) -> Option<f32> {
        #[cfg(windows)]
        {
            use imp::{MutexGuard, SMN_THM_TCON_CUR_TMP};
            let inner = self.inner.as_ref()?;
            let _guard = MutexGuard::acquire_pci();
            let raw = inner
                .pawnio
                .execute("ioctl_read_smn", &[SMN_THM_TCON_CUR_TMP], 1)?
                .first()
                .map(|&v| v as u32)?;

            // Decode dari LibreHardwareMonitor Amd17Cpu.cs:
            //   bit [31:21] = suhu × 0.125 °C
            //   bit 19 ("range select") atau bit [17:16] keduanya set ("Tj select")
            //   → kurangi 49 °C dari nilai mentah
            let range_sel = raw & 0x0008_0000 != 0;
            let tj_sel = raw & 0x0003_0000 == 0x0003_0000;
            let mut milli_c = (raw >> 21) as i32 * 125;
            if range_sel || tj_sel {
                milli_c -= 49_000;
            }
            return Some((milli_c as f32 / 1000.0).max(0.0));
        }

        #[cfg(target_os = "linux")]
        {
            let inner = self.inner.as_ref()?;
            let path = inner.temp_path.as_ref()?;
            let milli_c = imp::read_u64(path)? as f32;
            return Some(milli_c / 1000.0);
        }

        #[allow(unreachable_code)]
        None
    }

    /// Ambil snapshot counter energi kumulatif — HANYA relevan untuk
    /// `PowerSource::Energy` (amd_energy/RAPL) dan `PowerSource::Rapl`
    /// (RAPL powercap). Untuk `PowerSource::Instant`
    /// (zenpower) nilai ini tidak dipakai sama sekali (`calc_power_watts`
    /// baca langsung tanpa butuh selisih waktu), jadi `0` di sana aman.
    ///
    /// - Windows: raw LSB MSR (32-bit efektif, wrap-around ~tiap 40 detik).
    /// - Linux (`Energy`): microjoule (µJ) kumulatif dari hwmon (64-bit,
    ///   praktis tidak pernah wrap dalam durasi realistis).
    /// - Linux (`Rapl`): microjoule (µJ) kumulatif dari powercap — counter
    ///   zona paket bisa wrap di `max_energy_range_uj` (di mesin dev ~65 kJ,
    ///   artinya wrap tiap beberapa menit tergantung beban); aman karena
    ///   interval sample di sini cuma ~500 ms, jauh di bawah periode wrap,
    ///   dan `wrapping_sub` di `calc_power_watts` menanganinya dengan benar.
    ///
    /// Simpan nilainya, lalu berikan ke `calc_power_watts()` bersama selisih
    /// waktu untuk mendapat rata-rata Watt selama interval tersebut.
    pub fn sample_energy(&self) -> u64 {
        #[cfg(windows)]
        {
            use imp::MSR_PKG_ENERGY_STAT;
            if let Some(inner) = &self.inner {
                return inner
                    .pawnio
                    .execute("ioctl_read_msr", &[MSR_PKG_ENERGY_STAT], 1)
                    .and_then(|v| v.first().copied())
                    .unwrap_or(0)
                    & 0xFFFF_FFFF;
            }
        }
        #[cfg(target_os = "linux")]
        {
            if let Some(inner) = &self.inner {
                if let Some(p) = &inner.power_source {
                    if let imp::PowerSource::Energy(path) | imp::PowerSource::Rapl(path) = p {
                        return imp::read_u64(path).unwrap_or(0);
                    }
                }
            }
        }
        0
    }

    /// Hitung rata-rata power draw paket CPU (Watt) sejak `prev_energy`
    /// diambil, `delta_ms` milidetik yang lalu. `None` kalau sensor tidak
    /// tersedia atau `delta_ms` = 0.
    ///
    /// Di Linux, tiap sumber punya cara hitung yang berbeda-beda (lihat
    /// `imp::PowerSource`):
    /// - `Rapl` (powercap) & `Energy` (amd_energy/RAPL): counter kumulatif,
    ///   HARUS dihitung selisih dari `prev_energy`/`delta_ms` — persis
    ///   seperti jalur MSR Windows di atas.
    /// - `Instant` (zenpower): SUDAH watt sesaat, tinggal dijumlah semua
    ///   rail & dikonversi satuan — `prev_energy`/`delta_ms` TIDAK dipakai
    ///   sama sekali di cabang ini (parameter tetap ada demi signature yang
    ///   sama, cuma diabaikan).
    pub fn calc_power_watts(&self, prev_energy: u64, delta_ms: u64) -> Option<f32> {
        #[cfg(windows)]
        {
            let inner = self.inner.as_ref()?;
            let unit = inner.energy_unit_joules?;
            if delta_ms == 0 {
                return None;
            }
            // Pakai wrapping_sub karena counter MSR 32-bit bisa wrap-around —
            // aman selama interval tidak sampai lebih dari sekali wrap.
            let current = self.sample_energy();
            let delta_lsb = (current as u32).wrapping_sub(prev_energy as u32);
            let joules = delta_lsb as f64 * unit;
            let watts = joules / (delta_ms as f64 / 1000.0);
            return Some(watts.clamp(0.0, 9999.0) as f32);
        }

        #[cfg(target_os = "linux")]
        {
            let inner = self.inner.as_ref()?;
            return match inner.power_source.as_ref()? {
                imp::PowerSource::Energy(_) | imp::PowerSource::Rapl(_) => {
                    if delta_ms == 0 {
                        return None;
                    }
                    let current = self.sample_energy();
                    let delta_uj = current.wrapping_sub(prev_energy);
                    let joules = delta_uj as f64 / 1_000_000.0;
                    let watts = joules / (delta_ms as f64 / 1000.0);
                    Some(watts.clamp(0.0, 9999.0) as f32)
                }
                imp::PowerSource::Instant(paths) => {
                    let total_microwatts: u64 =
                        paths.iter().filter_map(|p| imp::read_u64(p)).sum();
                    Some((total_microwatts as f64 / 1_000_000.0).clamp(0.0, 9999.0) as f32)
                }
            };
        }

        #[cfg(not(any(windows, target_os = "linux")))]
        {
            let _ = (prev_energy, delta_ms);
        }

        #[allow(unreachable_code)]
        None
    }
}
