//! Volume master sistem dan judul lagu/media yang sedang diputar.
//!
//! - **Windows**: COM `IAudioEndpointVolume` (volume) + WinRT System Media
//!   Transport Controls (judul lagu — API resmi yang sama dipakai widget
//!   "now playing" bawaan Windows 11).
//! - **Linux**: `pactl` (volume — kompatibel PulseAudio MAUPUN
//!   PipeWire+pipewire-pulse) + `playerctl` (judul lagu via MPRIS — standar
//!   D-Bus yang didukung hampir semua pemutar media Linux: Spotify, VLC,
//!   Firefox/Chrome, dst). Keduanya dipanggil sebagai proses eksternal
//!   (bukan binding native) — jauh lebih sederhana & robust daripada
//!   implementasi client D-Bus/PulseAudio penuh, dengan trade-off perlu
//!   kedua tool itu terpasang (sangat umum ada di desktop Linux modern;
//!   kalau tidak ada, pesan instalasi dicetak sekali ke stderr).
//!
//! Query judul lagu dijalankan di THREAD TERPISAH (mirip capture audio di
//! `audio.rs`), bukan langsung di render loop, karena panggilannya
//! (WinRT async / spawn proses) sesekali bisa makan waktu — supaya frame
//! rate LCD tidak ikut tersendat kalau panggilan itu lambat.

use std::sync::{Arc, Mutex};

pub type SharedNowPlaying = Arc<Mutex<Option<String>>>;

#[cfg(windows)]
mod imp {
    use super::SharedNowPlaying;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use windows::Media::Control::GlobalSystemMediaTransportControlsSessionManager;
    use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
    use windows::Win32::Media::Audio::{eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator};
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};

    pub struct AudioMonitor {
        endpoint_volume: IAudioEndpointVolume,
    }

    impl AudioMonitor {
        pub fn new() -> anyhow::Result<Self> {
            unsafe {
                // Boleh gagal kalau COM sudah di-init dengan mode berbeda di
                // thread ini sebelumnya (mis. oleh crate lain) — bukan fatal.
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

                let enumerator: IMMDeviceEnumerator =
                    CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
                let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
                let endpoint_volume: IAudioEndpointVolume = device.Activate(CLSCTX_ALL, None)?;

                Ok(Self { endpoint_volume })
            }
        }

        /// `(volume_percent 0-100, muted)`.
        pub fn sample(&self) -> anyhow::Result<(f32, bool)> {
            unsafe {
                let level = self.endpoint_volume.GetMasterVolumeLevelScalar()?;
                let muted = self.endpoint_volume.GetMute()?.as_bool();
                Ok((level * 100.0, muted))
            }
        }
    }

    /// Mulai thread background yang polling judul lagu/media aktif tiap detik.
    pub fn spawn_now_playing_watcher() -> anyhow::Result<SharedNowPlaying> {
        let shared: SharedNowPlaying = Arc::new(Mutex::new(None));
        let shared_clone = shared.clone();

        std::thread::Builder::new()
            .name("now-playing-smtc".into())
            .spawn(move || {
                // COM/WinRT butuh di-init per-thread.
                unsafe {
                    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                }
                loop {
                    let title = query_now_playing().ok().flatten();
                    if let Ok(mut guard) = shared_clone.lock() {
                        *guard = title;
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
            })?;

        Ok(shared)
    }

    fn query_now_playing() -> anyhow::Result<Option<String>> {
        let manager = GlobalSystemMediaTransportControlsSessionManager::RequestAsync()?.get()?;
        let Ok(session) = manager.GetCurrentSession() else {
            return Ok(None);
        };
        let props = session.TryGetMediaPropertiesAsync()?.get()?;

        let title = props.Title()?.to_string();
        let artist = props.Artist()?.to_string();

        if title.trim().is_empty() {
            return Ok(None);
        }
        if artist.trim().is_empty() {
            Ok(Some(title))
        } else {
            Ok(Some(format!("{title} - {artist}")))
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::SharedNowPlaying;
    use std::process::Command;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Tidak perlu simpan koneksi apa pun — tiap `sample()` cuma spawn
    /// proses `pactl` singkat (murah, dipanggil hanya tiap sysinfo refresh
    /// interval, bukan tiap frame).
    pub struct AudioMonitor;

    impl AudioMonitor {
        pub fn new() -> anyhow::Result<Self> {
            Ok(Self)
        }

        /// `(volume_percent 0-100, muted)` lewat `pactl`.
        pub fn sample(&self) -> anyhow::Result<(f32, bool)> {
            let volume = query_default_sink_volume().unwrap_or(0.0);
            let muted = query_default_sink_muted().unwrap_or(false);
            Ok((volume, muted))
        }
    }

    fn run_pactl(args: &[&str]) -> Option<String> {
        let output = Command::new("pactl").args(args).output().ok()?;
        if !output.status.success() {
            return None;
        }
        String::from_utf8(output.stdout).ok()
    }

    /// Parse baris keluaran `pactl get-sink-volume @DEFAULT_SINK@`, misalnya:
    /// "Volume: front-left: 65536 / 100% / 0.00 dB, front-right: ..." —
    /// ambil angka persen PERTAMA yang ditemukan (channel manapun, cukup
    /// untuk ditampilkan sebagai satu angka "volume master").
    fn query_default_sink_volume() -> Option<f32> {
        let text = run_pactl(&["get-sink-volume", "@DEFAULT_SINK@"])?;
        text.split_whitespace()
            .find_map(|tok| tok.strip_suffix('%')?.parse::<f32>().ok())
    }

    /// Parse baris keluaran `pactl get-sink-mute @DEFAULT_SINK@`: "Mute: yes"/"Mute: no".
    fn query_default_sink_muted() -> Option<bool> {
        let text = run_pactl(&["get-sink-mute", "@DEFAULT_SINK@"])?.to_lowercase();
        if text.contains("yes") {
            Some(true)
        } else if text.contains("no") {
            Some(false)
        } else {
            None
        }
    }

    /// Mulai thread background yang polling judul lagu/media aktif tiap detik
    /// lewat `playerctl` (client MPRIS command-line paling umum dipakai di
    /// Linux — bekerja dengan semua pemutar yang dukung standar MPRIS:
    /// Spotify, VLC, tab browser yang sedang memutar audio, dst).
    pub fn spawn_now_playing_watcher() -> anyhow::Result<SharedNowPlaying> {
        let shared: SharedNowPlaying = Arc::new(Mutex::new(None));
        let shared_clone = shared.clone();

        std::thread::Builder::new()
            .name("now-playing-mpris".into())
            .spawn(move || {
                let mut warned_missing = false;
                loop {
                    match query_now_playing() {
                        Ok(title) => {
                            if let Ok(mut guard) = shared_clone.lock() {
                                *guard = title;
                            }
                        }
                        Err(_) if !warned_missing => {
                            eprintln!(
                                "PERINGATAN: 'playerctl' tidak ditemukan — judul lagu/media \
                                 (NOW PLAYING) akan selalu kosong.\n  Install: \
                                 sudo apt install playerctl   (Debian/Ubuntu)\n           \
                                 sudo pacman -S playerctl     (Arch)\n           \
                                 sudo dnf install playerctl   (Fedora)"
                            );
                            warned_missing = true;
                        }
                        Err(_) => {}
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
            })?;

        Ok(shared)
    }

    /// `\x1f` (unit separator ASCII) dipakai pemisah title/artist supaya
    /// cukup SATU panggilan `playerctl` per polling (bukan dua) — karakter
    /// ini praktis mustahil muncul di judul lagu asli.
    fn query_now_playing() -> anyhow::Result<Option<String>> {
        let output = Command::new("playerctl")
            .args(["metadata", "--format", "{{title}}\u{1f}{{artist}}"])
            .output()
            .map_err(|e| anyhow::anyhow!("playerctl tidak ditemukan: {e}"))?;

        if !output.status.success() {
            // Wajar & sering terjadi: memang tidak ada player MPRIS yang
            // sedang aktif sama sekali — bukan error.
            return Ok(None);
        }

        let text = String::from_utf8_lossy(&output.stdout);
        let mut parts = text.trim_end().splitn(2, '\u{1f}');
        let title = parts.next().unwrap_or("").trim();
        let artist = parts.next().unwrap_or("").trim();

        if title.is_empty() {
            return Ok(None);
        }
        if artist.is_empty() {
            Ok(Some(title.to_string()))
        } else {
            Ok(Some(format!("{title} - {artist}")))
        }
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod imp {
    use super::SharedNowPlaying;
    use std::sync::{Arc, Mutex};

    pub struct AudioMonitor;

    impl AudioMonitor {
        pub fn new() -> anyhow::Result<Self> {
            Ok(Self)
        }

        pub fn sample(&self) -> anyhow::Result<(f32, bool)> {
            Ok((0.0, false))
        }
    }

    pub fn spawn_now_playing_watcher() -> anyhow::Result<SharedNowPlaying> {
        Ok(Arc::new(Mutex::new(None)))
    }
}

pub use imp::{spawn_now_playing_watcher, AudioMonitor};
