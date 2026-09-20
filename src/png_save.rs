//! Simpan isi `Framebuffer` sebagai file **PNG** (lossless) dengan kompresi
//! deflate via crate `flate2` (backend murni Rust miniz_oxide — tanpa
//! dependensi C). Screenshot diambil lewat hotkey (jarang), jadi dipakai
//! level kompresi terbaik; latar yang rata (bar EQ, info sistem) biasanya
//! menyusut dari ±2,6 MB raw menjadi beberapa ratus KB. Sendiri bobotnya
//! kecil dan hanya aktif saat tombol hotkey ditekan (lihat src/hotkey.rs).

use std::io;
use std::io::Write;
use std::path::PathBuf;

use flate2::write::ZlibEncoder;
use flate2::Compression;

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

    // Level kompresi terbaik: screenshot jarang diambil, jadi kecepatan
    // tidak penting — yang penting ukuran file sekecil mungkin.
    push_chunk(&mut out, b"IDAT", &zlib_stream(&raw));

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

/// Bungkus `data` sebagai stream zlib (header + deflate + checksum adler-32)
/// dengan kompresi level terbaik.
fn zlib_stream(data: &[u8]) -> Vec<u8> {
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::best());
    enc.write_all(data)
        .expect("menulis ke ZlibEncoder::new(Vec) tidak mungkin gagal");
    enc.finish()
        .expect("menyelesaikan ZlibEncoder::new(Vec) tidak mungkin gagal")
}

/// Encode `fb` sebagai data PNG.
pub fn encode(fb: &Framebuffer) -> Vec<u8> {
    png_encode(fb.as_bytes(), fb.width(), fb.height())
}

/// Simpan isi framebuffer sebagai PNG di folder **Desktop** dengan nama
/// `{prefix}_YYYYMMDD_HHMMSS.png`, lalu kembalikan path lengkapnya.
///
/// Lokasi Desktop diambil dari API resmi Windows (SHGetKnownFolderPath →
/// FOLDERID_Desktop) sehingga tetap benar meski Desktop direlokasi OneDrive
/// atau di-redirect; di OS lain memakai `$HOME/Desktop`.
pub fn save(fb: &Framebuffer, prefix: &str) -> io::Result<PathBuf> {
    let dir = desktop_dir()?;
    std::fs::create_dir_all(&dir)?;
    let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let path = dir.join(format!("{}_{}.png", prefix, stamp));
    std::fs::write(&path, encode(fb))?;
    Ok(path)
}

/// Path folder Desktop user.
#[cfg(windows)]
fn desktop_dir() -> io::Result<PathBuf> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{FOLDERID_Desktop, KNOWN_FOLDER_FLAG, SHGetKnownFolderPath};

    unsafe {
        let pw = SHGetKnownFolderPath(&FOLDERID_Desktop, KNOWN_FOLDER_FLAG(0), None)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("SHGetKnownFolderPath(Desktop) gagal: {e}")))?;

        // pw bertipe PWSTR hasil CoTaskMemAlloc — salin jadi String dulu,
        // baru CoTaskMemFree.
        let wide: &[u16] = pw.as_wide();
        let dir = PathBuf::from(String::from_utf16_lossy(wide));

        CoTaskMemFree(Some(pw.as_ptr() as *const core::ffi::c_void));
        Ok(dir)
    }
}

/// Path folder Desktop (fallback non-Windows).
#[cfg(not(windows))]
fn desktop_dir() -> io::Result<PathBuf> {
    let home = std::env::var("HOME").map_err(|e| {
        io::Error::new(io::ErrorKind::NotFound, format!("variabel HOME tidak di-set: {e}"))
    })?;
    Ok(PathBuf::from(home).join("Desktop"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Framebuffer;
    use crate::Resolution;

    #[test]
    fn desktop_dir_points_to_existing_folder() {
        let dir = desktop_dir().expect("desktop_dir harus sukses");
        assert_eq!(dir.file_name().and_then(|s| s.to_str()), Some("Desktop"));
        assert!(dir.is_dir(), "path {dir:?} harus folder yang ada");
    }

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

    /// Latar rata (layar visualizer/isian solid) harus terkompresi jauh di
    /// bawah ukuran raw 1920x462 (≈2,6 MB) — ini inti alasan memakai deflate.
    #[test]
    fn flat_screen_compresses_well() {
        let mut fb = Framebuffer::new(Resolution::new(1920, 462));
        let px = fb.as_bytes_mut();
        for (i, byte) in px.iter_mut().enumerate() {
            // Isian quasi-solid: RGB hijau gelap, sedikit variasi bercak
            // terang supaya tetap "gambar" yang sah.
            *byte = if i % 97 == 0 { 0x30 } else { 0x12 };
        }
        let raw = px.len();
        let png = encode(&fb);
        std::fs::write(std::path::Path::new("target/png_test_flat.png"), &png)
            .expect("tulis contoh PNG");
        assert!(
            png.len() * 10 < raw,
            "PNG terlalu besar: {} byte vs raw {} byte",
            png.len(),
            raw
        );
    }
}