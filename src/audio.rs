//! Capture audio sistem (loopback — apa yang keluar dari speaker, BUKAN
//! mikrofon) untuk dipakai sebagai sumber data audio visualizer.
//!
//! - Di **Windows**: pakai WASAPI loopback lewat crate `wasapi` — ambil
//!   default *render* (output) device, lalu minta client dengan arah
//!   `Capture` supaya WASAPI otomatis mengaktifkan mode loopback (lihat
//!   `examples/record.rs` di crate `wasapi`: "Use `Direction::Render` for
//!   loopback mode (for capturing from a playback device)").
//! - Di **Linux**: pakai PulseAudio/PipeWire lewat `libpulse-simple-binding`
//!   — connect ke device spesial `@DEFAULT_MONITOR@` (monitor source dari
//!   sink default saat ini). Nama spesial ini di-resolve oleh SERVER
//!   PulseAudio/PipeWire-pulse sendiri (sama seperti dipakai `parec`/
//!   `pacat -r`), jadi otomatis ikut pindah kalau default output device
//!   diganti, dan bekerja sama baiknya di sistem PipeWire native (lewat
//!   layer kompatibilitas `pipewire-pulse` yang sudah standar di distro
//!   modern) maupun PulseAudio asli.
//! - Di **OS lain** (macOS/BSD, dipakai supaya kode ini tetap bisa
//!   di-compile-check): sumber sintetis — bukan audio nyata.
//!
//! Ketiga jalur mengisi buffer melingkar mono `SAMPLE_RATE` Hz yang sama,
//! jadi `main.rs` tidak perlu tahu OS apa yang dipakai.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Sample rate internal yang dipakai visualizer (capture di-resample /
/// diminta pada rate ini).
pub const SAMPLE_RATE: u32 = 44_100;

/// Jumlah sample mono maksimum yang disimpan di buffer (kira-kira 0.5 detik).
const RING_CAPACITY: usize = SAMPLE_RATE as usize / 2;

pub type SharedRing = Arc<Mutex<VecDeque<f32>>>;

/// Dipakai juga oleh `audio_macos` (loopback Core Audio) — makanya `pub(crate)`.
pub(crate) fn push_mono_samples(ring: &SharedRing, samples: impl Iterator<Item = f32>) {
    let mut buf = ring.lock().expect("audio ring mutex poisoned");
    for s in samples {
        if buf.len() >= RING_CAPACITY {
            buf.pop_front();
        }
        buf.push_back(s);
    }
}

/// Ambil `n` sample mono terbaru dari ring buffer, zero-pad di depan kalau
/// belum cukup (mis. baru mulai).
pub fn take_latest(ring: &SharedRing, n: usize) -> Vec<f32> {
    let buf = ring.lock().expect("audio ring mutex poisoned");
    let have = buf.len();
    let mut out = vec![0f32; n];
    if have == 0 {
        return out;
    }
    let take = have.min(n);
    // Ambil `take` sample paling baru (dari belakang), taruh di akhir buffer output.
    let skip = have - take;
    for (i, s) in buf.iter().skip(skip).enumerate() {
        out[n - take + i] = *s;
    }
    out
}

/// Mulai thread capture audio di background, kembalikan handle ring buffer
/// yang terus diisi.
pub fn spawn_capture() -> anyhow::Result<SharedRing> {
    let ring: SharedRing = Arc::new(Mutex::new(VecDeque::with_capacity(RING_CAPACITY)));

    #[cfg(windows)]
    {
        let ring_clone = ring.clone();
        std::thread::Builder::new()
            .name("audio-capture-wasapi".into())
            .spawn(move || {
                if let Err(e) = windows_loopback::run(ring_clone) {
                    eprintln!("Capture audio (WASAPI loopback) berhenti: {e:#}");
                }
            })?;
    }

    #[cfg(target_os = "linux")]
    {
        let ring_clone = ring.clone();
        std::thread::Builder::new()
            .name("audio-capture-pulse".into())
            .spawn(move || {
                if let Err(e) = linux_loopback::run(ring_clone) {
                    eprintln!(
                        "Capture audio (PulseAudio/PipeWire loopback) gagal: {e:#}\n\
                         Pastikan PulseAudio atau PipeWire (dengan paket pipewire-pulse) \
                         sedang jalan. Bar EQ akan diam total (tidak fallback ke data palsu)."
                    );
                }
            })?;
    }

    #[cfg(target_os = "macos")]
    {
        let ring_clone = ring.clone();
        std::thread::Builder::new()
            .name("audio-capture-tap".into())
            .spawn(move || {
                if let Err(e) = macos_loopback::run(ring_clone) {
                    eprintln!("Capture audio (Core Audio process tap) gagal: {e:#}");
                }
            })?;
    }

    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        let ring_clone = ring.clone();
        std::thread::Builder::new()
            .name("audio-capture-fallback".into())
            .spawn(move || fallback::run(ring_clone))?;
    }

    Ok(ring)
}

#[cfg(windows)]
mod windows_loopback {
    use super::{push_mono_samples, SharedRing, SAMPLE_RATE};
    use std::time::Duration;
    use wasapi::*;

    pub fn run(ring: SharedRing) -> anyhow::Result<()> {
        initialize_mta().ok()?;

        let enumerator = DeviceEnumerator::new()?;
        // PENTING: ambil device Render (output/speaker) di sini, BUKAN Capture —
        // ini yang membuat WASAPI menganggapnya sebagai permintaan loopback.
        let device = enumerator.get_default_device(&Direction::Render)?;
        let mut audio_client = device.get_iaudioclient()?;

        let desired_format = WaveFormat::new(32, 32, &SampleType::Float, SAMPLE_RATE as usize, 2, None);
        let (_def_time, min_time) = audio_client.get_device_period()?;

        // Sengaja PAKAI POLLING, bukan event (EventsShared): pada banyak driver,
        // event WASAPI loopback TIDAK pernah ditandai selama tidak ada audio
        // yang benar-benar sedang diputar (device idle/diam) — ini keterbatasan
        // WASAPI yang terdokumentasi, bukan bug perangkat. Menunggu event dalam
        // kondisi itu akan selalu timeout walau capture-nya sendiri sehat.
        // Polling tidak punya masalah ini: kalau lagi diam, `read_from_device_to_deque`
        // sederhana saja mengembalikan 0 byte baru, bar EQ tinggal diam di posisi rendah.
        let mode = StreamMode::PollingShared {
            autoconvert: true,
            buffer_duration_hns: min_time,
        };

        // Arah yang diminta ke client adalah Capture, walau device-nya Render —
        // kombinasi ini yang memicu AUDCLNT_STREAMFLAGS_LOOPBACK di dalam crate.
        audio_client.initialize_client(&desired_format, &Direction::Capture, &mode)?;

        let capture_client = audio_client.get_audiocaptureclient()?;
        let blockalign = desired_format.get_blockalign() as usize; // byte per frame (2 ch x 4 byte float)
        let channels = 2usize;

        let mut byte_queue: std::collections::VecDeque<u8> = std::collections::VecDeque::new();
        audio_client.start_stream()?;

        // Cek buffer kira-kira 2x lebih sering dari periode device (min_time
        // dalam satuan 100ns/"hns" ala WASAPI -> bagi 10 supaya jadi mikrodetik),
        // supaya tidak ketinggalan/nge-drop data tanpa perlu event handle sama sekali.
        let period_micros = (min_time as u64 / 10).max(1);
        let poll_interval = Duration::from_micros((period_micros / 2).max(2_000));

        // Dipakai ulang tiap iterasi (`clear()`, bukan realokasi) untuk
        // menampung semua sample mono hasil satu polling SEBELUM di-push ke
        // ring buffer bersama. Sebelumnya `push_mono_samples` (lock mutex)
        // dipanggil per SATU sample audio (bisa 44100x/detik) — sekarang
        // cuma sekali per polling, jauh lebih murah untuk CPU (lock/unlock
        // mutex berulang bukan gratis walau tidak macet).
        let mut mono_batch: Vec<f32> = Vec::with_capacity(256);

        loop {
            // Error transient (mis. device berganti sesaat) tidak langsung
            // mematikan thread capture — dicatat lalu dicoba lagi.
            if let Err(e) = capture_client.read_from_device_to_deque(&mut byte_queue) {
                eprintln!("Baca audio WASAPI gagal sementara: {e}");
                std::thread::sleep(poll_interval);
                continue;
            }

            // Ubah byte float32 interleaved stereo -> sample mono f32,
            // kumpulkan dulu di buffer lokal (belum kunci mutex sama sekali).
            mono_batch.clear();
            while byte_queue.len() >= blockalign {
                let mut frame_bytes = [0u8; 32]; // cukup untuk beberapa channel float32
                let frame_len = blockalign.min(frame_bytes.len());
                for b in frame_bytes.iter_mut().take(frame_len) {
                    *b = byte_queue.pop_front().unwrap();
                }
                let bytes_per_sample = 4usize;
                let mut sum = 0f32;
                for ch in 0..channels {
                    let start = ch * bytes_per_sample;
                    if start + 4 <= frame_len {
                        let v = f32::from_le_bytes([
                            frame_bytes[start],
                            frame_bytes[start + 1],
                            frame_bytes[start + 2],
                            frame_bytes[start + 3],
                        ]);
                        sum += v;
                    }
                }
                mono_batch.push(sum / channels as f32);
            }

            // Kunci mutex SEKALI untuk seluruh batch hasil polling ini.
            if !mono_batch.is_empty() {
                push_mono_samples(&ring, mono_batch.iter().copied());
            }

            std::thread::sleep(poll_interval);
        }
    }
}

/// Loopback via PulseAudio/PipeWire (`libpulse-simple-binding`). Device
/// `"@DEFAULT_MONITOR@"` adalah nama spesial yang di-resolve SERVER
/// (bukan client) jadi monitor source dari sink default saat ini — sama
/// persis yang dipakai `parec`/`pacat -r` bawaan `pulseaudio-utils`. Ini yang
/// membuatnya "loopback": kita capture dari OUTPUT (speaker), bukan input
/// (mikrofon).
#[cfg(target_os = "linux")]
mod linux_loopback {
    use super::{push_mono_samples, SharedRing, SAMPLE_RATE};
    use libpulse_binding::sample::{Format, Spec};
    use libpulse_binding::stream::Direction;
    use libpulse_simple_binding::Simple;

    pub fn run(ring: SharedRing) -> anyhow::Result<()> {
        let spec = Spec {
            format: Format::FLOAT32NE,
            channels: 2,
            rate: SAMPLE_RATE,
        };
        if !spec.is_valid() {
            anyhow::bail!("Sample spec PulseAudio tidak valid (channel/rate/format)");
        }

        let simple = Simple::new(
            None,                      // server default
            "trofeo-lcd",              // nama aplikasi
            Direction::Record,         // arah: rekam...
            Some("@DEFAULT_MONITOR@"), // ...dari monitor sink default (loopback)
            "system audio (EQ visualizer)",
            &spec,
            None, // channel map default
            None, // buffering attr default
        )
        .map_err(|e| {
            anyhow::anyhow!(
                "gagal konek ke server PulseAudio/PipeWire ({e}) — pastikan \
                 PulseAudio jalan, atau kalau pakai PipeWire pastikan paket \
                 'pipewire-pulse' terpasang & aktif"
            )
        })?;

        // 4 byte/sample (F32NE) x 2 channel = 8 byte/frame. ~512 frame per
        // panggilan `read()` — angka kecil sengaja dipilih supaya latensi
        // update bar EQ tetap rendah (mirip granularity WASAPI di Windows).
        const CHANNELS: usize = 2;
        const BYTES_PER_SAMPLE: usize = 4;
        const FRAME_BYTES: usize = BYTES_PER_SAMPLE * CHANNELS;
        let mut byte_buf = vec![0u8; FRAME_BYTES * 512];
        let mut mono_batch: Vec<f32> = Vec::with_capacity(512);

        loop {
            // `read()` blocking — server yang mengatur pacing (sama seperti
            // `parec`), jadi TIDAK perlu sleep manual tiap iterasi di sini.
            if let Err(e) = simple.read(&mut byte_buf) {
                eprintln!("Baca audio PulseAudio/PipeWire gagal sementara: {e}");
                std::thread::sleep(std::time::Duration::from_millis(50));
                continue;
            }

            mono_batch.clear();
            for frame in byte_buf.chunks_exact(FRAME_BYTES) {
                let l = f32::from_ne_bytes([frame[0], frame[1], frame[2], frame[3]]);
                let r = f32::from_ne_bytes([frame[4], frame[5], frame[6], frame[7]]);
                mono_batch.push((l + r) * 0.5);
            }
            if !mono_batch.is_empty() {
                push_mono_samples(&ring, mono_batch.iter().copied());
            }
        }
    }
}

/// Sumber sintetis untuk OS selain Windows & Linux, supaya kode ini tetap
#[cfg(target_os = "macos")]
#[cfg(target_os = "macos")]
use crate::audio_macos as macos_loopback;

/// Fallback untuk platform tanpa jalur loopback asli: hanya supaya kode bisa
/// di-compile & dijalankan (tanpa audio nyata) di sana. macOS punya jalur
/// asli sendiri (`macos_loopback`), Linux juga (lihat `linux_loopback`).
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod fallback {
    use super::{push_mono_samples, SharedRing, SAMPLE_RATE};
    use std::f32::consts::PI;
    use std::thread::sleep;
    use std::time::Duration;

    pub fn run(ring: SharedRing) {
        let mut phase = 0f32;
        let chunk = 512usize;
        let dt = chunk as f32 / SAMPLE_RATE as f32;
        loop {
            let samples = (0..chunk).map(|i| {
                let t = phase + i as f32 / SAMPLE_RATE as f32;
                // campuran beberapa nada supaya bar EQ tidak flat saat testing.
                0.3 * (2.0 * PI * 220.0 * t).sin()
                    + 0.2 * (2.0 * PI * 880.0 * t).sin()
                    + 0.1 * (2.0 * PI * 3000.0 * t).sin()
            });
            push_mono_samples(&ring, samples);
            phase += dt;
            sleep(Duration::from_secs_f32(dt));
        }
    }
}
