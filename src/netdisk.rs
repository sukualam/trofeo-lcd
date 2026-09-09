//! Network throughput dan disk IO.
//!
//! - **Windows**: `GetIfTable2` (IP Helper API) untuk network,
//!   `IOCTL_DISK_PERFORMANCE` dikirim langsung ke tiap `\\.\PhysicalDriveN`
//!   untuk disk — keduanya WinAPI native, tanpa PDH/library eksternal.
//! - **Linux**: `/proc/net/dev` untuk network, `/proc/diskstats` untuk disk
//!   — dua file teks standar kernel, selalu ada di semua distro, tidak
//!   butuh privilese root untuk dibaca.

#[cfg(windows)]
mod imp {
    use std::collections::HashMap;
    use std::time::Instant;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIfTable2, MIB_IF_TABLE2};
    use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::Ioctl::{DISK_PERFORMANCE, IOCTL_DISK_PERFORMANCE};
    use windows::Win32::System::IO::DeviceIoControl;

    /// Tipe interface "software loopback" (RFC 2863 `ifType`) — selalu
    /// dilewati supaya tidak ikut dihitung sebagai traffic network asli.
    const IF_TYPE_SOFTWARE_LOOPBACK: u32 = 24;
    /// Hanya dua tipe ini yang dianggap adapter FISIK (kabel Ethernet & WiFi).
    /// Adapter virtual (Hyper-V Default Switch, WSL, VMware, VPN, dst.) sering
    /// TIDAK ber-Type loopback tapi tetap bukan koneksi internet asli — kalau
    /// traffic internal adapter itu (mis. sinkronisasi WSL<->host) kebetulan
    /// lebih besar dari traffic internet, heuristik "delta terbesar" di bawah
    /// bisa salah pilih adapter itu, bikin angka jauh di atas batas ISP asli.
    const PHYSICAL_IF_TYPES: [u32; 2] = [6, 71]; // IF_TYPE_ETHERNET_CSMACD, IF_TYPE_IEEE80211
    /// Kata kunci pada `Description` adapter yang dibuang meski Type-nya
    /// kebetulan sama dengan Ethernet/WiFi (banyak virtual switch memakai
    /// Type Ethernet juga).
    const VIRTUAL_IF_KEYWORDS: [&str; 11] = [
        "virtual", "hyper-v", "vethernet", "wsl", "vmware", "virtualbox",
        "tunnel", "bluetooth", "tap-windows", "npcap", "pseudo",
    ];

    pub struct NetDiskMonitor {
        // Byte counter TERAKHIR per interface (key: InterfaceIndex), BUKAN
        // dijumlah dulu jadi satu total. Windows sering punya beberapa
        // adapter virtual (Hyper-V Default Switch, WSL, dst.) yang cuma
        // "mencerminkan" traffic adapter fisik yang sama — kalau semua
        // interface langsung dijumlah, traffic asli bisa kehitung berkali
        // lipat. Jadi tiap sample kita ambil SATU interface dengan delta
        // terbesar (asumsi: itu adapter fisik yang benar-benar dipakai),
        // bukan total semua interface.
        net_prev: HashMap<u32, (u64, u64)>,
        prev_disk_read: u64,
        prev_disk_write: u64,
        prev_time: Instant,
        primed: bool,
    }

    impl NetDiskMonitor {
        pub fn new() -> Self {
            Self {
                net_prev: HashMap::new(),
                prev_disk_read: 0,
                prev_disk_write: 0,
                prev_time: Instant::now(),
                primed: false,
            }
        }

        /// Kembalikan `(net_down_kb_s, net_up_kb_s, disk_read_mb_s, disk_write_mb_s)`.
        pub fn sample(&mut self) -> (f64, f64, f64, f64) {
            let net_rows = read_network_rows();
            let (disk_read, disk_write) = read_disk_totals();

            let now = Instant::now();
            let elapsed = now.duration_since(self.prev_time).as_secs_f64().max(0.001);

            let mut best_down = 0u64;
            let mut best_up = 0u64;
            if self.primed {
                for &(idx, in_bytes, out_bytes) in &net_rows {
                    if let Some(&(prev_in, prev_out)) = self.net_prev.get(&idx) {
                        let d_in = in_bytes.saturating_sub(prev_in);
                        let d_out = out_bytes.saturating_sub(prev_out);
                        if d_in + d_out > best_down + best_up {
                            best_down = d_in;
                            best_up = d_out;
                        }
                    }
                }
            }

            self.net_prev = net_rows
                .into_iter()
                .map(|(idx, i, o)| (idx, (i, o)))
                .collect();

            let result = if !self.primed {
                (0.0, 0.0, 0.0, 0.0)
            } else {
                (
                    best_down as f64 / elapsed / 1024.0,
                    best_up as f64 / elapsed / 1024.0,
                    disk_read.saturating_sub(self.prev_disk_read) as f64 / elapsed / 1_048_576.0,
                    disk_write.saturating_sub(self.prev_disk_write) as f64
                        / elapsed
                        / 1_048_576.0,
                )
            };

            self.prev_disk_read = disk_read;
            self.prev_disk_write = disk_write;
            self.prev_time = now;
            self.primed = true;

            result
        }
    }

    /// `(InterfaceIndex, InOctets, OutOctets)` per interface fisik/aktif
    /// (bukan loopback), sejak boot — dibandingkan dua kali panggilan
    /// PER INTERFACE (lihat komentar di `net_prev`) untuk dapat throughput.
    fn read_network_rows() -> Vec<(u32, u64, u64)> {
        unsafe {
            let mut table_ptr: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
            if GetIfTable2(&mut table_ptr).is_err() || table_ptr.is_null() {
                return Vec::new();
            }

            let table = &*table_ptr;
            let count = table.NumEntries as usize;
            // `Table` adalah array berukuran fleksibel (flexible array member)
            // di ujung struct C asli; di binding Rust representasinya array
            // 1-elemen, jadi harus dibaca manual sebagai slice sepanjang
            // `NumEntries` lewat pointer ke elemen pertama.
            let rows = std::slice::from_raw_parts(table.Table.as_ptr(), count);

            let mut out = Vec::with_capacity(count);
            for row in rows {
                if row.Type == IF_TYPE_SOFTWARE_LOOPBACK {
                    continue;
                }
                if row.OperStatus != IfOperStatusUp {
                    continue;
                }
                if !PHYSICAL_IF_TYPES.contains(&row.Type) {
                    continue;
                }
                let description = wide_to_string(&row.Description).to_ascii_lowercase();
                if VIRTUAL_IF_KEYWORDS.iter().any(|kw| description.contains(kw)) {
                    continue;
                }
                out.push((row.InterfaceIndex, row.InOctets, row.OutOctets));
            }

            FreeMibTable(table_ptr as *const _);
            out
        }
    }

    /// Konversi array `WCHAR` (null-terminated) dari struct WinAPI jadi `String`.
    fn wide_to_string(chars: &[u16]) -> String {
        let len = chars.iter().position(|&c| c == 0).unwrap_or(chars.len());
        String::from_utf16_lossy(&chars[..len])
    }

    /// Jumlah total byte read/write dari semua physical drive yang berhasil
    /// dibuka (index 0..15), sejak boot.
    fn read_disk_totals() -> (u64, u64) {
        let mut total_read = 0u64;
        let mut total_write = 0u64;

        for i in 0..16u32 {
            let path = format!(r"\\.\PhysicalDrive{i}");
            let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();

            unsafe {
                let handle = CreateFileW(
                    windows::core::PCWSTR(wide.as_ptr()),
                    0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    None,
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL,
                    None,
                );
                let Ok(handle) = handle else {
                    // Index drive ini tidak ada — index selanjutnya masih
                    // mungkin valid (bukan selalu berurutan rapat), jadi tetap
                    // lanjut ke iterasi berikutnya, bukan `break`.
                    continue;
                };

                let mut perf = DISK_PERFORMANCE::default();
                let mut bytes_returned = 0u32;
                let ok = DeviceIoControl(
                    handle,
                    IOCTL_DISK_PERFORMANCE,
                    None,
                    0,
                    Some(&mut perf as *mut _ as *mut _),
                    std::mem::size_of::<DISK_PERFORMANCE>() as u32,
                    Some(&mut bytes_returned),
                    None,
                );
                let _ = CloseHandle(handle);

                if ok.is_ok() {
                    total_read = total_read.saturating_add(perf.BytesRead as u64);
                    total_write = total_write.saturating_add(perf.BytesWritten as u64);
                }
            }
        }

        (total_read, total_write)
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::collections::HashMap;
    use std::fs;
    use std::time::Instant;

    /// Prefix nama interface virtual yang dibuang biar tidak ikut heuristik
    /// "delta terbesar" salah pilih (bridge Docker/Podman, pasangan veth
    /// container, WireGuard, PPP, dst.) — analog `VIRTUAL_IF_KEYWORDS` di
    /// jalur Windows.
    const VIRTUAL_IF_PREFIXES: [&str; 9] =
        ["veth", "docker", "br-", "virbr", "tun", "tap", "wg", "ppp", "vnet"];

    pub struct NetDiskMonitor {
        // Sama seperti Windows: simpan counter TERAKHIR per interface (key:
        // nama interface, mis. "enp3s0", "wlan0"), ambil delta TERBESAR
        // (bukan jumlah semua interface) tiap sample — lihat komentar
        // panjang di versi Windows soal alasannya.
        net_prev: HashMap<String, (u64, u64)>,
        prev_disk_read: u64,
        prev_disk_write: u64,
        prev_time: Instant,
        primed: bool,
    }

    impl NetDiskMonitor {
        pub fn new() -> Self {
            Self {
                net_prev: HashMap::new(),
                prev_disk_read: 0,
                prev_disk_write: 0,
                prev_time: Instant::now(),
                primed: false,
            }
        }

        /// Kembalikan `(net_down_kb_s, net_up_kb_s, disk_read_mb_s, disk_write_mb_s)`.
        pub fn sample(&mut self) -> (f64, f64, f64, f64) {
            let net_rows = read_network_rows();
            let (disk_read, disk_write) = read_disk_totals();

            let now = Instant::now();
            let elapsed = now.duration_since(self.prev_time).as_secs_f64().max(0.001);

            let mut best_down = 0u64;
            let mut best_up = 0u64;
            if self.primed {
                for (iface, in_bytes, out_bytes) in &net_rows {
                    if let Some(&(prev_in, prev_out)) = self.net_prev.get(iface) {
                        let d_in = in_bytes.saturating_sub(prev_in);
                        let d_out = out_bytes.saturating_sub(prev_out);
                        if d_in + d_out > best_down + best_up {
                            best_down = d_in;
                            best_up = d_out;
                        }
                    }
                }
            }

            self.net_prev = net_rows
                .into_iter()
                .map(|(name, i, o)| (name, (i, o)))
                .collect();

            let result = if !self.primed {
                (0.0, 0.0, 0.0, 0.0)
            } else {
                (
                    best_down as f64 / elapsed / 1024.0,
                    best_up as f64 / elapsed / 1024.0,
                    disk_read.saturating_sub(self.prev_disk_read) as f64 / elapsed / 1_048_576.0,
                    disk_write.saturating_sub(self.prev_disk_write) as f64
                        / elapsed
                        / 1_048_576.0,
                )
            };

            self.prev_disk_read = disk_read;
            self.prev_disk_write = disk_write;
            self.prev_time = now;
            self.primed = true;

            result
        }
    }

    /// `(nama_interface, rx_bytes, tx_bytes)` dari `/proc/net/dev`, sejak
    /// boot — dibandingkan dua kali panggilan PER INTERFACE untuk throughput.
    fn read_network_rows() -> Vec<(String, u64, u64)> {
        let Ok(content) = fs::read_to_string("/proc/net/dev") else {
            return Vec::new();
        };

        let mut out = Vec::new();
        // 2 baris pertama header ("Inter-|   Receive ..." dan "face |bytes ...").
        for line in content.lines().skip(2) {
            let Some((iface, rest)) = line.split_once(':') else { continue };
            let iface = iface.trim();
            if iface.is_empty() || iface == "lo" {
                continue;
            }
            let lname = iface.to_ascii_lowercase();
            if VIRTUAL_IF_PREFIXES.iter().any(|p| lname.starts_with(p)) {
                continue;
            }

            let fields: Vec<&str> = rest.split_whitespace().collect();
            // Kolom (0-based) sesuai format /proc/net/dev:
            // 0=rx_bytes ... 8=tx_bytes.
            if fields.len() < 9 {
                continue;
            }
            let rx_bytes: u64 = fields[0].parse().unwrap_or(0);
            let tx_bytes: u64 = fields[8].parse().unwrap_or(0);
            out.push((iface.to_string(), rx_bytes, tx_bytes));
        }
        out
    }

    /// Cek apakah nama device di `/proc/diskstats` adalah DISK UTUH (bukan
    /// partisi) — supaya tidak dihitung dobel (mis. "sda" DAN "sda1" sama-
    /// sama dijumlah, padahal sektor "sda1" sudah termasuk di dalam "sda").
    fn is_whole_disk(name: &str) -> bool {
        if name.starts_with("loop")
            || name.starts_with("dm-")
            || name.starts_with("md")
            || name.starts_with("zram")
            || name.starts_with("sr")
            || name.starts_with("fd")
        {
            return false;
        }
        if let Some(rest) = name.strip_prefix("nvme") {
            // "0n1" (whole disk) vs "0n1p1" (partisi)
            return !rest.contains('p');
        }
        if let Some(rest) = name.strip_prefix("mmcblk") {
            // "0" (whole disk) vs "0p1" (partisi)
            return !rest.contains('p');
        }
        for prefix in ["sd", "vd", "xvd", "hd"] {
            if let Some(rest) = name.strip_prefix(prefix) {
                // "a" (whole disk) vs "a1" (partisi)
                return !rest.chars().any(|c| c.is_ascii_digit());
            }
        }
        false
    }

    /// Total byte read/write (sektor × 512) dari semua disk UTUH (bukan
    /// partisi individual) di `/proc/diskstats`, sejak boot.
    fn read_disk_totals() -> (u64, u64) {
        const SECTOR_BYTES: u64 = 512;
        let Ok(content) = fs::read_to_string("/proc/diskstats") else {
            return (0, 0);
        };

        let mut total_read_sectors = 0u64;
        let mut total_write_sectors = 0u64;
        for line in content.lines() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            // Field (0-based): 2=nama device, 5=sektor terbaca, 9=sektor tertulis.
            if fields.len() < 10 {
                continue;
            }
            let name = fields[2];
            if !is_whole_disk(name) {
                continue;
            }
            let read_sectors: u64 = fields[5].parse().unwrap_or(0);
            let write_sectors: u64 = fields[9].parse().unwrap_or(0);
            total_read_sectors = total_read_sectors.saturating_add(read_sectors);
            total_write_sectors = total_write_sectors.saturating_add(write_sectors);
        }

        (total_read_sectors * SECTOR_BYTES, total_write_sectors * SECTOR_BYTES)
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod imp {
    pub struct NetDiskMonitor;

    impl NetDiskMonitor {
        pub fn new() -> Self {
            Self
        }

        pub fn sample(&mut self) -> (f64, f64, f64, f64) {
            (0.0, 0.0, 0.0, 0.0)
        }
    }
}

pub use imp::NetDiskMonitor;
