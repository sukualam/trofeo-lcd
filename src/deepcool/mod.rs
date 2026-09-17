//! Integrasi display **DeepCool Digital** (AIO/air cooler/casing) ke
//! trofeo-lcd — supaya cooler DeepCool tetap menampilkan data CPU tanpa harus
//! menjalankan program terpisah; semuanya jadi satu dengan trofeo-lcd.
//!
//! Device DeepCool digerakkan lewat HID (`hidapi`), sedangkan data sensornya
//! DIAMBIL DARI monitor yang sudah dimiliki trofeo-lcd sendiri:
//! - **Suhu + power** CPU: `CpuSensor` (PawnIO di Windows / sysfs di Linux) —
//!   instance yang SAMA (dibagi lewat `Arc<Mutex<_>>`) dengan baris info
//!   utama, jadi angka di kedua layar konsisten dan cuma buka satu handle
//!   driver.
//! - **Usage** CPU: `sysinfo::System` milik thread ini sendiri (baseline
//!   diambil di `read_instant()`, selisih di `get_usage()` — sama seperti
//!   `CpuInstant` di proyek deepcool).
//! - **Frekuensi**: `CpuFreq` (PDH di Windows / sysfs cpufreq di Linux).
//!
//! Driver per-device (`src/deepcool/*.rs`) di-port baris-per-baris dari proyek
//! [deepcool-digital-linux](https://github.com/Nortank12/deepcool-digital-linux)
//! (versi yang sama dengan yang dipakai `deepcool-digital-windows` di
//! `../deepcool`), hanya import/makro-nya yang disesuaikan. Loop utamanya
//! berjalan di **thread background** — kalau device tidak ketemu atau
//! tercabut, thread mencoba lagi otomatis, program utama (Trofeo LCD) tetap
//! jalan normal.
//!
//! Semua ini HANYA aktif kalau `hidapi` bisa membuka device. Kalau tidak ada
//! device DeepCool sama sekali, thread diam-diam mencoba setiap beberapa
//! detik (biaya kecil: enumerate HID) tanpa mengganggu loop utama.

pub mod ag_series;
pub mod ak400_pro;
pub mod ak620_pro;
pub mod ak_series;
pub mod ch510;
pub mod ch_series;
pub mod ch_series_gen2;
pub mod ld_series;
pub mod lp_series;
pub mod lq_series;
pub mod ls_series;

use crate::cpu_freq::CpuFreq;
use crate::cpu_sensor::CpuSensor;
use hidapi::HidApi;
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use sysinfo::System;

/// Vendor ID utama device DeepCool (HID).
pub const DEFAULT_VENDOR_ID: u16 = 13875;
/// Vendor ID casing CH510 (beda dari device DeepCool lain).
pub const CH510_VENDOR_ID: u16 = 13523;
/// Product ID casing CH510 MESH DIGITAL.
pub const CH510_PRODUCT_ID: u16 = 4352;

/// Periode pergantian mode di `Mode::Auto` — device DeepCool mengganti tampilan
/// setiap interval ini.
pub const AUTO_MODE_INTERVAL: Duration = Duration::from_millis(5000);

#[derive(PartialEq)]
pub enum Mode {
    Default,
    Auto,
    CpuTemperature,
    CpuUsage,
    CpuPower,
    CpuFrequency,
    CpuFan,
    GpuTemperature,
    GpuUsage,
    GpuPower,
    Cpu,
    Gpu,
    Psu,
}

impl Mode {
    pub const fn symbol(&self) -> &'static str {
        match self {
            Mode::Default => "",
            Mode::Auto => "auto",
            Mode::CpuTemperature => "cpu_temp",
            Mode::CpuUsage => "cpu_usage",
            Mode::CpuPower => "cpu_power",
            Mode::CpuFrequency => "cpu_freq",
            Mode::CpuFan => "cpu_fan",
            Mode::GpuTemperature => "gpu_temp",
            Mode::GpuUsage => "gpu_usage",
            Mode::GpuPower => "gpu_power",
            Mode::Cpu => "cpu",
            Mode::Gpu => "gpu",
            Mode::Psu => "psu",
        }
    }

    /// Sisi-error dari validasi mode — di trofeo-lcd tidak dibikin `exit(1)`
    /// seperti aslinya: arus data device cuma pindah ke mode default supaya
    /// thread tidak mati. Constructor hanya pernah menerima `Mode::Default`
    /// (dispatcher), jadi praktis tidak pernah terpanggil.
    pub fn support_error(&self) -> Mode {
        eprintln!(
            "DeepCool: display mode \"{}\" tidak didukung device ini — pakai default.",
            self.symbol()
        );
        self.clone()
    }

    /// Sama seperti `support_error`, untuk secondary display mode.
    pub fn support_error_secondary(&self) -> Mode {
        eprintln!(
            "DeepCool: secondary display mode \"{}\" tidak didukung device ini — pakai default.",
            self.symbol()
        );
        self.clone()
    }
}

impl Clone for Mode {
    fn clone(&self) -> Self {
        match self {
            Mode::Default => Mode::Default,
            Mode::Auto => Mode::Auto,
            Mode::CpuTemperature => Mode::CpuTemperature,
            Mode::CpuUsage => Mode::CpuUsage,
            Mode::CpuPower => Mode::CpuPower,
            Mode::CpuFrequency => Mode::CpuFrequency,
            Mode::CpuFan => Mode::CpuFan,
            Mode::GpuTemperature => Mode::GpuTemperature,
            Mode::GpuUsage => Mode::GpuUsage,
            Mode::GpuPower => Mode::GpuPower,
            Mode::Cpu => Mode::Cpu,
            Mode::Gpu => Mode::Gpu,
            Mode::Psu => Mode::Psu,
        }
    }
}

/// Pena suhu/usage/power/frekuensi CPU untuk driver device DeepCool, yang
/// menyambungkan ke sensor milik trofeo-lcd (`CpuSensor`, `CpuFreq`, sysinfo).
///
/// Tiap method meniru API `Cpu` di proyek deepcool (`get_temp(fahrenheit)`,
/// `read_energy()`, `get_power(prev, delta_ms)`, `get_usage(instant)`,
/// `get_frequency()`), agar driver-device bisa di-port nyaris tapa perubahan.
pub struct DeepCpu {
    sensor: Arc<Mutex<CpuSensor>>,
    freq: RefCell<CpuFreq>,
    sys: RefCell<System>,
    temp_warned: AtomicBool,
    power_warned: AtomicBool,
}

/// Snapshot baseline usage CPU (selisih dihitung di `DeepCpu::get_usage`).
pub struct CpuInstant;

impl DeepCpu {
    pub fn new(sensor: Arc<Mutex<CpuSensor>>) -> Self {
        DeepCpu {
            sensor,
            freq: RefCell::new(CpuFreq::new()),
            sys: RefCell::new(System::new()),
            temp_warned: AtomicBool::new(false),
            power_warned: AtomicBool::new(false),
        }
    }

    /// Ambil baseline usage CPU (reset counter `sysinfo`). Pasangannya
    /// `get_usage()` — dipanggil berurutan dengan sleep di antaranya, seperti
    /// `CpuInstant` di proyek deepcool.
    pub fn read_instant(&self) -> CpuInstant {
        self.sys.borrow_mut().refresh_cpu();
        CpuInstant
    }

    /// Usage CPU (`0-99`) sejak `read_instant()` terakhir dipanggil.
    pub fn get_usage(&self, _instant: &CpuInstant) -> u8 {
        let mut sys = self.sys.borrow_mut();
        sys.refresh_cpu();
        sys.global_cpu_info().cpu_usage().round().clamp(0.0, 99.0) as u8
    }

    /// Suhu paket CPU, `°C` atau `°F`. `0` kalau sensor tidak tersedia
    /// (makanya `get_temp` di proyek deepcool juga mengembalikan `0`).
    pub fn get_temp(&self, fahrenheit: bool) -> u8 {
        let c = self.sensor.lock().ok().and_then(|s| s.get_temp_c());
        match c {
            Some(c) => {
                let v = if fahrenheit { c * 9.0 / 5.0 + 32.0 } else { c };
                (v.round().max(0.0) as i64).min(i64::from(u8::MAX)) as u8
            }
            None => 0,
        }
    }

    /// Snapshot counter energi kumulatif — pasangannya `get_power()`.
    pub fn read_energy(&self) -> u64 {
        self.sensor.lock().ok().map(|s| s.sample_energy()).unwrap_or(0)
    }

    /// Rata-rata power draw paket CPU (Watt) sejak `prev_energy` diambil,
    /// `delta_ms` milidetik yang lalu. `0` kalau sensor tidak tersedia.
    pub fn get_power(&self, prev_energy: u64, delta_ms: u64) -> u16 {
        match self.sensor.lock().ok().and_then(|s| s.calc_power_watts(prev_energy, delta_ms)) {
            Some(w) => (w.round() as i64).clamp(0, i64::from(u16::MAX)) as u16,
            None => 0,
        }
    }

    /// Frekuensi CPU real-time (MHz). `0` kalau tidak tersedia.
    pub fn get_frequency(&self) -> u16 {
        match self.freq.borrow_mut().sample_mhz() {
            Some(m) => (m as i64).clamp(0, i64::from(u16::MAX)) as u16,
            None => 0,
        }
    }

    /// Warning satu kali kalau sensor suhu CPU tidak tersedia (jangan spam
    /// tiap reconnect).
    pub fn warn_temp(&self) {
        let available = self.sensor.lock().ok().and_then(|s| s.get_temp_c()).is_some();
        if !available && !self.temp_warned.swap(true, Ordering::Relaxed) {
            eprintln!(
                "DeepCool: sensor suhu CPU tidak tersedia (PawnIO/driver), layar \
                 cooler akan menampilkan 0."
            );
        }
    }

    /// Warning satu kali kalau sensor power CPU tidak tersedia.
    pub fn warn_rapl(&self) {
        let available = self.sensor.lock().ok().and_then(|s| s.calc_power_watts(0, 500)).is_some();
        if !available && !self.power_warned.swap(true, Ordering::Relaxed) {
            eprintln!(
                "DeepCool: sensor power CPU tidak tersedia (PawnIO/driver RAPL), \
                 layar cooler akan menampilkan 0."
            );
        }
    }
}

/// Stub GPU — di proyek deepcool, monitor GPU memang belum diimplementasikan
/// (hanya casing saja yang memakainya, dan di semua-nya 0). Dipertahankan
/// supaya driver CH/LP series bisa di-port tanpa perubahan berarti.
pub struct Gpu;

impl Gpu {
    pub fn get_temp(&self, _fahrenheit: bool) -> u8 {
        0
    }
    pub fn get_usage(&self) -> u8 {
        0
    }
    pub fn get_power(&self) -> u16 {
        0
    }
    pub fn get_frequency(&self) -> u16 {
        0
    }
    pub fn warn_missing(&self) {
        static WARNED: AtomicBool = AtomicBool::new(false);
        if !WARNED.swap(true, Ordering::Relaxed) {
            eprintln!("DeepCool: monitor GPU tidak diimplementasikan, bagian GPU di casing akan 0.");
        }
    }
}

/// Opsi thread integrasi DeepCool (lihat `spawn`).
pub struct Options {
    /// Interval pengiriman data ke display DeepCool, ms (dibatasi 100-2000).
    pub update_ms: u64,
}

/// Jalankan driver DeepCool di thread background. Program utama (Trofeo LCD)
/// tidak tergantung sama sekali pada thread ini: kalau device tidak ketemu,
/// cuma mencoba lagi beberapa detik kemudian.
pub fn spawn(sensor: Arc<Mutex<CpuSensor>>, opts: Options) {
    std::thread::Builder::new()
        .name("deepcool".to_string())
        .spawn(move || {
            let update = Duration::from_millis(opts.update_ms.clamp(100, 2000));

            loop {
                let api = match HidApi::new() {
                    Ok(api) => api,
                    Err(_) => {
                        std::thread::sleep(Duration::from_secs(5));
                        continue;
                    }
                };

                // Cari device DeepCool: vendor default, atau casing CH510.
                let mut product_id: u16 = 0;
                for device in api.device_list() {
                    if device.vendor_id() == DEFAULT_VENDOR_ID {
                        product_id = device.product_id();
                        break;
                    } else if device.vendor_id() == CH510_VENDOR_ID
                        && device.product_id() == CH510_PRODUCT_ID
                    {
                        product_id = device.product_id();
                        break;
                    }
                }
                if product_id == 0 {
                    std::thread::sleep(Duration::from_secs(5));
                    continue;
                }

                // Driver device memakai `.unwrap()` pada `device.write()` —
                // kalau device tercabut, panic terjadi; catch di sini supaya
                // loop bisa mencoba lagi (bukan thread mati diam-diam).
                let cpu = DeepCpu::new(Arc::clone(&sensor));
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    match product_id {
                        // AK Series (AK400/500/620 DIGITAL...)
                        1..=4 => {
                            let mut d = ak_series::Display::new(
                                cpu, &Mode::Default,
                                update, false, false,
                            );
                            d.run(&api, DEFAULT_VENDOR_ID, product_id);
                        }
                        // LS Series (LS520 SE / LS720 SE DIGITAL)
                        6 => {
                            let d = ls_series::Display::new(
                                cpu, &Mode::Default, update, false, false,
                            );
                            d.run(&api, DEFAULT_VENDOR_ID, product_id);
                        }
                        // AG Series (AG300/400/500/620 DIGITAL)
                        8 => {
                            let d = ag_series::Display::new(cpu, &Mode::Default, update, false);
                            d.run(&api, DEFAULT_VENDOR_ID, product_id);
                        }
                        // LD Series (LD240/LD360)
                        10 => {
                            let d = ld_series::Display::new(cpu, update, false, false);
                            d.run(&api, DEFAULT_VENDOR_ID, product_id);
                        }
                        // LP Series (LP240/LP360)
                        12 => {
                            let d = lp_series::Display::new(
                                cpu, Gpu,
                                &Mode::Default, &Mode::Default, update, false, 0,
                            );
                            d.run(&api, DEFAULT_VENDOR_ID, product_id);
                        }
                        // LQ Series, ASSASSIN IV, AK G2 Series, AK700
                        13 | 15 | 31 | 41 | 42 | 43 | 44 => {
                            let d = lq_series::Display::new(cpu, update, false);
                            d.run(&api, DEFAULT_VENDOR_ID, product_id);
                        }
                        // AK400 PRO
                        16 => {
                            let d = ak400_pro::Display::new(cpu, update, false);
                            d.run(&api, DEFAULT_VENDOR_ID, product_id);
                        }
                        // AK500 / AK620 PRO
                        17 | 18 => {
                            let d = ak620_pro::Display::new(cpu, update, false);
                            d.run(&api, DEFAULT_VENDOR_ID, product_id);
                        }
                        // CH170 | CH270 | CH690
                        19 | 22 | 27 => {
                            let d = ch_series_gen2::Display::new(
                                cpu, Gpu, &Mode::Default, update, false,
                            );
                            d.run(&api, DEFAULT_VENDOR_ID, product_id);
                        }
                        // CH Series & MORPHEUS
                        5 | 7 | 21 => {
                            let d = ch_series::Display::new(
                                cpu, Gpu, &Mode::Default, &Mode::Default, update, false,
                            );
                            d.run(&api, DEFAULT_VENDOR_ID, product_id);
                        }
                        // CH510 MESH DIGITAL
                        CH510_PRODUCT_ID => {
                            let d = ch510::Display::new(cpu, Gpu, &Mode::Default, update, false);
                            d.run(&api, CH510_VENDOR_ID, product_id);
                        }
                        _ => {
                            eprintln!(
                                "DeepCool: device terdeteksi (PID {product_id}) tapi belum \
                                 didukung program ini — mencoba lagi nanti."
                            );
                        }
                    }
                }));
                std::mem::drop(result);
                eprintln!("DeepCool: device tercabut/gagal, mencari lagi...");
                std::thread::sleep(Duration::from_secs(2));
            }
        })
        .ok();
}