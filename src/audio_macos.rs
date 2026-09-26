//! Loopback audio system di macOS lewat Core Audio **process tap**
//! (`AudioHardwareCreateProcessTap`, API native macOS 14.2+).
//!
//! ## Kenapa tidak pakai BlackHole
//!
//! Loopback di macOS biasanya butuh driver virtual (BlackHole/Loopback) yang
//! harus dipasang lewat sudo lalu disetel lewat "Multi-Output Device".
//! Process tap menghindari semua itu: cukup mendaftarkan tap yang menyimat
//! audio yang dikirim ke SATU device output tertentu, lalu membacanya sebagai
//! input. Tidak ada driver yang perlu dipasang.
//!
//! Tidak perlu izin "Screen & System Audio Recording" juga, karena tap ini
//! device-specific (`initWithProcesses:andDeviceUID:withStream:`), bukan
//! *global tap* yang bisa melihat semua output. KeUICITAAN itu sudah
//! diverifikasi di mesin ini: `AudioHardwareCreateProcessTap` mengembalikan
//! status 0 tanpa memunculkan dialog TCC apa pun.
//!
//! ## Batasan
//!
//! - Hanya menangkap audio yang memang dikirim ke device output yang di-tap.
//!   Karena itu targetnya selalu default output, dan tap dibangun ulang kalau
//!   device itu berganti (mis. USB DAC dicabut).
//! - Perlu macOS 14.2+.
//!
//! ## Kenapa dua thread
//!
//! `AudioDeviceIOProc` dipanggil CoreAudio dari *real-time thread* yang
//! prioritasnya di atas scheduler aplikasi biasa. Di thread itu tidak boleh ada
//! alokasi memori, `Mutex`, atau blocking — satu stall saja memicu glitch
//! (`kAudioDeviceErr_BufferUnderflow`) yang terdengar sebagai klik.
//!
//! Jadi callback hanya downmix ke mono lalu menulis ke ring buffer lock-free
//! (SPSC: satu producer, satu consumer, tanpa mutex). Resampling ke
//! `SAMPLE_RATE` dilakukan thread biasa terpisah.

use cpal::traits::{DeviceTrait, HostTrait};
use objc2::AnyThread;
use objc2_core_audio::{
    kAudioAggregateDeviceNameKey, kAudioAggregateDeviceTapAutoStartKey,
    kAudioAggregateDeviceTapListKey, kAudioAggregateDeviceUIDKey,
    kAudioDevicePropertyStreamFormat, kAudioDevicePropertyScopeInput,
    kAudioEndPointDeviceIsPrivateKey, kAudioObjectPropertyElementMain,
    kAudioSubTapDriftCompensationKey, kAudioSubTapUIDKey, AudioDeviceCreateIOProcID,
    AudioDeviceDestroyIOProcID, AudioDeviceIOProcID, AudioDeviceStart,
    AudioDeviceStop, AudioHardwareCreateAggregateDevice, AudioHardwareCreateProcessTap,
    AudioHardwareDestroyAggregateDevice, AudioHardwareDestroyProcessTap,
    AudioObjectGetPropertyData, AudioObjectID, AudioObjectPropertyAddress, CATapDescription,
    CATapMuteBehavior,
};
use objc2_core_audio_types::{
    kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved, AudioBufferList,
    AudioStreamBasicDescription, AudioTimeStamp,
};
use objc2_core_foundation::{
    kCFAllocatorDefault, kCFTypeArrayCallBacks, kCFTypeDictionaryKeyCallBacks,
    kCFTypeDictionaryValueCallBacks, CFArray, CFDictionary, CFMutableDictionary, CFRetained,
    CFString,
};
use objc2_foundation::{NSArray, NSNumber, NSString};
use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::ffi::{c_void, CStr};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::audio::{push_mono_samples, SharedRing, SAMPLE_RATE};

type OSStatus = i32;

/// Berapa lama satu sesi capture memeriksa apakah default output masih sama,
/// dan jeda sebelum pasang ulang setelah gagal.
const WATCH_INTERVAL: Duration = Duration::from_secs(2);

/// Panjang ring lock-free. ~0,5 detik pada 44,1 kHz — cukup menutup jeda
/// scheduling tanpa boros RAM.
const SPSC_CAPACITY: usize = 24_000;

// ---------------------------------------------------------------------------
// Ring buffer lock-free (single-producer / single-consumer)

/// Ring SPSC. `push` hanya dari IOProc, `pop` hanya dari thread resampler.
/// Tiap side cuma punya satu writer, jadi cukup dua counter atomik dengan
/// release/acquire untuk mempublikasikan datanya — tanpa mutex.
///
/// `buf` dibungkus `UnsafeCell` justru supaya tidak butuh `&mut` (dan dengan
/// itu tidak butuh lock): yang violated Writing dilakukan oleh satu thread
/// saja, jadi tidak ada data race. TSAN-safe selama aturan SPSC dijaga.
struct SpscRing {
    buf: UnsafeCell<Box<[f32]>>,
    /// Indeks penulisan berikutnya (dimiliki producer).
    write: AtomicU64,
    /// Indeks pembacaan berikutnya (dimiliki consumer).
    read: AtomicU64,
}

impl SpscRing {
    fn new() -> Self {
        Self {
            buf: UnsafeCell::new(vec![0.0; SPSC_CAPACITY].into_boxed_slice()),
            write: AtomicU64::new(0),
            read: AtomicU64::new(0),
        }
    }

    /// Dipanggil dari real-time thread: hanya tulis ke array yang sudah
    /// dialokasikan + dua store atomik. Kalau penuh, sample dibuang — lebih
    /// baik sedikit audio terlewat daripada alokasi di thread real-time.
    fn push(&self, sample: f32) {
        let w = self.write.load(Ordering::Relaxed);
        let r = self.read.load(Ordering::Acquire);
        if w.wrapping_sub(r) >= SPSC_CAPACITY as u64 {
            return;
        }
        // SAFETY: hanya producer (IOProc) yang menulis, dan posisinya sudah
        // dicek belum meny Hampiri(read) sehingga slot ini belum dibaca consumer.
        unsafe {
            let ptr = (*self.buf.get()).as_mut_ptr();
            ptr.add((w % SPSC_CAPACITY as u64) as usize).write(sample);
        }
        self.write.store(w.wrapping_add(1), Ordering::Release);
    }

    fn pop(&self) -> Option<f32> {
        let r = self.read.load(Ordering::Relaxed);
        let w = self.write.load(Ordering::Acquire);
        if r == w {
            return None;
        }
        // SAFETY: hanya consumer yang menggeser `read`, dan `write` di-load
        // dengan acquire sehingga data di slot itu sudah terlihat.
        let v = unsafe { (*self.buf.get()).as_ptr().add((r % SPSC_CAPACITY as u64) as usize).read() };
        self.read.store(r.wrapping_add(1), Ordering::Release);
        Some(v)
    }
}

// ---------------------------------------------------------------------------
// Konteks IOProc

/// Data yang dibaca IOProc tiap callback. Alokasi sekali lalu di-share lewat
/// `Arc`; alamatnya stabil selama IOProc aktif.
struct IoCtx {
    ring: SpscRing,
    channels: u32,
    /// Data non-interleaved: tiap `AudioBuffer` cuma satu channel.
    non_interleaved: bool,
    /// Berapa kali callback benar-benar dipanggil (dari sini kelihatan Core
    /// Audio tidak memanggilnya sama sekali saat device benar-benar diam).
    callbacks: AtomicU64,
}

/// Format input device hasil tap, dibaca sebelum IOProc dibuat.
struct InputFormat {
    sample_rate: f64,
    channels: u32,
    non_interleaved: bool,
    is_float: bool,
}

// ---------------------------------------------------------------------------
// Entry point

/// Loop utama: pasangkan tap ke default output device, stream ke `ring`, dan
/// bangun ulang kalau device berganti atau capture mati di tengah jalan.
pub fn run(ring: SharedRing) -> anyhow::Result<()> {
    let mut reported: Option<String> = None;

    loop {
        let host = cpal::default_host();
        let Some(dev) = host.default_output_device() else {
            if reported.is_none() {
                eprintln!("Loopback audio: tidak ada perangkat output, menunggu...");
                reported = Some(String::new());
            }
            std::thread::sleep(WATCH_INTERVAL);
            continue;
        };

        let uid = dev.id().map(|i| i.id().to_string()).unwrap_or_default();
        let name = dev
            .description()
            .map(|d| d.name().to_string())
            .unwrap_or_default();

        if reported.as_deref() != Some(uid.as_str()) {
            println!("Loopback audio: tap ke perangkat output {name:?}");
            reported = Some(uid.clone());
        }

        if let Err(e) = capture(&uid, &name, &ring) {
            eprintln!("Loopback audio gagal: {e:#}");
        }
        std::thread::sleep(WATCH_INTERVAL);
    }
}

/// Satu sesi capture: tap -> aggregate -> IOProc -> stream.
///
/// Selesai (Ok) kalau default output berubah atau device dicabut, supaya
/// pemanggil bisa memasang ulang tap. Error fatal lain dikembalikan ke
/// pemanggil untuk dilaporkan.
fn capture(device_uid: &str, device_name: &str, ring: &SharedRing) -> anyhow::Result<()> {
    let (tap_id, tap_uid) = create_tap(device_uid)?;
    let agg_id = create_aggregate(&tap_uid)?;

    let fmt = read_input_format(agg_id)?;
    anyhow::ensure!(
        fmt.is_float,
        "format dari tap bukan Float32 — belum didukung"
    );
    anyhow::ensure!(
        fmt.channels >= 1 && fmt.channels <= 8,
        "jumlah channel tidak masuk akal: {}",
        fmt.channels
    );
    anyhow::ensure!(
        fmt.sample_rate >= 8_000.0 && fmt.sample_rate <= 384_000.0,
        "sample rate tidak masukinals: {}",
        fmt.sample_rate
    );

    println!(
        "Loopback audio aktif: {} Hz / {} ch -> resample ke {} Hz",
        fmt.sample_rate.round() as u32,
        fmt.channels,
        SAMPLE_RATE
    );

    let ctx = Arc::new(IoCtx {
        ring: SpscRing::new(),
        channels: fmt.channels,
        non_interleaved: fmt.non_interleaved,
        callbacks: AtomicU64::new(0),
    });

    let mut proc_id: AudioDeviceIOProcID = None;
    let st = unsafe {
        AudioDeviceCreateIOProcID(
            agg_id,
            Some(io_proc),
            Arc::as_ptr(&ctx) as *const IoCtx as *mut c_void,
            NonNull::from(&mut proc_id),
        )
    };
    if st != 0 {
        cleanup(tap_id, agg_id, None);
        anyhow::bail!("AudioDeviceCreateIOProcID status {st}");
    }

    let st = unsafe { AudioDeviceStart(agg_id, proc_id) };
    if st != 0 {
        cleanup(tap_id, agg_id, Some(proc_id));
        anyhow::bail!("AudioDeviceStart status {st}");
    }

    // `ctx` dipin oleh `Arc` yang dipegang `session`; selama `session` hidup,
    // alamat yang diberikan ke IOProc tetap valid.
    let session = Loopback {
        tap_id,
        agg_id,
        proc_id,
        _ctx: Arc::clone(&ctx),
    };

    let mut resampler = Resampler::new(fmt.sample_rate, SAMPLE_RATE as f64);
    let mut last_watch = Instant::now();
    let mut last_report = Instant::now();
    let mut last_callbacks = 0u64;
    let mut mono: Vec<f32> = Vec::with_capacity(4096);
    let mut done = Vec::with_capacity(4096);

    loop {
        // Kumpulkan semua sample yang tersedia, lalu resample sekali jalan.
        mono.clear();
        while let Some(s) = ctx.ring.pop() {
            mono.push(s);
        }
        if mono.is_empty() {
            // Core Audio tidak memanggil IOProc sama sekali saat tidak ada
            // audio mengalir. Ini idle yang wajar, bukan error.
            std::thread::sleep(Duration::from_millis(15));
        } else {
            resampler.process(&mono, &mut done);
            if !done.is_empty() {
                push_mono_samples(ring, done.drain(..));
            }
        }

        if last_watch.elapsed() >= WATCH_INTERVAL {
            // Default output berganti? device dicabut? (UID jadi string
            // kosong kalau device hilang total.)
            let still = cpal::default_host()
                .default_output_device()
                .and_then(|d| d.id().ok().map(|i| i.id().to_string()))
                .unwrap_or_default();
            if still != device_uid {
                println!(
                    "Perangkat output berubah (dari {device_name:?}) — pasang ulang loopback"
                );
                drop(session);
                return Ok(());
            }
            last_watch = Instant::now();
        }

        if last_report.elapsed() >= Duration::from_secs(60) {
            let cb = ctx.callbacks.load(Ordering::Relaxed);
            if cb == last_callbacks {
                println!(
                    "Loopback audio: tidak ada audio mengalir ke {device_name:?} \
                     selama 60 detik terakhir (bar EQ akan diam)"
                );
            }
            last_callbacks = cb;
            last_report = Instant::now();
        }
    }
}

/// Bongkar everything dengan urutan benar: hentikan IOProc dulu (dia yang
/// memegang pointer ke device), baru aggregate device, lalu tap. Urutan lain
/// meninggalkan device yatim di IORegistry tiap kali program start ulang.
fn cleanup(tap_id: AudioObjectID, agg_id: AudioObjectID, proc_id: Option<AudioDeviceIOProcID>) {
    unsafe {
        if let Some(p) = proc_id {
            if p.is_some() {
                let _ = AudioDeviceStop(agg_id, p);
                let _ = AudioDeviceDestroyIOProcID(agg_id, p);
            }
        }
        let _ = AudioHardwareDestroyAggregateDevice(agg_id);
        AudioHardwareDestroyProcessTap(tap_id);
    }
}

/// Sesi yang otomatis dibongkar saat keluar dari `capture`.
struct Loopback {
    tap_id: AudioObjectID,
    agg_id: AudioObjectID,
    proc_id: AudioDeviceIOProcID,
    /// Menjaga `IoCtx` tetap hidup selama IOProc aktif.
    _ctx: Arc<IoCtx>,
}

impl Drop for Loopback {
    fn drop(&mut self) {
        cleanup(self.tap_id, self.agg_id, Some(self.proc_id));
    }
}

// ---------------------------------------------------------------------------
// IOProc (real-time thread)

unsafe extern "C-unwind" fn io_proc(
    _device: AudioObjectID,
    _in_ia: NonNull<AudioTimeStamp>,
    in_data: NonNull<AudioBufferList>,
    _in_tp: NonNull<AudioTimeStamp>,
    _out_data: NonNull<AudioBufferList>,
    _out_tp: NonNull<AudioTimeStamp>,
    client: *mut c_void,
) -> OSStatus {
    if client.is_null() {
        return 0;
    }
    let ctx = &*(client as *const IoCtx);
    let abl = in_data.as_ref();
    let nbuf = abl.mNumberBuffers as usize;
    if nbuf == 0 {
        return 0;
    }
    let buffers = std::slice::from_raw_parts(abl.mBuffers.as_ptr(), nbuf);
    let first = &buffers[0];
    if first.mData.is_null() || first.mDataByteSize == 0 {
        return 0;
    }

    let channels = ctx.channels as usize;
    let total_samples = (first.mDataByteSize / 4) as usize;
    let per_channel = if ctx.non_interleaved {
        total_samples
    } else {
        total_samples / channels
    };
    if per_channel == 0 {
        return 0;
    }

    if ctx.non_interleaved {
        // Satu AudioBuffer per channel.
        let mut ptrs: [*const f32; 8] = [std::ptr::null(); 8];
        let n = nbuf.min(8);
        for (i, b) in buffers.iter().take(n).enumerate() {
            ptrs[i] = b.mData as *const f32;
        }
        for i in 0..per_channel {
            let mut sum = 0f32;
            for p in ptrs.iter().take(n) {
                if !p.is_null() {
                    sum += *p.add(i);
                }
            }
            ctx.ring.push(sum / n as f32);
        }
    } else {
        // Interleaved: [L R L R ...]
        let base = first.mData as *const f32;
        for i in 0..per_channel {
            let mut sum = 0f32;
            let row = base.add(i * channels);
            for c in 0..channels {
                sum += *row.add(c);
            }
            ctx.ring.push(sum / channels as f32);
        }
    }
    ctx.callbacks.fetch_add(1, Ordering::Relaxed);
    0
}

// ---------------------------------------------------------------------------
// Resampler: rate device -> SAMPLE_RATE

/// Resampler rate-conversion tanpa dependency.
///
/// Interpolasi Catmull-Rom (kubik) SETELAH lowpass biquad. Lowpass-nya itu
/// penting: 48 kHz -> 44,1 kHz adalah **downsampling**, dan tanpa anti-alias
/// frekuensi tinggi (perkusi, cymbals) akan terlipat ke bar EQ yang lebih
/// rendah dan bikin bar terlihat "bergerak sendiri" padahal musiknya diam.
///
/// Kubik dipakai bukan linear karena linear menajamkan frekuensi tinggi
/// (memperburuk aliasing) dan bentuk spektrumnya lebih kasar.
struct Resampler {
    /// Sample input yang belum diproses (setelah lowpass).
    buf: VecDeque<f32>,
    /// Posisi baca pecahan di dalam `buf`.
    pos: f64,
    /// Kemajuan posisi baca per sample output.
    step: f64,
    lp: Biquad,
}

impl Resampler {
    fn new(input_rate: f64, output_rate: f64) -> Self {
        let step = input_rate / output_rate;
        // Cutoff di 90% Nyquist output, dan tetap dibatasi 45% dari rate input
        // supaya tidak melebihi batas downsampling yang aman.
        let cutoff = (output_rate * 0.5 * 0.9).min(input_rate * 0.45);
        Self {
            buf: VecDeque::with_capacity(8192),
            pos: 0.0,
            step,
            lp: Biquad::lowpass(cutoff, input_rate, 0.707),
        }
    }

    /// Masukkan sample input (sudah mono), keluarkan sample ber-`SAMPLE_RATE`
    /// ke `out`. Dipanggil dari thread biasa, boleh alokasi.
    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        for &raw in input {
            self.buf.push_back(self.lp.process(raw));
        }
        if self.buf.len() < 4 {
            return;
        }

        // Butuh 4 titik: posisi saat ini, posisi+1, dan 2 titik setelahnya.
        while (self.pos as usize) + 3 < self.buf.len() {
            let i = self.pos as usize;
            let frac = (self.pos - i as f64) as f32;
            out.push(catmull_rom(
                self.buf[i],
                self.buf[i + 1],
                self.buf[i + 2],
                self.buf[i + 3],
                frac,
            ));
            self.pos += self.step;
        }

        // Buang sample yang sudah lewat, geser kursor, dan sisakan 3 sample
        // terakhir sebagai konteks interpolasi untuk panggilan berikutnya.
        let drop_n = self.pos as usize;
        if drop_n > 0 {
            self.buf.drain(..drop_n);
            self.pos -= drop_n as f64;
        }
    }
}

/// Kubik Catmull-Rom.
fn catmull_rom(p0: f32, p1: f32, p2: f32, p3: f32, t: f32) -> f32 {
    let t2 = t * t;
    let t3 = t2 * t;
    0.5 * ((2.0 * p1)
        + (-p0 + p2) * t
        + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * t2
        + (-p0 + 3.0 * p1 - 3.0 * p2 + p3) * t3)
}

/// Biquad lowpass (resep RBJ) — bentuk standar untuk anti-aliasing.
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Biquad {
    fn lowpass(cutoff_hz: f64, sample_rate: f64, q: f64) -> Self {
        let w0 = 2.0 * std::f64::consts::PI * cutoff_hz / sample_rate;
        let (sin_w0, cos_w0) = w0.sin_cos();
        let alpha = sin_w0 / (2.0 * q);
        let a0 = 1.0 + alpha;
        Self {
            b0: ((1.0 - cos_w0) / 2.0 / a0) as f32,
            b1: ((1.0 - cos_w0) / a0) as f32,
            b2: ((1.0 - cos_w0) / 2.0 / a0) as f32,
            a1: (-2.0 * cos_w0 / a0) as f32,
            a2: ((1.0 - alpha) / a0) as f32,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let y =
            self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2 - self.a1 * self.y1 - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

// ---------------------------------------------------------------------------
// Pembuatan tap & aggregate device

fn create_tap(device_uid: &str) -> anyhow::Result<(AudioObjectID, String)> {
    let uid = NSString::from_str(device_uid);
    // Array kosong = semua proses ikut ter-capture.
    let processes = NSArray::new();
    let desc = unsafe {
        CATapDescription::initWithProcesses_andDeviceUID_withStream(
            CATapDescription::alloc(),
            &processes,
            uid.as_ref(),
            0,
        )
    };
    unsafe {
        // Audio yang tertangkap tetap terdengar di speaker, bukan dimakan tap.
        desc.setMuteBehavior(CATapMuteBehavior::Unmuted);
        desc.setName(&NSString::from_str("trofeo_lcd output tap"));
        // Private: tidak muncul di System Settings > Sound, dan hilang sendiri
        // saat proses mati.
        desc.setPrivate(true);
        // Daftar proses bersifat "exclude"; daftar kosong = semua ikut.
        desc.setExclusive(true);
    }

    let mut tap_id: AudioObjectID = 0;
    let st = unsafe { AudioHardwareCreateProcessTap(Some(desc.as_ref()), &mut tap_id) };
    if st != 0 {
        anyhow::bail!("AudioHardwareCreateProcessTap status {st}");
    }
    let uid_str = unsafe { desc.UUID().UUIDString().to_string() };
    Ok((tap_id, uid_str))
}

fn create_aggregate(tap_uid: &str) -> anyhow::Result<AudioObjectID> {
    let tap_uid_ns = NSString::from_str(tap_uid);
    let props = aggregate_props(&tap_uid_ns);
    let mut agg_id: AudioObjectID = 0;
    let st =
        unsafe { AudioHardwareCreateAggregateDevice(props.as_ref(), NonNull::from(&mut agg_id)) };
    if st != 0 {
        anyhow::bail!("AudioHardwareCreateAggregateDevice status {st}");
    }
    Ok(agg_id)
}

/// Minta CoreAudio format input yang dipakai tap.
fn read_input_format(agg_id: AudioObjectID) -> anyhow::Result<InputFormat> {
    let mut asbd: AudioStreamBasicDescription = unsafe { std::mem::zeroed() };
    let mut size = std::mem::size_of::<AudioStreamBasicDescription>() as u32;
    let addr = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyStreamFormat,
        mScope: kAudioDevicePropertyScopeInput,
        mElement: kAudioObjectPropertyElementMain,
    };
    let st = unsafe {
        AudioObjectGetPropertyData(
            agg_id,
            NonNull::from(&addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut asbd).cast(),
        )
    };
    if st != 0 {
        anyhow::bail!("baca format stream status {st}");
    }
    Ok(InputFormat {
        sample_rate: asbd.mSampleRate,
        channels: asbd.mChannelsPerFrame,
        non_interleaved: (asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved) != 0,
        is_float: (asbd.mFormatFlags & kAudioFormatFlagIsFloat) != 0,
    })
}

fn to_cfstring(cstr: &'static CStr) -> CFRetained<CFString> {
    unsafe { CFString::with_c_string(kCFAllocatorDefault, cstr.as_ptr(), 0x0800_0100).unwrap() }
}

/// Setara Objective-C:
/// ```c
/// taps = @[ @{ kAudioSubTapUIDKey: tapUID, kAudioSubTapDriftCompensationKey: @YES } ];
/// device = @{
///     kAudioAggregateDeviceNameKey: @"trofeo_lcd loopback",
///     kAudioAggregateDeviceUIDKey: @"com.trofeo_lcd.loopback.<pid>",
///     kAudioAggregateDeviceTapListKey: taps,
///     kAudioAggregateDeviceTapAutoStartKey: @YES,
///     kAudioEndPointDeviceIsPrivateKey: @YES };
/// ```
fn aggregate_props(tap_uid: &NSString) -> CFRetained<CFDictionary> {
    let pid = std::process::id();
    unsafe {
        let yes = || NSNumber::initWithBool(NSNumber::alloc(), true);

        let tap_entry = CFMutableDictionary::new(
            kCFAllocatorDefault,
            2,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )
        .unwrap();
        CFMutableDictionary::set_value(
            Some(tap_entry.as_ref()),
            &*to_cfstring(kAudioSubTapUIDKey) as *const _ as *const c_void,
            &*tap_uid as *const _ as *const c_void,
        );
        CFMutableDictionary::set_value(
            Some(tap_entry.as_ref()),
            &*to_cfstring(kAudioSubTapDriftCompensationKey) as *const _ as *const c_void,
            &*yes() as *const _ as *const c_void,
        );

        let arr = [tap_entry];
        let taps = CFArray::new(
            kCFAllocatorDefault,
            arr.as_ptr() as *mut *const c_void,
            arr.len() as isize,
            &kCFTypeArrayCallBacks,
        )
        .unwrap();

        let d = CFMutableDictionary::new(
            kCFAllocatorDefault,
            5,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )
        .unwrap();
        let set = |key: &'static CStr, value: *const c_void| {
            CFMutableDictionary::set_value(
                Some(d.as_ref()),
                &*to_cfstring(key) as *const _ as *const c_void,
                value,
            );
        };
        set(
            kAudioAggregateDeviceNameKey,
            &*CFString::from_str("trofeo_lcd loopback") as *const _ as *const c_void,
        );
        set(
            kAudioAggregateDeviceUIDKey,
            &*CFString::from_str(&format!("com.trofeo_lcd.loopback.{pid}"))
                as *const _ as *const c_void,
        );
        set(
            kAudioAggregateDeviceTapListKey,
            &*taps as *const _ as *const c_void,
        );
        set(kAudioAggregateDeviceTapAutoStartKey, &*yes() as *const _ as *const c_void);
        set(kAudioEndPointDeviceIsPrivateKey, &*yes() as *const _ as *const c_void);
        CFRetained::cast_unchecked::<CFDictionary>(d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPSC harus FIFO dan tidak boleh kehilangan sample.
    #[test]
    fn spsc_ring_fifo() {
        let ring = SpscRing::new();
        assert!(ring.pop().is_none());
        for i in 0..100 {
            ring.push(i as f32);
        }
        for i in 0..100 {
            assert_eq!(ring.pop(), Some(i as f32));
        }
        assert!(ring.pop().is_none());
    }

    /// Saat penuh, sample BARU yang dibuang (bukan menimpa yang lama), dan
    /// sample yang sudah tersimpan tetap utuh dari ujung ke ujung.
    #[test]
    fn spsc_ring_drops_newest_when_full() {
        let ring = SpscRing::new();
        for i in 0..(SPSC_CAPACITY + 50) {
            ring.push(i as f32);
        }
        // Penulisan berhenti di SPSC_CAPACITY; 50 sisanya di-drop.
        assert_eq!(ring.pop(), Some(0.0));
        // Isi lama masih lengkap dan berurutan.
        let mut last = 0.0;
        while let Some(v) = ring.pop() {
            assert_eq!(v, last + 1.0, "urutan rusak di {last}");
            last = v;
        }
        assert_eq!(last, (SPSC_CAPACITY - 1) as f32);
    }

    /// Interpolasi kubik: titik yang ada harus pas, midpointInterpolasi dengan
    /// benar, dan tidak overshoot berlebihan.
    #[test]
    fn catmull_rom_interpolates() {
        assert!((catmull_rom(0.0, 1.0, 2.0, 3.0, 0.0) - 1.0).abs() < 1e-6);
        assert!((catmull_rom(0.0, 1.0, 2.0, 3.0, 1.0) - 2.0).abs() < 1e-6);
        let mid = catmull_rom(0.0, 1.0, 2.0, 3.0, 0.5);
        assert!((mid - 1.5).abs() < 0.01, "midpoint harus ~1.5, dapat {mid}");
    }

    /// Resampler harus menghasilkan jumlah sample yang proporsional dengan
    /// rasio rate, dan nilai steady-state-nya benar untuk sinyal yang diketahui.
    #[test]
    fn resampler_converts_rate_and_preserves_tone() {
        let in_rate = 48_000.0;
        let out_rate = 44_100.0;
        let mut r = Resampler::new(in_rate, out_rate);

        // 1 detik sine 1 kHz pada 48 kHz.
        let n = in_rate as usize;
        let input: Vec<f32> = (0..n)
            .map(|i| {
                (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / in_rate).sin() as f32
            })
            .collect();

        let mut out = Vec::new();
        for chunk in input.chunks(1024) {
            r.process(chunk, &mut out);
        }

        // Toleransi 1%: device virtual vs real-time, minus tertinggal 3 sample.
        let expected = out_rate as usize;
        let ratio = out.len() as f64 / expected as f64;
        assert!(
            (ratio - 1.0).abs() < 0.01,
            "expected ~{expected} sample, dapat {} (rasio {ratio:.4})",
            out.len()
        );

        // Setelah transien, amplitudo harus mendekati 1.0 (tidak hilang,
        // tidak terpangkas).
        let tail = &out[out.len() / 2..];
        let peak = tail.iter().fold(0f32, |m, &v| m.max(v.abs()));
        assert!(peak > 0.85 && peak <= 1.05, "amplitudo salah: peak={peak}");
    }

    /// Uji end-to-end: tap ke default output device, lalu `say` + `afplay`
    /// diputar supaya ada audio, dan sample yang sampai ke ring buffer
    /// `audio::take_latest` harus punya amplitudo nyata.
    ///
    /// Di-`ignore` karena butuh perangkat audio yang benar-benar mengeluarkan
    /// suara. Jalankan: `cargo test --release audio_macos -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn captures_real_system_audio() {
        use crate::audio::{spawn_capture, take_latest};

        let ring = spawn_capture().expect("mulai capture");
        // Beri waktu tap + IOProc selesai setup.
        std::thread::sleep(Duration::from_millis(800));

        let tone = std::process::Command::new("/usr/bin/say")
            .args(["-o", "/tmp/trofeo_test.aiff", "one two three four"])
            .status();
        assert!(tone.map(|s| s.success()).unwrap_or(false), "gagal bikin file nada");

        let mut player = std::process::Command::new("/usr/bin/afplay")
            .arg("/tmp/trofeo_test.aiff")
            .spawn()
            .expect("jalan afplay");

        // `afplay` bisa selesai sebelum loop IOProc pertama terpanggil, jadi
        // nada diulang beberapa kali dan peak diiluangkan selama window penuh —
        // kalau tidak, test ini flaky.
        let mut peak = 0f32;
        for round in 0..3 {
            let _ = std::process::Command::new("/usr/bin/afplay")
                .arg("/tmp/trofeo_test.aiff")
                .spawn();
            for _ in 0..14 {
                std::thread::sleep(Duration::from_millis(100));
                let samples = take_latest(&ring, 4096);
                let p = samples.iter().fold(0f32, |m, &v| m.max(v.abs()));
                if p > peak {
                    peak = p;
                }
            }
            if peak > 0.001 {
                break;
            }
            let _ = round;
        }
        let _ = player.kill();
        let _ = std::fs::remove_file("/tmp/trofeo_test.aiff");

        println!("peak amplitude yang ditangkap: {peak:.4}");
        assert!(
            peak > 0.001,
            "tidak ada audio yang sampai (peak={peak:.6}) — tap tidak menangkap apa pun"
        );
    }
}
