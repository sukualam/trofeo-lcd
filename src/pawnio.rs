//! Minimal safe wrapper around **PawnIOLib.dll** (<https://github.com/namazso/PawnIO>).
//!
//! Ported dari proyek deepcool-digital-windows, disesuaikan ke crate `windows`
//! (bukan `windows-sys`) supaya konsisten dengan dependensi yang sudah ada di
//! proyek ini.
//!
//! PawnIO adalah kernel driver WHQL-signed untuk membaca MSR/SMN/PCI register
//! langsung tanpa perlu LibreHardwareMonitor maupun software lain berjalan di
//! background. Driver-nya harus sudah diinstall sekali secara sistem (lewat
//! `winget install namazso.PawnIO` atau installer dari <https://pawnio.eu>) —
//! modul ini hanya "bicara" dengan driver yang sudah ada, dan mundur gracefully
//! (return `None`) kalau driver tidak ditemukan.
//!
//! Kita load `PawnIOLib.dll` secara dynamic (`LoadLibraryW`/`GetProcAddress`)
//! — bukan link statis — supaya build tidak memerlukan PawnIO di mesin build,
//! dan program tetap bisa jalan (dengan sensor tidak tersedia) di mesin yang
//! tidak punya driver-nya.

// Seluruh isi modul ini hanya relevan di Windows.
#![cfg(windows)]

use std::ffi::CString;

use windows::Win32::Foundation::{HANDLE, HMODULE};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::core::{PCSTR, PCWSTR};

type HResult = i32;

fn succeeded(hr: HResult) -> bool {
    hr >= 0
}

// Tipe function pointer yang di-resolve dari PawnIOLib.dll lewat GetProcAddress.
// Tetap memakai tipe raw supaya kita bisa menyimpannya sebagai fn pointer biasa
// (FARPROC dari GetProcAddress tidak bisa disimpan langsung karena bukan Sized).
type PawnioOpenFn = unsafe extern "system" fn(*mut HANDLE) -> HResult;
type PawnioLoadFn = unsafe extern "system" fn(HANDLE, *const u8, usize) -> HResult;
#[allow(clippy::type_complexity)]
type PawnioExecuteFn = unsafe extern "system" fn(
    HANDLE,
    *const i8,
    *const u64,
    usize,
    *mut u64,
    usize,
    *mut usize,
) -> HResult;
type PawnioCloseFn = unsafe extern "system" fn(HANDLE) -> HResult;

struct Api {
    open: PawnioOpenFn,
    load: PawnioLoadFn,
    execute: PawnioExecuteFn,
    close: PawnioCloseFn,
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Load PawnIOLib.dll dan resolve 4 fungsi yang dibutuhkan. Coba nama DLL
/// biasa dulu (berhasil kalau sudah ada di PATH — yang ditambah installer
/// resmi), lalu fallback ke lokasi default install di `%ProgramFiles%\PawnIO`.
fn load_api() -> Option<Api> {
    let program_files =
        std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".to_string());
    let candidates = [
        "PawnIOLib.dll".to_string(),
        format!(r"{program_files}\PawnIO\PawnIOLib.dll"),
    ];

    let module: HMODULE = candidates.iter().find_map(|path| {
        let wide = to_wide(path);
        // SAFETY: wide adalah null-terminated UTF-16 yang valid.
        unsafe { LoadLibraryW(PCWSTR(wide.as_ptr())) }.ok()
    })?;

    // Resolve satu fungsi dari DLL yang sudah di-load.
    // SAFETY: T selalu salah satu dari tipe Pawnio*Fn di atas — semuanya
    // berukuran pointer (sama dengan FARPROC yang GetProcAddress kembalikan),
    // jadi transmute aman secara ukuran.
    unsafe fn resolve<T: Copy>(module: HMODULE, name: &str) -> Option<T> {
        let c_name = CString::new(name).ok()?;
        let addr = GetProcAddress(module, PCSTR(c_name.as_ptr() as *const u8))?;
        Some(std::mem::transmute_copy(&addr))
    }

    // SAFETY: semua resolve di bawah mengikuti ABI yang didokumentasikan
    // PawnIO, dan tipe-tipe fn di atas dibuat sesuai signature tersebut.
    unsafe {
        Some(Api {
            open: resolve(module, "pawnio_open")?,
            load: resolve(module, "pawnio_load")?,
            execute: resolve(module, "pawnio_execute")?,
            close: resolve(module, "pawnio_close")?,
        })
    }
}

/// Modul PawnIO yang sudah di-load — dalam program ini selalu berisi modul
/// AMD Family 17h (Zen1–Zen4) yang di-embed waktu compile (lihat `cpu_sensor.rs`).
pub struct PawnIo {
    api: Api,
    handle: HANDLE,
}

impl PawnIo {
    /// Buka PawnIO executor dan load blob modul yang diberikan.
    /// `None` kalau driver PawnIO tidak terpasang/berjalan, atau blob ditolak
    /// (tanda tangan salah, atau CPU tidak didukung modul tersebut — mis.
    /// CPU non-AMD atau family di luar yang di-cover modul itu).
    pub fn open_with_module(blob: &[u8]) -> Option<Self> {
        let api = load_api()?;

        let mut handle = HANDLE::default(); // null / tidak valid, diisi oleh pawnio_open
        if !succeeded(unsafe { (api.open)(&mut handle) }) {
            return None;
        }
        if !succeeded(unsafe { (api.load)(handle, blob.as_ptr(), blob.len()) }) {
            unsafe { (api.close)(handle) };
            return None;
        }
        Some(PawnIo { api, handle })
    }

    /// Panggil fungsi IOCTL yang di-export modul (mis. `ioctl_read_smn`,
    /// `ioctl_read_msr`) berdasarkan nama. `input`/`out_len` dihitung dalam
    /// sel `u64`, sesuai ukuran buffer `in[]`/`out[]` di source `.p` modul.
    pub fn execute(&self, name: &str, input: &[u64], out_len: usize) -> Option<Vec<u64>> {
        let c_name = CString::new(name).ok()?;
        let mut output = vec![0u64; out_len];
        let mut returned: usize = 0;

        let hr = unsafe {
            (self.api.execute)(
                self.handle,
                c_name.as_ptr(),
                input.as_ptr(),
                input.len(),
                output.as_mut_ptr(),
                output.len(),
                &mut returned,
            )
        };
        if !succeeded(hr) {
            return None;
        }
        output.truncate(returned);
        Some(output)
    }
}

impl Drop for PawnIo {
    fn drop(&mut self) {
        // SAFETY: handle valid (diisi oleh pawnio_open yang sukses).
        unsafe { (self.api.close)(self.handle) };
    }
}

// SAFETY: handle adalah referensi kernel object biasa; tidak ada state
// thread-local yang dipakai di sini, jadi aman dipindah antar thread.
// Kita tidak claim Sync karena execute() tidak dipanggil concurrent.
unsafe impl Send for PawnIo {}
