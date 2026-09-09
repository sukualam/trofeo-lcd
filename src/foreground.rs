//! Deteksi nama .exe dari window yang sedang aktif (foreground) di Windows.
//!
//! Dipakai `main.rs` untuk mengganti tampilan "NOW PLAYING" jadi nama
//! game/program yang sedang berjalan saat GPU usage tinggi (indikasi lagi
//! main game, bukan lagi dengerin musik).
//!
//! Caranya: `GetForegroundWindow` (window aktif) -> `GetWindowThreadProcessId`
//! (PID pemilik window itu) -> `OpenProcess` + `QueryFullProcessImageNameW`
//! (path exe dari PID itu). Tiga panggilan WinAPI standar, tidak butuh
//! privilese admin untuk proses biasa/game (pakai
//! `PROCESS_QUERY_LIMITED_INFORMATION`, hak akses paling minim yang cukup
//! untuk ini).

#[cfg(windows)]
mod imp {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

    /// Nama file exe (tanpa path, tanpa ekstensi ".exe") dari proses pemilik
    /// window foreground saat ini. `None` kalau tidak ada foreground window
    /// (mis. layar terkunci), atau proses tidak bisa dibuka (proses sistem
    /// dengan proteksi lebih tinggi dari akses kita).
    pub fn foreground_exe_name() -> Option<String> {
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.0.is_null() {
                return None;
            }

            let mut pid: u32 = 0;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid == 0 {
                return None;
            }

            let process =
                OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;

            let mut buf = [0u16; 1024];
            let mut size = buf.len() as u32;
            let result = QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                windows::core::PWSTR(buf.as_mut_ptr()),
                &mut size,
            );
            let _ = CloseHandle(process);
            result.ok()?;

            let path = String::from_utf16_lossy(&buf[..size as usize]);
            let file_name = path.rsplit(['\\', '/']).next().unwrap_or(&path);
            let name = file_name.strip_suffix(".exe").unwrap_or(file_name);

            if name.trim().is_empty() {
                None
            } else {
                Some(name.to_string())
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::fs;
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt};

    /// Nama file exe (tanpa path, tanpa ekstensi) dari proses pemilik window
    /// yang sedang aktif — lewat X11 (`_NET_ACTIVE_WINDOW` + `_NET_WM_PID`,
    /// standar EWMH yang didukung semua window manager modern: GNOME/Mutter,
    /// KDE/KWin, XFCE, i3, dst).
    ///
    /// KETERBATASAN: cuma jalan di sesi X11 (native ATAU lewat XWayland untuk
    /// aplikasi yang belum native Wayland). Di sesi Wayland MURNI (jendela
    /// native Wayland tanpa XWayland), `_NET_ACTIVE_WINDOW` tidak ada karena
    /// protokol dasar Wayland memang sengaja tidak mengekspos "window mana
    /// yang aktif" ke aplikasi lain (batasan keamanan/sandboxing arsitektur
    /// Wayland sendiri, bukan sesuatu yang bisa di-workaround dari sini) —
    /// beberapa compositor (Sway/wlroots) punya protokol tambahan sendiri
    /// (`wlr-foreign-toplevel-management`) untuk ini, tapi itu tidak
    /// standar lintas compositor. Kalau ini terjadi, fungsi ini `None` —
    /// mode game tetap jalan (GPU>50% based), cuma NOW PLAYING tidak
    /// berganti jadi nama game.
    pub fn foreground_exe_name() -> Option<String> {
        let (conn, screen_num) = x11rb::connect(None).ok()?;
        let root = conn.setup().roots.get(screen_num)?.root;

        let net_active_window = intern_atom(&conn, b"_NET_ACTIVE_WINDOW")?;
        let net_wm_pid = intern_atom(&conn, b"_NET_WM_PID")?;

        let active_reply = conn
            .get_property(false, root, net_active_window, AtomEnum::WINDOW, 0, 1)
            .ok()?
            .reply()
            .ok()?;
        let window = active_reply.value32()?.next()?;
        if window == 0 {
            return None;
        }

        let pid_reply = conn
            .get_property(false, window, net_wm_pid, AtomEnum::CARDINAL, 0, 1)
            .ok()?
            .reply()
            .ok()?;
        let pid = pid_reply.value32()?.next()?;
        if pid == 0 {
            return None;
        }

        exe_name_from_pid(pid)
    }

    fn intern_atom(conn: &impl Connection, name: &[u8]) -> Option<u32> {
        Some(conn.intern_atom(false, name).ok()?.reply().ok()?.atom)
    }

    /// Nama exe dari `/proc/{pid}/exe` (path lengkap, ambil nama file
    /// terakhir) — lebih akurat daripada `/proc/{pid}/comm` yang dipotong
    /// kernel maksimum 15 karakter. Fallback ke `comm` kalau `exe` tidak
    /// terbaca (mis. proses milik user lain / proteksi tambahan).
    fn exe_name_from_pid(pid: u32) -> Option<String> {
        if let Ok(target) = fs::read_link(format!("/proc/{pid}/exe")) {
            if let Some(name) = target.file_name().and_then(|n| n.to_str()) {
                if !name.trim().is_empty() {
                    return Some(name.to_string());
                }
            }
        }
        let comm = fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
        let name = comm.trim();
        (!name.is_empty()).then(|| name.to_string())
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod imp {
    pub fn foreground_exe_name() -> Option<String> {
        None
    }
}

pub use imp::foreground_exe_name;
