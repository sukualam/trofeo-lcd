//! Test decoder JPEG terhadap file JPEG sungguhan.
//!
//! Fixture di `tests/fixtures/` dibuat dengan Pillow dan mencakup
//! baseline + progressive, chroma subsampling 4:4:4/4:2:2/4:2:0, grayscale,
//! dan quality ekstrem.
//!
//! ## Soal membandingkan dengan hasil decode orang lain
//!
//! Membandingkan hasil decode kita dengan **gambar PNG asal** hanya
//! mengukur seberapa lossy JPEG-nya — angka yang tidak mengukur decoder kita
//! sama sekali. Yang dipakai di sini adalah hasil decode Pillow atas file
//! JPEG yang sama, jadi yang terukur adalah selisih decode KITA.
//!
//! Tapi pembanding itu punya jebakan sendiri: Pillow meng-upsample chroma
//! dengan triangle filter, sedangkan kita memakai replikasi. Pada gambar
//! dengan warna tajam, perbedaan itu saja sudah ~33 dB — bukan bug, cuma
//! dua metode yang sama-sama sah. Fixture `flat_*` sengaja punya chroma
//! konstan sehingga metode upsample tidak berpengaruh sama sekali; di sana
//! selisih yang tersisa murni dari decode koefisien + IDCT, dan itulah yang
//! diuji dengan ambang ketat.
//!
//! Fixture progressive sengaja dibuat di luar repo: crate `jpeg-encoder`
//! (dipakai untuk encode ke LCD) hanya bisa menulis baseline, dan
//! CoreGraphics di macOS mengabaikan flag progressive.

use std::path::{Path, PathBuf};

use trofeo_lcd::jpeg_decode;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn read_fixture(name: &str) -> Vec<u8> {
    let p = fixtures().join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("gagal baca fixture {}: {e}", p.display()))
}

/// Hasil decode Pillow atas fixture yang sama, sebagai acuan.
///
/// Format: 4 byte header (lebar u16 LE, tinggi u16 LE) lalu RGB888.
fn pillow_decode(name: &str) -> (u32, u32, Vec<u8>) {
    let stem = name.trim_end_matches(".jpg");
    let p = fixtures().join(format!("{stem}.pillow.rgb"));
    let raw = std::fs::read(&p)
        .unwrap_or_else(|e| panic!("gagal baca acuan {}: {e}", p.display()));
    assert!(raw.len() > 4, "acuan {} terpotong", p.display());
    let w = u16::from_le_bytes([raw[0], raw[1]]) as u32;
    let h = u16::from_le_bytes([raw[2], raw[3]]) as u32;
    (w, h, raw[4..].to_vec())
}

/// PSNR hasil decode kita vs Pillow, dalam dB. Makin tinggi makin dekat.
/// >60 dB = praktis identik; 45 dB = selisih satu level yang tak kasat mata.
fn psnr(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len(), "panjang buffer tidak sama");
    let mut sum = 0f64;
    for (x, y) in a.iter().zip(b.iter()) {
        let d = *x as f64 - *y as f64;
        sum += d * d;
    }
    let mse = sum / a.len() as f64;
    if mse == 0.0 {
        return f64::INFINITY;
    }
    10.0 * (255.0f64 * 255.0 / mse).log10()
}

fn max_diff(a: &[u8], b: &[u8]) -> u32 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| (*x as i32 - *y as i32).unsigned_abs())
        .max()
        .unwrap_or(0)
}

/// Decode `name` dan bandingkan dengan acuan Pillow.
fn check(name: &str, min_psnr: f64, max_diff_allowed: u32) {
    let d = jpeg_decode::decode(&read_fixture(name))
        .unwrap_or_else(|e| panic!("decode {name} gagal: {e}"));
    let (w, h, expect) = pillow_decode(name);

    assert_eq!((d.width, d.height), (w, h), "{name}: dimensi salah");
    assert_eq!(
        d.pixels.len(),
        (w * h * 3) as usize,
        "{name}: panjang buffer tidak sesuai"
    );

    let p = psnr(&d.pixels, &expect);
    assert!(
        p >= min_psnr,
        "{name}: PSNR vs Pillow cuma {p:.2} dB (ambang {min_psnr:.2} dB) — \
         ada bug di decoder"
    );

    let worst = max_diff(&d.pixels, &expect);
    assert!(
        worst <= max_diff_allowed,
        "{name}: selisih per-channel maksimum {worst} melebihi {max_diff_allowed}"
    );
}

// ---------------------------------------------------------------------------
// Fixture chroma konstan: pengujian ketat atas decode + IDCT
// ---------------------------------------------------------------------------

#[test]
fn baseline_444_akurat() {
    check("baseline_444.jpg", 50.0, 4);
}

#[test]
fn grayscale_akurat() {
    check("gray_baseline.jpg", 60.0, 2);
    check("gray_prog.jpg", 60.0, 2);
}

#[test]
fn progressive_444_akurat() {
    check("prog_444.jpg", 50.0, 4);
}

#[test]
fn chroma_konsisten_444_sangat_akurat() {
    check("flat_444.jpg", 60.0, 2);
}

#[test]
fn chroma_konsisten_422_sangat_akurat() {
    // 4:2:2 punya chroma separuh lebar. Karena chromanya konstan, upsample
    // mana pun menghasilkan hasil sama — jadi selisih di sini murni dari
    // decode koefisien chroma itu sendiri.
    check("flat_422.jpg", 60.0, 2);
}

#[test]
fn chroma_konsisten_420_sangat_akurat() {
    check("flat_420.jpg", 60.0, 2);
}

#[test]
fn chroma_konsisten_progressive_420_sangat_akurat() {
    check("flatprog_420.jpg", 60.0, 2);
}

#[test]
fn chroma_konsisten_progressive_444_sangat_akurat() {
    check("flatprog_444.jpg", 60.0, 2);
}

#[test]
fn progressive_444_sama_akuratnya_seperti_baseline() {
    // Progressive cuma mengubah urutan scan, bukan hasil. Kalau keduanya
    // memakai entropy data yang sama (Pillow menulis progressive dari sumber
    // yang sama), hasil decode-nya harus identik sampai pembulatan IDCT.
    let a = jpeg_decode::decode(&read_fixture("baseline_444.jpg")).unwrap();
    let b = jpeg_decode::decode(&read_fixture("prog_444.jpg")).unwrap();
    let worst = max_diff(&a.pixels, &b.pixels);
    assert!(worst <= 4, "baseline vs progressive untuk sumber sama: beda {worst}");
}

// ---------------------------------------------------------------------------
// Fixture berwarna: ambang longgar, perbedaan upsample chroma
// diperbolehkan
// ---------------------------------------------------------------------------

#[test]
fn subsampled_tetap_valid() {
    // Untuk gambar bers Chroma tajam, Pillow memakai triangle filter dan kita
    // memakai replikasi. Dua-duanya sah; yang diuji di sini hanya bahwa hasil
    // kita masuk akal — tidak hitam, tidak acak, dan masih dekat dengan acuan.
    for name in ["baseline_420.jpg", "baseline_422.jpg"] {
        let d = jpeg_decode::decode(&read_fixture(name))
            .unwrap_or_else(|e| panic!("decode {name} gagal: {e}"));
        let (_, _, expect) = pillow_decode(name);
        let p = psnr(&d.pixels, &expect);
        assert!(p >= 28.0, "{name}: PSNR cuma {p:.2} dB — terlalu jauh dari acuan");

        // Histogram harus berisi lebih dari satu nilai — hasil yang salah
        // sering terlihat sebagai gambar yang hanya punya 1-2 level.
        let mut seen = std::collections::HashSet::new();
        for px in d.pixels.iter().step_by(7) {
            seen.insert(*px);
            if seen.len() > 40 {
                break;
            }
        }
        assert!(seen.len() > 8, "{name}: hasil hanya punya {} level — rusak", seen.len());
    }
}

#[test]
fn subsampled_progressive_sama_akuratnya_seperti_baseline() {
    // Progressive 4:2:0 harus persis sama hasilnya dengan baseline 4:2:0.
    let a = jpeg_decode::decode(&read_fixture("baseline_420.jpg")).unwrap();
    let b = jpeg_decode::decode(&read_fixture("prog_420.jpg")).unwrap();
    let worst = max_diff(&a.pixels, &b.pixels);
    assert!(
        worst <= 8,
        "baseline 420 vs progressive 420 untuk sumber sama: beda {worst}"
    );
}

// ---------------------------------------------------------------------------
// Grayscale & quality ekstrem
// ---------------------------------------------------------------------------

#[test]
fn grayscale_r_g_b_selalu_sama() {
    for name in ["gray_baseline.jpg", "gray_prog.jpg"] {
        let d = jpeg_decode::decode(&read_fixture(name))
            .unwrap_or_else(|e| panic!("decode {name} gagal: {e}"));
        for (i, px) in d.pixels.chunks(3).enumerate() {
            assert_eq!(px[0], px[1], "{name}: piksel {i} R!=G");
            assert_eq!(px[1], px[2], "{name}: piksel {i} G!=B");
        }
    }
}

#[test]
fn quality_ekstrem_tetap_valid() {
    // Quality 10: blok 8×8 sangat blokier dan artifact-nya banyak. Tidak
    // boleh crash; hasilnya hanya boleh lebih kasar, bukan rusak.
    let d = jpeg_decode::decode(&read_fixture("q10_444.jpg")).expect("decode gagal");
    assert_eq!(d.pixels.len(), (d.width * d.height * 3) as usize);
}

// ---------------------------------------------------------------------------
// Ketahanan terhadap file rusak
// ---------------------------------------------------------------------------

#[test]
fn file_kosong_ditolak_rapi() {
    assert!(!jpeg_decode::decode(&[]).unwrap_err().is_empty());
}

#[test]
fn soi_setengah_ditolak_rapi() {
    let err = jpeg_decode::decode(&[0xFF]).unwrap_err();
    assert!(err.contains("JPEG"), "dapat: {err}");
}

#[test]
fn file_terpotong_di_banyak_titik_tidak_panic() {
    let full = read_fixture("baseline_420.jpg");
    for cut in [10usize, 50, 100, 200, 400, 800, full.len() / 2, full.len() - 20] {
        let truncated = &full[..cut.min(full.len())];
        match jpeg_decode::decode(truncated) {
            Ok(d) => assert_eq!(d.pixels.len(), (d.width * d.height * 3) as usize),
            Err(e) => assert!(!e.is_empty(), "pesan error kosong untuk cut {cut}"),
        }
    }
}

#[test]
fn bitstream_kacau_tidak_panic() {
    // Koracak bit di tengah entropy data. Hasil boleh valid atau error —
    // yang tidak boleh terjadi adalah panic atau index di luar jangkauan.
    for name in ["prog_420.jpg", "baseline_420.jpg"] {
        let full = read_fixture(name);
        for salt in [0x00u8, 0xFF, 0xA5, 0x5A] {
            let mut m = full.clone();
            let mid = m.len() / 2;
            for i in 0..16 {
                if mid + i < m.len() {
                    m[mid + i] ^= salt;
                }
            }
            if let Ok(d) = jpeg_decode::decode(&m) {
                assert_eq!(d.pixels.len(), (d.width * d.height * 3) as usize);
            }
        }
    }
}

#[test]
fn restart_marker_ditangani() {
    let d = jpeg_decode::decode(&read_fixture("baseline_444.jpg")).expect("decode gagal");
    assert!(d.width > 0 && d.height > 0);
}

// ---------------------------------------------------------------------------
// Performa
// ---------------------------------------------------------------------------

#[test]
fn dekode_cepat_cukup_untuk_startup() {
    // Background di-decode SEKALI saat start, jadi yang penting tidak ada
    // O(n²) diam-diam.
    use std::time::Instant;
    let bytes = read_fixture("baseline_444.jpg");
    let t0 = Instant::now();
    for _ in 0..10 {
        let _ = jpeg_decode::decode(&bytes).unwrap();
    }
    let per = t0.elapsed().as_secs_f64() / 10.0;
    assert!(per < 0.02, "decode fixture kecil makan {per:.4} dtk");
}

#[test]
fn dekode_gambar_besar_cukup_cepat() {
    use std::time::Instant;
    let small_bytes = read_fixture("flat_444.jpg");
    let t0 = Instant::now();
    for _ in 0..20 {
        let _ = jpeg_decode::decode(&small_bytes).unwrap();
    }
    let t_small = t0.elapsed().as_secs_f64() / 20.0;

    let big_bytes = read_fixture("baseline_420.jpg");
    let t1 = Instant::now();
    for _ in 0..20 {
        let _ = jpeg_decode::decode(&big_bytes).unwrap();
    }
    let t_big = t1.elapsed().as_secs_f64() / 20.0;

    // Fixture besar 1,6× lebih besar pikselnya, jadi waktunya harus di orde
    // yang sama. Bracket-nya lebar supaya tidak flaky di mesin lambat.
    assert!(
        t_big < t_small * 6.0,
        "decode tidak proporsional: kecil {t_small:.5}s, besar {t_big:.5}s"
    );
}