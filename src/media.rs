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

/// Volume master di macOS dibaca langsung dari CoreAudio (public API, tanpa
/// proses eksternal).
///
/// Device diambil lewat `kAudioHardwarePropertyDefaultOutputDevice`, lalu
/// `kAudioDevicePropertyVolumeScalar` + `kAudioDevicePropertyMute` pada scope
/// output. Keduanya dicek tiap sample supaya otomatis mengikuti pergantian
/// output device (mis. USB DAC dicabut) tanpa perlu cache ID di struct ini.
///
/// **Now-playing tidak ada di platform ini.** `MPNowPlayingInfoCenter`
/// (MediaPlayer) terbukti process-scoped: program A yang publish now-playing
/// tidak bisa dibaca program B, jadi tidak berguna untuk membaca app lain.
/// API private `FMPNowPlayingInfo*` sudah dihapus di macOS 26, dan varian
/// `MRMediaRemote*` ada tapi segfault (entitlement gate sejak macOS 15.4).
/// Satu-satunya jalur publik yang tersisa adalah AppleScript per-app, dan itu
/// butuh Music.app/Spotify terpasang — jadi sengaja dikosongkan di sini
/// daripada menulis polling `osascript` yang diam-diam selalu kosong.
#[cfg(target_os = "macos")]
mod imp {
    use super::SharedNowPlaying;
    use objc2_core_audio::{
        kAudioDevicePropertyMute, kAudioDevicePropertyScopeOutput, kAudioDevicePropertyVolumeScalar,
        kAudioHardwarePropertyDefaultOutputDevice, kAudioObjectPropertyElementMain,
        kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject, AudioObjectGetPropertyData,
        AudioObjectID, AudioObjectPropertyAddress,
    };
    use std::ptr::NonNull;
    use std::sync::{Arc, Mutex};

    /// `kAudioHardwareServiceDeviceVolume_VirtualMasterVolume` = `'vmvc'`
    /// (0x766D7663). Selector dari API `AudioHardwareService` lama yang tidak
    /// ada lagi di crate binding, tapi masih dipakai luas: banyak perangkat —
    /// terutama interface USB dan DAC — tidak mengimplementasikan properti
    /// modern `kAudioDevicePropertyVolumeScalar` (`'volm'`).
    ///
    /// Diuji di mesin ini: default output membalas `'volm'` dengan error
    /// `'who?'` (0x77686F3F), sedangkan `'vmvc'` mengembalikan status 0 dengan
    /// volume sebenarnya. Karena itu keduanya dicoba berurutan.
    const VIRTUAL_MASTER_VOLUME: u32 = 0x766D_7663;

    pub struct AudioMonitor;

    impl AudioMonitor {
        pub fn new() -> anyhow::Result<Self> {
            Ok(Self)
        }

        /// `(volume_percent 0-100, muted)`.
        pub fn sample(&self) -> anyhow::Result<(f32, bool)> {
            let device = default_output_device()
                .ok_or_else(|| anyhow::anyhow!("tidak ada default output device"))?;

            // Properti modern dulu, lalu jatuh ke legacy.
            let scalar: f32 = read_scalar(
                device,
                kAudioDevicePropertyVolumeScalar,
                kAudioDevicePropertyScopeOutput,
            )
            .or_else(|_| {
                read_scalar(
                    device,
                    VIRTUAL_MASTER_VOLUME,
                    kAudioDevicePropertyScopeOutput,
                )
            })?;

            let muted: u32 = read_scalar(
                device,
                kAudioDevicePropertyMute,
                kAudioDevicePropertyScopeOutput,
            )
            .unwrap_or(0);

            // Scalar 0.0-1.0 -> persen 0-100, dibatasi supaya toleransi atau
            // pembulatan driver tidak menampilkan angka di luar rentang.
            let pct = (scalar * 100.0).clamp(0.0, 100.0);
            Ok((pct, muted != 0))
        }
    }

    fn default_output_device() -> Option<AudioObjectID> {
        let mut id: AudioObjectID = 0;
        let mut size = std::mem::size_of::<AudioObjectID>() as u32;
        let addr = AudioObjectPropertyAddress {
            mSelector: kAudioHardwarePropertyDefaultOutputDevice,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };
        let st = unsafe {
            AudioObjectGetPropertyData(
                // Binding crate mendeklarasikan konstanta ini sebagai `i32`,
                // sedangkan parameter fungsi bertipe `u32`.
                kAudioObjectSystemObject as AudioObjectID,
                NonNull::from(&addr),
                0,
                std::ptr::null(),
                NonNull::from(&mut size),
                NonNull::from(&mut id).cast(),
            )
        };
        if st != 0 || id == 0 {
            return None;
        }
        Some(id)
    }

    /// Baca satu properti numerik dari device. `T` harus tipe POD sederhana
    /// (`f32` untuk scalar volume, `u32` untuk mute).
    fn read_scalar<T: Copy>(
        device: AudioObjectID,
        selector: u32,
        scope: u32,
    ) -> anyhow::Result<T> {
        let mut value: T = unsafe { std::mem::zeroed() };
        let mut size = std::mem::size_of::<T>() as u32;
        let addr = AudioObjectPropertyAddress {
            mSelector: selector,
            mScope: scope,
            mElement: kAudioObjectPropertyElementMain,
        };
        let st = unsafe {
            AudioObjectGetPropertyData(
                device,
                NonNull::from(&addr),
                0,
                std::ptr::null(),
                NonNull::from(&mut size),
                NonNull::from(&mut value).cast(),
            )
        };
        if st != 0 {
            anyhow::bail!("properti {selector:#x} status {st}");
        }
        Ok(value)
    }

    /// Polling judul lagu lewat **AppleScript** ke aplikasi Musik.
    ///
    /// Kenapa AppleScript dan bukan `MPNowPlayingInfoCenter` (MediaPlayer):
    /// API itu hanya bisa membaca now-playing dari prosesnya sendiri — sudah
    /// diverifikasi dengan uji dua proses terpisah, di mana proses|publisher
    /// tidak terbaca oleh proses|pembaca. AppleScript satu-satunya jalur
    /// publik yang benar-benar melihat apa yang diputar aplikasi lain.
    ///
    /// Batasnya jelas dan perlu diketahui: ini khusus aplikasi **Musik**
    /// (`/System/Applications/Music.app`). Pemutar lain (Spotify, browser,
    /// dsb.) tidak punya kamus AppleScript yang bisa dibaca seperti ini, jadi
    /// untuk itu tetap kosong di macOS.
    ///
    /// Biaya satu panggilan ±160 ms, jadi poll-nya 1,5 detik di thread
    /// terpisah supaya frame rate LCD tidak ikut tersendat.
    pub fn spawn_now_playing_watcher() -> anyhow::Result<SharedNowPlaying> {
        use std::process::Command;
        use std::time::Duration;

        let shared: SharedNowPlaying = Arc::new(Mutex::new(None));

        // Now-playing dimatikan: biayanya tidak sekencil yang terlihat.
        //
        // Satu pemanggilan `osascript` ternyata memakai ~70 ms CPU time (bukan
        // 70 ms wall time — itu hanya 240 ms menunggu Apple Event). Pada
        // interval 1,5 detik itu ~4,5% dari satu core, terus berjalan 24 jam
        // hanya untuk menampilkan judul lagu. Itu tidak sebanding dengan
        // nilainya, apalagi program ini dirancang hemat CPU (adaptive FPS,
        // cache sensor).
        //
        // Setel `true` untuk mengaktifkan lagi — tidak ada perubahan lain
        // yang perlu dilakukan.
        const POLLING_ENABLED: bool = false;
        if !POLLING_ENABLED {
            return Ok(shared);
        }

        let shared_clone = shared.clone();

        std::thread::Builder::new()
            .name("now-playing-applescript".into())
            .spawn(move || {
                let mut warned = false;
                loop {
                    match query_music(&mut Command::new("osascript")) {
                        Ok(raw) => {
                            if let Ok(mut guard) = shared_clone.lock() {
                                *guard = parse_track(&raw);
                            }
                        }
                        Err(e) => {
                            if !warned {
                                eprintln!("{e}");
                                warned = true;
                            }
                        }
                    }
                    std::thread::sleep(Duration::from_millis(1500));
                }
            })?;

        Ok(shared)
    }

    /// Kueri satu baris: `judul||artis||album`, atau `STOPPED` kalau tidak ada
    /// yang diputar. String dikembalikan apa adanya supaya pemanggil bisa
    /// membedakan "tidak ada yang diputar" dari "gagal memanggil".
    fn query_music(cmd: &mut std::process::Command) -> Result<String, String> {
        const SCRIPT: &str = r#"
            tell application "Music"
                set ps to player state as string
                if ps is "playing" or ps is "paused" then
                    set t to current track
                    return "OK||" & (name of t) & "||" & (artist of t) & "||" & (album of t)
                else
                    return "STOPPED"
                end if
            end tell
        "#;

        let out = cmd
            .args(["-e", SCRIPT])
            .output()
            .map_err(|e| format!("GAGAL menjalankan osascript: {e}"))?;

        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(format!(
                "Now-playing (AppleScript) gagal: {err}\n                   Pastikan aplikasi Musik terpasang, lalu izinkan otomatisasi di \
                 System Settings > Privacy & Security > Automation."
            ));
        }

        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// Ubah keluaran `query_music` menjadi string yang ditampilkan di LCD.
    fn parse_track(raw: &str) -> Option<String> {
        if !raw.starts_with("OK||") {
            return None;
        }
        let mut parts = raw["OK||".len()..].split("||");
        let title = parts.next()?.trim();
        let artist = parts.next().unwrap_or("").trim();
        if title.is_empty() {
            return None;
        }
        Some(if artist.is_empty() {
            title.to_string()
        } else {
            format!("{title} - {artist}")
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Volume master harus benar-benar terbaca dari default output device —
        /// inilah yang membuat baris info menampilkan "VOL 95%" bukan "N/A".
        ///
        /// Di-`ignore` karena butuh output device sungguhan, dan angkanya
        /// berbeda tiap mesin jadi tidak bisa jadi assertion nilai tetap.
        /// Jalankan: `cargo test --release media -- --ignored --nocapture`
        /// Format harus sama dengan platform lain: `judul - artis`, atau
        /// `judul` saja kalau artis kosong.
        #[test]
        fn parses_track_output() {
            assert_eq!(
                parse_track("OK||Yesterday||The Beatles||Help!"),
                Some("Yesterday - The Beatles".to_string())
            );
            assert_eq!(
                parse_track("OK||Stravinsky|| ||The Composer"),
                Some("Stravinsky".to_string())
            );
            assert_eq!(parse_track("STOPPED"), None);
            assert_eq!(parse_track("OK||||"), None);
        }

        /// Ny querying sungguhan ke aplikasi Musik. Di-`ignore` supaya tidak
        ///UyEB ikut jalan di `cargo test` biasa.
        #[test]
        #[ignore]
        fn queries_music_app() {
            let mut cmd = std::process::Command::new("osascript");
            let raw = query_music(&mut cmd).expect("osascript harus bisa dijalankan");
            println!("keluaran mentah: {raw:?}");
            println!("setelah parse: {:?}", parse_track(&raw));
        }

        #[test]
        #[ignore]
        fn reads_master_volume() {
            let mon = AudioMonitor::new().expect("AudioMonitor::new");
            let (pct, muted) = mon
                .sample()
                .expect("sample harus berhasil, bukan N/A");
            println!("volume terbaca = {pct:.1}%  muted = {muted}");
            assert!(
                (0.0..=100.0).contains(&pct),
                "volume di luar rentang 0-100: {pct}"
            );
        }
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod imp {
    use super::SharedNowPlaying;
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
