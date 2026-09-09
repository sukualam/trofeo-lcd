//! Sinkronisasi warna bar EQ dengan warna sebuah device di OpenRGB, lewat
//! *polling* (baca snapshot warna device tsb secara berkala) — BUKAN
//! mendaftarkan trofeo-lcd sebagai device yang dikontrol OpenRGB.
//!
//! Aktifkan dengan `--openrgb-device <nama-atau-sebagian-nama>` di argumen
//! command-line (lihat `main.rs`). Butuh OpenRGB berjalan dengan SDK Server
//! aktif (Settings > SDK Server > Enable, default port 6742, dipakai
//! `OpenRgbClient::connect()` bawaan tanpa perlu diatur manual).
//!
//! Karena ini polling (bukan device SDK terdaftar), kalau device sumbernya
//! di OpenRGB dipakaikan EFEK ANIMASI (rainbow/breathing/dst), yang kebaca
//! di sini cuma 1 snapshot warna per polling — jadi ikut berubah tapi
//! "patah-patah" sesuai `--openrgb-poll-ms`, bukan semulus animasi aslinya
//! di OpenRGB. Paling pas dipakai kalau device sumbernya diset warna STATIS.
//!
//! CATATAN: bagian ini belum sempat dicompile-check di sandbox pengembangan
//! (toolchain apt yang tersedia di sana cuma Rust 1.75, sedangkan crate
//! `openrgb2` butuh edition2024 / Rust >=1.85) — beda situasi dengan
//! rusb/wasapi/dll di proyek ini yang sudah lolos `cargo check` di sandbox
//! yang sama. Kodenya ditulis & direview manual mengikuti API resmi di
//! docs.rs/openrgb2, tapi kabari kalau ada error compile saat `cargo build`
//! di mesin Anda (kemungkinan besar cuma soal nama method/tipe yang sedikit
//! beda dari versi yang direview).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use openrgb2::OpenRgbClient;

/// Warna terakhir yang berhasil dibaca dari OpenRGB (RGB 0-255), dibagi ke
/// thread utama lewat `Mutex`. `None` berarti belum pernah berhasil
/// connect+baca sama sekali sejak program mulai — pemanggil sebaiknya pakai
/// warna fallback (mis. `ColorMode::Default` atau nilai `--color`) selama
/// masih `None`.
pub type SharedColor = Arc<Mutex<Option<(u8, u8, u8)>>>;

/// Jalankan polling OpenRGB di thread & runtime tokio terpisah (tidak
/// mengganggu loop utama yang sync). Auto-reconnect terus-menerus kalau
/// OpenRGB belum jalan / SDK Server belum aktif / device belum ketemu —
/// program utama tetap jalan normal (pakai warna fallback) selama itu.
pub fn spawn(device_match: String, poll_interval: Duration) -> SharedColor {
    let shared: SharedColor = Arc::new(Mutex::new(None));
    let shared_thread = Arc::clone(&shared);

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("gagal membuat runtime tokio untuk client OpenRGB");
        rt.block_on(run(device_match, poll_interval, shared_thread));
    });

    shared
}

async fn run(device_match: String, poll_interval: Duration, shared: SharedColor) {
    let mut warned_not_found = false;
    loop {
        match OpenRgbClient::connect().await {
            Ok(client) => {
                println!(
                    "OpenRGB: terhubung, cari device yang namanya mengandung '{device_match}'..."
                );
                loop {
                    match poll_once(&client, &device_match).await {
                        Ok(Some(color)) => {
                            *shared.lock().unwrap() = Some(color);
                            warned_not_found = false;
                        }
                        Ok(None) => {
                            if !warned_not_found {
                                eprintln!(
                                    "OpenRGB: tidak ada device dengan nama mengandung \
                                     '{device_match}'. Cocokkan dengan nama yang tampil di \
                                     aplikasi OpenRGB (klik device di panel kiri)."
                                );
                                warned_not_found = true;
                            }
                        }
                        Err(e) => {
                            eprintln!("OpenRGB: koneksi terputus ({e}), mencoba reconnect...");
                            break; // keluar loop dalam -> reconnect di loop luar
                        }
                    }
                    tokio::time::sleep(poll_interval).await;
                }
            }
            Err(e) => {
                eprintln!(
                    "OpenRGB: gagal connect ({e}) — pastikan OpenRGB sedang berjalan & SDK \
                     Server diaktifkan (Settings > SDK Server > Enable). Mencoba lagi 5 detik \
                     lagi..."
                );
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

/// Cari controller pertama yang namanya mengandung `device_match`
/// (case-insensitive, substring), kembalikan warna LED pertamanya.
async fn poll_once(
    client: &OpenRgbClient,
    device_match: &str,
) -> openrgb2::OpenRgbResult<Option<(u8, u8, u8)>> {
    let controllers = client.get_all_controllers().await?;
    let needle = device_match.to_ascii_lowercase();
    for c in controllers.iter() {
        if c.name().to_ascii_lowercase().contains(&needle) {
            if let Some(color) = c.colors().first() {
                return Ok(Some((color.r, color.g, color.b)));
            }
        }
    }
    Ok(None)
}
