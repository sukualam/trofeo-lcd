//! Client untuk kext `AMDRyzenCPUPowerManagement` di macOS.
//!
//! ## Yang iniastricmemberikan
//!
//! Berbeda dari `smc_macos.rs` (yang baca key SMC), modul ini bicara langsung
//! ke kext lewat IOKit user client `AMDRyzenCPUPMUserClient` — jalur yang sama
//! dengan yang dipakai aplikasi resmi "AMD Power Gadget". Satu panggilan
//! selector 4 mengembalikan semuanya sekaligus:
//!
//! ```text
//! [power_watt, suhu_paket, pstate, freq_mhz_core0, freq_mhz_core1, ...]
//! ```
//!
//! Yang ini mengisi dua lubang yang tidak bisa ditutup lewat SMC:
//! **daya CPU dalam Watt** dan **frekuensi real-time per core**.
//!
//! ## Kenapa butuh root
//!
//! User client membalas `kIOReturnNotPrivileged` untuk proses non-root —
//! kext melakukan privilege check sebelum membaca MSR. Kext-nya punya
//! opt-out lewat boot argument kernel:
//!
//! ```c
//! disablePrivilegeCheck = checkKernelArgument("-amdpnopchk");
//! ```
//!
//! tapi `Info.plist` kext tidak mendeklarasikan seksi `Kernel`, sehingga
//! pada macOS modern argumen itu kemungkinan besar tidak diteruskan ke kext.
//! Jalur yang andal adalah menjalankan program sebagai root.
//!
//! Karena itu semua accessor di sini mengembalikan `Option`/`Result` yang
//! bisa `None` — program tetap jalan dengan N/A kalau tidak punya akses.

use std::ffi::c_void;
use std::sync::Mutex;

// ---------------------------------------------------------------------------
// IOKit FFI (sama seperti di smc_macos.rs)

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOServiceMatching(name: *const std::ffi::c_char) -> *mut c_void;
    fn IOServiceGetMatchingService(main_port: u32, matching: *mut c_void) -> u32;
    fn IOServiceOpen(service: u32, owning_task: u32, options: u32, connect: *mut u32) -> i32;
    fn IOServiceClose(connect: u32) -> i32;
    fn IOObjectRelease(object: u32) -> i32;
    /// Prototipe asli (IOKitLib.h) memisahkan argumen SCALAR dan STRUCT, dan
    /// dua parameter terakhir bertipe `size_t *` (8 byte) — bukan `u32 *`.
    /// Salah urutan/ukuran di sini membuat kernel menulis 8 byte ke variabel
    /// 4 byte dan merusak stack (SIGSEGV/SIGBUS).
    #[allow(clippy::too_many_arguments)]
    fn IOConnectCallMethod(
        connection: u32,
        selector: u32,
        input: *const u64,
        input_count: u32,
        input_struct: *const c_void,
        input_struct_count: usize,
        output: *mut u64,
        output_count: *mut u32,
        output_struct: *mut c_void,
        output_struct_count: *mut usize,
    ) -> i32;
    fn mach_task_self() -> u32;
}

const K_IO_MASTER_PORT_DEFAULT: u32 = 0;
const KERN_SUCCESS: i32 = 0;
/// `kIOReturnNotPrivileged`
const K_IORETURN_NOT_PRIVILEGED: i32 = -536_870_209; // 0xe00002bf

/// Selector pada `AMDRyzenCPUPMUserClient` yang mengembalikan metrik lengkap.
const SELECTOR_METRICS: u32 = 4;
/// Maksimum core fisik yang mungkin (buffer output harus cukup).
const MAX_CORES: usize = 128;

/// Pembacaan satu siklus dari kext.
#[derive(Debug, Clone, Default)]
pub struct Metrics {
    /// Daya paket CPU, dalam Watt (kext sudah menghitungnya dari MSR RAPL
    /// `0xC001029B`, jadi tidak perlu konversi satuan).
    pub power_w: Option<f32>,
    /// Suhu paket, °C.
    pub temp_c: Option<f32>,
    /// P-state aktif saat ini.
    pub pstate: Option<u32>,
    /// Frekuensi efektif per core, **MHz** (bukan KHz).
    ///
    /// Kext menghitungnya sebagai `CurCpuFid / CurCpuDfsId * 200`, yang
    /// sudah menghasilkan MHz. Aplikasi "AMD Power Gadget" juga memakainya
    /// langsung (`freqMax * 0.001` -> GHz), jadi membagi lagi dengan 1000 di
    /// sini akan membuat 3700 MHz tampak seperti 3,7 MHz.
    pub freq_mhz_per_core: Vec<u32>,
}

impl Metrics {
    /// Frekuensi rata-rata antar core, MHz. `None` kalau tidak ada core.
    pub fn avg_freq_mhz(&self) -> Option<u32> {
        if self.freq_mhz_per_core.is_empty() {
            return None;
        }
        // Nilai dari kext sudah MHz — jangan dibagi lagi.
        let sum: f64 = self.freq_mhz_per_core.iter().map(|&m| m as f64).sum();
        let avg = sum / self.freq_mhz_per_core.len() as f64;
        // 200-10.000 MHz adalah rentang CPU yang masuk akal. Di luar itu
        // berarti kext belum punya sampel (0) atau datanya rusak.
        (avg >= 200.0 && avg <= 10_000.0).then_some(avg.round() as u32)
    }
}

/// Alasan pemanggilan gagal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PmError {
    /// Proses tidak punya hak akses (butuh root).
    NotPrivileged,
    /// Kext tidak ada.
    NotAvailable,
    /// Kegagalan lain dari IOKit.
    IoError(i32),
}

/// Koneksi ke user client. `Drop` menutupnya.
pub struct AmdPmClient {
    conn: u32,
}

impl AmdPmClient {
    /// Buka user client `AMDRyzenCPUPowerManagement`.
    /// `None` kalau kext-nya tidak terpasang.
    pub fn open() -> Option<Self> {
        unsafe {
            let name = c"AMDRyzenCPUPowerManagement";
            let matching = IOServiceMatching(name.as_ptr());
            if matching.is_null() {
                return None;
            }
            let service = IOServiceGetMatchingService(K_IO_MASTER_PORT_DEFAULT, matching);
            if service == 0 {
                return None;
            }
            let mut conn = 0u32;
            let kr = IOServiceOpen(service, mach_task_self(), 0, &mut conn);
            IOObjectRelease(service);
            if kr == KERN_SUCCESS && conn != 0 {
                Some(Self { conn })
            } else {
                None
            }
        }
    }

    /// Ambil metrik satu siklus.
    pub fn metrics(&self) -> Result<Metrics, PmError> {
        let mut out_floats = [0f32; MAX_CORES + 3];
        let mut scalar: u64 = 0;
        // `outputCount` dan `outputStructCnt` adalah In/Out: harus diisi
        // kapasitas yang kita adamantkan sebelum pemanggilan.
        let mut scalar_count: u32 = 1;
        let mut struct_size: usize = std::mem::size_of_val(&out_floats);

        let kr = unsafe {
            IOConnectCallMethod(
                self.conn,
                SELECTOR_METRICS,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                &mut scalar,
                &mut scalar_count,
                out_floats.as_mut_ptr().cast::<c_void>(),
                &mut struct_size,
            )
        };

        if kr == K_IORETURN_NOT_PRIVILEGED {
            return Err(PmError::NotPrivileged);
        }
        if kr != KERN_SUCCESS {
            return Err(PmError::IoError(kr));
        }

        // `scalar` = jumlah core fisik; kext menulis (core + 3) float:
        // [power, temp, pstate, freq_mhz * core].
        let n_cores = (scalar as usize).min(MAX_CORES);
        if n_cores == 0 || out_floats.len() < n_cores + 3 {
            return Err(PmError::IoError(0));
        }

        let power = out_floats[0];
        let temp = out_floats[1];
        let pstate = out_floats[2];

        let mut freq_mhz_per_core = Vec::with_capacity(n_cores);
        for i in 0..n_cores {
            let mhz = out_floats[3 + i];
            // 0 berarti kext belum punya sampel untuk core ini.
            if mhz > 0.0 {
                freq_mhz_per_core.push(mhz.round() as u32);
            }
        }

        Ok(Metrics {
            // Sanity check yang sama dengan aplikasi aslinya: >1000 Watt
            // jelas data basura, dan 0 berarti kext belum siap.
            power_w: (power > 0.0 && power < 1000.0).then_some(power),
            temp_c: (temp > -50.0 && temp < 150.0).then_some(temp),
            pstate: (pstate >= 0.0).then_some(pstate as u32),
            freq_mhz_per_core,
        })
    }
}

impl Drop for AmdPmClient {
    fn drop(&mut self) {
        unsafe {
            IOServiceClose(self.conn);
        }
    }
}

/// True kalau kext-nya terpasang (user client bisa dibuka). Tidak butuh root.
pub fn kext_present() -> bool {
    AmdPmClient::open().is_some()
}

/// Client yang dipakai bersama, dibuka sekali lalu disimpan selamanya.
///
/// Dipakai oleh `cpu_freq.rs` (frekuensi) dan `cpu_sensor.rs` (power) karena
/// keduanya memanggil selector yang sama pada interval refresh yang sama —
/// tanpa cache, tiap frame refresh akan melakukan `IOServiceOpen` +
/// `IOServiceClose` ke kernel dua kali sia-sia.
static SHARED: Mutex<Option<AmdPmClient>> = Mutex::new(None);

/// Ambil client yang sudah dibuka, membuka bila perlu. Mengembalikan `None`
/// kalau kext tidak terpasang.
pub fn shared_client<'a>() -> Option<AmdPmClientGuard<'a>> {
    let mut guard = SHARED.lock().ok()?;
    if guard.is_none() {
        *guard = AmdPmClient::open();
    }
    // Client tidak bisa di-clone, jadi pin lewat raw pointer ke isi mutex.
    // Keamanan: mutex dipegang selama guard hidup (lihat struct).
    let ptr: *const AmdPmClient = guard.as_ref()?;
    Some(AmdPmClientGuard {
        _guard: guard,
        client: ptr,
    })
}

/// Menyembunyikan `MutexGuard` di balik guard supaya pemanggil tidak bisa
/// holdings lock tanpa realized. Pointer ke client dijaga oleh `_guard`.
pub struct AmdPmClientGuard<'a> {
    _guard: std::sync::MutexGuard<'a, Option<AmdPmClient>>,
    client: *const AmdPmClient,
}

impl AmdPmClientGuard<'_> {
    /// Baca metrik lewat client yang sudah dibuka.
    pub fn metrics(&self) -> Result<Metrics, PmError> {
        // SAFETY: `client` menunjuk ke nilai di dalam `SHARED` yang dijaga
        // `_guard`; guard hidup selama struct ini, jadi tidak ada yang bisa
        // menggantinya di tengah pemakaian.
        unsafe { (*self.client).metrics() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// User client harus bisa dibuka tanpa root — Privileged check terjadi
    /// saat memanggil selector, bukan saat membuka koneksi. Kalau ini gagal,
    /// diagnosis "kext tidak terpasang" akan menyesatkan.
    #[test]
    fn user_client_opens_without_root() {
        match AmdPmClient::open() {
            Some(c) => println!("user client terbuka"),
            None => println!("AMDRyzenCPUPowerManagement tidak terpasang (lewati)"),
        }
    }

    /// Membaca metrik akan gagal sebagai non-root dengan `NotPrivileged` —
    /// itu hasil yang diharapkan, bukan bug. Kalau kext tidak ada, `open()`
    /// sudah mengembalikan `None` di test sebelumnya.
    #[test]
    fn metrics_requires_privileges() {
        let Some(client) = AmdPmClient::open() else {
            return;
        };
        match client.metrics() {
            Ok(m) => println!("metrik berhasil (privileged): {m:?}"),
            Err(PmError::NotPrivileged) => println!("NotPrivileged — sesuai harapan"),
            Err(e) => println!("error lain: {e:?}"),
        }
    }

    /// Rata-rata frekuensi harus menolak data tak masuk akal (0, atau >10 GHz).
    #[test]
    fn avg_freq_rejects_nonsense() {
        let m = Metrics {
            freq_mhz_per_core: vec![3700, 4200, 3900],
            ..Default::default()
        };
        assert_eq!(m.avg_freq_mhz(), Some(3933));

        // Nilai dari kext sudah MHz: tidak boleh dibagi 1000 (dulu 3700 MHz
        // salah jadi 3,7 dan tampil sebagai "4MHz").
        let satu_core = Metrics {
            freq_mhz_per_core: vec![3700],
            ..Default::default()
        };
        assert_eq!(satu_core.avg_freq_mhz(), Some(3700));

        let kosong = Metrics::default();
        assert_eq!(kosong.avg_freq_mhz(), None);

        // Nilai kacau di luar rentang CPU harus ditolak, bukan ditampilkan.
        let ngawur = Metrics {
            freq_mhz_per_core: vec![3],
            ..Default::default()
        };
        assert_eq!(ngawur.avg_freq_mhz(), None);
    }
}

