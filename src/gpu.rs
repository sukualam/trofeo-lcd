//! Pemakaian GPU (persen, total).
//!
//! - **Windows**: performance counter bawaan ("GPU Engine", tersedia sejak
//!   Windows 10 1803+ dengan driver WDDM 2.4+). CATATAN JUJUR: Windows tidak
//!   punya satu angka resmi "total GPU usage" lewat PDH — yang ada adalah
//!   utilization per "engine" (3D, Copy, Video Decode, dst.) per proses. Di
//!   sini kita jumlahkan semua instance bertipe `engtype_3d`, pendekatan
//!   umum dipakai banyak tool monitoring pihak ketiga dan biasanya paling
//!   dekat dengan angka "GPU %" di Task Manager.
//! - **Linux**: driver kernel `amdgpu` mengekspos angka ini LANGSUNG (sudah
//!   dihitung driver, bukan estimasi kita) lewat
//!   `/sys/class/drm/cardN/device/gpu_busy_percent` — cukup baca satu file
//!   teks, jauh lebih sederhana daripada jalur PDH di Windows. Path
//!   ditemukan sekali saat `new()` (cari card dengan PCI vendor ID `0x1002` =
//!   AMD/ATI), lalu dipakai ulang tiap `sample()`.
//!
//! Suhu GPU SENGAJA TIDAK diimplementasikan di sini — lihat `gpu_amd.rs`
//! (ADL di Windows, hwmon di Linux) untuk itu.

#[cfg(windows)]
mod imp {
    use windows::core::w;
    use windows::Win32::System::Performance::{
        PdhAddEnglishCounterW, PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhOpenQueryW,
        PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE,
    };

    pub struct GpuMonitor {
        // Handle PDH direpresentasikan sebagai `isize` mentah di crate
        // `windows` 0.58 (bukan tipe `PDH_HQUERY`/`PDH_HCOUNTER` terpisah).
        query: isize,
        counter: isize,
        // Counter tipe rate/utilization PDH butuh minimal 2 sample sebelum
        // nilainya valid — sample pertama selalu dibuang.
        primed: bool,
    }

    impl GpuMonitor {
        pub fn new() -> anyhow::Result<Self> {
            unsafe {
                let mut query: isize = 0;
                let status = PdhOpenQueryW(None, 0, &mut query);
                if status != 0 {
                    anyhow::bail!("PdhOpenQueryW gagal (kode {status:#x})");
                }

                let mut counter: isize = 0;
                // Wildcard "(*)" pada instance -> dibaca sebagai array berisi
                // SEMUA instance engine GPU (per proses, per jenis engine)
                // yang aktif saat itu.
                let path = w!(r"\GPU Engine(*)\Utilization Percentage");
                let status = PdhAddEnglishCounterW(query, path, 0, &mut counter);
                if status != 0 {
                    anyhow::bail!(
                        "PdhAddEnglishCounterW gagal (kode {status:#x}) — OS/driver mungkin \
                         tidak menyediakan counter 'GPU Engine' (butuh Windows 10 1803+ & WDDM 2.4+)"
                    );
                }

                Ok(Self {
                    query,
                    counter,
                    primed: false,
                })
            }
        }

        /// Total pemakaian GPU (0.0-100.0), atau `Ok(0.0)` selama belum ada
        /// aktivitas GPU 3D terdeteksi / sample pertama.
        pub fn sample(&mut self) -> anyhow::Result<f32> {
            unsafe {
                let status = PdhCollectQueryData(self.query);
                if status != 0 {
                    anyhow::bail!("PdhCollectQueryData gagal (kode {status:#x})");
                }

                if !self.primed {
                    self.primed = true;
                    return Ok(0.0);
                }

                let mut buffer_size: u32 = 0;
                let mut item_count: u32 = 0;
                // Panggilan pertama sengaja dibiarkan gagal (buffer belum
                // dialokasikan) cuma untuk mendapatkan ukuran buffer yang
                // dibutuhkan lewat `buffer_size`.
                let _ = PdhGetFormattedCounterArrayW(
                    self.counter,
                    PDH_FMT_DOUBLE,
                    &mut buffer_size,
                    &mut item_count,
                    None,
                );
                if buffer_size == 0 {
                    return Ok(0.0);
                }

                let mut buffer: Vec<u8> = vec![0u8; buffer_size as usize];
                let items_ptr = buffer.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
                let status = PdhGetFormattedCounterArrayW(
                    self.counter,
                    PDH_FMT_DOUBLE,
                    &mut buffer_size,
                    &mut item_count,
                    Some(items_ptr),
                );
                if status != 0 {
                    anyhow::bail!("PdhGetFormattedCounterArrayW gagal (kode {status:#x})");
                }

                let items = std::slice::from_raw_parts(items_ptr, item_count as usize);
                let mut total = 0.0f64;
                for item in items {
                    if item.szName.is_null() {
                        continue;
                    }
                    let name = item.szName.to_string().unwrap_or_default();
                    if name.to_ascii_lowercase().contains("engtype_3d") {
                        total += item.FmtValue.Anonymous.doubleValue;
                    }
                }
                Ok(total.clamp(0.0, 100.0) as f32)
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::fs;
    use std::path::PathBuf;

    pub struct GpuMonitor {
        busy_path: Option<PathBuf>,
    }

    impl GpuMonitor {
        pub fn new() -> anyhow::Result<Self> {
            let busy_path = find_amd_gpu_busy_path();
            if busy_path.is_none() {
                eprintln!(
                    "PERINGATAN: GPU usage tidak tersedia — tidak ketemu \
                     /sys/class/drm/card*/device/gpu_busy_percent untuk GPU AMD \
                     (driver kernel amdgpu). Baris info akan menampilkan N/A."
                );
            }
            Ok(Self { busy_path })
        }

        /// Total pemakaian GPU (0.0-100.0) — sudah dihitung langsung oleh
        /// driver kernel amdgpu, tinggal dibaca.
        pub fn sample(&mut self) -> anyhow::Result<f32> {
            let Some(path) = &self.busy_path else {
                return Ok(0.0);
            };
            let text = fs::read_to_string(path).unwrap_or_default();
            let pct: f32 = text.trim().parse().unwrap_or(0.0);
            Ok(pct.clamp(0.0, 100.0))
        }
    }

    /// Cari `/sys/class/drm/cardN/device/gpu_busy_percent` untuk card dengan
    /// PCI vendor ID `0x1002` (AMD/ATI). Melewati entry semacam
    /// "cardN-HDMI-A-1" (itu koneksi display, bukan device GPU itu sendiri).
    fn find_amd_gpu_busy_path() -> Option<PathBuf> {
        let entries = fs::read_dir("/sys/class/drm").ok()?;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Some(suffix) = name.strip_prefix("card") else { continue };
            if suffix.is_empty() || !suffix.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }

            let device_dir = entry.path().join("device");
            if let Ok(vendor) = fs::read_to_string(device_dir.join("vendor")) {
                if vendor.trim().eq_ignore_ascii_case("0x1002") {
                    let busy_path = device_dir.join("gpu_busy_percent");
                    if busy_path.exists() {
                        return Some(busy_path);
                    }
                }
            }
        }
        None
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod imp {
    /// Stub platform lain: GPU usage tidak tersedia, selalu `Ok(0.0)`.
    pub struct GpuMonitor;

    impl GpuMonitor {
        pub fn new() -> anyhow::Result<Self> {
            Ok(Self)
        }

        pub fn sample(&mut self) -> anyhow::Result<f32> {
            Ok(0.0)
        }
    }
}

pub use imp::GpuMonitor;
