# trofeo_lcd

**Bahasa Indonesia** · [English](./README.en.md)

Audio visualizer (bar EQ) + info sistem (CPU, RAM, jam, tanggal) untuk
**Thermalright Trofeo Vision 9.16 LCD** (USB VID:PID `0416:5408`, protokol
"LY" — chunked USB bulk; varian LY1 `0416:5409` juga didukung).

Driver USB-nya ditulis ulang byte-per-byte dari implementasi Python asli
proyek [thermalright-trcc-linux](https://github.com/Lexonight1/thermalright-trcc-linux)
(`LyLcd` di `src/trcc/adapters/.../ly_lcd.py`).

## Struktur

- `src/lib.rs` — driver inti: deteksi device, handshake, chunking, JPEG
  encode, plus `Framebuffer` (gambar kotak & teks bitmap 5x7).
- `src/font.rs` — font bitmap kecil buatan sendiri untuk `Framebuffer::draw_text`.
- `src/audio.rs` — sumber data audio untuk bar EQ:
  - **Windows**: capture *loopback* WASAPI (audio yang sedang diputar
    speaker/headphone — BUKAN mikrofon).
  - **Linux**: capture *loopback* PulseAudio/PipeWire (device
    `@DEFAULT_MONITOR@`, sama seperti dipakai `parec`).
  - **OS lain**: sumber sintetis (nada uji), supaya kode tetap bisa
    di-compile & dites.
- `src/media.rs` — volume master + judul lagu/media ("NOW PLAYING"):
  Windows lewat COM/WinRT, Linux lewat `pactl`/`playerctl`.
- `src/foreground.rs` — deteksi nama program (.exe) yang sedang jadi window
  aktif, dipakai mode game: Windows lewat WinAPI, Linux lewat X11 (EWMH).
- `src/cpu_freq.rs` — frekuensi CPU **real-time** (ikut beban/boost):
  Windows lewat PDH `% Processor Performance` × base clock registry, Linux
  lewat rata-rata `scaling_cur_freq` sysfs.
- `src/deepcool/` — integrasi **DeepCool Digital**: kirim suhu/usage/power/
  frekuensi CPU ke display cooler/casing DeepCool via USB HID (driver
  di-port dari proyek deepcool-digital).
- `src/main.rs` — program utama: loop capture audio → FFT → bar EQ +
  info sistem → kirim ke layar. Juga men-dispatch data CPU ke thread
  integrasi DeepCool di background.

## Linux — yang perlu dipasang

Semua sensor (CPU/GPU/net/disk) sudah jalan lewat sysfs kernel biasa (tidak
butuh driver tambahan). Yang butuh paket eksternal cuma:

- **Build**: `pkg-config` + header PulseAudio (`libpulse-dev` di
  Debian/Ubuntu, `libpulse-devel` di Fedora, sudah termasuk di paket
  `libpulse` di Arch) — dibutuhkan `libpulse-binding` untuk compile.
- **Runtime — audio EQ & volume**: PulseAudio ATAU PipeWire dengan
  `pipewire-pulse` aktif (default di distro modern: Ubuntu 22.10+, Fedora,
  dst). Cek dengan `pactl info`.
- **Runtime — NOW PLAYING**: `playerctl` (`sudo apt install playerctl` /
  `sudo pacman -S playerctl` / `sudo dnf install playerctl`). Tanpa ini,
  judul lagu selalu kosong (bukan crash) — dan volume/GPU/CPU/dst tetap
  jalan normal.
- **Runtime — akses USB tanpa root**: pasang `99-trofeo-lcd.rules` (lihat
  komentar di file itu untuk cara pasang) supaya tidak perlu `sudo` tiap
  jalankan program.
- **Runtime — power draw CPU (Watt)**: pilihan sumber tergantung generasi CPU.
  Program mencoba otomatis dengan urutan ini (tidak perlu konfigurasi):
  1. **RAPL powercap** (`/sys/class/powercap/intel-rapl:N/energy_uj`, zona
     "package") — **sumber UTAMA untuk CPU Zen 4+ (Ryzen 7000/8000/9000,
     termasuk 7500F)**. Driver kernel mainline `intel_rapl_msr` (dari config
     `CONFIG_INTEL_RAPL`, otomatis aktif di semua distro modern) membaca MSR
     RAPL AMD (`MSR_PKG_ENERGY_STAT` / `0xC001_029B`) — register yang sama
     dengan jalur Windows — jadi angkanya setara. Coba di CPU Zen 1-3 juga
     boleh, tapi di sana utamakan zenpower (poin 3) yang lebih akurat untuk
     generasi itu.
  2. **`amd_energy`** (hwmon RAPL) — **sudah dihapus total dari kernel
     Linux mainstream sejak versi 5.13** (2021); urutan ini khusus untuk
     kernel custom lama yang masih membawanya.
  3. **Driver komunitas `zenpower3`** — **HANYA untuk Zen 1-3** (telemetri
     SVI2). Di Zen 4+ yang sudah pindah ke SVI3 driver ini tidak bekerja,
     tapi kalau CPU kamu Zen 1-3 ini pilihan terbaik (AUR:
     `yay -S zenpower3-dkms-git` lalu restart).

  Penting untuk Zen 4+: kalau langkah 1 gagal, biasanya bukan karena
  driver-nya tidak ada (RAPL sudah built-in), melainkan karena file
  `energy_uj` **root-only secara default** sejak mitigasi kerentanan
  Platypus. Cek dulu tanpa sudo:
  `ls /sys/class/powercap/*/energy_uj` — kalau "Permission denied", pasang
  udev rule ini supaya bisa dibaca user biasa (langkah 1 mungkin butuh
  `sudo modprobe intel_rapl_msr`; kalau sudah otomatis ter-load, `modprobe`
  akan bilang "already loaded", itu normal):
  ```
  sudo tee /etc/udev/rules.d/99-rapl.rules <<'EOF'
  SUBSYSTEM=="powercap", ACTION=="add", RUN+="/bin/chmod 0444 /sys%p/energy_uj"
  EOF
  sudo udevadm control --reload-rules && sudo udevadm trigger --subsystem-match=powercap --action=add
  ```
  Tanpa sumber mana pun, power CPU akan N/A (bukan crash) — semua sensor lain
  tetap jalan normal.
- **Mode game (nama exe di NOW PLAYING)**: butuh sesi X11 (native atau
  XWayland) — di Wayland murni tanpa XWayland, bagian ini saja yang jadi
  N/A (fitur lain tidak terpengaruh). Lihat catatan di `src/foreground.rs`.

## Yang ditampilkan

- Bar EQ (48 bar, default) dari spektrum audio 40Hz–16kHz, skala logaritmik,
  warna hijau→kuning→merah sesuai level, dengan auto-gain (otomatis
  menyesuaikan ke volume yang sedang diputar) dan smoothing attack/decay.
- Baris info di atas: `CPU xx%   MEM used/total MB   UP [ND]HH:MM:SS   HH:MM:SS   YYYY-MM-DD`.
  Uptime (`UP ...`) memakai `System::uptime()` dari `sysinfo` — panggilan
  statis yang cuma baca counter OS, bukan snapshot CPU/RAM yang mahal, jadi
  dihitung tiap frame tanpa ikut aturan `SYSINFO_REFRESH_INTERVAL`.
- Frekuensi CPU real-time (ikut beban/boost) di bar info, dibaca via PDH
  (`% Processor Performance` × base clock) di Windows atau sysfs cpufreq di
  Linux — sama dengan angka "Speed" di Task Manager.

## Integrasi DeepCool Digital

Trofeo-lcd bisa sekaligus menggerakkan display **cooler/casing DeepCool**
(mis. AG300/400/500/620 DIGITAL, AK/K/Pro, LS, LQ, LD, LP, CH/CH-Gen2,
CH510) yang tersambung via USB HID — jadi **tidak perlu menjalankan program
DeepCool terpisah**. Suhu/power/frekuensi yang ditampilkan memakai sensor
yang sama dengan baris info (satu instance sensor, dibagi ke thread
DeepCool) dan dikirim ke display setiap interval tertentu.

Aktif secara default. Opsi:

| Argumen | Fungsi | Default |
|---|---|---|
| `--no-deepcool` | Matikan integrasi DeepCool sepenuhnya | aktif |
| `--deepcool-update-ms <N>` | Interval kirim data ke display, ms (di-clamp 100–2000) | `1000` |

Device dideteksi otomatis (auto-detect setiap beberapa detik kalau belum
ketemu). Kalau sensor suhu CPU tidak tersedia (mis. driver PawnIO belum
terpasang di Windows), display akan menampilkan 0 — data lain tetap jalan.

## Mode Second Monitor (`trofeo_screen`)

Selain visualizer, ada binary terpisah **`trofeo_screen`** yang mengubah LCD
Trofeo menjadi **monitor sekunder sungguhan**: tampilan desktop ditangkap
real-time via DXGI Desktop Duplication dan di-stream ke LCD. Jendela aplikasi
(Spotify, browser, dsb.) bisa digeser langsung ke layar Trofeo.

Membutuhkan *Virtual Display Driver* (VDD) dengan resolusi **1920×462** —
panduan lengkap (pemasangan VDD, opsi `--fps`/`--quality`/`--rotate`, dan
autorun saat login) ada di **[GUIDE_SECOND_MONITOR.md](./GUIDE_SECOND_MONITOR.md)**.

> ⚠️ `trofeo_lcd` dan `trofeo_screen` memakai LCD yang sama — jalankan
> **salah satu**, jangan keduanya bersamaan.

## Optimasi CPU

Beberapa titik yang tadinya boros CPU sudah dirapikan (semua sudah lolos
`cargo test`):

- **Rentang bin FFT per bar** dulu dihitung ulang tiap frame di `compute_bars`
  (termasuk 2 panggilan `powf()` per bar = 96x/frame untuk 48 bar). Sekarang
  dihitung sekali di awal (`precompute_bar_bins`), dipakai ulang tiap frame.
- **`Framebuffer`** dulu dialokasikan baru (~2.66MB) tiap frame lewat
  `Framebuffer::new()` di dalam loop utama. Sekarang dialokasikan sekali di
  luar loop, tiap frame cuma di-`clear()`.
- **`Framebuffer::clear()` & `fill_rect()`** (dipakai untuk bar EQ & teks) dulu
  menulis piksel satu-satu lewat `set_pixel` dengan bounds-check per piksel.
  Sekarang pakai teknik *doubling* (`copy_within`, memcpy blok besar yang
  digandakan tiap langkah) — O(log n) operasi copy, bukan O(n) panggilan
  fungsi per piksel, tanpa alokasi heap tambahan.
- **Capture audio WASAPI** (`src/audio.rs`, Windows) dulu mengunci mutex ring
  buffer per SATU sample audio (berpotensi puluhan ribu kali/detik). Sekarang
  sample dikumpulkan dulu per polling ke buffer lokal, baru mutex dikunci
  sekali untuk seluruh batch itu.

Kalau CPU usage masih terasa tinggi setelah ini di hardware nyata, dua tuas
paling ampuh berikutnya adalah menurunkan `--active-fps` (mis. 15 -> 10) —
lihat bagian "FPS adaptif" di atas — dan/atau `FFT_SIZE` di `main.rs`.
JPEG-encode + kirim USB tiap frame di resolusi 1920x462 tetap jadi biaya CPU
terbesar yang inheren ke fitur ini, dan itu berskala langsung dengan berapa
kali per detik encode itu dijalankan — makanya FPS idle (2 default) dibuat
jauh lebih rendah daripada FPS aktif.

## Tuning

Konstanta di bagian atas `src/main.rs`:

| Konstanta | Fungsi |
|---|---|
| `NUM_BARS` | Jumlah bar EQ |
| `FFT_SIZE` | Ukuran window FFT (resolusi vs latensi) |
| `FREQ_MIN` / `FREQ_MAX` | Rentang frekuensi yang dipetakan ke bar |
| `SYSINFO_REFRESH_INTERVAL` | Seberapa sering CPU/RAM di-refresh |

## FPS adaptif (argumen command-line)

FPS pengiriman ke layar **adaptif**: turun ke FPS idle saat tidak ada suara
terdeteksi (hemat CPU besar-besaran — JPEG-encode 1920x462 + kirim USB tiap
frame adalah biaya CPU terbesar di program ini), naik ke FPS aktif lagi
begitu ada suara. Diatur lewat argumen, bukan konstanta, supaya bisa dicoba-coba
tanpa rebuild:

```bash
./target/release/trofeo_lcd --idle-fps 2 --active-fps 15 \
    --silence-threshold 0.005 --silence-timeout-ms 800
```

| Argumen | Fungsi | Default |
|---|---|---|
| `--idle-fps <N>` | FPS saat diam | `2` |
| `--active-fps <N>` | FPS saat ada suara | `15` |
| `--silence-threshold <N>` | Ambang puncak amplitude (0.0-1.0) sample mentah untuk dianggap "diam" — naikkan kalau masih dianggap "ada suara" padahal cuma noise kecil/DC-offset, turunkan kalau suara pelan tidak terdeteksi | `0.005` |
| `--silence-timeout-ms <N>` | Berapa lama harus diam berturut-turut sebelum turun ke `idle-fps` (menaikkan FPS lagi SELALU langsung, tanpa delay ini) | `800` |

Jalankan dengan `--help` untuk melihat opsi ini langsung dari program.

Catatan: deteksi diam cuma dicek sekali per frame (pakai window audio yang
sama dengan yang dipakai untuk bar EQ), jadi saat lagi di `idle-fps` (mis. 2
FPS), butuh sampai ~1/idle-fps detik untuk "sadar" ada suara lagi dan naik
balik ke `active-fps` — biasanya tidak kerasa untuk visualizer semacam ini,
tapi kalau perlu lebih responsif, naikkan `--idle-fps` sedikit (mis. 4-5)
sebagai kompromi.

## Sinkron warna dengan OpenRGB

Warna bar EQ bisa disinkronkan (polling) dengan warna sebuah device yang
sudah kamu atur di **OpenRGB**:

```bash
./target/release/trofeo_lcd --openrgb-device "RAM" --openrgb-poll-ms 300
```

| Argumen | Fungsi | Default |
|---|---|---|
| `--openrgb-device <NAMA>` | Cocokkan sebagian nama device OpenRGB (case-insensitive) | (nonaktif) |
| `--openrgb-poll-ms <N>` | Interval baca ulang warna dari OpenRGB, ms | `300` |

Syarat: OpenRGB harus sedang berjalan dengan **SDK Server** aktif
(Settings → SDK Server → Enable, default port `6742`). Selama trofeo-lcd
belum berhasil connect atau device dengan nama cocok belum ketemu, warna
bar EQ jatuh ke fallback `--color` (atau gradien default kalau `--color`
tidak diberikan) — program tetap jalan normal, cuma warnanya belum ikut
OpenRGB.

**Ini polling satu arah** (baca snapshot warna device OpenRGB secara
berkala), **bukan** mendaftarkan trofeo-lcd sebagai device yang dikontrol
OpenRGB. Konsekuensinya:
- Cocok untuk device sumber yang warnanya **statis** di OpenRGB.
- Kalau device sumber memakai **efek animasi** (rainbow, breathing, dst),
  warna di LCD tetap ikut berubah tapi "patah-patah" mengikuti
  `--openrgb-poll-ms`, bukan semulus animasi aslinya.

## Sembunyikan jendela terminal (`--hide-console`)

Secara default program tetap tampil di terminal seperti biasa (log
langsung terlihat di layar). Kalau mau dijalankan di belakang layar (mis.
lewat shortcut atau Task Scheduler saat login, tanpa jendela hitam
nongol), tambahkan:

```bash
trofeo_lcd.exe --hide-console
```

Begitu argumen selesai diparse, jendela konsol langsung disembunyikan
(`FreeConsole`) — standar output/error tetap seperti biasa dan **tidak ada
file log yang ditulis**. Hanya berlaku di Windows; di build non-Windows
opsi ini diabaikan dengan peringatan.

## Instalasi dependensi sistem

`rusb` dipakai dengan fitur **`vendored`** — libusb di-compile langsung dari
source yang dibundel (via `cc`), jadi **tidak perlu install libusb terpisah
di OS manapun**, termasuk Windows. Yang tetap dibutuhkan hanyalah compiler C.

```bash
# Debian/Ubuntu
sudo apt install pkg-config build-essential

# Fedora
sudo dnf install pkgconf-pkg-config gcc

# Arch
sudo pacman -S pkgconf base-devel
```

### Izin USB di Linux (tanpa sudo)

```bash
sudo tee /etc/udev/rules.d/99-trofeo-lcd.rules > /dev/null << 'RULES'
SUBSYSTEM=="usb", ATTR{idVendor}=="0416", ATTR{idProduct}=="5408", MODE="0666"
SUBSYSTEM=="usb", ATTR{idVendor}=="0416", ATTR{idProduct}=="5409", MODE="0666"
RULES
sudo udevadm control --reload-rules && sudo udevadm trigger
# cabut-pasang ulang kabel USB device
```

## Menjalankan di Windows

### 1. Toolchain

- Install Rust lewat [rustup](https://rustup.rs) (target default
  `x86_64-pc-windows-msvc`).
- Karena `vendored` butuh compiler C, install **Visual Studio Build Tools**
  (workload "Desktop development with C++"). Alternatif: target
  `x86_64-pc-windows-gnu` + toolchain MinGW-w64.

### 2. Driver USB (WAJIB)

Device harus di-bind ke driver **WinUSB** dulu:

1. Colokkan Trofeo Vision LCD, jangan jalankan software TRCC resmi.
2. Download [Zadig](https://zadig.akeo.io/) (portable).
3. Options → **List All Devices**.
4. Cari device dengan ID `0416 5408`, pastikan driver target = **WinUSB**,
   klik **Replace Driver** / **Install Driver**.

> ⚠️ Ini mengganti driver device itu sendiri. Untuk kembali pakai software
> TRCC resmi nanti, kembalikan driver lewat Device Manager, atau cabut-colok
> ulang device supaya Windows pasang driver default lagi.

### 3. Build & jalankan

```powershell
cd trofeo-lcd
cargo build --release
.\target\release\trofeo_lcd.exe
```

Bar EQ akan mengikuti audio apa pun yang sedang diputar Windows (loopback
device default) — tidak perlu setting tambahan, WASAPI loopback otomatis
memakai output device default sistem.

> ℹ️ Integrasi DeepCool butuh driver PawnIO untuk membaca suhu/power CPU
> AMD di Windows (`winget install namazso.PawnIO`) — kalau belum ada, layar
> DeepCool tetap jalan tapi menampilkan 0. Program juga harus dijalankan
> sebagai Administrator untuk akses driver.

## Pakai (Linux/macOS)

```bash
cargo build --release
./target/release/trofeo_lcd
```

Di Linux/macOS bar EQ memakai sumber sintetis (bukan audio asli) — lihat
catatan di `src/audio.rs`.

## Status pengujian

- Logika inti (FFT, bucketing frekuensi ke bar, normalisasi auto-gain, gambar
  teks bitmap) diuji terpisah dengan sinyal sintetis (nada 220/880/3000 Hz) —
  puncak bar muncul tepat di frekuensi yang sesuai. Driver USB & unit test
  chunking/JPEG lolos `cargo test` (`cargo check`/`cargo build --release`
  bersih di Windows).
- Build Windows berjalan di hardware nyata: LCD Trofeo Vision 9.16
  (`0416:5408`), plus integrasi DeepCool dengan device AG Series
  (`VID_3633 PID_0008`) yang terdeteksi otomatis.

## Lisensi

Kode ini mengikuti lisensi proyek upstream yang menjadi rujukan protokolnya:
GPL-3.0-or-later.