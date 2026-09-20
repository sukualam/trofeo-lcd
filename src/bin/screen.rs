//! Program Second Monitor untuk Thermalright Trofeo Vision 9.16 LCD.
//!
//! Menangkap tampilan dari layar desktop Windows atau monitor virtual (misal
//! Virtual Display Driver) secara real-time via DXGI Desktop Duplication API,
//! lalu mengirimkannya ke layar Trofeo LCD via USB bulk transfer (protokol LY).

use std::path::PathBuf;
use std::time::{Duration, Instant};
use anyhow::{bail, Result};
use trofeo_lcd::dxgi_capture::{self, CaptureResult, DxgiSession};
use trofeo_lcd::{Framebuffer, LyLcd, TROFEO_VISION_9_16};

const DEFAULT_ACTIVE_FPS: f32 = 30.0;
const DEFAULT_IDLE_FPS: f32 = 10.0;
const DEFAULT_JPEG_QUALITY: u8 = 75;
/// Jarak minimal antar dua tangkapan layar berurutan (ms) — mencegah
/// menyimpan puluhan file saat tombol hotkey ditahan.
const SNAP_MIN_INTERVAL: Duration = Duration::from_millis(500);

struct Config {
    display_index: Option<usize>,
    active_fps: f32,
    idle_fps: f32,
    quality: u8,
    rotate_180: bool,
    hide_console: bool,
    list_only: bool,
    /// (virtual-key code tombol, label asli dari argumen) — `None` = nonaktif.
    screenshot_key: Option<(u32, String)>,
}

fn print_help() {
    println!(
        "trofeo_screen — Second Monitor Streamer untuk Thermalright Trofeo Vision 9.16 LCD\n\n\
        PENGGUNAAN:\n\
        \x20 trofeo_screen [OPSI]\n\n\
        OPSI:\n\
        \x20 -l, --list-displays       Tampilkan daftar seluruh monitor terdeteksi lalu keluar\n\
        \x20 -d, --display <INDEX>     Index monitor yang akan di-stream ke LCD\n\
        \x20                           (default: otomatis pilih monitor beresolusi 1920x462,\n\
        \x20                            atau monitor sekunder)\n\
        \x20     --fps <N>             Target FPS saat layar ada perubahan (default: {DEFAULT_ACTIVE_FPS})\n\
        \x20     --idle-fps <N>        Target polling FPS saat layar diam (default: {DEFAULT_IDLE_FPS})\n\
        \x20 -q, --quality <1-100>     Kualitas kompresi JPEG (default: {DEFAULT_JPEG_QUALITY})\n\
        \x20 -r, --rotate              Putar tampilan 180 derajat (jika layar terbalik)\n\
        \x20     --hide-console        Sembunyikan jendela konsol di Windows (cocok untuk autorun)\n\
        \x20 -k, --screenshot-key <KEY>  Global hotkey untuk menyimpan tangkapan layar\n\
        \x20                           frame LCD ke folder screenshots/ (f1-f12,\n\
        \x20                           atau printscreen). Default: NONAKTIF.\n\
        \x20 -h, --help                Tampilkan bantuan ini\n"
    );
}

fn parse_args() -> Result<Config> {
    let mut args = std::env::args().skip(1);
    let mut display_index = None;
    let mut active_fps = DEFAULT_ACTIVE_FPS;
    let mut idle_fps = DEFAULT_IDLE_FPS;
    let mut quality = DEFAULT_JPEG_QUALITY;
    let mut rotate_180 = false;
    let mut hide_console = false;
    let mut list_only = false;
    let mut screenshot_key: Option<(u32, String)> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            "-l" | "--list-displays" => {
                list_only = true;
            }
            "-d" | "--display" => {
                let raw = args.next().ok_or_else(|| anyhow::anyhow!("--display butuh index angka"))?;
                display_index = Some(raw.parse::<usize>()
                    .map_err(|_| anyhow::anyhow!("--display: '{raw}' bukan angka index valid"))?);
            }
            "-k" | "--screenshot-key" => {
                let raw = args.next().ok_or_else(|| anyhow::anyhow!("--screenshot-key butuh nama tombol"))?;
                screenshot_key = Some((parse_key_name(&raw)?, raw.trim().to_ascii_lowercase()));
            }
            "--fps" => {
                let raw = args.next().ok_or_else(|| anyhow::anyhow!("--fps butuh nilai angka"))?;
                active_fps = raw.parse::<f32>()
                    .map_err(|_| anyhow::anyhow!("--fps: '{raw}' bukan angka valid"))?;
            }
            "--idle-fps" => {
                let raw = args.next().ok_or_else(|| anyhow::anyhow!("--idle-fps butuh nilai angka"))?;
                idle_fps = raw.parse::<f32>()
                    .map_err(|_| anyhow::anyhow!("--idle-fps: '{raw}' bukan angka valid"))?;
            }
            "-q" | "--quality" => {
                let raw = args.next().ok_or_else(|| anyhow::anyhow!("--quality butuh angka 1-100"))?;
                quality = raw.parse::<u8>()
                    .map_err(|_| anyhow::anyhow!("--quality: '{raw}' bukan angka 1-100 valid"))?
                    .clamp(1, 100);
            }
            "-r" | "--rotate" => {
                rotate_180 = true;
            }
            "--hide-console" => {
                hide_console = true;
            }
            other => {
                bail!("Argumen tidak dikenal: '{other}'. Jalankan dengan --help untuk bantuan.");
            }
        }
    }

    if !(active_fps > 0.0) || !(idle_fps > 0.0) {
        bail!("--fps dan --idle-fps harus berupa angka > 0");
    }

    Ok(Config {
        display_index,
        active_fps,
        idle_fps,
        quality,
        rotate_180,
        hide_console,
        list_only,
        screenshot_key,
    })
}

/// Terjemahkan nama tombol hotkey ke virtual-key code Windows. Mendukung
/// `f1`-`f12` dan `printscreen` (plus alias `prtsc`/`print`/`snapshot`).
fn parse_key_name(raw: &str) -> Result<u32> {
    let s = raw.trim().to_ascii_lowercase();
    let s = s.as_str();
    if matches!(s, "printscreen" | "prtsc" | "print" | "snapshot") {
        return Ok(0x2C); // VK_SNAPSHOT
    }
    if let Some(num) = s.strip_prefix('f') {
        if let Ok(n) = num.parse::<u32>() {
            if (1..=12).contains(&n) {
                return Ok(0x70 + n - 1); // VK_F1 = 0x70
            }
        }
    }
    bail!("--screenshot-key: '{raw}' tidak dikenal (pakai f1-f12 atau printscreen).")
}

#[cfg(windows)]
mod win_hotkey {
    use windows::Win32::UI::Input::KeyboardAndMouse::HOT_KEY_MODIFIERS;
    use windows::Win32::UI::Input::KeyboardAndMouse::RegisterHotKey;
    use windows::Win32::UI::WindowsAndMessaging::{MSG, PM_REMOVE, PeekMessageW, WM_HOTKEY};

    /// Hotkey global yang terdaftar. Field `id` dipakai untuk memfilter pesan
    /// WM_HOTKEY dari queue thread.
    pub struct Hotkey {
        pub id: i32,
    }

    /// Daftarkan hotkey global (tanpa modifier, dengan MOD_NOREPEAT supaya
    /// tidak menembak berulang saat tombol ditahan). Gagal kalau tombol itu
    /// sudah dipakai program lain — caller boleh lanjut tanpa hotkey.
    pub fn register(vk: u32) -> anyhow::Result<Hotkey> {
        const HOTKEY_ID: i32 = 1;
        // SAFETY: tanpa window handle = hotkey global untuk thread ini; id
        // unik lokal dan tidak bentrok dengan hotkey lain dalam proses ini.
        unsafe { RegisterHotKey(None, HOTKEY_ID, HOT_KEY_MODIFIERS(0x4000), vk)? };
        Ok(Hotkey { id: HOTKEY_ID })
    }

    /// `true` kalau hotkey sempat ditekan sejak polling terakhir (drain pesan
    /// WM_HOTKEY dari queue thread ini).
    pub fn triggered(id: i32) -> bool {
        // SAFETY: msg lokal valid; semua window dilewati (None) supaya tidak
        // mengganggu queue window lain.
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_HOTKEY && msg.wParam.0 == id as usize {
                    return true;
                }
            }
        }
        false
    }
}

#[cfg(not(windows))]
mod win_hotkey {
    pub struct Hotkey {
        pub id: i32,
    }

    pub fn register(_vk: u32) -> anyhow::Result<Hotkey> {
        anyhow::bail!("hotkey tangkapan layar hanya didukung di Windows");
    }

    pub fn triggered(_id: i32) -> bool {
        false
    }
}

#[cfg(windows)]
fn hide_console_window() -> Result<()> {
    use windows::Win32::System::Console::FreeConsole;
    unsafe { FreeConsole()? };
    Ok(())
}

fn show_displays() -> Result<()> {
    println!("Memeriksa monitor yang terhubung...");
    let displays = dxgi_capture::list_displays()?;
    if displays.is_empty() {
        println!("Tidak ada monitor yang terdeteksi!");
        return Ok(());
    }

    println!("\nDaftar Monitor Terdeteksi:");
    println!("{:-<75}", "");
    println!("{:<6} {:<24} {:<16} {:<12} {:<10}", "INDEX", "ADAPTER", "DEVICE", "RESOLUSI", "STATUS");
    println!("{:-<75}", "");

    for d in &displays {
        let res = format!("{}x{}", d.width, d.height);
        let status = if d.is_attached { "Aktif" } else { "Nonaktif" };
        let marker = if d.width == 1920 && d.height == 462 { " [MATCH 1920x462]" } else { "" };
        println!(
            "{:<6} {:<24} {:<16} {:<12} {:<10}{}",
            d.index,
            d.adapter_name.chars().take(22).collect::<String>(),
            d.device_name,
            res,
            status,
            marker
        );
    }
    println!("{:-<75}\n", "");
    Ok(())
}

/// Simpan isi framebuffer (RGB888) sebagai file BMP 24-bit (lossless, tanpa
/// dependensi tambahan) ke folder `screenshots/`, lalu kembalikan path-nya.
fn save_framebuffer_bmp(fb: &Framebuffer) -> Result<PathBuf> {
    let w = fb.width() as usize;
    let h = fb.height() as usize;
    let row = w * 3;
    let pad = (4 - (row % 4)) % 4;
    let image_size = (row + pad) * h;
    let file_size = 54 + image_size;

    let mut out = Vec::with_capacity(file_size);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(file_size as u32).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&54u32.to_le_bytes());
    // BITMAPINFOHEADER (40 byte), biCompression = BI_RGB, 24 bpp.
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // planes
    out.extend_from_slice(&24u16.to_le_bytes()); // bpp
    out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    out.extend_from_slice(&(image_size as u32).to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes()); // ~72 DPI, X
    out.extend_from_slice(&2835i32.to_le_bytes()); // ~72 DPI, Y
    out.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    out.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant

    let px = fb.as_bytes();
    let mut row_buf = vec![0u8; row + pad];
    for y in 0..h {
        // BMP menyimpan baris dari bawah (bottom-up): baris layar teratas
        // ditulis paling akhir.
        let src_start = (h - 1 - y) * row;
        let src = &px[src_start..src_start + row];
        let mut i = 0;
        for rgb in src.chunks_exact(3) {
            row_buf[i] = rgb[2]; // B
            row_buf[i + 1] = rgb[1]; // G
            row_buf[i + 2] = rgb[0]; // R
            i += 3;
        }
        out.extend_from_slice(&row_buf);
    }

    let dir = std::path::Path::new("screenshots");
    std::fs::create_dir_all(dir)?;
    let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let path = dir.join(format!("trofeo_screen_{}.bmp", stamp));
    std::fs::write(&path, out)?;
    Ok(path)
}

fn main() -> Result<()> {
    let config = parse_args()?;

    if config.list_only {
        return show_displays();
    }

    #[cfg(windows)]
    if config.hide_console {
        hide_console_window()?;
    }

    println!("============================================================");
    println!("  Trofeo Vision 9.16 — Second Monitor Streamer");
    println!("============================================================");

    // Buka koneksi USB ke layar Trofeo LCD
    println!("Mencari perangkat Thermalright Trofeo Vision LCD (0416:5408)...");
    let lcd = match LyLcd::open() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("GAGAL: Tidak dapat membuka koneksi USB ke Trofeo LCD: {e}");
            eprintln!("Pastikan kabel USB terpasang dan driver WinUSB sudah dipasang (via Zadig).");
            bail!(e);
        }
    };

    let mut hs = lcd.handshake()?;
    hs.rotate_180 = config.rotate_180;
    println!("LCD Terhubung: {:?}, PM={} SUB={}, Rotate={}", lcd.variant(), hs.pm, hs.sub, hs.rotate_180);

    // Hotkey tangkapan layar (global, default NONAKTIF — aktif hanya kalau
    // argumen --screenshot-key diberikan).
    let mut snap_hotkey: Option<win_hotkey::Hotkey> = None;
    if let Some((vk, label)) = config.screenshot_key {
        match win_hotkey::register(vk) {
            Ok(h) => {
                println!(
                    "Hotkey tangkapan layar: {} (global) — tekan untuk menyimpan frame LCD ke screenshots/",
                    label.to_uppercase()
                );
                snap_hotkey = Some(h);
            }
            Err(e) => eprintln!("PERINGATAN: hotkey tangkapan layar tidak aktif: {e}"),
        }
    }

    // Inisialisasi sesi penangkap layar DXGI
    let mut session = match DxgiSession::new(config.display_index) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("\nGAGAL menginisialisasi capture monitor: {e}");
            eprintln!("Jalankan 'trofeo_screen --list-displays' untuk memeriksa monitor yang tersedia.");
            bail!(e);
        }
    };

    let (src_w, src_h) = session.src_resolution();
    let display_idx = session.display_index();
    println!(
        "Menangkap Display [{display_idx}] (Resolusi: {src_w}x{src_h}) -> Stream ke LCD (1920x462)"
    );
    println!(
        "Pengaturan: Active FPS: {:.1} | Idle FPS: {:.1} | Quality: {}%",
        config.active_fps, config.idle_fps, config.quality
    );
    println!("Streaming dimulai... Tekan Ctrl+C untuk berhenti.\n");

    let resolution = TROFEO_VISION_9_16;
    let mut fb = Framebuffer::new(resolution);

    let active_frame_time = Duration::from_secs_f32(1.0 / config.active_fps);
    let idle_frame_time = Duration::from_secs_f32(1.0 / config.idle_fps);
    let mut last_snap = Instant::now() - SNAP_MIN_INTERVAL;

    loop {
        let frame_start = Instant::now();

        // Hotkey tangkapan layar (global — tetap berfungsi meski jendela
        // tidak fokus). Frame yang disimpan adalah frame terakhir yang tampil.
        if let Some(h) = &snap_hotkey {
            if win_hotkey::triggered(h.id) && last_snap.elapsed() >= SNAP_MIN_INTERVAL {
                match save_framebuffer_bmp(&fb) {
                    Ok(p) => println!("Tangkapan layar disimpan: {}", p.display()),
                    Err(e) => eprintln!("Gagal menyimpan tangkapan layar: {e}"),
                }
                last_snap = Instant::now();
            }
        }

        // Tangkap frame berikutnya dari DXGI (timeout 100ms)
        let capture_result = session.acquire_next_frame(100, &mut fb);

        match capture_result {
            Ok(CaptureResult::NewFrame) => {
                // Layar berubah: kirim frame baru ke LCD
                if let Err(e) = lcd.send_framebuffer(&hs, &fb, config.quality) {
                    eprintln!("Peringatan USB: Gagal mengirim frame ke LCD ({e}), mencoba kembali...");
                    std::thread::sleep(Duration::from_millis(200));
                }

                // Jaga target active FPS
                let elapsed = frame_start.elapsed();
                if elapsed < active_frame_time {
                    std::thread::sleep(active_frame_time - elapsed);
                }
            }
            Ok(CaptureResult::Timeout) => {
                // Layar statis/diam: tidak perlu kirim apa-apa (hemat USB & CPU!)
                let elapsed = frame_start.elapsed();
                if elapsed < idle_frame_time {
                    std::thread::sleep(idle_frame_time - elapsed);
                }
            }
            Ok(CaptureResult::NeedsReinit) => {
                eprintln!("Mode monitor berubah atau akses terputus. Menginisialisasi ulang capture...");
                std::thread::sleep(Duration::from_millis(500));
                if let Ok(new_sess) = DxgiSession::new(Some(display_idx)) {
                    session = new_sess;
                    println!("Capture berhasil diinisialisasi ulang.");
                }
            }
            Err(e) => {
                eprintln!("Peringatan Capture: {e}");
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
}
