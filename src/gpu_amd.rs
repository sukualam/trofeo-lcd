//! Sensor GPU AMD (suhu, power, fan) via **AMD Display Library (ADL)** —
//! lebih tepatnya jalur PMLog (`ADL2_New_QueryPMLogData_Get`) yang dipakai
//! GPU RDNA/RDNA2/RDNA3 (RX 5000 ke atas), termasuk RX 6600.
//!
//! `atiadlxx.dll` sudah otomatis ter-install bersama driver Radeon — tidak
//! perlu SDK ADL, tidak perlu software tambahan. DLL di-load secara dynamic
//! (seperti pawnio.rs), jadi build tetap berhasil di mesin tanpa GPU AMD dan
//! program tetap jalan (sensor N/A) di mesin tanpa GPU/driver AMD.
//!
//! Empat nilai yang ditampilkan:
//! - **Suhu Edge** (°C) — suhu permukaan die GPU, setara "GPU Temperature" di
//!   Radeon Software/HWiNFO. Sensor index 8 di enum `ADL_PMLOG_SENSORS`.
//! - **ASIC Power** (Watt) — konsumsi daya seluruh chip GPU. Sensor index 23.
//! - **Fan RPM** — kecepatan kipas aktual. Sensor index 14.
//! - **Fullscreen FPS** — lewat jalur ADL yang beda (`ADL2_Adapter_FrameMetrics_*`,
//!   bukan PMLog), dipakai context ADL2 yang sama supaya tidak buka koneksi
//!   kedua. Hanya terisi kalau ada game yang jalan di mode *exclusive
//!   fullscreen* sungguhan — mode "borderless windowed" tidak terdeteksi,
//!   ini keterbatasan ADL sendiri, bukan bug di sini.
//!
//! Struct ini JUGA menyimpan Hotspot/Junction temp (index 27) supaya mudah
//! ditambahkan ke tampilan nanti kalau perlu, tapi sementara tidak ditampilkan
//! (terlalu ramai di baris 1 yang sudah padat).
//!
//! Sumber: ADL SDK resmi AMD (`adl_structures.h`, field `ADL_PMLOG_SENSORS`),
//! dikonfirmasi dengan output `adl_probe.exe` di hardware RX 6600 sendiri.

#[cfg(windows)]
mod imp {
    use std::ffi::{c_int, c_void, CString};

    use windows::core::PCSTR;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};

    // === Tipe dari adl_structures.h ===

    const ADL_MAX_PATH: usize = 256;

    /// Subset `AdapterInfo` (adl_structures.h). Layout HARUS byte-exact dengan
    /// header C asli karena dibaca langsung dari memori yang diisi DLL.
    #[repr(C)]
    struct AdapterInfo {
        size: c_int,
        adapter_index: c_int,
        udid: [i8; ADL_MAX_PATH],
        bus_number: c_int,
        device_number: c_int,
        function_number: c_int,
        vendor_id: c_int,
        adapter_name: [i8; ADL_MAX_PATH],
        display_name: [i8; ADL_MAX_PATH],
        present: c_int,
        exist: c_int,
        driver_path: [i8; ADL_MAX_PATH],
        driver_path_ext: [i8; ADL_MAX_PATH],
        pnp_string: [i8; ADL_MAX_PATH],
        os_display_index: c_int,
    }

    impl Default for AdapterInfo {
        fn default() -> Self {
            // SAFETY: semua field numerik/array byte, zeroed-out valid.
            unsafe { std::mem::zeroed() }
        }
    }

    /// Satu entri sensor di `ADLPMLogDataOutput.sensors[]`.
    /// `supported` adalah Win32 BOOL (4 byte), bukan bool 1 byte C++.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct AdlSingleSensorData {
        supported: c_int,
        value: c_int,
    }

    /// `ADLPMLogDataOutput` (adl_structures.h): array `sensors[256]` di-index
    /// langsung pakai ID sensor dari enum `ADL_PMLOG_SENSORS` — bukan list
    /// terkemas. `sensors[8]` = suhu Edge, `sensors[23]` = ASIC power, dst.
    #[repr(C)]
    struct AdlPMLogDataOutput {
        size: c_int,
        sensors: [AdlSingleSensorData; 256],
    }

    impl Default for AdlPMLogDataOutput {
        fn default() -> Self {
            unsafe { std::mem::zeroed() }
        }
    }

    // ID sensor dari enum ADL_PMLOG_SENSORS (adl_structures.h), dikonfirmasi
    // dengan output adl_probe.exe di RX 6600.
    pub(super) const PMLOG_TEMPERATURE_EDGE: usize = 8;
    pub(super) const PMLOG_TEMPERATURE_HOTSPOT: usize = 27;
    pub(super) const PMLOG_FAN_RPM: usize = 14;
    pub(super) const PMLOG_ASIC_POWER: usize = 23;

    // === Tipe function pointer, di-resolve dari DLL ===

    type AdlMallocFn = unsafe extern "system" fn(c_int) -> *mut c_void;
    type AdlMainControlCreateFn = unsafe extern "system" fn(AdlMallocFn, c_int) -> c_int;
    type AdlMainControlDestroyFn = unsafe extern "system" fn() -> c_int;
    type AdlAdaptersGetFn = unsafe extern "system" fn(*mut c_int) -> c_int;
    type AdlAdapterInfoGetFn = unsafe extern "system" fn(*mut AdapterInfo, c_int) -> c_int;
    type AdlContext = *mut c_void;
    type AdlMainControlCreate2Fn =
        unsafe extern "system" fn(AdlMallocFn, c_int, *mut AdlContext) -> c_int;
    type AdlMainControlDestroy2Fn = unsafe extern "system" fn(AdlContext) -> c_int;
    type AdlPMLogQueryFn =
        unsafe extern "system" fn(AdlContext, c_int, *mut AdlPMLogDataOutput) -> c_int;

    // FrameMetrics (Fullscreen FPS) — jalur ADL terpisah dari PMLog, tapi
    // pakai context ADL2 dan adapter_index yang sama.
    type AdlFrameMetricsCapsFn = unsafe extern "system" fn(AdlContext, c_int, *mut c_int) -> c_int;
    type AdlFrameMetricsStartFn = unsafe extern "system" fn(AdlContext, c_int, c_int) -> c_int;
    type AdlFrameMetricsGetFn =
        unsafe extern "system" fn(AdlContext, c_int, c_int, *mut f32) -> c_int;
    type AdlFrameMetricsStopFn = unsafe extern "system" fn(AdlContext, c_int, c_int) -> c_int;

    // Callback alokasi yang diminta ADL saat init. ADL hanya memanggil ini
    // sesekali saat inisialisasi (bukan per-sample), jadi leak kecil di sini
    // tidak jadi masalah di konteks program yang jalan terus-menerus.
    unsafe extern "system" fn adl_malloc(size: c_int) -> *mut c_void {
        if size <= 0 {
            return std::ptr::null_mut();
        }
        match std::alloc::Layout::from_size_align(size as usize, 8) {
            Ok(layout) => std::alloc::alloc(layout) as *mut c_void,
            Err(_) => std::ptr::null_mut(),
        }
    }

    unsafe fn resolve<T: Copy>(module: HMODULE, name: &str) -> Option<T> {
        let c_name = CString::new(name).ok()?;
        let addr = GetProcAddress(module, PCSTR(c_name.as_ptr() as *const u8))?;
        Some(std::mem::transmute_copy(&addr))
    }

    /// State internal ADL: context ADL2 + adapter index target.
    /// Dibuat sekali di `GpuAmdSensor::new()` dan dipakai berulang tiap sample.
    pub(super) struct GpuAmdInner {
        query_fn: AdlPMLogQueryFn,
        // `Some` hanya kalau Caps + Start FrameMetrics berhasil saat init.
        frame_metrics_get_fn: Option<AdlFrameMetricsGetFn>,
        frame_metrics_stop_fn: Option<AdlFrameMetricsStopFn>,
        destroy2_fn: Option<AdlMainControlDestroy2Fn>,
        context: AdlContext,
        adapter_index: c_int,
    }

    impl GpuAmdInner {
        pub(super) fn new() -> Option<Self> {
            // Load DLL (ada di PATH setelah driver Radeon ter-install)
            let dll = CString::new("atiadlxx.dll").ok()?;
            let module = unsafe {
                LoadLibraryA(PCSTR(dll.as_ptr() as *const u8))
            }
            .ok()?;
            if module.is_invalid() {
                return None;
            }

            unsafe {
                // --- ADL v1: hanya buat enumerasi adapter, lalu kita destroy ---
                let create1 = resolve::<AdlMainControlCreateFn>(module, "ADL_Main_Control_Create")?;
                let get_num = resolve::<AdlAdaptersGetFn>(module, "ADL_Adapter_NumberOfAdapters_Get")?;
                let get_info = resolve::<AdlAdapterInfoGetFn>(module, "ADL_Adapter_AdapterInfo_Get")?;

                if create1(adl_malloc, 1) != 0 {
                    return None;
                }

                let mut num: c_int = 0;
                if get_num(&mut num) != 0 || num <= 0 {
                    return None;
                }

                let mut adapters: Vec<AdapterInfo> =
                    (0..num).map(|_| AdapterInfo::default()).collect();
                let buf_sz = (std::mem::size_of::<AdapterInfo>() as c_int) * num;
                if get_info(adapters.as_mut_ptr(), buf_sz) != 0 {
                    return None;
                }

                // Cari adapter AMD (vendor_id 1002 desimal — bukan hex 0x1002 =
                // 4098 — ini kelakuan ADL yang sedikit aneh, dikonfirmasi dari
                // output probe) yang "present" pertama.
                let adapter_index = adapters
                    .iter()
                    .find(|a| a.present != 0 && a.vendor_id == 1002)
                    .map(|a| a.adapter_index)?;

                // Selesai dengan ADL v1, bisa di-destroy sekarang.
                if let Some(destroy1) =
                    resolve::<AdlMainControlDestroyFn>(module, "ADL_Main_Control_Destroy")
                {
                    destroy1();
                }

                // --- ADL2: untuk PMLog query berulang kali ---
                let create2 =
                    resolve::<AdlMainControlCreate2Fn>(module, "ADL2_Main_Control_Create")?;
                let query_fn =
                    resolve::<AdlPMLogQueryFn>(module, "ADL2_New_QueryPMLogData_Get")?;
                let destroy2_fn =
                    resolve::<AdlMainControlDestroy2Fn>(module, "ADL2_Main_Control_Destroy");

                let mut context: AdlContext = std::ptr::null_mut();
                if create2(adl_malloc, 1, &mut context) != 0 || context.is_null() {
                    return None;
                }

                // --- FrameMetrics (Fullscreen FPS) — pakai context di atas ---
                // Kalau adapter tidak mendukung, atau salah satu simbol tidak
                // ada (driver lama), `frame_metrics_get_fn` tetap `None` dan
                // `sample()` otomatis melaporkan fps sebagai N/A. Tidak
                // mempengaruhi sensor PMLog yang sudah jalan di atas.
                let (frame_metrics_get_fn, frame_metrics_stop_fn) = (|| -> Option<(
                    AdlFrameMetricsGetFn,
                    AdlFrameMetricsStopFn,
                )> {
                    // Closure = scope baru: blok `unsafe` di fungsi luar TIDAK
                    // otomatis berlaku di sini, jadi perlu diulang eksplisit.
                    unsafe {
                        let caps_fn = resolve::<AdlFrameMetricsCapsFn>(
                            module,
                            "ADL2_Adapter_FrameMetrics_Caps",
                        )?;
                        let start_fn = resolve::<AdlFrameMetricsStartFn>(
                            module,
                            "ADL2_Adapter_FrameMetrics_Start",
                        )?;
                        let get_fn = resolve::<AdlFrameMetricsGetFn>(
                            module,
                            "ADL2_Adapter_FrameMetrics_Get",
                        )?;
                        let stop_fn = resolve::<AdlFrameMetricsStopFn>(
                            module,
                            "ADL2_Adapter_FrameMetrics_Stop",
                        )?;

                        let mut supported: c_int = 0;
                        if caps_fn(context, adapter_index, &mut supported) != 0 || supported == 0 {
                            return None;
                        }
                        if start_fn(context, adapter_index, 0) != 0 {
                            return None;
                        }
                        Some((get_fn, stop_fn))
                    }
                })()
                .map_or((None, None), |(g, s)| (Some(g), Some(s)));

                Some(GpuAmdInner {
                    query_fn,
                    frame_metrics_get_fn,
                    frame_metrics_stop_fn,
                    destroy2_fn,
                    context,
                    adapter_index,
                })
            }
        }

        pub(super) fn sample(&self) -> super::GpuAmdData {
            let mut output = AdlPMLogDataOutput::default();
            output.size = std::mem::size_of::<AdlPMLogDataOutput>() as c_int;

            let status = unsafe { (self.query_fn)(self.context, self.adapter_index, &mut output) };
            if status != 0 {
                return super::GpuAmdData::default();
            }

            // Baca nilai sensor — return None kalau `supported` == 0
            let get = |idx: usize| -> Option<i32> {
                let s = output.sensors[idx];
                if s.supported != 0 { Some(s.value) } else { None }
            };

            // Fullscreen FPS: `None` kalau FrameMetrics tidak didukung/gagal
            // start (lihat `new()`), ATAU kalau ADL mengembalikan -1 (artinya
            // tidak ada game exclusive-fullscreen yang sedang berjalan).
            let fps = self.frame_metrics_get_fn.and_then(|get_fn| {
                let mut value: f32 = -1.0;
                let status =
                    unsafe { get_fn(self.context, self.adapter_index, 0, &mut value) };
                if status == 0 && value >= 0.0 {
                    Some(value.round() as i32)
                } else {
                    None
                }
            });

            super::GpuAmdData {
                temp_edge_c: get(PMLOG_TEMPERATURE_EDGE),
                temp_hotspot_c: get(PMLOG_TEMPERATURE_HOTSPOT),
                power_w: get(PMLOG_ASIC_POWER),
                fan_rpm: get(PMLOG_FAN_RPM),
                fps,
            }
        }
    }

    impl Drop for GpuAmdInner {
        fn drop(&mut self) {
            if let Some(stop) = self.frame_metrics_stop_fn {
                unsafe { stop(self.context, self.adapter_index, 0) };
            }
            if let Some(destroy) = self.destroy2_fn {
                unsafe { destroy(self.context) };
            }
        }
    }

    // SAFETY: diakses dari 1 thread (main loop). Tidak ada concurrent access.
    unsafe impl Send for GpuAmdInner {}
}

// ---------------------------------------------------------------------------
// Linux: sysfs hwmon (kernel amdgpu) — tidak butuh library ADL/driver
// tambahan, cukup baca file teks di bawah
// /sys/class/drm/cardN/device/hwmon/hwmonM/.
//
// TIDAK ADA jalur untuk "Fullscreen FPS" di sini: itu fitur spesifik ADL
// Windows (`ADL2_Adapter_FrameMetrics_*`), tidak ada API generik yang sama
// di Linux — tiap compositor (X11/Wayland/gamescope) punya cara sendiri
// (atau tidak punya sama sekali) untuk mengekspos angka ini. `fps` selalu
// `None` di platform ini.
// ---------------------------------------------------------------------------
#[cfg(target_os = "linux")]
mod imp {
    use std::fs;
    use std::path::{Path, PathBuf};

    pub(super) struct GpuAmdInner {
        edge_path: Option<PathBuf>,
        hotspot_path: Option<PathBuf>,
        power_path: Option<PathBuf>,
        fan_path: Option<PathBuf>,
    }

    impl GpuAmdInner {
        pub(super) fn new() -> Option<Self> {
            let hwmon_dir = find_amd_gpu_hwmon()?;
            let edge_path = temp_path_for(&hwmon_dir, "edge", 1);
            let hotspot_path = temp_path_for(&hwmon_dir, "junction", 2);
            let power_path = existing(hwmon_dir.join("power1_average"))
                .or_else(|| existing(hwmon_dir.join("power1_input")));
            let fan_path = existing(hwmon_dir.join("fan1_input"));
            Some(Self { edge_path, hotspot_path, power_path, fan_path })
        }

        pub(super) fn sample(&self) -> super::GpuAmdData {
            super::GpuAmdData {
                temp_edge_c: self.edge_path.as_deref().and_then(read_i64).map(|m| (m / 1000) as i32),
                temp_hotspot_c: self.hotspot_path.as_deref().and_then(read_i64).map(|m| (m / 1000) as i32),
                power_w: self.power_path.as_deref().and_then(read_i64).map(|u| (u / 1_000_000) as i32),
                fan_rpm: self.fan_path.as_deref().and_then(read_i64).map(|v| v as i32),
                fps: None,
            }
        }
    }

    fn existing(path: PathBuf) -> Option<PathBuf> {
        path.exists().then_some(path)
    }

    fn read_i64(path: &Path) -> Option<i64> {
        fs::read_to_string(path).ok()?.trim().parse().ok()
    }

    /// Cari folder hwmon (`/sys/class/drm/cardN/device/hwmon/hwmonM`) untuk
    /// card dengan PCI vendor ID `0x1002` (AMD/ATI). Biasanya cuma ada 1
    /// subfolder hwmonM di dalam `device/hwmon/`.
    fn find_amd_gpu_hwmon() -> Option<PathBuf> {
        let entries = fs::read_dir("/sys/class/drm").ok()?;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Some(suffix) = name.strip_prefix("card") else { continue };
            if suffix.is_empty() || !suffix.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }

            let device_dir = entry.path().join("device");
            let is_amd = fs::read_to_string(device_dir.join("vendor"))
                .map(|v| v.trim().eq_ignore_ascii_case("0x1002"))
                .unwrap_or(false);
            if !is_amd {
                continue;
            }

            if let Ok(hwmon_entries) = fs::read_dir(device_dir.join("hwmon")) {
                if let Some(hw) = hwmon_entries.flatten().next() {
                    return Some(hw.path());
                }
            }
        }
        None
    }

    /// Cari `tempN_input` yang label-nya (`tempN_label`) mengandung
    /// `label_substr` (mis. "edge", "junction"). Kalau tidak ketemu (driver
    /// versi lama tanpa label), fallback ke `temp{fallback_index}_input`
    /// apa adanya.
    fn temp_path_for(hwmon_dir: &Path, label_substr: &str, fallback_index: u32) -> Option<PathBuf> {
        if let Ok(entries) = fs::read_dir(hwmon_dir) {
            for entry in entries.flatten() {
                let file_name = entry.file_name();
                let file_name = file_name.to_string_lossy();
                if let Some(idx) = file_name.strip_prefix("temp").and_then(|s| s.strip_suffix("_label")) {
                    if let Ok(label) = fs::read_to_string(entry.path()) {
                        if label.to_lowercase().contains(label_substr) {
                            return existing(hwmon_dir.join(format!("temp{idx}_input")));
                        }
                    }
                }
            }
        }
        existing(hwmon_dir.join(format!("temp{fallback_index}_input")))
    }
}

// ---------------------------------------------------------------------------

/// Hasil satu pembacaan sensor GPU AMD.
/// Semua field `Option` — `None` kalau sensor tidak didukung GPU/driver ini,
/// atau kalau ADL tidak tersedia sama sekali.
#[derive(Default)]
pub struct GpuAmdData {
    /// Suhu Edge/Die surface (°C) — setara "GPU Temperature" di HWiNFO.
    pub temp_edge_c: Option<i32>,
    /// Suhu Hotspot/Junction (°C) — titik terpanas di die. Ada tapi tidak
    /// ditampilkan di baris 1 supaya tidak terlalu ramai (mudah ditambah).
    pub temp_hotspot_c: Option<i32>,
    /// Konsumsi daya seluruh chip GPU (Watt).
    pub power_w: Option<i32>,
    /// Kecepatan kipas GPU (RPM).
    pub fan_rpm: Option<i32>,
    /// Fullscreen FPS via ADL FrameMetrics. `None` kalau tidak ada game
    /// exclusive-fullscreen yang sedang jalan, GPU/driver tidak mendukung,
    /// atau ADL tidak tersedia sama sekali.
    pub fps: Option<i32>,
}

#[cfg(windows)]
type GpuAmdInnerAlias = imp::GpuAmdInner;
#[cfg(target_os = "linux")]
type GpuAmdInnerAlias = imp::GpuAmdInner;
#[cfg(not(any(windows, target_os = "linux")))]
type GpuAmdInnerAlias = ();

/// Wrapper publik `GpuAmdSensor` — selalu bisa di-construct di semua platform,
/// tapi hanya aktif di Windows (GPU AMD + driver Radeon) atau Linux (GPU AMD
/// + driver kernel amdgpu).
pub struct GpuAmdSensor {
    inner: Option<GpuAmdInnerAlias>,
}

impl GpuAmdSensor {
    /// Inisialisasi. Kalau sensor tidak ditemukan (driver/DLL tidak ada, atau
    /// bukan GPU AMD), cetak peringatan dan lanjut (sensor N/A).
    pub fn new() -> Self {
        #[cfg(windows)]
        {
            let inner = imp::GpuAmdInner::new();
            if inner.is_none() {
                eprintln!(
                    "GPU AMD ADL: tidak tersedia — pastikan driver AMD Radeon \
                     terpasang dan GPU-nya AMD. Suhu/power/fan GPU akan N/A."
                );
            }
            return GpuAmdSensor { inner };
        }

        #[cfg(target_os = "linux")]
        {
            let inner = imp::GpuAmdInner::new();
            if inner.is_none() {
                eprintln!(
                    "GPU AMD hwmon: tidak tersedia — pastikan GPU-nya AMD dan \
                     driver kernel amdgpu aktif (cek: ls /sys/class/drm/*/device/hwmon). \
                     Suhu/power/fan GPU akan N/A. Fullscreen FPS memang selalu N/A \
                     di Linux (tidak ada API generiknya)."
                );
            }
            return GpuAmdSensor { inner };
        }

        #[cfg(not(any(windows, target_os = "linux")))]
        GpuAmdSensor { inner: None }
    }

    /// Baca sensor. Dipanggil tiap sysinfo refresh interval (~500 ms),
    /// bukan tiap frame — biayanya memang ringan tapi tidak perlu tiap frame.
    pub fn sample(&self) -> GpuAmdData {
        if let Some(inner) = &self.inner {
            return inner.sample();
        }
        GpuAmdData::default()
    }
}
