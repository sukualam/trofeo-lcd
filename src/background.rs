//! Background image untuk framebuffer.
//!
//! Dibatasi oleh satu angka keras dari firmware LCD:
//!
//! ```text
//! MAX_FRAME_BYTES = 450_000   // src/lib.rs
//! ```
//!
//! Frame dikirim sebagai JPEG, dan JPEG tidak bisa-Pack ukuran frame secara
//! gratis. Diukur pada 1920×462 quality 75:
//!
//! | Isi frame            | JPEG   | Chunk/frame |
//! |----------------------|--------|-------------|
//! | Layar polos + teks    | 18 KB  | 38          |
//! | Gradasi halus         | 19 KB  | 41          |
//! | Foto/noise kasar      | 584 KB | GAGAL       |
//!
//! Jadi gambar bertekstur tinggi **tidak bisa** dipakai pada quality 75 —
//! [`pick_quality`] mencari quality tertinggi yang masih muat, dan kalau
//! bahkan quality terendah tidak muat, gambar ditolak dengan pesan jelas
//! (lebih baik gagal di awal daripada gagal tiap frame).
//!
//! Decode hanya mendukung PNG 8-bit non-interlaced dan BMP tidak
//! terkompresi, dan sengaja tanpa crate `image` supaya tidak menambah
//! dependency tree besar ke program yang biasanya jalan ~12 MB RSS.
//! `flate2` yang sudah dipakai untuk PNG *writer* di `png_save.rs` juga
//! melayani inflate di sini.

use trofeo_lcd::{Framebuffer, Resolution, MAX_FRAME_BYTES};
use std::path::Path;

/// Margin aman: Quality: ada teks EQ bar & status yang menambah sedikit
/// byte di atas background polos. Pakai 92% dari batas supaya frame terburuk
/// (semua bar aktif) tidak pernah melewati batas firmware.
const SIZE_SAFETY: usize = 92;

/// Quality tertinggi yang dicoba. Di bawah ini teks mulai kelihatan buruk,
/// jadi turun ke situ lebih baik daripada memotong.
const MAX_QUALITY: u8 = 75;
const MIN_QUALITY: u8 = 20;

/// Gambar RGB888 yang sudah di-decode, sebelum di-scale ke ukuran LCD.
pub struct Decoded {
    pub width: u32,
    pub height: u32,
    /// `width * height * 3` byte.
    pub pixels: Vec<u8>,
}

/// Background siap pakai untuk kedua orientasi.
pub struct Background {
    landscape: Vec<u8>, // 1920 x 462
    portrait: Vec<u8>,  // 462 x 1920
    quality: u8,
    source: String,
    /// Seberapa besar background diredupkan, 0..=100 (`0` = tidak diubah).
    dim_percent: u8,
}

/// Warna teks cadangan saat kontras dengan background inadequate.
pub const TEXT_ON_DARK: (u8, u8, u8) = (0xF2, 0xF2, 0xF2);
pub const TEXT_ON_LIGHT: (u8, u8, u8) = (0x0A, 0x0A, 0x0A);

/// Rasio kontras WCAG minimum sebelum warna teks diganti paksa.
///
/// 4.5 = ambang "AA untuk teks normal". Teks di LCD ini kecil dan dibaca dari
/// jarak jauh, jadi ambang longgar 3.0 (ukuran "AA untuk teks besar") terlalu
/// optimistis.
///
/// Catatan: 4.5 adalah ambang PEMICU, bukan jaminan. Untuk background tepat di
/// abu-abu 111, kontras terbaik yang mungkin hanya 4.45 — lihat
/// `worst_case_contrast`. Jadi di satu titik terburuk, teks bisa berhenti
/// sedikit di bawah 4.5 dan itu memang batas fisiknya, bukan bug.
const MIN_CONTRAST: f64 = 4.5;

/// Kontras terbaik yang bisa dicapai untuk background berapa pun.
///
/// Rasio kontras untuk warna terang dan gelap saling menyamai saat
/// `(B + 0.05)² = (terang + 0.05) × (gelap + 0.05)`. Di titik itu keduanya
/// sama-sama sekitar 4.2, dan itu titik_background yang paling menyiksa —
/// abu-abu sedang. Artinya auto-kontras tidak bisa menjamin 4.5 untuk semua
/// background; yang bisa dijamin hanya "selalu memilih warna terbaik yang
/// tersedia". Justru inilah alasan opsi "redupkan background" penting: ia
/// mendorong background keluar dari abu-abu sedang ke arah gelap, tempat
/// kontrasnya jauh lebih lega.
fn worst_case_contrast() -> f64 {
    let w = relative_luminance(
        TEXT_ON_DARK.0 as f64,
        TEXT_ON_DARK.1 as f64,
        TEXT_ON_DARK.2 as f64,
    ) + 0.05;
    let k = relative_luminance(
        TEXT_ON_LIGHT.0 as f64,
        TEXT_ON_LIGHT.1 as f64,
        TEXT_ON_LIGHT.2 as f64,
    ) + 0.05;
    w / (w * k).sqrt()
}

/// Batas atas titik sampel per panggilan `avg_luminance`.
const MAX_LUMA_SAMPLES: u32 = 256;

impl Background {
    /// Quality JPEG yang harus dipakai untuk frame dengan background ini.
    pub fn quality(&self) -> u8 {
        self.quality
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    /// Piksel RGB untuk orientasi tertentu, sized `width*height*3`.
    pub fn pixels_for(&self, portrait: bool) -> &[u8] {
        if portrait {
            &self.portrait
        } else {
            &self.landscape
        }
    }

    /// Seberapa besar background diredupkan (persen).
    pub fn dim_percent(&self) -> u8 {
        self.dim_percent
    }

    /// Rata-rata luminance (0..=255) background di area tertentu.
    ///
    /// Dibaca langsung dari buffer background — BUKAN dari framebuffer — jadi
    /// teks yang sudah digambar sebelumnya tidak ikut terambil dan
    /// warnanya tidak menjadi bergantung pada dirinya sendiri.
    pub fn avg_luminance(&self, x: u32, y: u32, w: u32, h: u32, portrait: bool) -> f64 {
        let px = if portrait { &self.portrait } else { &self.landscape };
        let (bw, bh) = if portrait { (462u32, 1920u32) } else { (1920u32, 462u32) };
        let x0 = x.min(bw);
        let y0 = y.min(bh);
        let x1 = (x + w).min(bw);
        let y1 = (y + h).min(bh);
        if x1 <= x0 || y1 <= y0 {
            return 0.0;
        }

        // Sampel diambil berjejang dan dibatasi `MAX_LUMA_SAMPLES` titik.
        // Rata-rata luminance tidak butuh tiap piksel, dan ini membuat biaya
        // pemanggilan tetap kecil walau areanya besar — penting karena area
        // jam di idle clock bisa berukuran ratusan piksel.
        let (aw, ah) = (x1 - x0, y1 - y0);
        let step = (((aw * ah) / MAX_LUMA_SAMPLES) as f64)
            .sqrt()
            .ceil()
            .max(1.0) as u32;

        let mut sum = 0f64;
        let mut n = 0f64;
        let mut yy = y0;
        while yy < y1 {
            let mut xx = x0;
            while xx < x1 {
                let i = ((yy * bw + xx) * 3) as usize;
                // Luminance perseptual (Rec. 709).
                sum += 0.2126 * px[i] as f64 + 0.7152 * px[i + 1] as f64 + 0.0722 * px[i + 2] as f64;
                n += 1.0;
                xx += step;
            }
            yy += step;
        }
        if n == 0.0 {
            0.0
        } else {
            sum / n
        }
    }

    /// Warna teks yang kontras dengan background di area tertentu.
    ///
    /// `accent` itu warna yang biasanya dipakai (putih default, atau warna
    /// custom OpenRGB). Kalau kontrasnya sudah cukup, accent dikembalikan
    /// apa adanya supaya pilihan user tidak ditimpa. Kalau tidak, diganti
    /// hitam atau putih mana yang kontrasnya lebih baik — itu selalu lebih
    /// terbaca daripada memaksa warna accent di atas background yang
    /// warnanya mirip.
    pub fn text_color_for(
        &self,
        accent: (u8, u8, u8),
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        portrait: bool,
    ) -> (u8, u8, u8) {
        let bg_l = self.avg_luminance(x, y, w, h, portrait);
        if contrast_ratio(accent, bg_l) >= MIN_CONTRAST {
            return accent;
        }
        let on_dark = contrast_ratio(TEXT_ON_DARK, bg_l);
        let on_light = contrast_ratio(TEXT_ON_LIGHT, bg_l);
        if on_dark >= on_light {
            TEXT_ON_DARK
        } else {
            TEXT_ON_LIGHT
        }
    }
}

/// Muat gambar dari `path`, scale-crop ke ukuran LCD di kedua orientasi, lalu
/// cari quality JPEG yang muat dalam batas firmware.
///
/// Gambar di-*cover*-crop (bukan di-stretch) supaya tidak gepeng: bagian
/// tengah dipotong, sisanya dibuang.
pub fn load(
    path: &Path,
    landscape: Resolution,
    portrait: Resolution,
    dim_percent: u8,
) -> Result<Background, String> {
    let bytes = std::fs::read(path)
        .map_err(|e| format!("gagal baca {}: {e}", path.display()))?;
    let img = decode(&bytes)
        .map_err(|e| format!("gagal decode {}: {e}", path.display()))?;

    let mut land = cover_crop(&img, landscape.width, landscape.height);
    let mut port = cover_crop(&img, portrait.width, portrait.height);

    // Redupkan SEKALI di sini. Karena background hanya di-muat sekali,
    // operasi per-piksel ini gratis — tidak ada biaya per frame sama sekali.
    // Tujuannya bukan sekadar gelapkan, tapi menurunkan "ramai"-nya background
    // supaya teks kecil terbaca dari jauh.
    let dim_percent = dim_percent.min(100);
    if dim_percent > 0 {
        dim_in_place(&mut land, dim_percent);
        dim_in_place(&mut port, dim_percent);
    }

    // Quality ditentukan dari frame TANPA teks dan SETELAH diredupkan — ini
    // yang benar-benar dikirim ke LCD.
    let quality = pick_quality(landscape.width, landscape.height, &land)?;

    Ok(Background {
        landscape: land,
        portrait: port,
        quality,
        source: path.display().to_string(),
        dim_percent,
    })
}

/// Cari quality JPEG tertinggi yang frame-nya masih muat dalam batas firmware.
///
/// Frame diuji tanpa teks karena itu kasus terburuk — menambah teks hanya
/// menaikkan ukuran, tidak pernah menurunkannya.
fn pick_quality(width: u32, height: u32, pixels: &[u8]) -> Result<u8, String> {
    let limit = MAX_FRAME_BYTES * SIZE_SAFETY / 100;
    let mut smallest_seen = 0usize;

    // `Framebuffer.pixels` privat, jadi isi lewat `as_bytes_mut()` yang publik.
    let mut fb = Framebuffer::new(Resolution::new(width, height));
    fb.as_bytes_mut().copy_from_slice(pixels);

    for q in (MIN_QUALITY..=MAX_QUALITY).rev() {
        match fb.to_jpeg(q) {
            Ok(jpeg) => {
                smallest_seen = smallest_seen.max(jpeg.len());
                if jpeg.len() <= limit {
                    return Ok(q);
                }
            }
            Err(_) => continue,
        }
    }

    Err(format!(
        "gambar background terlalu detail untuk dikirim ke LCD ini: bahkan quality {} \
         menghasilkan {} KB, sedangkan batasnya {} KB.\n\
         Coba perkecil dimensinya, kompres dengan kualitas lebih rendah, atau \
         haluskan (blur) — detail tinggi tidak bisa dikompres JPEG dalam batas firmware.",
        MIN_QUALITY,
        smallest_seen / 1024,
        limit / 1024,
    ))
}

/// Turunkan kecerahan semua piksel sebesar `percent` persen (0..=100).
///
/// Tiap channel dikalikan faktor yang sama, jadi perbandingan antar channel —
/// dan karenanya rona warna — tetap persis; tidak berubah jadi abu-abu.
/// Piksel yang sudah gelap otomatis makin gelap, sehingga teks terang di
/// atasnya makin kontras.
///
/// Dipanggil SEKALI saat load, jadi nol biaya per frame.
fn dim_in_place(px: &mut [u8], percent: u8) {
    let keep = 1.0 - (percent.min(100) as f64 / 100.0);
    for c in px.chunks_exact_mut(3) {
        c[0] = (c[0] as f64 * keep) as u8;
        c[1] = (c[1] as f64 * keep) as u8;
        c[2] = (c[2] as f64 * keep) as u8;
    }
}

/// Rasio kontras WCAG antara warna `fg` dan background dengan luminansi `bg_l`.
///
/// `bg_l` HARUS sudah berada di skala 0..255 — nilai yang dikembalikan
/// `avg_luminance`. Fungsi ini yang responsible menormalisasinya ke 0..1
/// sebelum dibandingkan, karena `relative_luminance` bekerja di 0..1. Kalau
/// satuan ini sampai tertukar, rasio kontras selalu didominasi background
/// dan auto-kontras tidak pernah aktif sama sekali.
fn contrast_ratio(fg: (u8, u8, u8), bg_l: f64) -> f64 {
    let fg_l = relative_luminance(fg.0 as f64, fg.1 as f64, fg.2 as f64);
    let bg_l = srgb_to_linear((bg_l / 255.0).clamp(0.0, 1.0));
    let hi = fg_l.max(bg_l);
    let lo = fg_l.min(bg_l);
    (hi + 0.05) / (lo + 0.05)
}

fn relative_luminance(r: f64, g: f64, b: f64) -> f64 {
    0.2126 * srgb_to_linear(r / 255.0) + 0.7152 * srgb_to_linear(g / 255.0)
        + 0.0722 * srgb_to_linear(b / 255.0)
}

/// Satu channel sRGB (0..1) -> luminance linear (0..1), kurva WCAG.
fn srgb_to_linear(c: f64) -> f64 {
    if c <= 0.039_28 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

// ---------------------------------------------------------------------------
// Scaling

/// Scale `img` supaya menutup `dst_w x dst_h` (aspect ratio terjaga), lalu
/// potong bagian tengah. Pakai bilinear — gambar sumber biasanya lebih besar
/// dari LCD, dan nearest-neighbour akan menghasilkan aliasing yang jelas di
/// foto yang diperkecil.
fn cover_crop(img: &Decoded, dst_w: u32, dst_h: u32) -> Vec<u8> {
    let mut out = vec![0u8; (dst_w * dst_h * 3) as usize];
    if img.width == 0 || img.height == 0 || dst_w == 0 || dst_h == 0 {
        return out;
    }

    // Skala = max(lebar, tinggi) supaya menutup kedua sisi.
    let scale = (dst_w as f64 / img.width as f64).max(dst_h as f64 / img.height as f64);
    let scaled_w = img.width as f64 * scale;
    let scaled_h = img.height as f64 * scale;
    // Offset supaya hasil skala berada di tengah.
    let off_x = (scaled_w - dst_w as f64) / 2.0;
    let off_y = (scaled_h - dst_h as f64) / 2.0;

    for y in 0..dst_h {
        // Tanpa offset -0.5: koordinat di sini sudah mewakili pusat piksel,
        // jadi menggesernya membuat gambar yang ukurannya sama persis ikut
        // ter-interpolasi (blur) — dan blur diam-diam membuat gambar ber-noise
        // lebih gampang dikompres JPEG, sehingga yang harusnya ditolak lolos.
        let sy = ((y as f64 + off_y) / scale).max(0.0);
        for x in 0..dst_w {
            let sx = ((x as f64 + off_x) / scale).max(0.0);
            let c = sample_bilinear(img, sx, sy);
            let di = ((y * dst_w + x) * 3) as usize;
            out[di] = c[0];
            out[di + 1] = c[1];
            out[di + 2] = c[2];
        }
    }
    out
}

/// Contoh bilinear satu titik. Koordinat di-clamp ke batas gambar supaya
/// tepi tidak bergeser.
fn sample_bilinear(img: &Decoded, fx: f64, fy: f64) -> [u8; 3] {
    let x0 = (fx.floor().max(0.0) as u32).min(img.width - 1);
    let y0 = (fy.floor().max(0.0) as u32).min(img.height - 1);
    let x1 = (x0 + 1).min(img.width - 1);
    let y1 = (y0 + 1).min(img.height - 1);
    let tx = (fx - x0 as f64).clamp(0.0, 1.0) as f32;
    let ty = (fy - y0 as f64).clamp(0.0, 1.0) as f32;

    let px = |x: u32, y: u32| -> [f32; 3] {
        let i = ((y * img.width + x) * 3) as usize;
        [
            img.pixels[i] as f32,
            img.pixels[i + 1] as f32,
            img.pixels[i + 2] as f32,
        ]
    };

    let c00 = px(x0, y0);
    let c10 = px(x1, y0);
    let c01 = px(x0, y1);
    let c11 = px(x1, y1);

    let mut out = [0u8; 3];
    for ch in 0..3 {
        let top = c00[ch] + (c10[ch] - c00[ch]) * tx;
        let bot = c01[ch] + (c11[ch] - c01[ch]) * tx;
        out[ch] = (top + (bot - top) * ty).round().clamp(0.0, 255.0) as u8;
    }
    out
}

// ---------------------------------------------------------------------------
// Decode

/// Tebak format dari magic bytes, lalu decode ke RGB888.
fn decode(bytes: &[u8]) -> Result<Decoded, String> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        decode_png(bytes)
    } else if bytes.starts_with(b"BM") {
        decode_bmp(bytes)
    } else {
        Err("format tidak dikenal (harus PNG atau BMP)".to_string())
    }
}

const PNG_SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

/// PNG 8-bit, non-interlaced. Color type 0 (abu), 2 (RGB), 3 (palette), 6 (RGBA).
fn decode_png(bytes: &[u8]) -> Result<Decoded, String> {
    if !bytes.starts_with(&PNG_SIG) {
        return Err("signature PNG tidak cocok".to_string());
    }

    let mut pos = 8usize;
    let mut ihdr: Option<(u32, u32, u8, u8)> = None;
    let mut palette: Vec<[u8; 3]> = Vec::new();
    let mut trns: Vec<u8> = Vec::new();
    let mut idat: Vec<u8> = Vec::new();

    while pos + 8 <= bytes.len() {
        let len = u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        let ctype = &bytes[pos + 4..pos + 8];
        let data_start = pos + 8;
        let data_end = data_start
            .checked_add(len)
            .ok_or_else(|| "chunk PNG overflow".to_string())?;
        if data_end + 4 > bytes.len() {
            return Err("chunk PNG terpotong".to_string());
        }
        let data = &bytes[data_start..data_end];

        match ctype {
            b"IHDR" => {
                if data.len() < 13 {
                    return Err("IHDR terpotong".to_string());
                }
                let w = u32::from_be_bytes(data[0..4].try_into().unwrap());
                let h = u32::from_be_bytes(data[4..8].try_into().unwrap());
                let depth = data[8];
                let color = data[9];
                let interlace = data[12];
                if depth != 8 {
                    return Err(format!(
                        "PNG bit depth {depth} belum didukung (hanya 8-bit)"
                    ));
                }
                if interlace != 0 {
                    return Err("PNG interlaced belum didukung".to_string());
                }
                if !matches!(color, 0 | 2 | 3 | 6) {
                    return Err(format!("PNG color type {color} belum didukung"));
                }
                ihdr = Some((w, h, depth, color));
            }
            b"PLTE" => {
                palette = data.chunks_exact(3).map(|c| [c[0], c[1], c[2]]).collect();
            }
            b"tRNS" => trns = data.to_vec(),
            b"IDAT" => idat.extend_from_slice(data),
            b"IEND" => break,
            _ => {}
        }
        pos = data_end + 4;
    }

    let (w, h, _depth, color) = ihdr.ok_or_else(|| "PNG tanpa IHDR".to_string())?;
    if w == 0 || h == 0 {
        return Err("PNG berukuran nol".to_string());
    }
    // Batas keras: file PNG yang bomb-proof bisagbaUint32::MAX dan membuat
    // alokasi gagal. 60 MP sudah jauh di atas 1920×462.
    if w as u64 * h as u64 > 60_000_000 {
        return Err("PNG terlalu besar".to_string());
    }

    let channels: usize = match color {
        0 => 1,
        2 => 3,
        3 => 1,
        6 => 4,
        _ => unreachable!(),
    };
    let bpp = channels; // bit depth sudah dipastikan 8
    let stride = (w as usize) * bpp;

    let raw = inflate_idat(&idat, h as usize * (stride + 1))?;
    let unfiltered = png_unfilter(&raw, w as usize, h as usize, bpp)?;

    let mut pixels = vec![0u8; (w * h * 3) as usize];
    for y in 0..h as usize {
        for x in 0..w as usize {
            let si = y * stride + x * bpp;
            let (r, g, b) = match color {
                0 => {
                    let v = unfiltered[si];
                    (v, v, v)
                }
                2 => (unfiltered[si], unfiltered[si + 1], unfiltered[si + 2]),
                3 => {
                    let idx = unfiltered[si] as usize;
                    let p = palette.get(idx).copied().unwrap_or([0, 0, 0]);
                    (p[0], p[1], p[2])
                }
                6 => (
                    unfiltered[si],
                    unfiltered[si + 1],
                    unfiltered[si + 2],
                ),
                _ => unreachable!(),
            };
            let di = (y * w as usize + x) * 3;
            pixels[di] = r;
            pixels[di + 1] = g;
            pixels[di + 2] = b;
        }
    }
    // tRNS hanya relevan untuk palette; alpha diabaikan (LCD tidak ada komposit), jadi file semi-transparan akan tampil dengan warna aslinya.
    let _ = trns;

    Ok(Decoded { width: w, height: h, pixels })
}

/// Inflate seluruh stream IDAT. `expected` hanya untuk pesan error.
fn inflate_idat(data: &[u8], expected: usize) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut out = Vec::with_capacity(expected);
    flate2::read::ZlibDecoder::new(data)
        .read_to_end(&mut out)
        .map_err(|e| format!("inflate PNG gagal: {e}"))?;
    Ok(out)
}

/// Buka filter tiap scanline PNG (spec: 5 filter type).
fn png_unfilter(data: &[u8], w: usize, h: usize, bpp: usize) -> Result<Vec<u8>, String> {
    let stride = w * bpp;
    let need = h * (stride + 1);
    if data.len() < need {
        return Err(format!(
            "data PNG tidak lengkap: {} byte,minimal {need}",
            data.len()
        ));
    }
    let mut out = vec![0u8; stride * h];
    let mut prev_row = vec![0u8; stride];

    for y in 0..h {
        let src = &data[y * (stride + 1)..];
        let filter = src[0];
        let line = &src[1..stride + 1];
        let (before, cur) = out.split_at_mut(y * stride);
        let cur = &mut cur[..stride];

        for i in 0..stride {
            let a = if i >= bpp { cur[i - bpp] as i32 } else { 0 };
            let b = prev_row[i] as i32;
            let c = if i >= bpp {
                prev_row[i - bpp] as i32
            } else {
                0
            };
            let x = line[i] as i32;
            let v = match filter {
                0 => x,
                1 => x + a,
                2 => x + b,
                3 => x + (a + b) / 2,
                4 => x + paeth(a, b, c),
                _ => return Err(format!("filter PNG tidak dikenal: {filter}")),
            };
            cur[i] = (v & 0xff) as u8;
        }
        let _ = before;
        prev_row.copy_from_slice(cur);
    }
    Ok(out)
}

fn paeth(a: i32, b: i32, c: i32) -> i32 {
    let p = a + b - c;
    let pa = (p - a).abs();
    let pb = (p - b).abs();
    let pc = (p - c).abs();
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// BMP tidak terkompresi (BI_RGB), 24-bit BGR atau 32-bit BGRA.
/// Tinggi negatif berarti baris tersimpan dari atas ke bawah.
fn decode_bmp(bytes: &[u8]) -> Result<Decoded, String> {
    if bytes.len() < 54 {
        return Err("BMP terpotong".to_string());
    }
    let data_offset = u32::from_le_bytes(bytes[10..14].try_into().unwrap()) as usize;
    let header_size = u32::from_le_bytes(bytes[14..18].try_into().unwrap()) as usize;
    if header_size < 40 {
        return Err(format!("header BMP {header_size} belum didukung (min 40)"));
    }
    let w = i32::from_le_bytes(bytes[18..22].try_into().unwrap());
    let h_raw = i32::from_le_bytes(bytes[22..26].try_into().unwrap());
    let bpp = u16::from_le_bytes(bytes[28..30].try_into().unwrap()) as usize;
    let compression = u32::from_le_bytes(bytes[30..34].try_into().unwrap());

    if compression != 0 {
        return Err(format!("BMP terkompresi (tipe {compression}) belum didukung"));
    }
    if bpp != 24 && bpp != 32 {
        return Err(format!("BMP {bpp}-bit belum didukung (24 atau 32)"));
    }
    let top_down = h_raw < 0;
    let w = w.max(0) as u32;
    let h = h_raw.unsigned_abs();
    if w == 0 || h == 0 || w as u64 * h as u64 > 60_000_000 {
        return Err("BMP berukuran tidak valid".to_string());
    }

    let bytes_per_px = bpp / 8;
    // Setiap baris dipad ke kelipatan 4 byte.
    let row_bytes = ((w as usize * bytes_per_px + 3) / 4) * 4;
    let need = data_offset
        .checked_add(row_bytes * h as usize)
        .ok_or_else(|| "BMP overflow".to_string())?;
    if bytes.len() < need {
        return Err("BMP terpotong".to_string());
    }

    let mut pixels = vec![0u8; (w * h * 3) as usize];
    for y in 0..h as usize {
        let src_row = if top_down { y } else { h as usize - 1 - y };
        let row_start = data_offset + src_row * row_bytes;
        for x in 0..w as usize {
            let si = row_start + x * bytes_per_px;
            // BMP simpan BGR(A).
            let di = (y * w as usize + x) * 3;
            pixels[di] = bytes[si + 2];
            pixels[di + 1] = bytes[si + 1];
            pixels[di + 2] = bytes[si];
        }
    }

    Ok(Decoded { width: w, height: h, pixels })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PNG abu-abu 4×2 buatan sendiri, filter 0. exercising decode + palette
    /// dan jalur grayscale sekaligus.
    fn tiny_png_gray() -> Vec<u8> {
        fn crc(data: &[u8]) -> [u8; 4] {
            let mut c: u32 = 0xffff_ffff;
            for &b in data {
                c ^= b as u32;
                for _ in 0..8 {
                    c = if c & 1 != 0 {
                        (c >> 1) ^ 0xedb8_8320
                    } else {
                        c >> 1
                    };
                }
            }
            (!c).to_be_bytes()
        }
        fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            let mut body = kind.to_vec();
            body.extend_from_slice(data);
            out.extend_from_slice(&body);
            out.extend_from_slice(&crc(&body));
        }

        let mut raw = Vec::new();
        for row in [0u8, 1u8] {
            raw.push(0u8); // filter None
            for col in 0u8..4 {
                raw.push((row * 4 + col) * 10);
            }
        }
        let z = {
            use flate2::write::ZlibEncoder;
            use flate2::Compression;
            use std::io::Write;
            let mut e = ZlibEncoder::new(Vec::new(), Compression::default());
            e.write_all(&raw).unwrap();
            e.finish().unwrap()
        };

        let mut png = PNG_SIG.to_vec();
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&4u32.to_be_bytes());
        ihdr.extend_from_slice(&2u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 0, 0, 0, 0]); // depth 8, color 0, no interlace
        chunk(&mut png, b"IHDR", &ihdr);
        chunk(&mut png, b"IDAT", &z);
        chunk(&mut png, b"IEND", &[]);
        png
    }

    #[test]
    fn decodes_gray_png() {
        let img = decode_png(&tiny_png_gray()).expect("decode harus sukses");
        assert_eq!((img.width, img.height), (4, 2));
        assert_eq!(img.pixels.len(), 4 * 2 * 3);
        // Baris 0: abu-abu 0,10,20,30 -> semua channel sama.
        assert_eq!(&img.pixels[0..3], &[0, 0, 0]);
        assert_eq!(&img.pixels[3..6], &[10, 10, 10]);
        // Baris 1: abu-abu 40,50,60,70.
        assert_eq!(&img.pixels[12..15], &[40, 40, 40]);
        assert_eq!(&img.pixels[21..24], &[70, 70, 70]);
    }

    /// Cover-crop harus menghasilkan ukuran target persis dan tidak boleh
    /// panic kalau gambar sumber lebih kecil dari target (harus upscale).
    #[test]
    fn cover_crop_fills_target() {
        let src = Decoded {
            width: 4,
            height: 4,
            pixels: vec![128u8; 4 * 4 * 3],
        };
        let out = cover_crop(&src, 1920, 462);
        assert_eq!(out.len(), 1920 * 462 * 3);
        assert!(out.iter().all(|&v| v == 128));

        // Upscale dari gambar kecil.
        let tiny = Decoded {
            width: 2,
            height: 2,
            pixels: vec![10u8; 2 * 2 * 3],
        };
        assert_eq!(cover_crop(&tiny, 462, 1920).len(), 462 * 1920 * 3);
    }

    /// Kalau ukuran gambar sudah sama persis dengan target, `cover_crop` harus
    /// menyalin piksel apa adanya. Regression test: versi sebelumnya menggeser
    /// koordinat sampling setengah piksel sehingga gambar ikut ter-blur, dan
    /// blur membuat noise lebih gampang dikompres JPEG — background yang
    /// seharusnya ditolak malah lolos.
    #[test]
    fn cover_crop_is_identity_at_same_size() {
        let w = 1920u32;
        let h = 462u32;
        let mut src = vec![0u8; (w * h * 3) as usize];
        let mut s = 12345u32;
        for px in src.chunks_exact_mut(3) {
            s = s.wrapping_mul(1103515245).wrapping_add(12345);
            px[0] = (s >> 16) as u8;
            px[1] = (s >> 8) as u8;
            px[2] = s as u8;
        }
        let img = Decoded { width: w, height: h, pixels: src.clone() };
        assert_eq!(cover_crop(&img, w, h), src, "cover_crop mengubah gambar pada ukuran sama");
    }

    /// Noise asli (LCG) memang tidak bisa dikompres serendah itu, jadi
    /// auto-quality harus menurunkan quality — tapi tetap menemukan nilai yang
    /// muat, bukan gagal.
    #[test]
    fn noisy_background_drops_quality() {
        let w = 1920u32;
        let h = 462u32;
        let mut noise = vec![0u8; (w * h * 3) as usize];
        let mut s = 12345u32;
        for px in noise.chunks_exact_mut(3) {
            s = s.wrapping_mul(1103515245).wrapping_add(12345);
            px[0] = (s >> 16) as u8;
            px[1] = (s >> 8) as u8;
            px[2] = s as u8;
        }
        let q = pick_quality(w, h, &noise).expect("harus ada quality yang muat");
        assert!(
            q < MAX_QUALITY,
            "noise harus menurunkan quality dari {MAX_QUALITY}, dapat {q}"
        );
    }

    /// Gambar polos (solid) harus selalu muat pada quality tertinggi — ini
    /// yang bikin gradasi halus aman dipakai.
    #[test]
    fn flat_background_fits_at_high_quality() {
        let flat = cover_crop(
            &Decoded {
                width: 1920,
                height: 462,
                pixels: vec![24u8; 1920 * 462 * 3],
            },
            1920,
            462,
        );
        let q = pick_quality(1920, 462, &flat).expect("gambar polos harus muat");
        assert_eq!(q, MAX_QUALITY, "gambar polos sebaiknya tetap di quality max");
    }

    // ---- auto-kontras + redupkan (opsi 3 & 4) -----------------------------

    /// Background solid warna tertentu, ukuran landscape 1920×462.
    fn solid_bg(r: u8, g: u8, b: u8) -> Background {
        let mut px = Vec::with_capacity(1920 * 462 * 3);
        for _ in 0..(1920 * 462) {
            px.extend_from_slice(&[r, g, b]);
        }
        Background {
            landscape: px,
            portrait: vec![0; 462 * 1920 * 3],
            quality: 75,
            source: "test".into(),
            dim_percent: 0,
        }
    }

    #[test]
    fn dim_menurunkan_kecerahan_tanpa_mengubah_r() {
        let mut px = vec![200u8, 100, 50];
        dim_in_place(&mut px, 50);
        assert_eq!(px, vec![100, 50, 25], "setiap channel harus dikalikan 0.5");
    }

    #[test]
    fn dim_nol_adalah_identitas() {
        let mut px = vec![200u8, 100, 50];
        dim_in_place(&mut px, 0);
        assert_eq!(px, vec![200, 100, 50]);
    }

    #[test]
    fn dim_menghormati_rona_warna() {
        // Merah dan biru Startifier. Kalau `dim_in_place` pernahayscale
        // piksel gelap (seperti versi lama), perbandingan channel berubah.
        let mut px = vec![180u8, 20, 20];
        dim_in_place(&mut px, 60);
        let ratio_before = 180.0 / 20.0;
        let ratio_after = px[0] as f64 / px[1] as f64;
        assert!(
            (ratio_before - ratio_after).abs() < 0.6,
            "rasio channel_before={ratio_before} after={ratio_after} — rona berubah"
        );
    }

    #[test]
    fn luminance_membaca_dari_area_yang_diminta() {
        let bg = solid_bg(255, 255, 255);
        assert!((bg.avg_luminance(0, 0, 100, 100, false) - 255.0).abs() < 1.0);
    }

    #[test]
    fn luminance_membatasi_diri_pada_ukuran_buffer() {
        // Area melampaui batas harus dijepit, bukan panic.
        let bg = solid_bg(10, 10, 10);
        let v = bg.avg_luminance(1800, 400, 9999, 9999, false);
        assert!((v - 10.0).abs() < 1.0, "area meluber harus tetap di-clamp, dapat {v}");
    }

    #[test]
    fn area_kosong_menghasilkan_nol() {
        let bg = solid_bg(255, 255, 255);
        assert_eq!(bg.avg_luminance(100, 100, 0, 0, false), 0.0);
        assert_eq!(bg.avg_luminance(5000, 100, 10, 10, false), 0.0);
    }

    #[test]
    fn sampling_berjejang_masih_akurat() {
        // Setengah atas gelap, setengah bawah terang. Area yang hanya
        // mencakup bagian atas harus melaporkan nilai gelap.
        let mut bg = solid_bg(0, 0, 0);
        for c in bg.landscape.chunks_exact_mut(3).skip(1920 * 231) {
            c[0] = 255;
            c[1] = 255;
            c[2] = 255;
        }
        let top = bg.avg_luminance(0, 0, 400, 100, false);
        assert!(top < 5.0, "area atas harus gelap, dapat {top}");
    }

    #[test]
    fn teks_putih_diatas_background_gelap_tetap_putih() {
        let bg = solid_bg(16, 16, 20);
        assert_eq!(
            bg.text_color_for((0xE0, 0xE0, 0xE0), 0, 0, 300, 20, false),
            (0xE0, 0xE0, 0xE0),
            "kontras sudah cukup, warna user tidak boleh ditimpa"
        );
    }

    #[test]
    fn teks_putih_diatas_background_terang_jadi_hitam() {
        let bg = solid_bg(250, 250, 250);
        assert_eq!(
            bg.text_color_for((0xE0, 0xE0, 0xE0), 0, 0, 300, 20, false),
            TEXT_ON_LIGHT,
            "putih di atas putih tidak terbaca — harus diganti hitam"
        );
    }

    #[test]
    fn warna_custom_kontras_tinggi_tidak_ditimpa() {
        // Kuning di atas biru gelap: kontrasnya sangat tinggi.
        let bg = solid_bg(10, 10, 60);
        assert_eq!(
            bg.text_color_for((0xFF, 0xD0, 0x20), 0, 0, 300, 20, false),
            (0xFF, 0xD0, 0x20)
        );
    }

    #[test]
    fn warna_custom_rendah_kontras_ditukar_ke_hitam() {
        // Biru gelap di atas biru tua: nyaris tak terbaca -> harus ditukar.
        let bg = solid_bg(12, 12, 40);
        let got = bg.text_color_for((0x18, 0x20, 0x60), 0, 0, 300, 20, false);
        assert_eq!(got, TEXT_ON_DARK, "kontras rendah harus diganti putih");
    }

    #[test]
    fn teks_terbaca_pada_setiap_nilai_redupkan() {
        // Simulasikan alur nyata: muat -> redupkan -> pilih warna teks.
        // Yang diverifikasi adalah INVARIANT (warna terpilih selalu punya
        // kontras cukup), bukan warna tertentu — untuk background setelah
        // diredupkan, teks hitam bisa jadi pilihan lebih baik, dan itu
        // memang keputusan yang benar.
        let mut px = Vec::new();
        for _ in 0..(1920 * 462) {
            px.extend_from_slice(&[250, 250, 250]);
        }
        let mut bg = Background {
            landscape: px,
            portrait: vec![0; 462 * 1920 * 3],
            quality: 75,
            source: "test".into(),
            dim_percent: 0,
        };

        let bg_l = bg.avg_luminance(0, 0, 300, 20, false);
        let before = bg.text_color_for((0xE0, 0xE0, 0xE0), 0, 0, 300, 20, false);
        assert!(
            contrast_ratio(before, bg_l) >= MIN_CONTRAST,
            "warna terpilih harus selalu terbaca, dapat {before:?}"
        );

        // Sapu nilai redupkan dari 0..=100. Uji ini sengaja TIDAK menguji
        // "nilai default": konstanta itu tinggal di `main.rs` (bin), dan versi
        // lama yang menyebutnya di sini ikut busuk begitu default berubah.
        // Yang penting dijamin: secukup apa pun nilainya, teks tetap terbaca.
        //
        // Catatan: ada satu nilai yang secara matematis TERBURUK. Untuk gambar
        // putih, redupkan ~52% mengubah luminance 250 ke ~120, yang persis
        // abu-abu tengah tempat kontras terbaik hanya ~4.2. Jadi "redupkan
        // lebih banyak" tidak monotonik lebih baik.
        let floor = worst_case_contrast();
        for dim in 0..=100u8 {
            let mut px = Vec::new();
            for _ in 0..(1920 * 462) {
                px.extend_from_slice(&[250, 250, 250]);
            }
            let mut bg = Background {
                landscape: px,
                portrait: vec![0; 462 * 1920 * 3],
                quality: 75,
                source: "test".into(),
                dim_percent: dim,
            };
            dim_in_place(&mut bg.landscape, dim);

            let bg_l = bg.avg_luminance(0, 0, 300, 20, false);
            let got = bg.text_color_for((0xE0, 0xE0, 0xE0), 0, 0, 300, 20, false);
            let ratio = contrast_ratio(got, bg_l);
            // Jaminannya `worst_case_contrast()` (~4.2), bukan MIN_CONTRAST (4.5):
            // yang latter hanya ambang pemicu, bukan jaminan hasil.
            assert!(
                ratio >= floor - 1e-6,
                "dim {dim}% (luminance {bg_l:.0}): kontras hanya {ratio:.3} dengan {got:?} (batas {floor:.3})"
            );
            // Redupkan tidak boleh membuat gambar LEBIH terang.
            assert!(
                bg_l <= 250.0,
                "dim {dim}% menaikkan luminance dari 250 ke {bg_l}"
            );
        }
    }

    #[test]
    fn invariant_kontras_berlaku_untuk_setiap_luminance_background() {
        // Sapu seluruh rentang 0..255. Untuk SETIAP background, warna yang
        // dipilih harus mencapai batas terbaik yang mungkin — inilah jaminan
        // yang sebenarnya penting untuk pengguna: teks selalu terbaca.
        //
        // Batasnya sekitar 4.2, bukan 4.5, karena abu-abu sedang adalah titik
        // terburuk secara matematis (lihat `worst_case_contrast`).
        let floor = worst_case_contrast();
        assert!(
            (floor - 4.2).abs() < 0.05,
            "batas terendah yang realistis: {floor}"
        );

        for step in 0..=255u32 {
            let v = step as u8;
            let bg = solid_bg(v, v, v);
            let bg_l = bg.avg_luminance(0, 0, 100, 20, false);
            let got = bg.text_color_for((0xE0, 0xE0, 0xE0), 0, 0, 100, 20, false);
            let ratio = contrast_ratio(got, bg_l);
            assert!(
                ratio >= floor - 1e-6,
                "background abu {v}: kontras hanya {ratio:.3} dengan {got:?} (batas {floor:.3})"
            );
        }
    }

    #[test]
    fn auto_kontras_selalu_memilih_warna_terbaik_yang_tersedia() {
        // Di luar ambang 4.5, hasil HARUS persis warna dengan kontras tertinggi,
        // bukan sekadar "cukup"— kalau tidak, masih ada ruang untuk membaik.
        for step in 0..=255u32 {
            let v = step as u8;
            let bg = solid_bg(v, v, v);
            let bg_l = bg.avg_luminance(0, 0, 100, 20, false);
            let got = bg.text_color_for((0xE0, 0xE0, 0xE0), 0, 0, 100, 20, false);

            let best = contrast_ratio(TEXT_ON_DARK, bg_l)
                .max(contrast_ratio(TEXT_ON_LIGHT, bg_l));
            let chosen = contrast_ratio(got, bg_l);
            assert!(
                chosen >= best - 1e-9 || chosen >= MIN_CONTRAST,
                "bg {v}: memilih {got:?} ({chosen:.2}) padahal tersedia {best:.2}"
            );
        }
    }

    #[test]
    fn invariant_kontras_bertahan_untuk_warna_accent_apapun() {
        // Auto-kontras juga harus menyelamatkan warna custom milik user,
        // termasuk yang warnanya sudah bagus.
        let floor = worst_case_contrast();
        for step in 0..=255u32 {
            let v = step as u8;
            let bg = solid_bg(v, v, v);
            let bg_l = bg.avg_luminance(0, 0, 100, 20, false);
            for accent in [
                (0xE0, 0xE0, 0xE0),
                (0xFF, 0x00, 0x00),
                (0x00, 0xFF, 0x00),
                (0x30, 0x60, 0xC0),
            ] {
                let got = bg.text_color_for(accent, 0, 0, 100, 20, false);
                let ratio = contrast_ratio(got, bg_l);
                assert!(
                    ratio >= floor - 1e-6,
                    "bg {v} accent {accent:?} -> {got:?} kontras {ratio:.2} (batas {floor:.2})"
                );
            }
        }
    }

    #[test]
    fn dim_lebih_dari_seratus_tidak_yg_tampau_gelap() {
        // Persentase di-clamp: 100% = hitam total, lebih dari itu tidak boleh
        // menghasilkan nilai negatif yang akan meluap saat dikonversi ke u8.
        let mut px = vec![200u8, 100, 50];
        dim_in_place(&mut px, 250);
        assert_eq!(px, vec![0, 0, 0]);
    }
}



#[cfg(test)]
mod render_check {
    use super::*;
    use crate::png_save;
    use crate::Framebuffer;

    /// Background gradien gelap -> putih: kasus terburuk untuk teks putih.
    fn gradient_bg(dim: u8, bright_first: bool) -> Background {
        let mut land = Vec::with_capacity(1920 * 462 * 3);
        for y in 0..462u32 {
            for x in 0..1920u32 {
                let mut t = x as f64 / 1919.0;
                if bright_first {
                    t = 1.0 - t;
                }
                // penambahan rona sedikit supaya bukan abu-abu murni
                let v = (255.0 * t) as u8;
                land.push(v);
                land.push((v as f64 * 0.85) as u8);
                land.push((255.0 - v as f64 * 0.4) as u8);
                let _ = y;
            }
        }
        if dim > 0 {
            dim_in_place(&mut land, dim);
        }
        Background {
            landscape: land,
            portrait: vec![0; 462 * 1920 * 3],
            quality: 75,
            source: format!("grad-dim{dim}"),
            dim_percent: dim,
        }
    }

    fn render(dim: u8, bright_first: bool, out: &str) {
        let bg = gradient_bg(dim, bright_first);
        let res = crate::Resolution::new(1920, 462);
        let mut fb = Framebuffer::new(res);
        fb.as_bytes_mut().copy_from_slice(bg.pixels_for(false));

        let accent = (0xE0u8, 0xE0, 0xE0);
        let scale = 3u32;
        let line_height = Framebuffer::text_height(scale) + 4;
        let lines = [
            "CPU 45% 4700MHZ 62C 31W",
            "GPU 18% 56C 25W 1830RPM",
            "MEM 8192/32768MB  UP 03:42:11",
            "NET 12/340KB/S D 0.4/1.2MB/S",
            "VOL 62%  NOW PLAYING: some song title",
        ];
        let mut y = 8u32;
        for l in lines {
            let w = fb.width() - 40;
            let (r, g, b) = bg.text_color_for(accent, 20, y, w, Framebuffer::text_height(scale), false);
            fb.draw_text(20, y, l, r, g, b, scale);
            y += line_height;
        }
        std::fs::write(out, png_save::encode(&fb)).expect("tulis png gagal");
        println!("  ditulis {out} (dim {dim}%)");
    }

    /// Preview visual, bukan assertion. Jalankan manual:
    ///   cargo test --release render_untuk_dipreview -- --ignored --nocapture
    /// lalu buka /tmp/preview_*.png. Di-`#[ignore]` supaya `cargo test` biasa
    /// tidak menulis file ke /tmp.
    #[test]
    #[ignore]
    fn render_untuk_dipreview() {
        // Gradien gelap-kiri: teks putih di atas gelap -> tetap putih
        render(0, false, "/tmp/preview_dark_left.png");
        // Gradien TERANG-kiri: memaksa auto-kontras chooses black
        render(0, true, "/tmp/preview_bright_left.png");
        render(15, true, "/tmp/preview_bright_dim15.png");
    }
}
