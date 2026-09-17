//! Driver Rust untuk keluarga perangkat LCD USB "LY" Thermalright — termasuk
//! **Trofeo Vision 9.16 LCD** (VID:PID `0416:5408`) dan varian LY1 (`0416:5409`).
//!
//! Diterjemahkan langsung dari implementasi Python asli proyek
//! `thermalright-trcc-linux` (`src/trcc/adapters/.../ly_lcd.py`, kelas `LyLcd`),
//! termasuk dua detail non-obvious yang sengaja dipertahankan supaya perilaku
//! byte-per-byte identik dengan versi yang sudah tervalidasi di hardware nyata
//! (lihat catatan soal laporan issue #248 di source Python):
//!
//! 1. **`_prepare_frame` selalu `+1` chunk** — kalau `total_size` persis
//!    kelipatan 496, akan ada satu chunk "terminator" kosong ekstra di akhir.
//!    Ini BUKAN `ceil()` biasa; jangan "diperbaiki" jadi `div_ceil`.
//! 2. **`_write_frame` menambah `pos` dengan `USB_WRITE_SIZE` (4096) tetap**,
//!    walau tulisan terakhir cuma 2048 byte. Ini aman karena `prepare_frame`
//!    menjamin total buffer selalu kelipatan 2048 byte (lewat padding ke
//!    kelipatan-4 chunk), jadi sisa sebelum iterasi terakhir selalu tepat 0
//!    atau 2048 — tapi ini bukan loop "umum", jangan dipakai untuk ukuran lain.
//!
//! # Yang penting diketahui soal panel ini
//! - **Payload frame adalah gambar JPEG, bukan RGB565/RGB888 mentah.** Panel
//!   "jpeg=true" ini dibatasi kira-kira 450.000 byte per frame (konstanta
//!   `max_frame_bytes` versi C#, dikonfirmasi lewat pengukuran hardware nyata
//!   di source Python: ~360 KB tampil, ~570 KB gagal). `Framebuffer` di modul
//!   ini menyimpan piksel RGB888 dan punya `to_jpeg()` untuk encode ke JPEG
//!   sebelum dikirim.
//! - Resolusi Trofeo Vision 9.16 LCD, menurut komentar pada source Python
//!   asli, adalah **1920x462** — lihat konstanta [`TROFEO_VISION_9_16`].
//! - Rotasi gambar: `Handshake::rotate_180` SELALU `false` sekarang (heuristik
//!   otomatis sebelumnya, berdasar SUB byte, terbukti salah arah di hardware
//!   nyata — lihat riwayat di git). Kalau panel Anda perlu rotasi 180°, atur
//!   manual lewat `ROTATE_180_OVERRIDE` di `main.rs`, bukan lewat field ini.
//! - Endpoint bulk OUT/IN **tidak di-hardcode** — dideteksi otomatis dari
//!   deskriptor USB device saat `open()` (cari interface dengan sepasang
//!   endpoint bulk OUT+IN). Ini sengaja, karena alamat endpoint yang tertulis
//!   di source Python asli (`0x01`/`0x81`) terbukti **tidak selalu cocok**
//!   dengan hardware nyata — beda unit/firmware Trofeo Vision 9.16 bisa
//!   memakai alamat endpoint berbeda (dokumentasi protokol hasil decompile
//!   lama malah menyebut EP09 OUT). Kalau `open()` gagal dengan
//!   `LcdError::NotFound`, pakai [`LyLcd::probe_endpoints`] untuk melihat
//!   semua endpoint yang sebenarnya ada di device Anda.

use rusb::{Context, DeviceHandle, UsbContext};
use std::time::Duration;
use thiserror::Error;

mod font;
pub mod dxgi_capture;

pub const VENDOR_ID: u16 = 0x0416;
/// Trofeo Vision 9.16 LCD.
pub const PID_LY: u16 = 0x5408;
pub const PID_LY1: u16 = 0x5409;

const HANDSHAKE_HEADER: [u8; 16] = [
    0x02, 0xFF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];
const HANDSHAKE_READ_SIZE: usize = 512;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(1000);
const WRITE_TIMEOUT: Duration = Duration::from_millis(5000);
const READ_TIMEOUT: Duration = Duration::from_millis(1000);

const CHUNK_SIZE: usize = 512;
const CHUNK_HEADER_SIZE: usize = 16;
const CHUNK_DATA_SIZE: usize = 496;
const USB_WRITE_SIZE: usize = 4096;

/// Batas ukuran JPEG yang diterima firmware (konstanta C# TRCC 2.1.6).
pub const MAX_FRAME_BYTES: usize = 450_000;

/// Resolusi default Trofeo Vision 9.16 LCD, sesuai catatan di source Python asli.
pub const TROFEO_VISION_9_16: Resolution = Resolution::new(1920, 462);

#[derive(Debug, Error)]
pub enum LcdError {
    #[error("USB error: {0}")]
    Usb(#[from] rusb::Error),
    #[error("perangkat LY (0416:5408 / 0416:5409) tidak ditemukan")]
    NotFound,
    #[error("handshake gagal, respons tidak valid: {0:02x?}")]
    BadHandshake(Vec<u8>),
    #[error("frame kosong")]
    EmptyFrame,
    #[error("frame {0} byte melebihi batas firmware {MAX_FRAME_BYTES} byte")]
    FrameTooLarge(usize),
    #[error("gagal encode JPEG: {0}")]
    Jpeg(String),
}

pub type Result<T> = std::result::Result<T, LcdError>;

/// Varian keluarga LY, terdeteksi otomatis dari PID saat `open()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    /// 0416:5408 — chunk header byte[8]=1, jumlah chunk dibulatkan ke kelipatan 4.
    Ly,
    /// 0416:5409 — chunk header byte[8]=2, tanpa pembulatan chunk.
    Ly1,
}

impl Variant {
    fn from_pid(pid: u16) -> Option<Self> {
        match pid {
            PID_LY => Some(Variant::Ly),
            PID_LY1 => Some(Variant::Ly1),
            _ => None,
        }
    }

    fn chunk_cmd(self) -> u8 {
        match self {
            Variant::Ly => 1,
            Variant::Ly1 => 2,
        }
    }

    fn pad_multiple(self) -> usize {
        match self {
            Variant::Ly => 4,
            Variant::Ly1 => 1,
        }
    }
}

/// Hasil parse respons handshake (setara `HandshakeResult` di Python).
#[derive(Debug, Clone)]
pub struct Handshake {
    pub raw_response: Vec<u8>,
    pub pm: u8,
    pub sub: u8,
    /// Heuristik rotasi 180° — lihat catatan modul. `true` = putar 180° sebelum encode.
    pub rotate_180: bool,
}

/// Resolusi layar dalam piksel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
}

impl Resolution {
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }
}

/// Handle ke perangkat LY LCD yang sudah terbuka.
pub struct LyLcd {
    handle: DeviceHandle<Context>,
    iface: u8,
    variant: Variant,
    /// Alamat endpoint bulk OUT & IN, dideteksi otomatis dari deskriptor USB
    /// device saat `open()` — TIDAK di-hardcode, karena alamat endpoint
    /// nyata terbukti berbeda antar hardware/driver (lihat catatan modul
    /// di atas soal 0x01 vs 0x09).
    ep_write: u8,
    ep_read: u8,
}

/// Info endpoint yang terdeteksi pada satu interface — dipakai `open()` dan
/// bisa dipakai untuk diagnostik (lihat `LyLcd::probe_endpoints`).
#[derive(Debug, Clone, Copy)]
pub struct EndpointInfo {
    pub interface: u8,
    pub address: u8,
    pub direction_in: bool,
}

impl LyLcd {
    /// Buka koneksi ke perangkat LY pertama yang ditemukan (0416:5408 atau 0416:5409).
    ///
    /// Endpoint bulk OUT/IN dideteksi otomatis dari deskriptor USB, bukan
    /// hardcode — beberapa unit Trofeo Vision 9.16 ternyata memakai alamat
    /// endpoint yang berbeda dari yang tertulis di source Python asli.
    pub fn open() -> Result<Self> {
        let context = Context::new()?;
        for device in context.devices()?.iter() {
            let desc = device.device_descriptor()?;
            if desc.vendor_id() != VENDOR_ID {
                continue;
            }
            let Some(variant) = Variant::from_pid(desc.product_id()) else {
                continue;
            };

            let handle = device.open()?;
            let config = device.active_config_descriptor()?;

            // Cari interface yang punya SEPASANG endpoint bulk OUT + IN.
            let mut found: Option<(u8, u8, u8)> = None; // (iface_num, ep_out, ep_in)
            for interface in config.interfaces() {
                for iface_desc in interface.descriptors() {
                    let mut ep_out = None;
                    let mut ep_in = None;
                    for ep in iface_desc.endpoint_descriptors() {
                        if ep.transfer_type() != rusb::TransferType::Bulk {
                            continue;
                        }
                        match ep.direction() {
                            rusb::Direction::Out => ep_out = Some(ep.address()),
                            rusb::Direction::In => ep_in = Some(ep.address()),
                        }
                    }
                    if let (Some(out), Some(inp)) = (ep_out, ep_in) {
                        found = Some((interface.number(), out, inp));
                    }
                }
            }

            let (iface_num, ep_write, ep_read) = found.ok_or(LcdError::NotFound)?;

            if handle.kernel_driver_active(iface_num).unwrap_or(false) {
                handle.detach_kernel_driver(iface_num)?;
            }
            handle.claim_interface(iface_num)?;

            return Ok(Self {
                handle,
                iface: iface_num,
                variant,
                ep_write,
                ep_read,
            });
        }
        Err(LcdError::NotFound)
    }

    pub fn variant(&self) -> Variant {
        self.variant
    }

    /// Alamat endpoint bulk OUT/IN yang terdeteksi (untuk debugging).
    pub fn endpoints(&self) -> (u8, u8) {
        (self.ep_write, self.ep_read)
    }

    /// Daftar SEMUA endpoint di semua interface perangkat LY pertama yang
    /// ditemukan — berguna untuk debugging kalau `open()` gagal `NotFound`
    /// atau handshake gagal karena endpoint salah.
    pub fn probe_endpoints() -> Result<Vec<EndpointInfo>> {
        let context = Context::new()?;
        for device in context.devices()?.iter() {
            let desc = device.device_descriptor()?;
            if desc.vendor_id() != VENDOR_ID || Variant::from_pid(desc.product_id()).is_none() {
                continue;
            }
            let config = device.active_config_descriptor()?;
            let mut out = Vec::new();
            for interface in config.interfaces() {
                for iface_desc in interface.descriptors() {
                    for ep in iface_desc.endpoint_descriptors() {
                        out.push(EndpointInfo {
                            interface: interface.number(),
                            address: ep.address(),
                            direction_in: ep.direction() == rusb::Direction::In,
                        });
                    }
                }
            }
            return Ok(out);
        }
        Err(LcdError::NotFound)
    }

    /// Kirim payload handshake (16 + 2032 byte) dan baca+validasi respons 512
    /// byte, lalu ekstrak PM/SUB — setara `LyLcd._do_handshake` di Python.
    pub fn handshake(&self) -> Result<Handshake> {
        let mut payload = vec![0u8; 16 + 2032];
        payload[..16].copy_from_slice(&HANDSHAKE_HEADER);

        self.handle
            .write_bulk(self.ep_write, &payload, HANDSHAKE_TIMEOUT)?;

        let mut resp = vec![0u8; HANDSHAKE_READ_SIZE];
        let n = self.handle.read_bulk(self.ep_read, &mut resp, HANDSHAKE_TIMEOUT)?;
        resp.truncate(n);

        if resp.len() < 37 || resp[0] != 3 || resp[1] != 0xFF || resp[8] != 1 {
            return Err(LcdError::BadHandshake(resp));
        }

        let (pm, sub) = match self.variant {
            Variant::Ly => {
                let mut raw = resp[20];
                if raw <= 3 {
                    raw = 1;
                }
                let pm = 64 + raw;
                let raw_sub = resp.get(22).copied().unwrap_or(0);
                (pm, raw_sub + 1)
            }
            Variant::Ly1 => {
                let raw_sub = resp.get(22).copied().unwrap_or(0);
                let pm = 49 + resp[20];
                (pm, raw_sub)
            }
        };

        // Heuristik lama (SUB 3/5 -> 180°, SUB 4 -> 0°) TERBUKTI SALAH di
        // hardware nyata: unit dengan PM=65 SUB=3 justru tampil upside-down
        // ketika rotate_180=true. Daripada menebak lagi, default sekarang
        // TIDAK merotasi apa pun — kalau panel Anda ternyata butuh rotasi
        // 180°, pakai override manual di `main.rs` (`ROTATE_180_OVERRIDE`)
        // daripada bergantung ke field ini.
        let rotate_180 = false;

        Ok(Handshake {
            raw_response: resp,
            pm,
            sub,
            rotate_180,
        })
    }

    /// Susun payload mentah (bytes JPEG) menjadi buffer chunk 512-byte,
    /// setara `LyLcd._prepare_frame` — termasuk quirk "+1 chunk"-nya.
    fn prepare_frame(&self, payload: &[u8]) -> Vec<u8> {
        build_chunks(self.variant, payload)
    }

    /// Tulis buffer frame yang sudah di-chunk dalam tulisan 4096-byte (2048
    /// byte untuk sisa terakhir pada LY), lalu baca ACK 512-byte. Setara
    /// `LyLcd._write_frame`, termasuk `pos += 4096` yang tetap.
    fn write_frame(&self, frame: &[u8]) -> Result<()> {
        let total_bytes = frame.len();
        let mut pos = 0usize;
        while pos < total_bytes {
            let remaining = total_bytes - pos;
            let write_size = if remaining >= USB_WRITE_SIZE {
                USB_WRITE_SIZE
            } else if self.variant == Variant::Ly {
                remaining.min(2048)
            } else {
                remaining
            };
            self.handle
                .write_bulk(self.ep_write, &frame[pos..pos + write_size], WRITE_TIMEOUT)?;
            pos += USB_WRITE_SIZE;
        }

        let mut ack = [0u8; HANDSHAKE_READ_SIZE];
        self.handle.read_bulk(self.ep_read, &mut ack, READ_TIMEOUT)?;
        Ok(())
    }

    /// Kirim satu frame. `payload` untuk panel ini harus berupa bytes JPEG
    /// yang sudah di-encode (lihat [`Framebuffer::to_jpeg`]) — bukan RGB mentah.
    pub fn send_frame(&self, payload: &[u8]) -> Result<()> {
        if payload.is_empty() {
            return Err(LcdError::EmptyFrame);
        }
        if payload.len() > MAX_FRAME_BYTES {
            return Err(LcdError::FrameTooLarge(payload.len()));
        }
        let frame = self.prepare_frame(payload);
        self.write_frame(&frame)
    }

    /// Rangkaian: encode `Framebuffer` ke JPEG (menghormati `handshake.rotate_180`)
    /// lalu kirim.
    pub fn send_framebuffer(&self, handshake: &Handshake, fb: &Framebuffer, quality: u8) -> Result<()> {
        let jpeg = if handshake.rotate_180 {
            fb.rotated_180().to_jpeg(quality)?
        } else {
            fb.to_jpeg(quality)?
        };
        self.send_frame(&jpeg)
    }

    pub fn release(self) -> Result<()> {
        self.handle.release_interface(self.iface)?;
        Ok(())
    }
}

/// Logika murni penyusunan chunk — dipisah dari `LyLcd` supaya bisa dites
/// tanpa hardware USB nyata. Setara `LyLcd._prepare_frame` di Python,
/// termasuk quirk "+1 chunk" saat `total_size` persis kelipatan 496.
fn build_chunks(variant: Variant, payload: &[u8]) -> Vec<u8> {
    let total_size = payload.len();
    let num_chunks = total_size / CHUNK_DATA_SIZE + 1;
    let last_chunk_data = total_size % CHUNK_DATA_SIZE;

    let mut chunks = vec![0u8; num_chunks * CHUNK_SIZE];
    for i in 0..num_chunks {
        let offset = i * CHUNK_SIZE;
        let is_last = i == num_chunks - 1;
        let data_len = if is_last { last_chunk_data } else { CHUNK_DATA_SIZE };

        chunks[offset] = 0x01;
        chunks[offset + 1] = 0xFF;
        chunks[offset + 2..offset + 6].copy_from_slice(&(total_size as u32).to_le_bytes());
        chunks[offset + 6..offset + 8].copy_from_slice(&(data_len as u16).to_le_bytes());
        chunks[offset + 8] = variant.chunk_cmd();
        chunks[offset + 9..offset + 11].copy_from_slice(&(num_chunks as u16).to_le_bytes());
        chunks[offset + 11..offset + 13].copy_from_slice(&(i as u16).to_le_bytes());

        let src_offset = i * CHUNK_DATA_SIZE;
        let dst = offset + CHUNK_HEADER_SIZE;
        chunks[dst..dst + data_len].copy_from_slice(&payload[src_offset..src_offset + data_len]);
    }

    // Padding zero-byte murni (bukan chunk ber-header) sampai kelipatan-4
    // chunk untuk LY (tanpa efek untuk LY1), supaya total panjang buffer
    // selalu kelipatan 2048 byte untuk `write_frame`.
    let pad_multiple = variant.pad_multiple();
    let mut padded_chunks = num_chunks;
    let remainder = padded_chunks % pad_multiple;
    if remainder != 0 {
        padded_chunks += pad_multiple - remainder;
    }
    chunks.resize(padded_chunks * CHUNK_SIZE, 0);
    chunks
}

/// Buffer piksel RGB888 yang bisa diisi manual (teks, grafik, monitoring
/// CPU/GPU, dll), lalu di-encode ke JPEG untuk dikirim ke panel.
pub struct Framebuffer {
    width: u32,
    height: u32,
    pixels: Vec<u8>, // RGB888, 3 byte per piksel
}

impl Framebuffer {
    pub fn new(resolution: Resolution) -> Self {
        Self {
            width: resolution.width,
            height: resolution.height,
            pixels: vec![0u8; (resolution.width * resolution.height * 3) as usize],
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.pixels
    }

    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        &mut self.pixels
    }

    /// Isi seluruh buffer dengan satu warna. Dipanggil sekali per frame (di
    /// atas seluruh layar 1920x462 = ~2.66MB), jadi ini titik yang lumayan
    /// panas — dioptimalkan pakai teknik "doubling": isi 3 byte pertama
    /// manual, lalu tiap langkah gandakan bagian yang sudah terisi lewat
    /// `copy_within` (memcpy blok besar, bukan loop per-piksel dengan
    /// bounds-check satu-satu). Jumlah operasi copy jadi O(log n), bukan O(n).
    pub fn clear(&mut self, r: u8, g: u8, b: u8) {
        if self.pixels.is_empty() {
            return;
        }
        self.pixels[0..3].copy_from_slice(&[r, g, b]);
        let mut filled = 3usize;
        let total = self.pixels.len();
        while filled < total {
            let copy_len = filled.min(total - filled);
            self.pixels.copy_within(0..copy_len, filled);
            filled += copy_len;
        }
    }

    pub fn set_pixel(&mut self, x: u32, y: u32, r: u8, g: u8, b: u8) {
        if x >= self.width || y >= self.height {
            return;
        }
        let idx = ((y * self.width + x) * 3) as usize;
        self.pixels[idx..idx + 3].copy_from_slice(&[r, g, b]);
    }

    /// Gambar kotak berisi warna — dasar untuk bar grafik monitoring & teks
    /// (tiap bit glyph di `draw_text` juga lewat sini). Dipakai ratusan-ribuan
    /// kali per frame (48 bar + puluhan karakter status), jadi dioptimalkan:
    /// per baris, isi piksel pertama lalu gandakan sisanya lewat `copy_within`
    /// (doubling, sama seperti `clear`) — bukan `set_pixel` per piksel dengan
    /// bounds-check individual, dan tanpa alokasi heap tambahan sama sekali.
    pub fn fill_rect(&mut self, x0: u32, y0: u32, w: u32, h: u32, r: u8, g: u8, b: u8) {
        let x0 = x0.min(self.width);
        let y0 = y0.min(self.height);
        let x1 = (x0 + w).min(self.width);
        let y1 = (y0 + h).min(self.height);
        if x0 >= x1 || y0 >= y1 {
            return;
        }

        let stride = self.width as usize; // piksel per baris
        let row_w = (x1 - x0) as usize; // piksel di rentang ini
        let row_bytes = row_w * 3;

        for y in y0..y1 {
            let row_start = (y as usize * stride + x0 as usize) * 3;
            let row = &mut self.pixels[row_start..row_start + row_bytes];
            row[0..3].copy_from_slice(&[r, g, b]);
            let mut filled = 3usize;
            while filled < row_bytes {
                let copy_len = filled.min(row_bytes - filled);
                row.copy_within(0..copy_len, filled);
                filled += copy_len;
            }
        }
    }

    /// Gambar teks memakai font bitmap 5x7 internal (lihat `font.rs`). Hanya
    /// mendukung huruf kapital, digit, dan simbol umum (`: % . - /`) —
    /// karakter lain digambar sebagai spasi. `scale` = ukuran piksel per
    /// "piksel" glyph (1 = 5x7 asli, 2 = 10x14, dst).
    ///
    /// Mengembalikan lebar total teks dalam piksel (berguna untuk
    /// menengahkan/menyusun teks lain).
    pub fn draw_text(&mut self, x: u32, y: u32, text: &str, r: u8, g: u8, b: u8, scale: u32) -> u32 {
        let scale = scale.max(1);
        let advance = (font::GLYPH_WIDTH + 1) * scale;
        let mut cursor_x = x;

        for ch in text.chars() {
            let rows = font::glyph(ch);
            for (row_idx, row_bits) in rows.iter().enumerate() {
                for col in 0..font::GLYPH_WIDTH {
                    let bit = font::GLYPH_WIDTH - 1 - col; // bit4 = kolom kiri
                    if (row_bits >> bit) & 1 == 1 {
                        let px = cursor_x + col * scale;
                        let py = y + row_idx as u32 * scale;
                        self.fill_rect(px, py, scale, scale, r, g, b);
                    }
                }
            }
            cursor_x += advance;
        }

        cursor_x.saturating_sub(x)
    }

    /// Sama seperti `draw_text`, tapi `x` boleh negatif (`i64`) dan hasil
    /// gambarnya dipotong (clip) supaya cuma piksel yang jatuh di rentang
    /// `[clip_x0, clip_x1)` yang benar-benar digambar. Dipakai untuk efek
    /// scroll/marquee: karakter yang sedang "keluar" di kiri/kanan jendela
    /// otomatis tidak digambar, tanpa perlu artimetika u32 yang bisa
    /// underflow. Satu karakter dianggap "utuh" (semua-atau-tidak per kolom
    /// piksel glyph) — tidak ada pemotongan sub-piksel di tengah karakter.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_text_clipped(
        &mut self,
        x: i64,
        y: u32,
        text: &str,
        r: u8,
        g: u8,
        b: u8,
        scale: u32,
        clip_x0: u32,
        clip_x1: u32,
    ) {
        let scale = scale.max(1);
        let advance = (font::GLYPH_WIDTH + 1) * scale;
        let mut cursor_x = x;
        let clip_x0 = clip_x0 as i64;
        let clip_x1 = clip_x1 as i64;

        for ch in text.chars() {
            let rows = font::glyph(ch);
            for (row_idx, row_bits) in rows.iter().enumerate() {
                for col in 0..font::GLYPH_WIDTH {
                    let bit = font::GLYPH_WIDTH - 1 - col;
                    if (row_bits >> bit) & 1 == 1 {
                        let px = cursor_x + (col * scale) as i64;
                        if px >= clip_x0 && px + scale as i64 <= clip_x1 {
                            let py = y + row_idx as u32 * scale;
                            self.fill_rect(px as u32, py, scale, scale, r, g, b);
                        }
                    }
                }
            }
            cursor_x += advance as i64;
        }
    }

    /// Lebar total (piksel) kalau `text` digambar dengan `draw_text` di `scale` ini.
    pub fn text_width(text: &str, scale: u32) -> u32 {
        let scale = scale.max(1);
        let advance = (font::GLYPH_WIDTH + 1) * scale;
        text.chars().count() as u32 * advance
    }

    /// Tinggi (piksel) satu baris teks pada `scale` ini.
    pub fn text_height(scale: u32) -> u32 {
        font::GLYPH_HEIGHT * scale.max(1)
    }

    /// Salinan yang diputar 180° (dipakai saat `Handshake::rotate_180 == true`).
    pub fn rotated_180(&self) -> Framebuffer {
        let mut out = Framebuffer::new(Resolution::new(self.width, self.height));
        for y in 0..self.height {
            for x in 0..self.width {
                let src = (((self.height - 1 - y) * self.width + (self.width - 1 - x)) * 3) as usize;
                let dst = ((y * self.width + x) * 3) as usize;
                out.pixels[dst..dst + 3].copy_from_slice(&self.pixels[src..src + 3]);
            }
        }
        out
    }

    /// Encode ke JPEG. `quality` 1-100. Mengembalikan error kalau hasilnya
    /// melebihi `MAX_FRAME_BYTES` (turunkan `quality` kalau kejadian).
    pub fn to_jpeg(&self, quality: u8) -> Result<Vec<u8>> {
        use jpeg_encoder::{ColorType, Encoder};

        let mut out = Vec::new();
        let encoder = Encoder::new(&mut out, quality);
        encoder
            .encode(&self.pixels, self.width as u16, self.height as u16, ColorType::Rgb)
            .map_err(|e| LcdError::Jpeg(e.to_string()))?;

        if out.len() > MAX_FRAME_BYTES {
            return Err(LcdError::FrameTooLarge(out.len()));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_count_has_trailing_empty_chunk_on_exact_multiple() {
        // total_size persis 2 * 496 -> harus 3 chunk (2 penuh + 1 kosong).
        let payload = vec![0xAAu8; CHUNK_DATA_SIZE * 2];
        let frame = build_chunks(Variant::Ly, &payload);
        // 3 chunk asli, dibulatkan ke kelipatan 4 -> 4 chunk -> 2048 byte.
        assert_eq!(frame.len(), 4 * CHUNK_SIZE);
        // Header chunk terakhir dari 3 chunk asli (index 2) punya data_len 0.
        let last_real_chunk_offset = 2 * CHUNK_SIZE;
        assert_eq!(frame[last_real_chunk_offset + 6], 0);
        assert_eq!(frame[last_real_chunk_offset + 7], 0);
    }

    #[test]
    fn chunk_count_normal_case() {
        let payload = vec![0xBBu8; 1000];
        let frame = build_chunks(Variant::Ly, &payload);
        // 1000/496 + 1 = 3 chunk asli -> dibulatkan ke kelipatan 4 -> 4 chunk.
        assert_eq!(frame.len(), 4 * CHUNK_SIZE);
    }

    #[test]
    fn ly1_has_no_padding() {
        let payload = vec![0xCCu8; CHUNK_DATA_SIZE * 2]; // -> 3 chunk asli
        let frame = build_chunks(Variant::Ly1, &payload);
        assert_eq!(frame.len(), 3 * CHUNK_SIZE); // tanpa pembulatan ke kelipatan 4
    }

    #[test]
    fn framebuffer_rotate_180_swaps_corners() {
        let mut fb = Framebuffer::new(Resolution::new(2, 2));
        fb.set_pixel(0, 0, 1, 0, 0);
        fb.set_pixel(1, 1, 2, 0, 0);
        let rotated = fb.rotated_180();
        assert_eq!(rotated.as_bytes()[0], 2); // (0,0) rotated <- (1,1) asli
        let idx_11 = ((1 * 2 + 1) * 3) as usize;
        assert_eq!(rotated.as_bytes()[idx_11], 1); // (1,1) rotated <- (0,0) asli
    }
}
