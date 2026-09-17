//! Baca frekuensi CPU **real-time** (berganti-ganti mengikuti beban/boost).
//!
//! # Windows — PDH
//! Counter PDH `\Processor Information(_Total)\Processor Frequency` TIDAK
//! dapat diandalkan (nilainya statis di banyak Windows — sudah diverifikasi di
//! mesin ini: tetap 3701 MHz walau 12 core di-load penuh). Yang benar-benar
//! bergerak adalah `\Processor Information(_Total)\% Processor Performance`.
//!
//! Rumus yang dipakai (sama persis dengan yang Task Manager pakai untuk
//! kolom "Speed"):
//! ```text
//! frekuensi saat ini = base clock × (% Processor Performance / 100)
//! ```
//! Base clock dibaca dari registry `HKLM\HARDWARE\DESCRIPTION\System\
//! CentralProcessor\0\~MHz` (statis, dibaca sekali saat init).
//!
//! # Linux — sysfs cpufreq
//! Rata-rata `/sys/devices/system/cpu/cpuN/cpufreq/scaling_cur_freq` (kHz)
//! di semua core. Kalau driver cpufreq tidak ada (mesin tanpa governor),
//! fallback ke nilai statis `cpuinfo_max_freq`/`/proc/cpuinfo model name`
//! sehingga tetap menampilkan angka (base clock), bukan N/A.

// ---------------------------------------------------------------------------
// Windows: PDH `% Processor Performance` × base clock registry
// ---------------------------------------------------------------------------
#[cfg(windows)]
mod imp {
    use std::time::Duration;

    use windows::Win32::System::Performance::{
        PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterValue,
        PdhOpenQueryW, PDH_FMT_COUNTERVALUE, PDH_FMT_DOUBLE,
    };
    use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RegGetValueW};
    use windows::core::PCWSTR;

    pub(super) struct Inner {
        query: isize,
        counter_perf: isize,
        base_mhz: u32,
    }

    impl Drop for Inner {
        fn drop(&mut self) {
            // SAFETY: query valid selama `Inner` hidup; dicolose sekali saja.
            unsafe { let _ = PdhCloseQuery(self.query); }
        }
    }

    /// Baca base clock dari registry (`~MHz` = base frequency, DWORD).
    fn registry_base_mhz() -> Option<u32> {
        let subkey: Vec<u16> = "HARDWARE\\DESCRIPTION\\System\\CentralProcessor\\0"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let value: Vec<u16> = "~MHz".encode_utf16().chain(std::iter::once(0)).collect();

        let mut data: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        // SAFETY: string UTF-16 valid; data/size menunjuk variable lokal.
        let result = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                PCWSTR(subkey.as_ptr()),
                PCWSTR(value.as_ptr()),
                RRF_RT_REG_DWORD,
                None,
                Some(&mut data as *mut u32 as *mut _),
                Some(&mut size),
            )
        };
        result.is_ok().then_some(data)
    }

    impl Inner {
        pub(super) fn new() -> Option<Self> {
            // SAFETY: panggilan PDH API; handle diisi DLL lewat pointer.
            unsafe {
                let mut query: isize = 0;
                let err = PdhOpenQueryW(None, 0, &mut query);
                if err != 0 {
                    eprintln!("PERINGATAN: buka query PDH gagal (0x{err:08X}), frekuensi CPU akan N/A.");
                    return None;
                }

                // Coba nama counter umum dulu, fallback varian lama.
                let mut counter_perf: isize = 0;
                let mut ok = false;
                for path in [
                    "\\Processor Information(_Total)\\% Processor Performance",
                    "\\Processor(_Total)\\% Processor Performance",
                ] {
                    let wide: Vec<u16> =
                        path.encode_utf16().chain(std::iter::once(0)).collect();
                    let err = PdhAddEnglishCounterW(query, PCWSTR(wide.as_ptr()), 0, &mut counter_perf);
                    if err == 0 {
                        ok = true;
                        break;
                    }
                }
                if !ok {
                    eprintln!(
                        "PERINGATAN: counter '% Processor Performance' tidak dapat didaftarkan, \
                         frekuensi CPU akan N/A."
                    );
                    PdhCloseQuery(query);
                    return None;
                }

                // Warm-up beberapa sample — counter persen butuh sample awal
                // (2 sample berjarak) sebelum nilai pertamanya valid.
                for _ in 0..3 {
                    let _ = PdhCollectQueryData(query);
                    std::thread::sleep(Duration::from_millis(150));
                }

                let base_mhz = registry_base_mhz().unwrap_or(0);
                if base_mhz == 0 {
                    eprintln!(
                        "PERINGATAN: base clock CPU tidak terbaca dari registry, \
                         frekuensi CPU akan N/A."
                    );
                    PdhCloseQuery(query);
                    return None;
                }

                Some(Self { query, counter_perf, base_mhz })
            }
        }

        /// Frekuensi saat ini dalam MHz = base × (%ProcessorPerformance / 100).
        pub(super) fn sample_mhz(&self) -> Option<u32> {
            // SAFETY: query/counter valid selama `self` hidup.
            unsafe {
                let _ = PdhCollectQueryData(self.query);
                let mut value = PDH_FMT_COUNTERVALUE::default();
                if PdhGetFormattedCounterValue(self.counter_perf, PDH_FMT_DOUBLE, None, &mut value)
                    != 0
                {
                    return None;
                }
                // CStatus != 0 berarti belum ada data valid antar 2 collect.
                if value.CStatus != 0 {
                    return None;
                }
                let perf = value.Anonymous.doubleValue.max(0.0);
                Some((self.base_mhz as f64 * perf / 100.0).round().clamp(0.0, f64::from(u32::MAX)) as u32)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Linux: sysfs cpufreq
// ---------------------------------------------------------------------------
#[cfg(target_os = "linux")]
mod imp {
    use std::fs;

    pub(super) struct Inner {
        /// Base clock (kHz) sebagai fallback kalau `scaling_cur_freq` tidak ada.
        base_khz: u64,
    }

    /// Baca base clock statis: prefer `cpuinfo_max_freq` (kHz), fallback parse
    /// `model name` di `/proc/cpuinfo` (mis. "3.70GHz" -> 3_700_000 kHz).
    fn base_khz() -> Option<u64> {
        let via_sysfs =
            fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq")
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok());
        if let Some(khz) = via_sysfs {
            return Some(khz);
        }

        let cpuinfo = fs::read_to_string("/proc/cpuinfo").ok()?;
        for line in cpuinfo.lines() {
            let line = line.trim();
            if !line.starts_with("model name") {
                continue;
            }
            if let Some(ghz_pos) = line.find("GHz") {
                let segment = &line[..ghz_pos];
                if let Some(last_space) = segment.rfind(' ') {
                    if let Ok(ghz) = segment[last_space + 1..].trim().parse::<f64>() {
                        return Some((ghz * 1_000_000.0).round() as u64);
                    }
                }
            }
        }
        None
    }

    /// Rata-rata `scaling_cur_freq` (kHz) di semua core (cpuN). SMT double-count
    /// tidak masalah — konvensi yang sama dipakai banyak tool monitor lain.
    fn read_scaling_khz(base: u64) -> u64 {
        let dirs = match fs::read_dir("/sys/devices/system/cpu") {
            Ok(d) => d,
            Err(_) => return base,
        };
        let mut total = 0u64;
        let mut n = 0u32;
        for entry in dirs.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with("cpu") || !name[3..].chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let path = entry.path().join("cpufreq/scaling_cur_freq");
            if let Ok(khz) = fs::read_to_string(&path)
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
            {
                total += khz;
                n += 1;
            }
        }
        if n == 0 { base } else { total / u64::from(n) }
    }

    impl Inner {
        pub(super) fn new() -> Option<Self> {
            let base_khz = base_khz()?;
            if base_khz == 0 {
                return None;
            }
            Some(Self { base_khz })
        }

        /// Frekuensi saat ini (MHz): rata-rata `scaling_cur_freq`, fallback base.
        pub(super) fn sample_mhz(&self) -> Option<u32> {
            let khz = read_scaling_khz(self.base_khz);
            Some((khz / 1000).min(u64::from(u32::MAX)) as u32)
        }
    }
}

// ---------------------------------------------------------------------------

#[cfg(windows)]
struct Inner {
    imp: imp::Inner,
}
#[cfg(target_os = "linux")]
struct Inner {
    imp: imp::Inner,
}
#[cfg(not(any(windows, target_os = "linux")))]
struct Inner(());

/// Monitor frekuensi CPU real-time. Di platform yang tidak didukung, semua
/// method graceful return `None` — program tetap jalan, data tampil "N/A".
pub struct CpuFreq {
    inner: Option<Inner>,
}

impl CpuFreq {
    /// Inisialisasi pembaca frekuensi. Kalau gagal (driver/API tidak ada),
    /// cetak peringatan dan lanjut (bukan exit).
    pub fn new() -> Self {
        #[cfg(windows)]
        {
            return CpuFreq {
                inner: imp::Inner::new().map(|imp| Inner { imp }),
            };
        }
        #[cfg(target_os = "linux")]
        {
            return CpuFreq {
                inner: imp::Inner::new().map(|imp| Inner { imp }),
            };
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        CpuFreq { inner: None }
    }

    /// Frekuensi CPU saat ini dalam MHz (real-time, ikut beban/boost).
    /// `None` kalau belum ada data valid / tidak didukung.
    pub fn sample_mhz(&self) -> Option<u32> {
        #[cfg(windows)]
        {
            return self.inner.as_ref()?.imp.sample_mhz();
        }
        #[cfg(target_os = "linux")]
        {
            return self.inner.as_ref()?.imp.sample_mhz();
        }
        #[allow(unreachable_code)]
        None
    }
}