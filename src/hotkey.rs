//! Global hotkey untuk tangkapan layar frame LCD (dipakai trofeo_lcd dan
//! trofeo_screen). Di Windows memakai `RegisterHotKey` — hotkey "global"
//! (berfungsi walau jendela program tidak sedang fokus). Di OS lain tidak
//! tersedia: `register` gagal dengan pesan yang jelas, program tetap bisa
//! jalan tanpa hotkey.

/// Hotkey global yang terdaftar. Field `id` dipakai untuk memfilter pesan
/// WM_HOTKEY dari queue thread (Windows).
pub struct Hotkey {
    pub id: i32,
}

#[cfg(windows)]
mod imp {
    use super::Hotkey;
    use windows::Win32::UI::Input::KeyboardAndMouse::HOT_KEY_MODIFIERS;
    use windows::Win32::UI::Input::KeyboardAndMouse::RegisterHotKey;
    use windows::Win32::UI::WindowsAndMessaging::{MSG, PM_REMOVE, PeekMessageW, WM_HOTKEY};

    pub fn register(vk: u32) -> anyhow::Result<Hotkey> {
        const HOTKEY_ID: i32 = 1;
        // SAFETY: tanpa window handle = hotkey global untuk thread ini; id
        // unik lokal dan tidak bentrok dengan hotkey lain dalam proses ini.
        unsafe { RegisterHotKey(None, HOTKEY_ID, HOT_KEY_MODIFIERS(0x4000), vk)? };
        Ok(Hotkey { id: HOTKEY_ID })
    }

    pub fn triggered(id: i32) -> bool {
        // SAFETY: msg lokal valid; semua window dilewati (None) supaya tidak
        // mengganggu queue window lain.
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_HOTKEY && msg.wParam.0 == id as usize {
                    return true;
                }
            }
        }
        false
    }
}

#[cfg(not(windows))]
mod imp {
    use super::Hotkey;

    pub fn register(_vk: u32) -> anyhow::Result<Hotkey> {
        anyhow::bail!("hotkey tangkapan layar hanya didukung di Windows");
    }

    pub fn triggered(_id: i32) -> bool {
        false
    }
}

/// Daftarkan hotkey global (tanpa tombol modifier, dengan MOD_NOREPEAT supaya
/// tidak menembak berulang saat tombol ditahan). Gagal kalau tombol itu sudah
/// dipakai program lain — caller boleh lanjut tanpa hotkey.
pub fn register(vk: u32) -> anyhow::Result<Hotkey> {
    imp::register(vk)
}

/// `true` kalau hotkey sempat ditekan sejak polling terakhir (Windows).
pub fn triggered(id: i32) -> bool {
    imp::triggered(id)
}

/// Terjemahkan nama tombol hotkey ke virtual-key code Windows. Mendukung
/// `f1`-`f12` dan `printscreen` (plus alias `prtsc`/`print`/`snapshot`).
pub fn parse_key_name(raw: &str) -> anyhow::Result<u32> {
    let s = raw.trim().to_ascii_lowercase();
    let s = s.as_str();
    if matches!(s, "printscreen" | "prtsc" | "print" | "snapshot") {
        return Ok(0x2C); // VK_SNAPSHOT
    }
    if let Some(num) = s.strip_prefix('f') {
        if let Ok(n) = num.parse::<u32>() {
            if (1..=12).contains(&n) {
                return Ok(0x70 + n - 1); // VK_F1 = 0x70
            }
        }
    }
    anyhow::bail!("--screenshot-key: '{raw}' tidak dikenal (pakai f1-f12 atau printscreen).")
}