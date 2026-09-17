//! Program Second Monitor untuk Thermalright Trofeo Vision 9.16 LCD.
//!
//! Menangkap tampilan dari layar desktop Windows atau monitor virtual (misal
//! Virtual Display Driver) secara real-time via DXGI Desktop Duplication API,
//! lalu mengirimkannya ke layar Trofeo LCD via USB bulk transfer (protokol LY).

use std::time::{Duration, Instant};
use anyhow::{bail, Result};
use trofeo_lcd::dxgi_capture::{self, CaptureResult, DxgiSession};
use trofeo_lcd::{Framebuffer, LyLcd, TROFEO_VISION_9_16};

const DEFAULT_ACTIVE_FPS: f32 = 30.0;
const DEFAULT_IDLE_FPS: f32 = 10.0;
const DEFAULT_JPEG_QUALITY: u8 = 75;

struct Config {
    display_index: Option<usize>,
    active_fps: f32,
    idle_fps: f32,
    quality: u8,
    rotate_180: bool,
    hide_console: bool,
    list_only: bool,
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
    })
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

    loop {
        let frame_start = Instant::now();

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
