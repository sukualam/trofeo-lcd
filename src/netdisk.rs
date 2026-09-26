//! Network throughput dan disk IO.
//!
//! - **Windows**: `GetIfTable2` (IP Helper API) untuk network,
//!   `IOCTL_DISK_PERFORMANCE` dikirim langsung ke tiap `\\.\PhysicalDriveN`
//!   untuk disk — keduanya WinAPI native, tanpa PDH/library eksternal.
//! - **Linux**: `/proc/net/dev` untuk network, `/proc/diskstats` untuk disk
//!   — dua file teks standar kernel, selalu ada di semua distro, tidak
//!   butuh privilese root untuk dibaca.
//! - **macOS**: `netstat -ib` untuk network, `ioreg` untuk disk. Dua perintah
//!   ini hanya ~10-20 ms, tapi tetap dipanggil dari thread background dengan
//!   interval 1 detik — bukan langsung di loop render yang jalan 2-15 FPS,
//!   supaya biaya fork/exec tidak ikut dihitung di frame time. `sample()`
//!   hanya membaca hasil yang sudah dihitung thread tersebut.

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

#[cfg(target_os = "macos")]
mod imp {
    use std::collections::HashMap;
    use std::process::Command;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    /// Jeda antar refresh counter mentah. `netstat` + `ioreg` together ~35 ms
    /// per putaran; 1 detik cukup halus untuk EQ bar dan jauh lebih murah
    /// daripada spawn subprocess di loop render.
    const REFRESH_INTERVAL: Duration = Duration::from_millis(1000);

    /// Prefix interface yang dibuang supaya heuristik "delta terbesar" tidak
    /// salah pilih adapter virtual. Tujuannya sama dengan
    /// `VIRTUAL_IF_PREFIXES` di jalur Linux: `utun*` (VPN), `awdl`/`llw`
    /// (AirDrop & WiFi direct-link), `bridge*` (VM), `gif`/`stf` (tunnel),
    /// `anpi` (Apple Network Private Interface) sering bukan loopback tapi
    /// tetap bukan koneksi internet asli.
    const VIRTUAL_IF_PREFIXES: [&str; 12] = [
        "lo", "gif", "stf", "utun", "awdl", "llw", "anpi", "bridge", "XHC", "vmnet", "vlan",
        "ap",
    ];

    /// Throughput terbaru, dihitung oleh thread background. `f64` karena
    /// sudah dalam satuan per-detik (KB/s dan MB/s), jadi `sample()` di loop
    /// render tinggal mengcopinya tanpa membagi lagi.
    #[derive(Clone, Copy, Default)]
    struct Rate {
        net_down_kb: f64,
        net_up_kb: f64,
        disk_read_mb: f64,
        disk_write_mb: f64,
    }

    pub struct NetDiskMonitor {
        rate: Arc<Mutex<Rate>>,
    }

    impl NetDiskMonitor {
        pub fn new() -> Self {
            let rate: Arc<Mutex<Rate>> = Arc::new(Mutex::new(Rate::default()));
            let bg = Arc::clone(&rate);
            // Kalau thread gagal spawn, `rate` tetap default (0.0) — program
            // jalan normal dengan angka N/A, sama seperti sensor yang tidak
            // ditemukan. Tidak layak menghentikan program hanya karena ini.
            let _ = std::thread::Builder::new()
                .name("netdisk-macos".into())
                .spawn(move || run(bg));
            Self { rate }
        }

        /// Kembalikan `(net_down_kb_s, net_up_kb_s, disk_read_mb_s, disk_write_mb_s)`.
        pub fn sample(&mut self) -> (f64, f64, f64, f64) {
            let r = self.rate.lock().expect("netdisk mutex poisoned");
            (r.net_down_kb, r.net_up_kb, r.disk_read_mb, r.disk_write_mb)
        }
    }

    /// Loop thread background: baca counter mentah, simpan turunan ke
    /// `shared`, ulangi. `elapsed` dihitung dari `Instant` lokal (bukan
    /// counter kernel) supaya tidak perlu takut clock melompat.
    fn run(shared: Arc<Mutex<Rate>>) {
        let mut net_prev: HashMap<String, (u64, u64)> = HashMap::new();
        let mut disk_prev: (u64, u64) = (0, 0);
        let mut prev_time = Instant::now();
        let mut primed = false;

        loop {
            std::thread::sleep(REFRESH_INTERVAL);

            let rows = read_network_rows();
            let disk = read_disk_totals();

            let now = Instant::now();
            let elapsed = now.duration_since(prev_time).as_secs_f64().max(0.001);

            let mut next = Rate::default();
            if primed {
                // Sama seperti Windows/Linux: ambil SATU interface dengan delta
                // TERBESAR, bukan jumlah semua interface — lihat komentar
                // panjang di `net_prev` versi Windows soal alasannya.
                let mut best_down = 0u64;
                let mut best_up = 0u64;
                for (iface, in_bytes, out_bytes) in &rows {
                    if let Some(&(prev_in, prev_out)) = net_prev.get(iface) {
                        let d_in = in_bytes.saturating_sub(prev_in);
                        let d_out = out_bytes.saturating_sub(prev_out);
                        if d_in + d_out > best_down + best_up {
                            best_down = d_in;
                            best_up = d_out;
                        }
                    }
                }
                next.net_down_kb = best_down as f64 / elapsed / 1024.0;
                next.net_up_kb = best_up as f64 / elapsed / 1024.0;
                next.disk_read_mb =
                    disk.0.saturating_sub(disk_prev.0) as f64 / elapsed / 1_048_576.0;
                next.disk_write_mb =
                    disk.1.saturating_sub(disk_prev.1) as f64 / elapsed / 1_048_576.0;
            }

            *shared.lock().expect("netdisk mutex poisoned") = next;

            net_prev = rows
                .into_iter()
                .map(|(name, i, o)| (name, (i, o)))
                .collect();
            disk_prev = disk;
            prev_time = now;
            primed = true;
        }
    }

    /// `(nama_interface, rx_bytes, tx_bytes)` dari `netstat -ib`, sejak boot —
    /// dibandingkan dua kali panggilan PER INTERFACE untuk throughput.
    ///
    /// Hanya baris level-link (`Network` = `<Link#N>`) yang diambil. Baris
    /// per-IP di bawah interface yang sama (mis. `en0` punya baris untuk
    /// 192.168.1.x DAN untuk 2404:c0:ba0::) mengulang counter yang sama; kalau
    /// keduanya ikut dihitung, traffic jadi dobel.
    fn read_network_rows() -> Vec<(String, u64, u64)> {
        let Ok(out) = Command::new("netstat").args(["-ib"]).output() else {
            return Vec::new();
        };
        let text = String::from_utf8_lossy(&out.stdout);

        let mut rows = Vec::new();
        // Baris pertama adalah header ("Name  Mtu  Network  Address  Ipkts ...").
        for line in text.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            // Kolom (0-based) sesuai format `netstat -ib`:
            // 0=Name, 1=Mtu, 2=Network, 3=Address, 4=Ipkts, 5=Ierrs, 6=Ibytes,
            // 7=Opkts, 8=Oerrs, 9=Obytes, 10=Coll.
            if f.len() < 10 {
                continue;
            }
            let (name, network) = (f[0], f[2]);
            if !network.starts_with("<Link#") {
                continue;
            }
            // Tanda `*` = interface sedang nonaktif; skip supaya tidak
            // ikut jadi kandidat "delta terbesar".
            if name.ends_with('*') {
                continue;
            }
            // Mtu 0 = interface virtual yang tidak pernah dipakai (mis. XHC*).
            if f[1].parse::<u64>().ok() == Some(0) {
                continue;
            }
            if VIRTUAL_IF_PREFIXES.iter().any(|p| name.starts_with(p)) {
                continue;
            }
            let (Ok(rx), Ok(tx)) = (f[6].parse::<u64>(), f[9].parse::<u64>()) else {
                continue;
            };
            rows.push((name.to_string(), rx, tx));
        }
        rows
    }

    /// Total byte read/write dari semua disk FISIK, sejak boot.
    ///
    /// Membaca `Statistics` di tiap `IOBlockStorageDriver` lalu MENJUMLAHKAN
    /// hanya yang induknya storage device sungguhan (`IOAHCIBlockStorageDevice`
    /// / `IONVMeBlockStorageDevice`). Disk image (DMG, ramdisk) juga punya
    /// `IOBlockStorageDriver` sendiri, dan IO-nya menumpuk di atas disk fisik
    /// — jadi kalau ikut dihitung, throughput disk terlempar jauh melebihi
    /// batas disk sebenarnya.
    ///
    /// Pindai output `ioreg` berurutan dari atas: pada tree IORegistry, induk
    /// selalu tercetak SEBELUM anak, jadi cukup mengingat penanda terakhir yang
    /// terlihat sebelum baris `Statistics` stumbled.
    fn read_disk_totals() -> (u64, u64) {
        let Ok(out) = Command::new("ioreg")
            .args(["-c", "IOBlockStorageDriver", "-w", "0"])
            .output()
        else {
            return (0, 0);
        };
        let text = String::from_utf8_lossy(&out.stdout);

        let mut physical = false;
        let mut total_read = 0u64;
        let mut total_write = 0u64;
        for line in text.lines() {
            if line.contains("AppleDiskImageBlockStorageDevice") {
                // Anak disk image TIDAK ikut dihitung, dan penandanya harus
                // menimpa status "physical" supaya disk image berikutnya yang
                // muncul tidak salah warisan flag.
                physical = false;
                continue;
            }
            if line.contains("IOAHCIBlockStorageDevice")
                || line.contains("IONVMeBlockStorageDevice")
            {
                physical = true;
                continue;
            }
            if !line.contains("\"Statistics\" = {") || !physical {
                continue;
            }
            if let Some(v) = stat_field(line, "Bytes (Read)") {
                total_read = total_read.saturating_add(v);
            }
            if let Some(v) = stat_field(line, "Bytes (Write)") {
                total_write = total_write.saturating_add(v);
            }
        }

        (total_read, total_write)
    }

    /// Ambil nilai numerik satu key dari dictionary `Statistics`.
    ///
    /// Formatnya `"Bytes (Read)"=8077312` (tanpa spasi sebelum `=`), key
    /// berikutnya dipisah koma. Pencocokan dilakukan per-key dengan batas
    /// `"` di kiri dan `=` di kanan supaya `"Bytes (Read)"` tidak ikut cocok
    /// dengan key lain yang namanya diawali sama.
    fn stat_field<'a>(line: &'a str, key: &str) -> Option<u64> {
        let pat = format!("\"{key}\"=");
        let start = line.find(&pat)? + pat.len();
        let rest = &line[start..];
        let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        rest[..end].parse().ok()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// `Bytes (Read)` dan `Bytes (Write)` harus terpisah benar. Kunci lain di
        /// dictionary yang sama berakhiran "Write"/"Read" (`Operations (Write)`,
        /// `Total Time (Read)`, `Errors (Read)`, ...) dan nilai `Bytes (Write)`
        /// selalu 0 di macOS — kalau pencocokan longgar, hasil read/write bisa
        /// tertukar atau nol.
        #[test]
        fn stat_field_reads_exact_key() {
            let line = r#""Statistics" = {"Operations (Write)"=714981,"Bytes (Read)"=28099985920,"Total Time (Read)"=432,"Errors (Read)"=0,"Bytes (Write)"=34910511104,"Operations (Read)"=730589}"#;
            assert_eq!(stat_field(line, "Bytes (Read)"), Some(28_099_985_920));
            assert_eq!(stat_field(line, "Bytes (Write)"), Some(34_910_511_104));
            // Key yang tidak ada -> None, bukan 0.
            assert_eq!(stat_field(line, "Bytes (Nope)"), None);
        }

        /// Nilai diakhiri koma atau `}`, dan boleh diulang/di-share antar disk.
        #[test]
        fn stat_field_handles_boundaries() {
            let line = r#""Statistics" = {"Bytes (Read)"=0,"Bytes (Write)"=123}"#;
            assert_eq!(stat_field(line, "Bytes (Read)"), Some(0));
            assert_eq!(stat_field(line, "Bytes (Write)"), Some(123));
        }

        /// `netstat -ib` di macOS: hanya baris `<Link#N>` yang dipakai, dan
        /// interface virtual dibuang. Cek di mesin ini: `en0` (Ethernet fisik)
        /// harus lolos, `utun*` (VPN) dan `lo0` tidak.
        #[test]
        fn network_rows_excludes_virtual_interfaces() {
            let rows = read_network_rows();
            let names: Vec<&str> = rows.iter().map(|(n, _, _)| n.as_str()).collect();
            assert!(
                !names.is_empty(),
                "tidak ada interface fisik — parser rusak atau tidak ada jaringan"
            );
            assert!(
                !names.iter().any(|n| n.starts_with("utun")),
                "utun (VPN) ikut terhitung: {names:?}"
            );
            assert!(!names.iter().any(|n| n.starts_with("lo")), "lo0 ikut: {names:?}");
            assert!(
                !names.iter().any(|n| n.ends_with('*')),
                "interface nonaktif ikut: {names:?}"
            );
        }

        /// Baris per-IP Interface mengulang counter yang sama; kalau ikut
        /// terhitung, `en0` akan muncul 3x dan traffic jadi dobel.
        #[test]
        fn network_rows_are_link_level_only() {
            let rows = read_network_rows();
            let mut seen = std::collections::HashSet::new();
            for (name, _, _) in &rows {
                assert!(seen.insert(name.clone()), "interface duplikat: {name}");
            }
        }

        /// Disk fisik harus terdeteksi dan counter-nya bukan nol. Kalau 0, berarti
        /// tidak ada yang ter-sum (mis. filter `physical` tidak pernah true).
        #[test]
        fn disk_totals_are_nonzero() {
            let (read, write) = read_disk_totals();
            assert!(read > 0, "total disk read 0 — counter tidak terbaca");
            assert!(write > 0, "total disk write 0 — counter tidak terbaca");
        }

        /// Disk image harus TIDAK ikut dihitung. Disimulasikan dengan urutan
        /// baris seperti yang dihasilkan `ioreg`: penanda disk image overriding
        /// status physical, dan `Statistics`-nya tidak boleh masuk.
        #[test]
        fn disk_image_stats_are_skipped() {
            let fake = concat!(
                "| +-o IOAHCIBlockStorageDevice\n",
                "|   +-o IOBlockStorageDriver\n",
                "      \"Statistics\" = {\"Bytes (Read)\"=100,\"Bytes (Write)\"=10}\n",
                "| +-o AppleDiskImageBlockStorageDeviceOutKernel\n",
                "|   +-o IOBlockStorageDriver\n",
                "      \"Statistics\" = {\"Bytes (Read)\"=999999,\"Bytes (Write)\"=888888}\n",
            );
            // Fungsi aslinya menjalankan `ioreg` sungguhan, jadi di sini kita
            // hanya memverifikasi logika flag lewat helper yang sama.
            let mut physical = false;
            let mut total = 0u64;
            for line in fake.lines() {
                if line.contains("AppleDiskImageBlockStorageDevice") {
                    physical = false;
                    continue;
                }
                if line.contains("IOAHCIBlockStorageDevice") {
                    physical = true;
                    continue;
                }
                if !line.contains("\"Statistics\" = {") || !physical {
                    continue;
                }
                total += stat_field(line, "Bytes (Read)").unwrap_or(0);
            }
            assert_eq!(total, 100, "disk image ikut terhitung");
        }

        /// Uji end-to-end: pastikan `sample()` benar-benar melaporkan angka
        /// BUKAN 0 saat ada traffic sungguhan. Di-`ignore` supaya tidak ikut
        /// jalan di `cargo test` biasa (butuh jaringan + IO disk).
        ///
        /// Jalankan: `cargo test --release netdisk -- --ignored --nocapture`
        #[test]
        #[ignore]
        fn reports_live_traffic() {
            let mut mon = NetDiskMonitor::new();
            // Tunggu thread background menyelesaikan satu putaran + meng-prime
            // delta (butuh 2 putaran ~2 detik sebelum angka muncul).
            std::thread::sleep(REFRESH_INTERVAL * 2);

            // Traffic disk: tulis file besar, lalu hapus biar tidak memenuhi disk.
            let tmp = std::env::temp_dir().join("trofeo_netdisk_test.bin");
            let _ = std::fs::remove_file(&tmp);
            let blob = vec![0u8; 192 * 1024 * 1024];
            std::fs::write(&tmp, &blob).expect("tulis file uji");
            let written = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
            let _ = std::fs::remove_file(&tmp);

            // Traffic network: unduh sesuatu supaya counter di interface fisik
            // naik. Kalau tidak ada jaringan, bagian ini dilewati.
            let _ = std::process::Command::new("curl")
                .args([
                    "-s",
                    "-o",
                    "/dev/null",
                    "https://speed.cloudflare.com/__down?bytes=40000000",
                ])
                .status();

            std::thread::sleep(REFRESH_INTERVAL * 2);
            let (down, up, read, w) = mon.sample();
            println!("net_down={down:.1} KB/s  net_up={up:.1} KB/s");
            println!("disk_read={read:.2} MB/s  disk_write={w:.2} MB/s (tulis {written} B)");

            assert!(
                read > 0.0 || w > 0.0,
                "disk traffic {written} B tapi sample() melaporkan 0 — thread background tidak jalan"
            );
        }
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
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
