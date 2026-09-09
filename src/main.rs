//! Audio visualizer (bar EQ) + info sistem untuk Trofeo Vision 9.16 LCD.
//!
//! Menampilkan:
//! - Bar EQ dari audio yang sedang diputar di komputer (loopback — bukan
//!   mikrofon). Diimplementasikan asli di **Windows** (WASAPI) dan **Linux**
//!   (PulseAudio/PipeWire) — lihat `src/audio.rs`; di OS lain dipakai sumber
//!   sintetis supaya kode tetap bisa di-compile & dites, bukan audio asli.
//! - Baris info: CPU usage, RAM used/total, uptime sistem, jam, tanggal.
//!
//! FPS bersifat **adaptif**: turun ke FPS idle (default 2) saat tidak ada
//! suara terdeteksi (hemat CPU — JPEG-encode + kirim USB jadi biaya CPU
//! terbesar di program ini), naik ke FPS aktif (default 15) begitu ada suara
//! lagi. Semua nilai ini + ambang batas "diam" bisa diatur lewat argumen
//! command-line — jalankan dengan `--help` untuk daftar lengkap.
//!
//! Tuning cepat lainnya ada di konstanta `NUM_BARS`, `FFT_SIZE`, dll di bawah.

mod audio;
mod cpu_sensor;
mod foreground;
mod gpu;
mod gpu_amd;
mod media;
mod netdisk;
mod openrgb_sync;
mod pawnio;

use std::time::{Duration, Instant};

use chrono::Local;
use rustfft::num_complex::Complex32;
use rustfft::FftPlanner;
use sysinfo::System;
use trofeo_lcd::{Framebuffer, LyLcd, TROFEO_VISION_9_16};

/// Jumlah bar EQ yang digambar.
const NUM_BARS: usize = 48;
/// Ukuran window FFT (sample). Lebih besar = resolusi frekuensi lebih halus,
/// tapi window lebih lambat merespons (latensi lebih tinggi).
const FFT_SIZE: usize = 1024;
/// Rentang frekuensi yang dipetakan ke bar (Hz). Di luar ini diabaikan.
const FREQ_MIN: f32 = 40.0;
const FREQ_MAX: f32 = 16_000.0;

/// Skala teks status (dipakai baris CPU/GPU/NET/DISK/VOL & now-playing).
const STATUS_TEXT_SCALE: u32 = 3;
/// Kecepatan scroll teks "now playing" yang kepanjangan, dalam piksel/detik.
const MARQUEE_SPEED_PX_S: f32 = 45.0;
/// Jarak kosong antar pengulangan teks saat scroll (biar keliatan seperti
/// running text yang menyambung, bukan langsung mepet ke pengulangan berikutnya).
const MARQUEE_GAP: &str = "     ";

/// State scroll (marquee) untuk teks "now playing" yang kepanjangan buat muat
/// di lebar layar. Kalau judul lagu ganti, otomatis reset ke posisi awal —
/// dan kalau judulnya muat tanpa discroll, dia idle (offset selalu 0).
struct Marquee {
    text: String,
    offset_px: f32,
    last_tick: Instant,
}

impl Marquee {
    fn new() -> Self {
        Self {
            text: String::new(),
            offset_px: 0.0,
            last_tick: Instant::now(),
        }
    }

    /// Panggil tiap frame sebelum digambar, dengan teks yang mau ditampilkan
    /// sekarang (mis. judul lagu terbaru) dan lebar area yang tersedia untuk
    /// menampilkannya (piksel). Kembalikan `true` kalau perlu di-scroll
    /// (teks lebih lebar dari area), `false` kalau cukup digambar statis.
    fn tick(&mut self, current_text: &str, available_width: u32) -> bool {
        if current_text != self.text {
            self.text = current_text.to_string();
            self.offset_px = 0.0;
            self.last_tick = Instant::now();
        }

        let text_width = Framebuffer::text_width(&self.text, STATUS_TEXT_SCALE);
        if text_width <= available_width {
            self.offset_px = 0.0;
            return false;
        }

        let now = Instant::now();
        let dt = now.duration_since(self.last_tick).as_secs_f32().min(0.25);
        self.last_tick = now;

        let loop_text = format!("{}{}", self.text, MARQUEE_GAP);
        let loop_width = Framebuffer::text_width(&loop_text, STATUS_TEXT_SCALE).max(1) as f32;
        self.offset_px = (self.offset_px + MARQUEE_SPEED_PX_S * dt) % loop_width;
        true
    }
}

/// Nilai default argumen CLI (lihat `Config` & `parse_args`) — semua bisa
/// dioverride lewat `--idle-fps`, `--active-fps`, dll.
const DEFAULT_IDLE_FPS: f32 = 2.0;
const DEFAULT_ACTIVE_FPS: f32 = 15.0;
/// Ambang batas puncak amplitude time-domain (0.0-1.0) untuk dianggap "diam".
/// Nilai audio digital murni (bukan noise mikrofon) biasanya benar-benar 0
/// saat diam, jadi ambang kecil ini terutama jaga-jaga dari noise/DC-offset
/// sangat kecil dari path capture WASAPI.
const DEFAULT_SILENCE_THRESHOLD: f32 = 0.005;
/// Berapa lama harus diam TERUS-MENERUS sebelum turun ke `idle-fps` — supaya
/// tidak "kedip-kedip" antar FPS pas ada jeda pendek di antara lagu/suara.
/// Naik ke `active-fps` sebaliknya SELALU langsung (tanpa delay) begitu ada
/// suara, supaya visualizer tetap responsif.
const DEFAULT_SILENCE_TIMEOUT_MS: u64 = 800;
/// Seberapa sering info sistem (CPU/mem, lumayan mahal) di-refresh.
const SYSINFO_REFRESH_INTERVAL: Duration = Duration::from_millis(500);

/// Override manual rotasi layar. Ganti jadi `true` kalau tampilan di layar
/// Anda kebalik (upside-down); `false` kalau sudah benar. Field
/// `Handshake::rotate_180` bawaan SELALU `false` (heuristik otomatisnya
/// terbukti tidak bisa diandalkan di hardware nyata) — jadi ini satu-satunya
/// tempat untuk mengatur rotasi.
const ROTATE_180_OVERRIDE: bool = false;

/// Mode warna bar EQ: gradien default (hijau->kuning->merah berdasarkan
/// level), atau satu warna custom tetap (kecerahannya tetap mengikuti level
/// suara supaya dinamika visual tidak hilang).
#[derive(Clone, Copy, Debug)]
enum ColorMode {
    Default,
    Custom(u8, u8, u8),
}

/// Default interval polling warna OpenRGB (lihat `--openrgb-poll-ms`).
const DEFAULT_OPENRGB_POLL_MS: u64 = 300;

/// Konfigurasi dari argumen command-line (lihat `parse_args`).
struct Config {
    idle_fps: f32,
    active_fps: f32,
    silence_threshold: f32,
    silence_timeout: Duration,
    color_mode: ColorMode,
    /// Kalau `Some`, warna bar EQ mengikuti (polling) warna device OpenRGB
    /// yang namanya mengandung string ini — lihat `src/openrgb_sync.rs`.
    /// `color_mode` di atas dipakai sebagai fallback selama belum ada
    /// pembacaan pertama yang berhasil (OpenRGB belum jalan/device belum
    /// ketemu).
    openrgb_device: Option<String>,
    openrgb_poll_ms: u64,
    /// Kalau `true`, jendela terminal disembunyikan (`FreeConsole`) begitu
    /// argumen selesai diparse, dan seluruh `println!`/`eprintln!`
    /// selanjutnya dialihkan ke file log (lihat `hide_console_and_redirect_to_log`).
    /// Hanya berlaku di Windows — di OS lain diabaikan (dengan peringatan).
    hide_console: bool,
}

fn print_help() {
    println!(
        "Pemakaian: trofeo_lcd [OPSI]\n\
         \n\
         FPS pengiriman ke layar adaptif: turun ke --idle-fps saat tidak ada\n\
         suara terdeteksi, naik ke --active-fps begitu ada suara lagi.\n\
         \n\
         Opsi:\n\
         \x20\x20--idle-fps <N>            FPS saat diam (default: {DEFAULT_IDLE_FPS})\n\
         \x20\x20--active-fps <N>          FPS saat ada suara (default: {DEFAULT_ACTIVE_FPS})\n\
         \x20\x20--silence-threshold <N>   Ambang puncak amplitude (0.0-1.0) untuk\n\
         \x20\x20                          dianggap diam (default: {DEFAULT_SILENCE_THRESHOLD})\n\
         \x20\x20--silence-timeout-ms <N>  Lama diam berturut-turut sebelum turun ke\n\
         \x20\x20                          idle-fps, dalam milidetik (default: {DEFAULT_SILENCE_TIMEOUT_MS})\n\
         \x20\x20--color <MODE>            Warna bar EQ: 'default' (gradien hijau->\n\
         \x20\x20                          kuning->merah, ini nilai bawaan), atau warna\n\
         \x20\x20                          custom satu warna tetap dalam format\n\
         \x20\x20                          '#RRGGBB' atau 'R,G,B' (mis. '--color red',\n\
         \x20\x20                          '--color #ff0000', atau '--color 255,0,0')\n\
         \x20\x20--openrgb-device <NAMA>   Sinkronkan warna bar EQ dengan warna device\n\
         \x20\x20                          OpenRGB yang namanya mengandung <NAMA> (cocok\n\
         \x20\x20                          sebagian, tanpa peduli huruf besar/kecil; lihat\n\
         \x20\x20                          nama persis di panel kiri aplikasi OpenRGB).\n\
         \x20\x20                          Butuh OpenRGB berjalan + SDK Server aktif\n\
         \x20\x20                          (Settings > SDK Server > Enable). Selama belum\n\
         \x20\x20                          terhubung/device belum ketemu, dipakai --color\n\
         \x20\x20                          (atau default) sebagai fallback. Ini POLLING\n\
         \x20\x20                          (baca snapshot warna secara berkala), bukan\n\
         \x20\x20                          mendaftarkan trofeo-lcd sebagai device OpenRGB —\n\
         \x20\x20                          efek animasi di device sumber tidak ikut mulus.\n\
         \x20\x20--openrgb-poll-ms <N>     Interval polling OpenRGB dalam ms\n\
         \x20\x20                          (default: {DEFAULT_OPENRGB_POLL_MS})\n\
         \x20\x20--hide-console            Sembunyikan jendela terminal begitu program\n\
         \x20\x20                          mulai jalan (log selanjutnya ditulis ke file\n\
         \x20\x20                          trofeo_lcd.log di folder yang sama dengan .exe,\n\
         \x20\x20                          bukan ke layar). Cocok dipakai lewat shortcut/\n\
         \x20\x20                          Task Scheduler saat login. Hanya berlaku di\n\
         \x20\x20                          Windows; defaultnya (tanpa opsi ini) terminal\n\
         \x20\x20                          tetap terbuka & log tampil seperti biasa.\n\
         \x20\x20-h, --help                Tampilkan bantuan ini"
    );
}

/// Nama warna umum yang bisa dipakai langsung tanpa perlu tahu kode hex/RGB.
fn named_color(name: &str) -> Option<(u8, u8, u8)> {
    Some(match name.to_ascii_lowercase().as_str() {
        "red" | "merah" => (0xFF, 0x00, 0x00),
        "green" | "hijau" => (0x00, 0xFF, 0x00),
        "blue" | "biru" => (0x00, 0x00, 0xFF),
        "yellow" | "kuning" => (0xFF, 0xFF, 0x00),
        "cyan" => (0x00, 0xFF, 0xFF),
        "magenta" | "pink" => (0xFF, 0x00, 0xFF),
        "white" | "putih" => (0xFF, 0xFF, 0xFF),
        "orange" | "oranye" => (0xFF, 0xA5, 0x00),
        "purple" | "ungu" => (0x80, 0x00, 0x80),
        _ => return None,
    })
}

/// Parse argumen `--color`: `"default"`, nama warna umum (`"red"`, `"merah"`,
/// dst.), `"#RRGGBB"`, atau `"R,G,B"`.
fn parse_color(raw: &str) -> anyhow::Result<ColorMode> {
    let raw = raw.trim();

    if raw.eq_ignore_ascii_case("default") {
        return Ok(ColorMode::Default);
    }

    if let Some((r, g, b)) = named_color(raw) {
        return Ok(ColorMode::Custom(r, g, b));
    }

    if let Some(hex) = raw.strip_prefix('#') {
        if hex.len() == 6 {
            let parse_byte = |s: &str| {
                u8::from_str_radix(s, 16)
                    .map_err(|_| anyhow::anyhow!("--color: '{raw}' bukan hex RGB yang valid"))
            };
            let r = parse_byte(&hex[0..2])?;
            let g = parse_byte(&hex[2..4])?;
            let b = parse_byte(&hex[4..6])?;
            return Ok(ColorMode::Custom(r, g, b));
        }
        anyhow::bail!("--color: '{raw}' harus berformat '#RRGGBB' (6 digit hex)");
    }

    let parts: Vec<&str> = raw.split(',').map(str::trim).collect();
    if parts.len() == 3 {
        let parse_component = |s: &str| {
            s.parse::<u8>()
                .map_err(|_| anyhow::anyhow!("--color: '{raw}' bukan format 'R,G,B' yang valid (tiap komponen 0-255)"))
        };
        let r = parse_component(parts[0])?;
        let g = parse_component(parts[1])?;
        let b = parse_component(parts[2])?;
        return Ok(ColorMode::Custom(r, g, b));
    }

    anyhow::bail!(
        "--color: '{raw}' tidak dikenali (pakai 'default', nama warna seperti 'red', \
         '#RRGGBB', atau 'R,G,B')"
    );
}

/// Parse argumen command-line, isi nilai default kalau tidak diberikan.
fn parse_args() -> anyhow::Result<Config> {
    let mut idle_fps = DEFAULT_IDLE_FPS;
    let mut active_fps = DEFAULT_ACTIVE_FPS;
    let mut silence_threshold = DEFAULT_SILENCE_THRESHOLD;
    let mut silence_timeout_ms = DEFAULT_SILENCE_TIMEOUT_MS;
    let mut color_mode = ColorMode::Default;
    let mut openrgb_device: Option<String> = None;
    let mut openrgb_poll_ms = DEFAULT_OPENRGB_POLL_MS;
    let mut hide_console = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--idle-fps" => idle_fps = next_f32(&mut args, "--idle-fps")?,
            "--active-fps" => active_fps = next_f32(&mut args, "--active-fps")?,
            "--silence-threshold" => {
                silence_threshold = next_f32(&mut args, "--silence-threshold")?
            }
            "--silence-timeout-ms" => {
                silence_timeout_ms = next_u64(&mut args, "--silence-timeout-ms")?
            }
            "--color" => {
                let raw = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--color butuh satu nilai setelahnya"))?;
                color_mode = parse_color(&raw)?;
            }
            "--openrgb-device" => {
                let raw = args.next().ok_or_else(|| {
                    anyhow::anyhow!("--openrgb-device butuh satu nilai setelahnya")
                })?;
                if raw.trim().is_empty() {
                    anyhow::bail!("--openrgb-device tidak boleh kosong");
                }
                openrgb_device = Some(raw);
            }
            "--openrgb-poll-ms" => {
                openrgb_poll_ms = next_u64(&mut args, "--openrgb-poll-ms")?;
            }
            "--hide-console" => hide_console = true,
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            other => {
                anyhow::bail!("argumen tidak dikenal: '{other}' (pakai --help untuk daftar opsi)");
            }
        }
    }

    if !(idle_fps > 0.0) || !(active_fps > 0.0) {
        anyhow::bail!("--idle-fps dan --active-fps harus berupa angka > 0");
    }
    if openrgb_poll_ms == 0 {
        anyhow::bail!("--openrgb-poll-ms harus > 0");
    }

    Ok(Config {
        idle_fps,
        active_fps,
        silence_threshold,
        silence_timeout: Duration::from_millis(silence_timeout_ms),
        color_mode,
        openrgb_device,
        openrgb_poll_ms,
        hide_console,
    })
}

/// Path file log yang dipakai saat `--hide-console` aktif: selalu di folder
/// yang sama dengan file .exe (bukan CWD saat ini — supaya konsisten dicari
/// walau program dijalankan dari shortcut/Task Scheduler dengan direktori
/// kerja yang beda-beda).
#[cfg(windows)]
fn log_file_path() -> anyhow::Result<std::path::PathBuf> {
    let exe = std::env::current_exe()
        .map_err(|e| anyhow::anyhow!("gagal menentukan lokasi trofeo_lcd.exe: {e}"))?;
    Ok(exe.with_file_name("trofeo_lcd.log"))
}

/// Sembunyikan jendela terminal (`FreeConsole`) & alihkan `stdout`/`stderr`
/// proses ke file log — HARUS dipanggil sebelum `println!`/`eprintln!`
/// pertama kali dipakai di program ini, supaya Rust belum sempat
/// "menghafal" handle konsol lama (stdout/stderr di-cache lazy saat
/// pertama dipakai, sebelum itu masih bisa dialihkan).
#[cfg(windows)]
fn hide_console_and_redirect_to_log() -> anyhow::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Console::{FreeConsole, SetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};

    let path = log_file_path()?;
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| anyhow::anyhow!("gagal membuka file log '{}': {e}", path.display()))?;

    // Handle file ini dipegang langsung oleh Windows sebagai stdout/stderr
    // proses setelah SetStdHandle, jadi jangan di-drop (yang akan
    // menutup/melepas handle-nya) — `forget` supaya tetap hidup selama
    // proses berjalan.
    let raw_handle = HANDLE(log_file.as_raw_handle() as _);
    std::mem::forget(log_file);

    unsafe {
        SetStdHandle(STD_OUTPUT_HANDLE, raw_handle)?;
        SetStdHandle(STD_ERROR_HANDLE, raw_handle)?;
        FreeConsole()?;
    }
    Ok(())
}

fn next_f32(args: &mut impl Iterator<Item = String>, flag: &str) -> anyhow::Result<f32> {
    let raw = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("{flag} butuh satu nilai angka setelahnya"))?;
    raw.parse::<f32>()
        .map_err(|_| anyhow::anyhow!("{flag}: '{raw}' bukan angka yang valid"))
}

fn next_u64(args: &mut impl Iterator<Item = String>, flag: &str) -> anyhow::Result<u64> {
    let raw = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("{flag} butuh satu nilai angka setelahnya"))?;
    raw.parse::<u64>()
        .map_err(|_| anyhow::anyhow!("{flag}: '{raw}' bukan angka bulat yang valid"))
}

/// Deteksi "diam": puncak amplitude time-domain (`|sample|` maksimum di
/// window ini) di bawah `threshold`. Dihitung dari sample mentah (bukan hasil
/// FFT), jadi tidak tergantung skala windowing/normalisasi bar EQ.
fn is_silent(samples: &[f32], threshold: f32) -> bool {
    samples.iter().fold(0f32, |acc, &s| acc.max(s.abs())) < threshold
}

fn main() -> anyhow::Result<()> {
    let config = parse_args()?;

    // HARUS di baris paling awal setelah parse_args — sebelum println!/
    // eprintln! apa pun lain di bawah ini (lihat dokumentasi fungsinya).
    #[cfg(windows)]
    if config.hide_console {
        hide_console_and_redirect_to_log()?;
    }
    #[cfg(not(windows))]
    if config.hide_console {
        eprintln!(
            "PERINGATAN: --hide-console cuma berlaku di Windows, diabaikan di build ini."
        );
    }

    let lcd = LyLcd::open()?;
    let mut hs = lcd.handshake()?;
    hs.rotate_180 = ROTATE_180_OVERRIDE;
    println!("Terhubung: {:?}, PM={} SUB={}", lcd.variant(), hs.pm, hs.sub);
    println!(
        "FPS: idle={:.1} aktif={:.1} (silence-threshold={} timeout={}ms)",
        config.idle_fps,
        config.active_fps,
        config.silence_threshold,
        config.silence_timeout.as_millis()
    );
    match config.color_mode {
        ColorMode::Default => println!("Warna: default (gradien hijau->kuning->merah)"),
        ColorMode::Custom(r, g, b) => {
            println!("Warna: custom #{r:02X}{g:02X}{b:02X} (kecerahan mengikuti level)")
        }
    }

    // Kalau --openrgb-device diberikan, warna di atas cuma dipakai sebagai
    // FALLBACK selama poller ini belum berhasil connect+baca warna pertama
    // kali (lihat src/openrgb_sync.rs untuk detail & batasannya).
    let openrgb_color: Option<openrgb_sync::SharedColor> = config.openrgb_device.as_ref().map(|d| {
        println!(
            "OpenRGB: sinkron aktif, cari device mengandung '{d}' (poll tiap {}ms)",
            config.openrgb_poll_ms
        );
        openrgb_sync::spawn(d.clone(), Duration::from_millis(config.openrgb_poll_ms))
    });

    // Sensor suhu + power CPU (AMD Zen1-Zen4 only) — PawnIO di Windows,
    // sysfs hwmon (k10temp/amd_energy atau zenpower) di Linux.
    // Graceful: kalau driver tidak ada / CPU tidak didukung, warning di stderr
    // dan lanjut — data ditampilkan sebagai N/A.
    let cpu_sensor = cpu_sensor::CpuSensor::new();
    // Baseline energy counter sebelum loop dimulai — buat snapshot pertama
    // power draw di iterasi sysinfo refresh pertama (lihat bagian loop).
    let mut last_cpu_energy = cpu_sensor.sample_energy();

    // Sensor GPU AMD (suhu Edge, ASIC power, fan RPM) via ADL PMLog.
    // Graceful: kalau driver tidak ada / GPU bukan AMD, warning dan lanjut (N/A).
    let gpu_amd = gpu_amd::GpuAmdSensor::new();
    let mut latest_gpu_data = gpu_amd::GpuAmdData::default();

    let audio_ring = audio::spawn_capture()?;
    #[cfg(not(any(windows, target_os = "linux")))]
    println!(
        "PERINGATAN: build ini bukan Windows/Linux, jadi bar EQ memakai sumber audio \
         sintetis (bukan audio asli) — lihat src/audio.rs."
    );

    // Monitor tambahan: GPU usage, network+disk IO, volume, judul lagu/media.
    // Kalau salah satu gagal diinisialisasi (mis. GPU Engine counter tidak
    // tersedia di sistem ini), program tetap jalan — baris info itu saja yang
    // menampilkan "N/A", bukan program keluar.
    let mut gpu_monitor = gpu::GpuMonitor::new().ok();
    if gpu_monitor.is_none() {
        eprintln!("PERINGATAN: GPU usage tidak tersedia di sistem ini, baris info akan menampilkan N/A.");
    }
    let mut netdisk_monitor = netdisk::NetDiskMonitor::new();
    let audio_endpoint = media::AudioMonitor::new().ok();
    if audio_endpoint.is_none() {
        eprintln!("PERINGATAN: volume master tidak terbaca, baris info akan menampilkan N/A.");
    }
    let now_playing = media::spawn_now_playing_watcher()?;
    let mut now_playing_marquee = Marquee::new();

    let mut latest_gpu_percent: Option<f32> = None;
    let mut latest_net_kb = (0.0f64, 0.0f64); // (down, up)
    let mut latest_disk_mb = (0.0f64, 0.0f64); // (read, write)
    let mut latest_volume: Option<(f32, bool)> = None; // (percent, muted)
    let mut latest_cpu_temp: Option<f32> = None; // °C
    let mut latest_cpu_power: Option<f32> = None; // Watt

    let resolution = TROFEO_VISION_9_16;
    let mut sys = System::new_all();
    sys.refresh_all();
    let mut last_sysinfo_refresh = Instant::now();

    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(FFT_SIZE);
    let hann = hann_window(FFT_SIZE);
    // Rentang bin FFT per bar (tergantung FREQ_MIN/FREQ_MAX/NUM_BARS/FFT_SIZE,
    // semuanya konstan) dihitung SEKALI di sini, bukan tiap frame — menghindari
    // panggilan `powf()` berulang (transcendental, relatif mahal) di hot loop.
    let bar_bins = precompute_bar_bins(FFT_SIZE);

    let mut bar_heights = vec![0f32; NUM_BARS];
    let mut running_max: f32 = 1e-6;

    // Framebuffer dialokasikan SEKALI di luar loop lalu dipakai ulang tiap
    // frame (cuma di-`clear()`, bukan realokasi Vec ~2.6MB 15x/detik) —
    // mengurangi churn heap & page fault yang tidak perlu.
    let mut fb = Framebuffer::new(resolution);

    // Kapan terakhir kali ada suara (bukan diam) terdeteksi. Diinisialisasi
    // ke "baru saja" supaya program mulai di `active_fps` (grace period
    // selama `silence_timeout`), bukan langsung nge-drop ke `idle_fps` kalau
    // kebetulan diam pas startup.
    let mut last_sound = Instant::now();

    loop {
        let frame_start = Instant::now();

        // 1) Ambil window audio terbaru, cek apakah lagi diam, & hitung spektrum.
        let samples = audio::take_latest(&audio_ring, FFT_SIZE);
        if !is_silent(&samples, config.silence_threshold) {
            last_sound = Instant::now();
        }
        let is_idle = last_sound.elapsed() >= config.silence_timeout;
        let target_fps = if is_idle {
            config.idle_fps
        } else {
            config.active_fps
        };
        let target_frame_time = Duration::from_secs_f32(1.0 / target_fps);

        let bars = compute_bars(&samples, &hann, fft.as_ref(), &bar_bins, &mut running_max);
        for (h, &target) in bar_heights.iter_mut().zip(bars.iter()) {
            if target > *h {
                *h = target; // attack cepat
            } else {
                *h = *h * 0.75 + target * 0.25; // decay lebih pelan
            }
        }

        // 2) Refresh info sistem secukupnya saja (bukan tiap frame).
        // Catatan: `System::uptime()` TIDAK ikut aturan ini — itu panggilan
        // statis yang cuma baca counter OS (bukan snapshot proses/CPU/RAM
        // yang mahal), jadi aman dipanggil tiap frame di `draw_status_line`.
        if last_sysinfo_refresh.elapsed() >= SYSINFO_REFRESH_INTERVAL {
            // Ukur delta waktu sebelum reset — dipakai untuk hitung power Watt.
            let sysinfo_delta_ms = last_sysinfo_refresh.elapsed().as_millis() as u64;

            sys.refresh_cpu();
            sys.refresh_memory();
            last_sysinfo_refresh = Instant::now();

            if let Some(gpu) = gpu_monitor.as_mut() {
                latest_gpu_percent = gpu.sample().ok();
            }
            let (down, up, read, write) = netdisk_monitor.sample();
            latest_net_kb = (down, up);
            latest_disk_mb = (read, write);
            if let Some(audio_ep) = audio_endpoint.as_ref() {
                latest_volume = audio_ep.sample().ok();
            }

            // Suhu + power CPU (hanya AMD Zen1-Zen4, lihat cpu_sensor.rs).
            latest_cpu_temp = cpu_sensor.get_temp_c();
            latest_cpu_power =
                cpu_sensor.calc_power_watts(last_cpu_energy, sysinfo_delta_ms);
            last_cpu_energy = cpu_sensor.sample_energy();

            // Sensor GPU AMD: suhu Edge, ASIC power, fan RPM via ADL PMLog.
            latest_gpu_data = gpu_amd.sample();
        }
        // Deteksi mode "lagi ngegame": GPU usage > 50%. Dipakai buat dua hal:
        // ganti isi "NOW PLAYING" jadi nama exe foreground, DAN ganti area
        // bar EQ/jam jadi dashboard performa (lihat draw_game_dashboard).
        let gaming_mode = latest_gpu_percent.is_some_and(|p| p > 50.0);

        // Judul "NOW PLAYING" biasanya lagu/media yang diputar, TAPI kalau GPU
        // usage lagi tinggi (indikasi lagi main game), diganti nama program
        // .exe yang sedang jadi foreground window (mis. nama game itu
        // sendiri) — lebih berguna daripada judul lagu background saat main.
        let now_playing_title = if gaming_mode {
            foreground::foreground_exe_name()
                .or_else(|| now_playing.lock().ok().and_then(|g| g.clone()))
        } else {
            now_playing.lock().ok().and_then(|g| g.clone())
        };

        // 3) Gambar (pakai kembali framebuffer yang sama, cuma di-clear).
        // Saat lagi diam (idle) bar EQ nggak ada gunanya digambar rata
        // (isinya nol/decay ke bawah terus) — daripada layar kosong sia-sia,
        // area itu dipakai buat jam digital besar. Baris info kecil di atas
        // (`draw_status_lines`) tetap selalu tampil seperti biasa.
        // Warna efektif frame ini: kalau sinkron OpenRGB aktif DAN sudah
        // pernah berhasil baca warna sekali, pakai itu; kalau belum (baru
        // start / OpenRGB belum jalan / device belum ketemu), fallback ke
        // --color / default seperti biasa.
        let color_mode = match &openrgb_color {
            Some(shared) => match *shared.lock().unwrap() {
                Some((r, g, b)) => ColorMode::Custom(r, g, b),
                None => config.color_mode,
            },
            None => config.color_mode,
        };

        fb.clear(0x08, 0x08, 0x10);
        if gaming_mode {
            draw_game_dashboard(
                &mut fb,
                &sys,
                latest_gpu_percent,
                &latest_gpu_data,
                latest_cpu_temp,
                latest_cpu_power,
                color_mode,
            );
        } else if is_idle {
            draw_idle_clock(&mut fb, color_mode);
        } else {
            draw_bars(&mut fb, &bar_heights, color_mode);
        }
        draw_status_lines(
            &mut fb,
            &sys,
            latest_gpu_percent,
            &latest_gpu_data,
            latest_net_kb,
            latest_disk_mb,
            latest_volume,
            latest_cpu_temp,
            latest_cpu_power,
            now_playing_title.as_deref(),
            &mut now_playing_marquee,
        );

        // 4) Kirim ke layar.
        lcd.send_framebuffer(&hs, &fb, 75)?;

        // 5) Atur kecepatan biar mendekati `target_fps` saat ini (idle atau
        // aktif — bisa beda tiap iterasi), tanpa memaksa kalau memang lebih
        // lambat dari itu.
        let elapsed = frame_start.elapsed();
        if elapsed < target_frame_time {
            std::thread::sleep(target_frame_time - elapsed);
        }
    }
}

/// Jendela Hann standar (mengurangi spectral leakage sebelum FFT).
fn hann_window(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| {
            let x = (std::f32::consts::PI * i as f32 / (n - 1) as f32).sin();
            x * x
        })
        .collect()
}

/// Hitung rentang bin FFT `(bin_lo, bin_hi)` per bar, sekali saja saat startup
/// (dipanggil di luar loop utama). Sebelumnya ini dihitung ulang tiap frame
/// di dalam `compute_bars`, padahal hasilnya selalu sama selama `FFT_SIZE`
/// tidak berubah — termasuk dua panggilan `powf()` per bar (96 panggilan per
/// frame untuk `NUM_BARS = 48`) yang jadi sia-sia diulang di hot loop.
fn precompute_bar_bins(fft_size: usize) -> Vec<(usize, usize)> {
    let bin_hz = audio::SAMPLE_RATE as f32 / fft_size as f32;
    let nyquist_bin = fft_size / 2;

    (0..NUM_BARS)
        .map(|i| {
            let f_lo = FREQ_MIN * (FREQ_MAX / FREQ_MIN).powf(i as f32 / NUM_BARS as f32);
            let f_hi = FREQ_MIN * (FREQ_MAX / FREQ_MIN).powf((i + 1) as f32 / NUM_BARS as f32);
            let bin_lo = ((f_lo / bin_hz) as usize).min(nyquist_bin.saturating_sub(1));
            let bin_hi = (((f_hi / bin_hz) as usize) + 1).clamp(bin_lo + 1, nyquist_bin);
            (bin_lo, bin_hi)
        })
        .collect()
}

/// Windowing + FFT + bucketing logaritmik ke `NUM_BARS` (pakai `bar_bins` yang
/// sudah dihitung sekali di awal lewat `precompute_bar_bins`), dinormalisasi
/// 0..1 pakai auto-gain (`running_max` meluruh pelan, dipakai sebagai referensi).
fn compute_bars(
    samples: &[f32],
    hann: &[f32],
    fft: &dyn rustfft::Fft<f32>,
    bar_bins: &[(usize, usize)],
    running_max: &mut f32,
) -> Vec<f32> {
    let mut buf: Vec<Complex32> = samples
        .iter()
        .zip(hann.iter())
        .map(|(s, w)| Complex32::new(s * w, 0.0))
        .collect();
    fft.process(&mut buf);

    let n = buf.len();
    let nyquist_bin = n / 2;

    // Magnitude per bin (cuma butuh separuh pertama, sisanya cermin).
    let magnitudes: Vec<f32> = buf[..nyquist_bin].iter().map(|c| c.norm()).collect();

    let mut frame_max = 1e-6f32;
    let mut bars = vec![0f32; NUM_BARS];

    for (bar, &(bin_lo, bin_hi)) in bars.iter_mut().zip(bar_bins.iter()) {
        let mag = magnitudes[bin_lo..bin_hi]
            .iter()
            .fold(0f32, |acc, &m| acc.max(m));
        *bar = mag;
        frame_max = frame_max.max(mag);
    }

    // Auto-gain: naikkan referensi cepat kalau lebih keras, turunkan pelan
    // kalau makin pelan, supaya bar tetap kelihatan proporsional di volume apa pun.
    *running_max = if frame_max > *running_max {
        frame_max
    } else {
        *running_max * 0.98
    };
    let reference = running_max.max(1e-6);

    for bar in bars.iter_mut() {
        *bar = (*bar / reference).clamp(0.0, 1.0);
    }
    bars
}

fn draw_bars(fb: &mut Framebuffer, heights: &[f32], color_mode: ColorMode) {
    let width = fb.width();
    let height = fb.height();

    let top_margin = 100u32; // ruang untuk 3 baris status di atas
    let bottom_margin = 10u32;
    let area_height = height.saturating_sub(top_margin + bottom_margin);
    let area_top = top_margin;

    let gap = 3u32;
    let total_gap = gap * (heights.len() as u32 + 1);
    let bar_width = (width.saturating_sub(total_gap)) / heights.len() as u32;

    let mut x = gap;
    for &h in heights {
        let bar_h = (area_height as f32 * h).round() as u32;
        let y = area_top + (area_height - bar_h);

        let (r, g, b) = level_color(h, color_mode);
        fb.fill_rect(x, y, bar_width, bar_h, r, g, b);

        x += bar_width + gap;
    }
}

/// Warna bar berdasarkan level (0.0-1.0) & `color_mode`:
/// - `Default`: gradien hijau (pelan) -> kuning -> merah (keras), seperti semula.
/// - `Custom(r, g, b)`: satu warna tetap, tapi kecerahannya tetap diskalakan
///   mengikuti level (dengan batas bawah supaya bar tidak pernah gelap total)
///   supaya dinamika visual EQ tidak hilang walau warnanya cuma satu.
fn level_color(level: f32, color_mode: ColorMode) -> (u8, u8, u8) {
    let level = level.clamp(0.0, 1.0);
    match color_mode {
        ColorMode::Default => {
            if level < 0.6 {
                let t = level / 0.6;
                (
                    (0x20 as f32 + t * (0xE0 - 0x20) as f32) as u8,
                    0xE0,
                    0x30,
                )
            } else {
                let t = (level - 0.6) / 0.4;
                (0xE0, (0xE0 as f32 * (1.0 - t)) as u8, 0x30)
            }
        }
        ColorMode::Custom(r, g, b) => {
            const MIN_BRIGHTNESS: f32 = 0.25;
            let factor = MIN_BRIGHTNESS + (1.0 - MIN_BRIGHTNESS) * level;
            (
                (r as f32 * factor) as u8,
                (g as f32 * factor) as u8,
                (b as f32 * factor) as u8,
            )
        }
    }
}

/// Warna "aksen" solid dipakai buat jam idle — beda dari `level_color` yang
/// diskalakan mengikuti level bar, jam ini selalu tampil terang penuh (bukan
/// diredupkan) supaya jelas kebaca dari jarak jauh saat layar lagi "kosong".
fn accent_color(color_mode: ColorMode) -> (u8, u8, u8) {
    match color_mode {
        ColorMode::Default => (0xE0, 0xE0, 0xE0),
        ColorMode::Custom(r, g, b) => (r, g, b),
    }
}

/// Dashboard grid: 4 kotak (FPS, GPU, CPU, RAM) sejajar — pengganti bar EQ/jam
/// saat `gaming_mode` aktif (GPU usage > 50%, lihat main loop). Baris info
/// kecil (`draw_status_lines`) tetap tampil terpisah di atas seperti biasa.
///
/// Tiap kotak: label kecil di atas, angka besar di tengah, detail kecil di
/// bawah. Dipilih dari 3 opsi tampilan yang diajukan (angka besar+gauge,
/// grafik history, dashboard grid) — user pilih grid.
fn draw_game_dashboard(
    fb: &mut Framebuffer,
    sys: &System,
    gpu_percent: Option<f32>,
    gpu_data: &gpu_amd::GpuAmdData,
    cpu_temp: Option<f32>,
    cpu_power: Option<f32>,
    color_mode: ColorMode,
) {
    let width = fb.width();
    let height = fb.height();

    // Area sama persis dipakai draw_bars/draw_idle_clock, supaya semua mode
    // tampilan menempati ruang yang identik (tidak geser baris info di atas).
    let top_margin = 100u32;
    let bottom_margin = 10u32;
    let area_top = top_margin;
    let area_height = height.saturating_sub(top_margin + bottom_margin);

    let side_margin = 20u32;
    let gap = 16u32;
    let panel_count = 4u32;
    let usable_width = width.saturating_sub(side_margin * 2);
    let panel_width =
        (usable_width.saturating_sub(gap * (panel_count - 1))) / panel_count;

    let border = accent_color(color_mode);
    let panel_bg = (0x14u8, 0x14u8, 0x1Cu8);
    let label_color = (0xA0u8, 0xA0u8, 0xA8u8);
    let value_color = (0xF0u8, 0xF0u8, 0xF0u8);
    let border_thickness = 2u32;

    // Data tiap panel disiapkan dulu (string), baru digambar dalam satu loop
    // di bawah supaya layout ke-4 kotak konsisten (tidak duplikasi kode).
    struct Panel {
        label: &'static str,
        value: String,
        detail: String,
    }

    let fps_value = gpu_data.fps.map_or_else(|| "--".to_string(), |f| f.to_string());
    // Kalau GPU usage tinggi tapi fps kosong, kemungkinan besar game-nya
    // borderless windowed (bukan exclusive fullscreen) — lihat catatan di
    // gpu_amd.rs. Kasih hint ini daripada cuma "--" polos yang bikin bingung.
    let fps_detail = if gpu_data.fps.is_some() {
        "FULLSCREEN".to_string()
    } else {
        "NO FULLSCREEN".to_string()
    };

    let gpu_value = gpu_percent.map_or_else(|| "N/A".to_string(), |p| format!("{p:.0}%"));
    let gpu_detail = {
        let mut parts: Vec<String> = Vec::new();
        if let Some(t) = gpu_data.temp_edge_c { parts.push(format!("{t}C")); }
        if let Some(w) = gpu_data.power_w { parts.push(format!("{w}W")); }
        if let Some(r) = gpu_data.fan_rpm { parts.push(format!("{r}rpm")); }
        if parts.is_empty() { "N/A".to_string() } else { parts.join(" ") }
    };

    let cpu_pct = sys.global_cpu_info().cpu_usage();
    let cpu_value = format!("{cpu_pct:.0}%");
    let cpu_detail = match (cpu_temp, cpu_power) {
        (Some(t), Some(w)) => format!("{t:.0}C {w:.0}W"),
        (Some(t), None) => format!("{t:.0}C"),
        (None, Some(w)) => format!("{w:.0}W"),
        (None, None) => "N/A".to_string(),
    };

    let used_mb = sys.used_memory() / 1024 / 1024;
    let total_mb = sys.total_memory() / 1024 / 1024;
    let ram_pct = if total_mb > 0 { (used_mb as f32 / total_mb as f32) * 100.0 } else { 0.0 };
    let ram_value = format!("{ram_pct:.0}%");
    let ram_detail = format!("{used_mb}/{total_mb}MB");

    let panels = [
        Panel { label: "FPS", value: fps_value, detail: fps_detail },
        Panel { label: "GPU", value: gpu_value, detail: gpu_detail },
        Panel { label: "CPU", value: cpu_value, detail: cpu_detail },
        Panel { label: "RAM", value: ram_value, detail: ram_detail },
    ];

    // Skala 18 dipilih supaya string 4 karakter terpanjang yang realistis
    // muncul di sini ("100%") masih muat di lebar 1 kotak (~458px pada
    // resolusi 1920x462): 4 char * 6 * 18 = 432px, masih ada sisa margin.
    let value_scale = 18u32;
    let label_scale = 3u32;
    let detail_scale = 4u32;
    let label_height = Framebuffer::text_height(label_scale);
    let detail_height = Framebuffer::text_height(detail_scale);
    let value_height = Framebuffer::text_height(value_scale);
    let padding = 14u32;

    let mut x = side_margin;
    for panel in &panels {
        // Border (kotak luar) lalu isi sedikit lebih kecil di dalamnya —
        // efek "outline" tanpa perlu fungsi gambar garis terpisah.
        fb.fill_rect(x, area_top, panel_width, area_height, border.0, border.1, border.2);
        fb.fill_rect(
            x + border_thickness,
            area_top + border_thickness,
            panel_width.saturating_sub(border_thickness * 2),
            area_height.saturating_sub(border_thickness * 2),
            panel_bg.0, panel_bg.1, panel_bg.2,
        );

        let label_width = Framebuffer::text_width(panel.label, label_scale);
        let label_x = x + panel_width.saturating_sub(label_width) / 2;
        let label_y = area_top + padding;
        fb.draw_text(label_x, label_y, panel.label, label_color.0, label_color.1, label_color.2, label_scale);

        let detail_width = Framebuffer::text_width(&panel.detail, detail_scale);
        let detail_x = x + panel_width.saturating_sub(detail_width) / 2;
        let detail_y = area_top + area_height.saturating_sub(detail_height + padding);
        fb.draw_text(detail_x, detail_y, &panel.detail, label_color.0, label_color.1, label_color.2, detail_scale);

        // Angka besar diposisikan tepat di tengah ruang KOSONG antara label
        // (atas) dan detail (bawah) — bukan tengah kotak penuh — supaya
        // tidak terasa "turun" kalau label/detail memakan cukup ruang.
        let value_width = Framebuffer::text_width(&panel.value, value_scale);
        let value_x = x + panel_width.saturating_sub(value_width) / 2;
        let middle_top = label_y + label_height;
        let middle_bottom = detail_y;
        let middle_space = middle_bottom.saturating_sub(middle_top);
        let value_y = middle_top + middle_space.saturating_sub(value_height) / 2;
        fb.draw_text(value_x, value_y, &panel.value, value_color.0, value_color.1, value_color.2, value_scale);

        x += panel_width + gap;
    }
}

/// Gambar jam digital besar di tengah area yang biasanya dipakai bar EQ,
/// dipanggil sebagai pengganti `draw_bars` saat lagi diam/idle (bar EQ kosong
/// nggak ada gunanya digambar terus). Baris info kecil (`draw_status_lines`)
/// tetap digambar terpisah seperti biasa, tidak terpengaruh fungsi ini.
fn draw_idle_clock(fb: &mut Framebuffer, color_mode: ColorMode) {
    let width = fb.width();
    let height = fb.height();

    // Batas area yang sama dipakai `draw_bars`, supaya jam & bar EQ selalu
    // "menempati ruang" yang identik dan tidak tumpang-tindih baris info.
    let top_margin = 100u32;
    let bottom_margin = 10u32;
    let area_top = top_margin;
    let area_height = height.saturating_sub(top_margin + bottom_margin);

    let (r, g, b) = accent_color(color_mode);
    let now = Local::now();

    let time_scale = 20u32;
    let time_str = now.format("%H:%M:%S").to_string();
    let time_width = Framebuffer::text_width(&time_str, time_scale);
    let time_height = Framebuffer::text_height(time_scale);

    let date_scale = 6u32;
    let date_str = now.format("%A, %d %B %Y").to_string();
    let date_width = Framebuffer::text_width(&date_str, date_scale);
    let date_height = Framebuffer::text_height(date_scale);

    let gap = 20u32;
    let block_height = time_height + gap + date_height;
    let block_top = area_top + area_height.saturating_sub(block_height) / 2;

    let time_x = width.saturating_sub(time_width) / 2;
    fb.draw_text(time_x, block_top, &time_str, r, g, b, time_scale);

    let date_y = block_top + time_height + gap;
    let date_x = width.saturating_sub(date_width) / 2;
    fb.draw_text(date_x, date_y, &date_str, r, g, b, date_scale);
}

/// Gambar 3 baris info di atas layar:
/// - Baris 1: CPU, GPU usage, RAM, uptime, jam, tanggal.
/// - Baris 2: network throughput (KB/s) & disk IO (MB/s).
/// - Baris 3: volume master & judul lagu/media yang sedang diputar.
///
/// Field yang datanya tidak tersedia (mis. GPU usage gagal diinisialisasi,
/// atau tidak ada lagu yang diputar) ditampilkan sebagai "N/A" / "-",
/// bukan dihilangkan, supaya posisi baris tetap konsisten.
#[allow(clippy::too_many_arguments)]
fn draw_status_lines(
    fb: &mut Framebuffer,
    sys: &System,
    gpu_percent: Option<f32>,
    gpu_data: &gpu_amd::GpuAmdData,
    net_kb: (f64, f64),
    disk_mb: (f64, f64),
    volume: Option<(f32, bool)>,
    cpu_temp: Option<f32>,
    cpu_power: Option<f32>,
    now_playing: Option<&str>,
    marquee: &mut Marquee,
) {
    let scale = STATUS_TEXT_SCALE;
    let color = (0xE0, 0xE0, 0xE0);
    let line_height = Framebuffer::text_height(scale) + 6;

    // --- Baris 1: CPU, GPU, RAM, uptime, jam, tanggal ---
    let cpu = sys.global_cpu_info().cpu_usage();
    let used_mb = sys.used_memory() / 1024 / 1024;
    let total_mb = sys.total_memory() / 1024 / 1024;
    let uptime_str = format_uptime(System::uptime());
    let now = Local::now();
    let time_str = now.format("%H:%M:%S").to_string();
    let date_str = now.format("%Y-%m-%d").to_string();

    // GPU utilization % dari PDH
    let gpu_str = match gpu_percent {
        Some(p) => format!("{p:.0}%"),
        None => "N/A".to_string(),
    };
    // GPU sensor (suhu, power, fan) dari ADL PMLog — hanya tampilkan field
    // yang didukung. Format: "41C 4W 0rpm" atau subset kalau ada yang None.
    // Pakai "C" bukan "°C" karena font bitmap hanya ASCII.
    let gpu_hw_str = {
        let mut parts: Vec<String> = Vec::new();
        if let Some(t) = gpu_data.temp_edge_c { parts.push(format!("{t}C")); }
        if let Some(w) = gpu_data.power_w     { parts.push(format!("{w}W")); }
        if let Some(r) = gpu_data.fan_rpm     { parts.push(format!("{r}rpm")); }
        // Cuma muncul saat ada game exclusive-fullscreen yang jalan; kalau
        // tidak, field ini `None` dan baris info tidak berubah sama sekali
        // (tidak ada "0fps" atau semacamnya yang membingungkan saat idle).
        if let Some(f) = gpu_data.fps         { parts.push(format!("{f}fps")); }
        parts.join(" ")
    };
    let gpu_full = if gpu_hw_str.is_empty() {
        gpu_str
    } else {
        format!("{gpu_str} {gpu_hw_str}")
    };

    // CPU sensor (suhu + power) — lihat cpu_sensor.rs
    let cpu_hw_str = match (cpu_temp, cpu_power) {
        (Some(t), Some(w)) => format!("{t:.0}C {w:.0}W"),
        (Some(t), None)    => format!("{t:.0}C"),
        (None,    Some(w)) => format!("{w:.0}W"),
        (None,    None)    => "N/A".to_string(),
    };
    let line1 = format!(
        "CPU {cpu:.0}% {cpu_hw_str}  GPU {gpu_full}  MEM {used_mb}/{total_mb}MB  UP {uptime_str}  {time_str}  {date_str}"
    );

    // --- Baris 2: network + disk IO ---
    let (net_down, net_up) = net_kb;
    let (disk_read, disk_write) = disk_mb;
    let line2 = format!(
        "NET DN {net_down:.0}KB/S UP {net_up:.0}KB/S  DISK R {disk_read:.1}MB/S W {disk_write:.1}MB/S"
    );

    // --- Baris 3: volume + now playing (judul di-scroll kalau kepanjangan) ---
    let volume_str = match volume {
        Some((_, true)) => "MUTE".to_string(),
        Some((pct, false)) => format!("{pct:.0}%"),
        None => "N/A".to_string(),
    };
    let song_str = now_playing.unwrap_or("-");
    let line3_prefix = format!("VOL {volume_str}  NOW PLAYING: ");

    let mut y = 8u32;
    fb.draw_text(20, y, &line1, color.0, color.1, color.2, scale);
    y += line_height;
    fb.draw_text(20, y, &line2, color.0, color.1, color.2, scale);
    y += line_height;

    let prefix_width = fb.draw_text(20, y, &line3_prefix, color.0, color.1, color.2, scale);
    let title_x0 = 20 + prefix_width;
    let title_x1 = fb.width().saturating_sub(20);
    let available_width = title_x1.saturating_sub(title_x0);

    if available_width == 0 {
        // Layar kelewat sempit buat nampilin judul sama sekali — lewati saja.
    } else if marquee.tick(song_str, available_width) {
        // Judul kepanjangan -> scroll: gambar 2 salinan (teks + jarak)
        // berdampingan, supaya begitu salinan pertama habis "lewat", salinan
        // kedua langsung menyambung mulus tanpa lompatan/kedipan.
        let loop_text = format!("{song_str}{MARQUEE_GAP}");
        let loop_width = Framebuffer::text_width(&loop_text, scale) as i64;
        let base_x = title_x0 as i64 - marquee.offset_px as i64;
        fb.draw_text_clipped(
            base_x, y, &loop_text, color.0, color.1, color.2, scale, title_x0, title_x1,
        );
        fb.draw_text_clipped(
            base_x + loop_width, y, &loop_text, color.0, color.1, color.2, scale, title_x0,
            title_x1,
        );
    } else {
        // Muat pas atau lebih kecil dari lebar area -> tampil statis, diam.
        fb.draw_text(title_x0, y, song_str, color.0, color.1, color.2, scale);
    }
}

/// Format uptime sistem (detik sejak boot) jadi `"HH:MM:SS"`, atau
/// `"NDHH:MM:SS"` (mis. `"3D02:15:07"`) kalau sudah lewat 1 hari.
fn format_uptime(total_secs: u64) -> String {
    let days = total_secs / 86_400;
    let hours = (total_secs % 86_400) / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    if days > 0 {
        format!("{}D{:02}:{:02}:{:02}", days, hours, minutes, seconds)
    } else {
        format!("{:02}:{:02}:{:02}", hours, minutes, seconds)
    }
}
