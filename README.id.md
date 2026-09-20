# trofeo_lcd

**Bahasa Indonesia** · [English](./README.md)

Visualizer audio + monitor info sistem untuk layar **Thermalright Trofeo
Vision 9.16 LCD** (USB `0416:5408`, protokol "LY"). Driver ditulis ulang
byte-per-byte dari [thermalright-trcc-linux](https://github.com/Lexonight1/thermalright-trcc-linux).

## Fitur

- Bar EQ (48 bar) dari audio yang sedang diputar (WASAPI loopback di
  Windows, PulseAudio/PipeWire di Linux), warna hijau→kuning→merah.
- Info sistem: CPU %, frekuensi CPU real-time, RAM, uptime, jam & tanggal.
- Bisa sekaligus menyalakan display **DeepCool** (kirim data CPU via HID).
- **Mode second monitor** (`trofeo_screen`): LCD jadi monitor sekunder asli.
- Sinkron warna bar EQ dengan device **OpenRGB**.
- FPS adaptif: turun ke idle saat diam → hemat CPU.

## Cara pakai

```bash
cargo build --release
./target/release/trofeo_lcd          # Windows: .\target\release\trofeo_lcd.exe
```

**Windows** — untuk membaca suhu/power CPU AMD, install
[PawnIO](https://github.com/namazso/PawnIO) dan jalankan sebagai
Administrator.

**Linux** — butuh `pkg-config` + header PulseAudio saat build, dan
`playerctl` untuk judul lagu. Izin USB tanpa `sudo`: pasang `99-trofeo-lcd.rules` (lihat isi filenya).

## Opsi utama

| Opsi | Fungsi | Default |
|---|---|---|
| `--idle-fps` / `--active-fps` | FPS saat diam / ada suara | `2` / `15` |
| `--no-deepcool` | Matikan integrasi DeepCool | aktif |
| `--deepcool-update-ms` | Interval kirim data DeepCool (100–2000 ms) | `1000` |
| `--openrgb-device <NAMA>` | Sinkron warna dengan device OpenRGB | nonaktif |
| `--hide-console` | Sembunyikan jendela terminal (Windows) | nonaktif |
| `-k, --screenshot-key` | Global hotkey untuk menyimpan frame LCD sebagai tangkapan layar (f1-f12, printscreen) | off |

## Mode second monitor

Jalankan **`trofeo_screen`** (bukan `trofeo_lcd` — sama-sama pakai LCD, tak
boleh bersamaan):

```bash
./target/release/trofeo_screen --list-displays   # cek monitor
./target/release/trofeo_screen                   # stream ke LCD
```

Butuh *Virtual Display Driver* (VDD) 1920×462 — lihat
**[GUIDE_SECOND_MONITOR.id.md](./GUIDE_SECOND_MONITOR.id.md)**.

## Struktur

- `src/lib.rs` — driver USB: handshake, chunking, JPEG encode, `Framebuffer`.
- `src/audio.rs` — capture audio (WASAPI / PulseAudio-PipeWire).
- `src/cpu_sensor.rs`, `src/cpu_freq.rs` — suhu/power & frekuensi CPU.
- `src/deepcool/` — driver display DeepCool (HID).
- `src/dxgi_capture.rs` + `src/bin/screen.rs` — mode second monitor.
- `src/main.rs` — loop utama: audio → FFT → bar EQ → kirim ke layar.

## Performa

Diukur pada PC Windows (12 logical processor), `trofeo_lcd` berjalan dengan
pengaturan default (idle FPS 2 / aktif 15, DeepCool aktif):

| Metrik | Hasil ukur |
|---|---|
| RAM | ~13 MB working set (stabil, tidak membesar) |

Ringannya ini berkat FPS adaptif (JPEG-encode + kirim USB hanya terjadi saat
ada audio) dan optimasi CPU di `src/main.rs` / `src/lib.rs`.

## Lisensi

GPL-3.0-or-later (mengikuti proyek upstream rujukan).