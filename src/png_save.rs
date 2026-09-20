//! Simpan isi `Framebuffer` sebagai file **PNG** (lossless) tanpa dependensi
//! tambahan. Encodermya menulis PNG dengan blok deflate "stored" (tanpa
//! kompresi — tetap 100% valid untuk semua penampil PNG). Konsekuensinya
//! file persis sebesar data raw (1920x462 ≈ 2,6 MB), tapi implementasinya
//! kecil, bebas crate baru, dan hanya dijalankan sekali-sekali (saat hotkey
//! tangkapan layar ditekan — lihat src/hotkey.rs).

use std::io;
use std::path::PathBuf;

use crate::Framebuffer;

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// Encode `pixels` (RGB888, width*height*3 byte) sebagai data PNG.
fn png_encode(pixels: &[u8], width: u32, height: u32) -> Vec<u8> {
    let row_len = (width * 3) as usize;

    // Raw scanline PNG: tiap baris diawali filter byte 0 (None), lalu RGB.
    let mut raw = Vec::with_capacity(height as usize * (row_len + 1));
    for y in 0..height {
        raw.push(0);
        let start = (y as usize) * row_len;
        raw.extend_from_slice(&pixels[start..start + row_len]);
    }

    let mut out = Vec::with_capacity(raw.len() + raw.len() / 64 + 64);
    out.extend_from_slice(&PNG_SIGNATURE);

    // IHDR: width, height, bit depth 8, color type 2 (RGB), dll.
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.push(8); // bit depth
    ihdr.push(2); // color type: RGB
    ihdr.push(0); // compression: zlib
    ihdr.push(0); // filter: adaptive
    ihdr.push(0); // interlace: none
    push_chunk(&mut out, b"IHDR", &ihdr);

    push_chunk(&mut out, b"IDAT", &zlib_stored(&raw));

    push_chunk(&mut out, b"IEND", &[]);
    out
}

/// Tulis satu chunk PNG: length (BE) + type + data + CRC32(type+data).
fn push_chunk(out: &mut Vec<u8>, ctype: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(ctype);
    out.extend_from_slice(data);
    let mut crc = crc_update(0xFFFF_FFFF, ctype);
    crc = crc_update(crc, data);
    out.extend_from_slice(&(crc ^ 0xFFFF_FFFF).to_be_bytes());
}

/// CRC-32 (polinom 0xEDB88320, sama seperti zlib). Implementasi per-bit —
/// cukup cepat untuk screencap yang jarang-jarang.
fn crc_update(mut crc: u32, data: &[u8]) -> u32 {
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    crc
}

/// Stream zlib berisi blok deflate "stored" (tidak dikompres tapi valid) —
/// menghindari ketergantungan ke crate flate2/zlib.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / 65_535 * 5 + 6);
    out.push(0x78); // CMF: window 32K, method deflate
    out.push(0x01); // FLG: FCHECK = 1 (0x7801 % 31 == 0), FDICT off

    let mut pos = 0usize;
    loop {
        let remaining = data.len() - pos;
        let len = remaining.min(65_535);
        let is_last = pos + len == data.len();
        out.push(if is_last { 0x01 } else { 0x00 }); // BFINAL + tipe stored
        out.extend_from_slice(&(len as u16).to_le_bytes());
        out.extend_from_slice(&(!(len as u16)).to_le_bytes());
        out.extend_from_slice(&data[pos..pos + len]);
        pos += len;
        if is_last {
            break;
        }
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

/// Adler-32 — checksum footer yang diminta spesifikasi zlib.
fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65_521;
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + u32::from(byte)) % MOD;
        b = (b + a) % MOD;
    }
    (b << 16) | a
}

/// Encode `fb` sebagai data PNG.
pub fn encode(fb: &Framebuffer) -> Vec<u8> {
    png_encode(fb.as_bytes(), fb.width(), fb.height())
}

/// Simpan isi framebuffer sebagai PNG di folder `screenshots/` dengan nama
/// `{prefix}_YYYYMMDD_HHMMSS.png`, lalu kembalikan path lengkapnya.
pub fn save(fb: &Framebuffer, prefix: &str) -> io::Result<PathBuf> {
    let dir = std::path::Path::new("screenshots");
    std::fs::create_dir_all(dir)?;
    let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let path = dir.join(format!("{}_{}.png", prefix, stamp));
    std::fs::write(&path, encode(fb))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Framebuffer;
    use crate::Resolution;

    #[test]
    fn png_signature_ihdr_dimensions_iend() {
        let fb = Framebuffer::new(Resolution::new(1, 1));
        let w = fb.width();
        let h = fb.height();
        let png = encode(&fb);
        assert_eq!(&png[..8], &PNG_SIGNATURE);
        // IHDR dimulai byte 8: length(4) + "IHDR".
        assert_eq!(&png[12..16], b"IHDR");
        let ihdr_w = u32::from_be_bytes(png[16..20].try_into().unwrap());
        let ihdr_h = u32::from_be_bytes(png[20..24].try_into().unwrap());
        assert_eq!((ihdr_w, ihdr_h), (w, h));
        // Harus diakhiri IEND.
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
    }

    /// Tulis contoh PNG ke target/ untuk divalidasi decoder eksternal (test
    /// manual via System.Drawing PowerShell).
    #[test]
    fn write_sample_png_for_external_validation() {
        let mut fb = Framebuffer::new(Resolution::new(16, 8));
        let px = fb.as_bytes_mut();
        for (i, byte) in px.iter_mut().enumerate() {
            *byte = (i * 7) as u8; // pola detereministik
        }
        let out = encode(&fb);
        std::fs::write(std::path::Path::new("target/png_test_sample.png"), out)
            .expect("tulis contoh PNG");
    }
}