//! Pembacaan sensor CPU lewat **SMC** (System Management Controller) di macOS.
//!
//! ## Kenapa ini jalan di Hackintosh
//!
//! Di Intel Mac asli, suhu CPU diekspos ke macOS lewat `powermetrics` (MSR) atau
//! SMC key yang diisi firmware Apple. Keduanya tidak ada di CPU AMD: `powermetrics`
//! membaca lewat XCP Intel, dan motherboard Ryzen tidak mengisi key SMC Apple.
//!
//! Tapi dengan VirtualSMC + SMCAMDProcessor (kext komunitas Hackintosh),_sensor
//! CPU AMD diisi ke key SMC standar milik Apple — `TC0P` dan sejenisnya. Jadi
//!reading lewat SMC benar-benar bisa di macOS, asalkan kext-nya terpasang.
//!
//! Diuji di mesin ini (Ryzen 7500F, macOS 26.7): `TC0P` bergerak dari 48 °C idle
//! ke 66 °C saat CPU dibebani, jadi datanya hidup, bukan angka statis.
//!
//! ## Protokol
//!
//! Pembacaan satu key butuh **dua** panggilan ke user client `AppleSMC`:
//!
//! 1. `data8 = 9` (`READ_KEYINFO`) — minta metadata key: `dataSize` + `dataType`.
//! 2. `data8 = 5` (`READ_BYTES`) — dengan `dataSize` hasil langkah 1, ambil nilainya.
//!
//! `dataType` penting: nilai yang sama punya arti berbeda tergantung tipenya
//! (`sp78` = signed + pecahan/256, `fpe2` = float IEEE 16-bit, `ui8 ` = byte
//! polos, dst). Mengasumsikan `sp78` untuk semua key akan menghasilkan angka
//! yang terlihat masuk akal tapi salah untuk key bertipe lain.

use std::sync::OnceLock;
use std::ffi::{c_char, c_void};

// ---------------------------------------------------------------------------
// IOKit FFI
//
// Deklarasi manual supaya tidak perlu menambah crate IOKit hanya untukLima
// fungsi di bawah. Semua tipe klever berupa integer/pointer, jadi tidak ada
// ABI yang bisa meleset di sini — yang sensitif cuma `#[repr(C)] SmcKeyData`.

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOServiceMatching(name: *const c_char) -> *mut c_void;
    fn IOServiceGetMatchingService(main_port: u32, matching: *mut c_void) -> u32;
    fn IOServiceOpen(service: u32, owning_task: u32, options: u32, connect: *mut u32) -> i32;
    fn IOServiceClose(connect: u32) -> i32;
    fn IOObjectRelease(object: u32) -> i32;
    fn IOConnectCallStructMethod(
        connect: u32,
        selector: u32,
        input: *const c_void,
        input_size: usize,
        output: *mut c_void,
        output_size: *mut usize,
    ) -> i32;
    fn mach_task_self() -> u32;
}

/// `kIOMasterPortDefault` = 0 pada macOS modern.
const K_IO_MASTER_PORT_DEFAULT: u32 = 0;
/// Selector user client AppleSMC.
const KERNEL_INDEX_SMC: u32 = 2;
/// `data8` untuk langkah metadata.
const SMC_CMD_READ_KEYINFO: i8 = 9;
/// `data8` untuk langkah pembacaan nilai.
const SMC_CMD_READ_BYTES: i8 = 5;

const KERN_SUCCESS: i32 = 0;

#[repr(C)]
struct SmcVers {
    major: i8,
    minor: i8,
    build: i8,
    reserved: [i8; 1],
    release: u16,
}

#[repr(C)]
struct SmcPLimit {
    version: u16,
    length: u16,
    cpu_p_limit: u32,
    gpu_p_limit: u32,
    mem_p_limit: u32,
}

#[repr(C)]
struct SmcKeyInfo {
    data_size: u32,
    /// 4 huruf tipe data, di-packed big-endian di u32.
    data_type: u32,
    data_attributes: i8,
}

/// Struktur input/output untuk `IOConnectCallStructMethod`. Field persis
/// mengikuti ABI `AppleSMC` — urutan, tipe, dan nama tidak boleh diubah.
#[repr(C)]
struct SmcKeyData {
    key: u32,
    vers: SmcVers,
    p_limit: SmcPLimit,
    key_info: SmcKeyInfo,
    result: i8,
    status: i8,
    /// Dipakai sebagai kode perintah (9 atau 5), bukan data.
    data8: i8,
    data32: u32,
    bytes: [u8; 32],
}

/// Key SMC untuk suhu CPU, diurut dari yang paling mungkin.
///
/// `TC0P` = "CPU proximity temperature", yang dipakai VirtualSMC untuk AMD.
/// Sisanya adalah padanan Intel/Apple lama supaya modul ini tetap berguna di
/// Mac Intel asli (TC0C/TC0D/TC0E/TC0H adalah keyIntel; `Tp09`/`Tp0T` dipakai
/// beberapa motherboard).
///
/// Dicoba berurutan sampai ada yang mengembalikan nilai masuk akal, bukan 0.
const CPU_TEMP_KEYS: [&str; 7] = ["TC0P", "TC0C", "TC0D", "TC0E", "TC0H", "Tp09", "Tp0T"];

/// Batas kewajaran untuk suhu CPU. Di luar rentang ini nilainya hampir pasti
/// noise / key yang tidak terisi, lebih baikshown sebagai N/A daripada angka
/// seperti 32767 °C.
const TEMP_PLAUSIBLE_RANGE: (f64, f64) = (1.0, 125.0);

static CONNECTION: OnceLock<Option<u32>> = OnceLock::new();

/// Buka (sekali, lalu di-cache) koneksi ke `AppleSMC` user client.
/// `None` berarti tidak ada SMC — mis. mesin tanpa VirtualSMC.
fn smc_connection() -> Option<u32> {
    *CONNECTION.get_or_init(|| unsafe {
        let name = c"AppleSMC";
        let matching = IOServiceMatching(name.as_ptr());
        if matching.is_null() {
            return None;
        }
        let service = IOServiceGetMatchingService(K_IO_MASTER_PORT_DEFAULT, matching);
        if service == 0 {
            return None;
        }
        let mut conn = 0u32;
        let kr = IOServiceOpen(service, mach_task_self(), 0, &mut conn);
        IOObjectRelease(service);
        if kr == KERN_SUCCESS && conn != 0 {
            Some(conn)
        } else {
            None
        }
    })
}

/// Ubah 4 huruf jadi u32 big-endian, sesuai representasi internal SMC.
fn key_to_u32(key: &str) -> u32 {
    let b = key.as_bytes();
    let mut v = 0u32;
    for i in 0..4 {
        v = (v << 8) | *b.get(i).unwrap_or(&b' ') as u32;
    }
    v
}

/// Baca satu key: metadata dulu, lalu nilainya. Mengembalikan
/// `(dataType, bytes)` kalau key ada.
fn smc_read_key(key: &str) -> Option<(u32, [u8; 32])> {
    let conn = smc_connection()?;

    unsafe {
        // Langkah 1: metadata key.
        let mut input = std::mem::zeroed::<SmcKeyData>();
        let mut output = std::mem::zeroed::<SmcKeyData>();
        input.key = key_to_u32(key);
        input.data8 = SMC_CMD_READ_KEYINFO;

        let mut out_size = std::mem::size_of::<SmcKeyData>();
        let kr = IOConnectCallStructMethod(
            conn,
            KERNEL_INDEX_SMC,
            &input as *const SmcKeyData as *const c_void,
            std::mem::size_of::<SmcKeyData>(),
            &mut output as *mut SmcKeyData as *mut c_void,
            &mut out_size,
        );
        if kr != KERN_SUCCESS || output.result != 0 {
            return None;
        }

        let data_size = output.key_info.data_size;
        if data_size == 0 || data_size > 32 {
            return None;
        }
        // PENTING: `data_type` hanya terisi di respons langkah 1. Di langkah 2
        // field itu kembali kosong — kalau diambil dari sana, `decode()` dapat
        // tipe yang tidak dikenal dan selalu mengembalikan None.
        let data_type = output.key_info.data_type;

        // Langkah 2: nilai, dengan dataSize dari langkah 1.
        input.key_info.data_size = data_size;
        input.data8 = SMC_CMD_READ_BYTES;
        out_size = std::mem::size_of::<SmcKeyData>();

        let kr = IOConnectCallStructMethod(
            conn,
            KERNEL_INDEX_SMC,
            &input as *const SmcKeyData as *const c_void,
            std::mem::size_of::<SmcKeyData>(),
            &mut output as *mut SmcKeyData as *mut c_void,
            &mut out_size,
        );
        if kr != KERN_SUCCESS || output.result != 0 {
            return None;
        }

        Some((data_type, output.bytes))
    }
}

/// Terjemahkan 4 huruf `dataType` jadi string, buat pesan error yang berguna.
fn data_type_name(code: u32) -> String {
    let b = code.to_be_bytes();
    String::from_utf8_lossy(&b).trim_end().to_string()
}

/// Decode nilai SMC sesuai tipenya.
fn decode(code: u32, bytes: &[u8; 32]) -> Option<f64> {
    let t = data_type_name(code);
    match t.as_str() {
        // signed byte + pecahan 1/256 — format temperature Apple klasik.
        "sp78" => {
            if bytes.len() < 2 {
                return None;
            }
            Some(bytes[0] as i8 as f64 + bytes[1] as f64 / 256.0)
        }
        // Half-precision float (IEEE 754 binary16).
        "fpe2" => {
            if bytes.len() < 2 {
                return None;
            }
            let bits = u16::from_be_bytes([bytes[0], bytes[1]]);
            Some(f16_to_f64(bits))
        }
        // Unsigned integer.
        "ui8" | "ui8 " => Some(bytes.first().copied().unwrap_or(0) as f64),
        "ui16" | "ui16" => {
            if bytes.len() < 2 {
                return None;
            }
            Some(u16::from_be_bytes([bytes[0], bytes[1]]) as f64)
        }
        "ui32" | "ui32" => {
            if bytes.len() < 4 {
                return None;
            }
            Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as f64)
        }
        // 4-byte float.
        "flt " | "flt" => {
            if bytes.len() < 4 {
                return None;
            }
            Some(f32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as f64)
        }
        _ => None,
    }
}

/// Konversi half-precision (binary16) ke f64 tanpa dependency.
fn f16_to_f64(bits: u16) -> f64 {
    let sign = if bits & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = ((bits >> 10) & 0x1f) as i32;
    let frac = (bits & 0x03ff) as f64;
    if exp == 0 {
        // Subnormal.
        sign * frac * 2f64.powi(-24)
    } else if exp == 0x1f {
        // Inf / NaN.
        if frac == 0.0 {
            sign * f64::INFINITY
        } else {
            f64::NAN
        }
    } else {
        sign * (1.0 + frac / 1024.0) * 2f64.powi(exp - 15)
    }
}

/// Suhu CPU dalam °C, atau `None` kalau tidak ada sensor yang terbaca.
pub fn cpu_temp_c() -> Option<f32> {
    let (lo, hi) = TEMP_PLAUSIBLE_RANGE;
    for key in CPU_TEMP_KEYS {
        let Some((data_type, bytes)) = smc_read_key(key) else {
            continue;
        };
        let Some(value) = decode(data_type, &bytes) else {
            continue;
        };
        if value.is_finite() && (lo..=hi).contains(&value) {
            return Some(value as f32);
        }
    }
    None
}

/// True kalau SMC bisa dibuka — dipakai untuk pesan diagnostik saat start.
pub fn smc_available() -> bool {
    smc_connection().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Layout struct harus persis dengan ABI `AppleSMC`; kalau bergeser,
    /// panggilan IOKit akan diam-diam mengembalikan data sampah.
    #[test]
    fn struct_layout_is_stable() {
        // key 4 + vers 6 + pad 2 + p_limit 16 + key_info 12 (9 byte data tapi
        // align 4) + result/status/data8 3 + pad 1 + data32 4 + bytes 32 = 80.
        assert_eq!(std::mem::size_of::<SmcKeyData>(), 80);
        assert_eq!(std::mem::align_of::<SmcKeyData>(), 4);
    }

    /// Konversi 4 huruf ke u32 harus big-endian, supaya kunci "TC0P" sampai ke
    /// SMC sebagai yang benar.
    #[test]
    fn key_encoding_is_big_endian() {
        assert_eq!(key_to_u32("TC0P"), 0x5443_3050);
        // String pendek di-pad dengan spasi, bukan NUL.
        assert_eq!(key_to_u32("TC"), 0x5443_2020);
    }

    /// `sp78`: byte pertama signed bulat, byte kedua pecahan /256. Nilai
    /// negatif harus tetap negatif (suhu di bawah 0 °C mungkin, tapi decode
    ///_signed_ yang benar).
    #[test]
    fn decodes_sp78() {
        let code = u32::from_be_bytes(*b"sp78");
        let mut b = [0u8; 32];
        b[0] = 66; // 66
        b[1] = 64; // 64/256 = 0.25
        assert!((decode(code, &b).unwrap() - 66.25).abs() < 1e-9);

        b[0] = 255; // -1 sebagai i8
        b[1] = 0;
        assert!((decode(code, &b).unwrap() - (-1.0)).abs() < 1e-9);
    }

    /// `fpe2` = half float: 1.0 harus balik persis 1.0, 0.5 jadi 0.5.
    #[test]
    fn decodes_fpe2() {
        let code = u32::from_be_bytes(*b"fpe2");
        let one = f64_to_f16(1.0);
        let mut b = [0u8; 32];
        b[0..2].copy_from_slice(&one.to_be_bytes());
        assert!((decode(code, &b).unwrap() - 1.0).abs() < 1e-6);

        let half = f64_to_f16(0.5);
        b[0..2].copy_from_slice(&half.to_be_bytes());
        assert!((decode(code, &b).unwrap() - 0.5).abs() < 1e-6);
    }

    /// Tipe tak dikenal harus `None`, bukan tebakan — lebih baik N/A daripada
    /// angka yang salah.
    #[test]
    fn unknown_type_is_none() {
        let code = u32::from_be_bytes(*b"zzzz");
        assert!(decode(code, &[0u8; 32]).is_none());
    }

    /// End-to-end ke SMC sungguhan. Di-)ignore_ karena butuh VirtualSMC
    /// + SMCAMDProcessor terpasang.
    #[test]
    #[ignore]
    fn reads_cpu_temperature() {
        assert!(smc_available(), "SMC tidak bisa dibuka");
        match cpu_temp_c() {
            Some(t) => {
                println!("suhu CPU dari SMC = {t:.2} C");
                let (lo, hi) = TEMP_PLAUSIBLE_RANGE;
                assert!((lo..=hi).contains(&(t as f64)), "di luar rentang wajar: {t}");
            }
            None => panic!("tidak ada key suhu CPU yang terbaca"),
        }
    }
}

/// Ubah f64 ke half-precision, hanya dipakai test.
#[cfg(test)]
fn f64_to_f16(v: f64) -> u16 {
    let bits = v.to_bits();
    let sign = ((bits >> 48) & 0x8000) as u16;
    let exp = ((bits >> 52) & 0x7ff) as i32;
    let frac = bits & 0x000f_ffff_ffff_ffff;
    let e = exp - 1023 + 15;
    if e <= 0 {
        sign
    } else if e >= 0x1f {
        sign | 0x7c00
    } else {
        sign | ((e as u16) << 10) | ((frac >> 42) as u16)
    }
}

