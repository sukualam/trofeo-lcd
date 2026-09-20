# Panduan: Menggunakan Trofeo Vision 9.16 Sebagai Second Monitor

**Bahasa Indonesia** · [English](./GUIDE_SECOND_MONITOR.md)

Panduan ini menjelaskan cara mengubah layar **Thermalright Trofeo Vision 9.16
LCD** menjadi **monitor sekunder asli di Windows**, sehingga Anda bisa
menggeser jendela aplikasi (seperti Spotify, Discord, browser, atau task
manager) langsung ke layar Trofeo.

---

## 1. Cara Kerja

Layar Trofeo terhubung ke PC hanya melalui kabel USB (bukan HDMI /
DisplayPort). Oleh karena itu:

1. Kita membuat sebuah **Monitor Virtual** di Windows dengan resolusi native
   **1920×462**.
2. Program **`trofeo_screen`** menangkap tampilan monitor virtual tersebut
   secara real-time via GPU (DXGI Desktop Duplication API) dan mengirimkannya
   ke layar Trofeo melalui USB.

---

## 2. Langkah 1: Memasang Virtual Display Driver (VDD)

1. Kunjungi rilis open-source **[Virtual-Display-Driver (VDD)](https://github.com/itsmebias/virtual-display-driver/releases)**.
2. Unduh file `.zip` versi terbaru dan ekstrak foldernya (misalnya ke
   `C:\VirtualDisplayDriver`).
3. Buka file konfigurasi `vdd.xml` (atau `options.txt` tergantung versi VDD)
   menggunakan Notepad.
4. Tambahkan resolusi khusus Trofeo Vision:
   ```xml
   <resolution>
     <width>1920</width>
     <height>462</height>
     <refresh_rate>60</refresh_rate>
   </resolution>
   ```
5. Klik kanan file `install.bat` (atau jalankan perintah instalasi VDD
   sebagai Administrator).
6. Buka **Windows Settings → System → Display**:
   - Anda akan melihat monitor baru muncul (**Display 2**).
   - Atur resolusinya ke **1920 × 462**.
   - Posisikan monitor kedua di atas, bawah, atau samping monitor utama
     sesuai keinginan.

---

## 3. Langkah 2: Menjalankan `trofeo_screen`

### Cek Monitor yang Terdeteksi

```bash
cargo run --bin trofeo_screen -- --list-displays
```

Atau dari binary release (`.\target\release\trofeo_screen.exe --list-displays`):

```text
Daftar Monitor Terdeteksi:
---------------------------------------------------------------------------
INDEX  ADAPTER                  DEVICE           RESOLUSI     STATUS
---------------------------------------------------------------------------
0      AMD Radeon RX 6600       \\.\DISPLAY1     1920x1080    Aktif
1      IddCx Virtual Display    \\.\DISPLAY2     1920x462     Aktif [MATCH 1920x462]
---------------------------------------------------------------------------
```

*Program otomatis mendeteksi display yang beresolusi 1920×462!*

### Mulai Streaming ke Layar Trofeo

```bash
cargo run --release --bin trofeo_screen
```

Atau langsung jalankan binary yang sudah di-compile:

```powershell
.\target\release\trofeo_screen.exe
```

Program akan otomatis:

1. Terhubung ke layar Trofeo via USB bulk (`0416:5408`).
2. Menangkap tampilan monitor virtual secara real-time.
3. Mengirimkan pergerakan jendela dan kursor mouse ke layar Trofeo.

---

## 4. Opsi & Parameter Perintah

| Opsi | Fungsi | Default |
|---|---|---|
| `-l`, `--list-displays` | Tampilkan daftar monitor terdeteksi lalu keluar | - |
| `-d`, `--display <N>` | Tentukan index monitor secara manual jika ada beberapa | Auto (1920x462) |
| `--fps <N>` | Target frame rate maksimum saat layar bergerak | `30` |
| `--idle-fps <N>` | Kecepatan polling saat layar diam/statis (menghemat CPU) | `10` |
| `-q`, `--quality <1-100>` | Kualitas kompresi gambar JPEG | `75` |
| `-r`, `--rotate` | Putar tampilan 180° jika fisik layar dipasang terbalik | `false` |
| `--hide-console` | Sembunyikan jendela hitam terminal (bagus untuk startup) | `false` |
| `-k`, `--screenshot-key <KEY>` | Global hotkey untuk menyimpan tangkapan layar frame LCD (f1-f12, `printscreen`). File PNG lossless disimpan ke **Desktop** | nonaktif |

### Contoh Penggunaan Khusus

* **Jika layar dipasang terbalik**:
  ```powershell
  .\target\release\trofeo_screen.exe --rotate
  ```
* **Kualitas gambar lebih tajam (misal untuk teks kecil)**:
  ```powershell
  .\target\release\trofeo_screen.exe --quality 85 --fps 25
  ```
* **Dijalankan otomatis saat Windows login tanpa jendela hitam**:
  ```powershell
  .\target\release\trofeo_screen.exe --hide-console
  ```

---

## 5. Menjalankan Otomatis Saat Boot / Login Windows

1. Tekan `Win + R`, ketik `shell:startup`, lalu tekan **Enter**.
2. Klik kanan di dalam folder tersebut → **New** → **Shortcut**.
3. Arahkan target ke lokasi executable:
   ```text
   "C:\Users\USER\Documents\Default Project\trofeo-lcd\target\release\trofeo_screen.exe" --hide-console
   ```
4. Klik **Next** dan beri nama `Trofeo Screen`. Sekarang monitor sekunder
   akan otomatis aktif setiap kali Anda masuk ke Windows!

---

> ⚠️ **Penting:** `trofeo_screen` dan `trofeo_lcd` sama-sama memakai LCD yang
> sama — jalankan **salah satu**, jangan keduanya bersamaan.