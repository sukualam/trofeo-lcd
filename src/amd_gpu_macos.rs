//! Pembacaan statistik GPU AMD di macOS, lewat IOKit registry (`ioreg`).
//!
//! Driver AMD di macOS menaruh semua sensor di satu dictionary
//! `PerformanceStatistics` pada node `IOAccelerator` miliknya, antara lain
//! `Temperature(C)`, `Total Power(W)`, `Fan Speed(RPM)`, dan `GPU Activity(%)`.
//! Satu panggilan `ioreg` jadi cukup untuk semuanya.
//!
//! Dua hal yang perlu diperhatikan di platform ini:
//!
//! 1. **Nama node mengandung model GPU** (`AMDRadeonX6000_AMDNavi23…`), jadi
//!    nama class tidak bisa dipatok. Query-nya memakai class dasar
//!    `IOAccelerator` (cocok juga untuk Intel/NVIDIA) lalu node difilter lewat
//!    awalan nama `AMDRadeon` di dalam program.
//! 2. **Node yang sama bisa muncul lebih dari sekali** di tree IORegistry
//!    (terdaftar di dua cabang service tree), jadi yang dipakai adalah
//!    kemunculan pertama yang namanya AMD — bukan dijumlahkan.
//!
//! `ioreg` keluar ~15 ms, sementara `gpu.rs` (usage) dan `gpu_amd.rs`
//! (suhu/power/fan) sama-sama dipanggil pada loop refresh yang sama. supaya
//! tidak spawn dua subprocess untuk data yang identik, hasilnya di-cache
//! sebentar: panggilan kedua dalam jendela cache akan menerima snapshot yang
//! sama.

use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Jeda validitas cache.
///
/// `SYSINFO_REFRESH_INTERVAL` di `main.rs` = 500 ms, dan `gpu.rs` +
/// `gpu_amd.rs` dipanggil pada refresh yang sama. Dengan TTL 600 ms, satu
/// putaran hanya menjalankan satu `ioreg`: pembacaan pertama mengisi cache,
/// pembacaan kedua di putaran yang sama kena cache, dan putaran berikutnya
/// (umur 500 ms) juga masih kena cache — jadi `ioreg` baru jalan lagi pada
/// t=1000 ms. Hasilnya ~1 spawn/detik, bukan 2.
///
/// TTL harus DI ATAS 500 ms. Kalau lebih kecil, cache selalu basi di refresh
/// berikutnya sehingga jadi dua spawn per detik sia-sia.
const CACHE_TTL: Duration = Duration::from_millis(600);

/// Awalan nama registry entry node GPU AMD (dipakai untuk menyaring node
/// Intel/NVIDIA yang juga ikut cocok dengan class `IOAccelerator`).
const AMD_NODE_PREFIX: &str = "AMDRadeon";

/// Satu pembacaan sensor GPU AMD. Semua field `Option` — `None` kalau key-nya
/// tidak ada di dictionary (GPU/driver lama) atau nilainya tidak bermakna
/// (mis. fan 0 RPM pada card tanpa kipas).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Stats {
    /// Beban GPU (%), setara "GPU usage" di Activity Monitor.
    pub activity_pct: Option<i32>,
    /// Suhu die/edge (°C).
    pub temp_c: Option<i32>,
    /// Konsumsi daya chip GPU (Watt).
    pub power_w: Option<i32>,
    /// Kecepatan kipas (RPM). `None` kalau 0 — kartu pasif tidak punya kipas
    /// sama sekali, jadi menampilkan "0 RPM" akan menyesatkan.
    pub fan_rpm: Option<i32>,
}

impl Stats {
    /// True kalau tidak ada satu pun sensor yang terbaca.
    pub fn is_empty(&self) -> bool {
        self.activity_pct.is_none()
            && self.temp_c.is_none()
            && self.power_w.is_none()
            && self.fan_rpm.is_none()
    }
}

/// Snapshot terakhir + waktu bacanya, biar permintaan berdekatan (dari `gpu.rs`
/// dan `gpu_amd.rs`) tidak mengulang `ioreg` di detik yang sama.
static CACHE: Mutex<Option<(Instant, Option<Stats>)>> = Mutex::new(None);

/// Baca statistik GPU AMD terbaru. Mengembalikan `None` kalau tidak ada node
/// AMD sama sekali (GPU Intel/NVIDIA, atau belum ada GPU yang terpasang).
pub fn read() -> Option<Stats> {
    let mut cache = CACHE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((taken_at, snapshot)) = cache.as_ref() {
        if taken_at.elapsed() < CACHE_TTL {
            return *snapshot;
        }
    }
    let fresh = query();
    *cache = Some((Instant::now(), fresh));
    fresh
}

/// True kalau driver AMD terdaftar di IORegistry — dipakai untuk pesan
/// peringatan saat start, supaya pengguna tahu kenapa sensor tetap N/A.
pub fn driver_present() -> bool {
    read().is_some()
}

/// Jalankan `ioreg` dan ambil dictionary `PerformanceStatistics` pertama
/// milik node AMD.
fn query() -> Option<Stats> {
    let out = Command::new("ioreg")
        .args(["-c", "IOAccelerator", "-r", "-d", "1", "-l"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);

    // Ikuti node yang sedang dibaca: di output `ioreg`, setiap blok node
    // diawali baris "+-o <nama>" lalu diikuti propertinya. Simpan apakah node
    // itu milik AMD, dan reset setiap kali lewat node baru supaya blok node
    // Intel tidak mewarisi flag dari blok sebelumnya.
    let mut node_is_amd = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("+-o ") {
            node_is_amd = trimmed[4..].trim_start().starts_with(AMD_NODE_PREFIX);
            continue;
        }
        if !node_is_amd || !trimmed.contains("\"PerformanceStatistics\" = {") {
            continue;
        }

        let activity_pct = stat_field(line, "GPU Activity(%)");
        let temp_c = stat_field(line, "Temperature(C)");
        let power_w = stat_field(line, "Total Power(W)");
        // 0 RPM = tidak ada kipas yang berputar (card pasif) ATAU driver tidak
        // melapor. Keduanya lebih jujur ditampilkan sebagai N/A daripada "0".
        let fan_rpm = stat_field(line, "Fan Speed(RPM)").filter(|&rpm| rpm > 0);

        let stats = Stats {
            activity_pct,
            temp_c,
            power_w,
            fan_rpm,
        };
        // Kalau dictionary-nya ada tapi seluruh key yang kita cari tidak ada,
        // itu meananya node ini bukan yang kita maksud — terus ke node
        // berikutnya, jangan langsung return.
        if stats.is_empty() {
            continue;
        }
        return Some(stats);
    }
    None
}

/// Ambil nilai numerik satu key dari dictionary `PerformanceStatistics`.
///
/// Formatnya `"Temperature(C)"=57` (tanpa spasi sebelum `=`), pasangan
/// berikutnya dipisah koma. Pencocokan menyertakan tanda kutip sebelum nama key
/// supaya `"Total Time (Read)"` tidak ikut cocok dengan `"Total Power(W)"`.
fn stat_field(line: &str, key: &str) -> Option<i32> {
    let pat = format!("\"{key}\"=");
    let start = line.find(&pat)? + pat.len();
    let rest = &line[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nilai selalu integer di dictionary ini; dan `Total Power(W)` harus
    /// tidak tertukar dengan `Total Time (Read)`/key lain yang mirip.
    #[test]
    fn stat_field_reads_exact_key() {
        let line = r#""PerformanceStatistics" = {"Total Time (Read)"=432,"Total Power(W)"=24,"GPU Activity(%)"=13,"Temperature(C)"=49}"#;
        assert_eq!(stat_field(line, "Total Power(W)"), Some(24));
        assert_eq!(stat_field(line, "GPU Activity(%)"), Some(13));
        assert_eq!(stat_field(line, "Temperature(C)"), Some(49));
        assert_eq!(stat_field(line, "Nonexistent"), None);
    }

    /// Nilai bisa 0 (GPU idle / load 0%) dan boleh diakhiri koma atau `}`.
    #[test]
    fn stat_field_handles_zero_and_boundaries() {
        let line = r#""PerformanceStatistics" = {"GPU Activity(%)"=0,"Fan Speed(RPM)"=1830}"#;
        assert_eq!(stat_field(line, "GPU Activity(%)"), Some(0));
        assert_eq!(stat_field(line, "Fan Speed(RPM)"), Some(1830));
    }

    /// End-to-end ke IOKit sungguhan: pada mesin ini ada GPU AMD, jadi
    /// `read()` harus mengembalikan minimal satu sensor.
    #[test]
    fn reads_live_gpu() {
        match read() {
            Some(s) => {
                println!("{s:?}");
                assert!(
                    s.temp_c.is_some() || s.activity_pct.is_some(),
                    "node AMD ketemu tapi tidak ada sensor yang terbaca: {s:?}"
                );
            }
            None => println!("tidak ada GPU AMD terdeteksi (lewati)"),
        }
    }

    /// Cache harus benar-benar bekerja: panggilan kedua dalam jendela TTL
    /// tidak boleh spawn `ioreg` lagi. Kalau bocor, `gpu.rs` dan `gpu_amd.rs`
    /// akan masing-masing menjalankan `ioreg` sendiri di refresh yang sama.
    #[test]
    fn cache_avoids_second_spawn() {
        let first = read();
        let t0 = Instant::now();
        let second = read();
        let elapsed = t0.elapsed();

        assert_eq!(first, second, "cache mengembalikan data berbeda");
        // `ioreg` sendiri ~15 ms; cache hit harus jauh di bawah itu.
        assert!(
            elapsed < Duration::from_millis(5),
            "panggilan kedua butuh {elapsed:?} — kayaknya cache tidak kepakai"
        );
    }
}
