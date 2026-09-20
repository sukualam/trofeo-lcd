# trofeo_lcd

**English** · [Bahasa Indonesia](./README.md)

Audio visualizer (EQ bars) + system info (CPU, RAM, clock, date) for the
**Thermalright Trofeo Vision 9.16 LCD** (USB VID:PID `0416:5408`, "LY"
protocol — chunked USB bulk transfer; the LY1 variant `0416:5409` is also
supported).

The USB driver was rewritten byte-for-byte from the original Python
implementation of the [thermalright-trcc-linux](https://github.com/Lexonight1/thermalright-trcc-linux)
project (`LyLcd` in `src/trcc/adapters/.../ly_lcd.py`).

## Structure

- `src/lib.rs` — core driver: device detection, handshake, chunking, JPEG
  encoding, plus `Framebuffer` (drawing boxes and 5x7 bitmap text).
- `src/font.rs` — small hand-made bitmap font for `Framebuffer::draw_text`.
- `src/audio.rs` — audio source for the EQ bars:
  - **Windows**: WASAPI *loopback* capture (audio currently playing to
    speaker/headphones — NOT the microphone).
  - **Linux**: PulseAudio/PipeWire *loopback* capture (device
    `@DEFAULT_MONITOR@`, same one used by `parec`).
  - **Other OS**: synthetic source (test tone) so the code still compiles
    and can be tested.
- `src/media.rs` — master volume + song/media title ("NOW PLAYING"):
  Windows via COM/WinRT, Linux via `pactl`/`playerctl`.
- `src/foreground.rs` — active window / program name (.exe) detection, used
  by the game mode: Windows via WinAPI, Linux via X11 (EWMH).
- `src/cpu_freq.rs` — **real-time** CPU frequency (follows load/boost):
  Windows via PDH `% Processor Performance` × registry base clock, Linux via
  average sysfs `scaling_cur_freq`.
- `src/deepcool/` — **DeepCool Digital** integration: sends CPU
  temp/usage/power/frequency to a DeepCool cooler/case display over USB HID
  (drivers ported from the deepcool-digital project).
- `src/main.rs` — main program: audio capture → FFT → EQ bars + system info
  → send to screen. Also feeds CPU data to the background DeepCool thread.

## Linux — what to install

All sensors (CPU/GPU/net/disk) work through the plain kernel sysfs (no extra
drivers needed). Only these external packages are required:

- **Build**: `pkg-config` + PulseAudio headers (`libpulse-dev` on
  Debian/Ubuntu, `libpulse-devel` on Fedora, already bundled in the
  `libpulse` package on Arch) — needed by `libpulse-binding` to compile.
- **Runtime — audio EQ & volume**: PulseAudio OR PipeWire with
  `pipewire-pulse` enabled (default on modern distros: Ubuntu 22.10+,
  Fedora, etc.). Check with `pactl info`.
- **Runtime — NOW PLAYING**: `playerctl` (`sudo apt install playerctl` /
  `sudo pacman -S playerctl` / `sudo dnf install playerctl`). Without it the
  song title is just empty (no crash) — volume/GPU/CPU/etc. still work.
- **Runtime — USB access without root**: install `99-trofeo-lcd.rules` (see
  the comments in that file for how) so you don't need `sudo` every time.
- **Runtime — CPU power draw (Watt)**: the source depends on CPU generation.
  The program tries automatically, in this order (no configuration needed):
  1. **RAPL powercap** (`/sys/class/powercap/intel-rapl:N/energy_uj`,
     "package" zone) — **primary source for Zen 4+ CPUs (Ryzen 7000/8000/9000,
     incl. 7500F)**. The mainline `intel_rapl_msr` driver (from
     `CONFIG_INTEL_RAPL`, enabled by default on all modern distros) reads the
     AMD RAPL MSR (`MSR_PKG_ENERGY_STAT` / `0xC001_029B`) — the same register
     as the Windows path — so the numbers match. You can also try it on
     Zen 1–3, but there zenpower (point 3) is preferred for accuracy.
  2. **`amd_energy`** (hwmon RAPL) — **removed from mainline Linux entirely
     since 5.13** (2021); this entry is only for old custom kernels that
     still carry it.
  3. **Community driver `zenpower3`** — **Zen 1–3 ONLY** (SVI2 telemetry).
     It does not work on Zen 4+ (they moved to SVI3), but if you have a
     Zen 1–3 CPU this is the best option (AUR: `yay -S zenpower3-dkms-git`,
     then reboot).

  Important for Zen 4+: if step 1 fails, it's usually not because the driver
  is missing (RAPL is built-in), but because `energy_uj` is **root-only by
  default** since the Platypus vulnerability mitigation. Check first without
  sudo: `ls /sys/class/powercap/*/energy_uj` — if it says "Permission
  denied", install this udev rule so a normal user can read it (step 1 may
  need `sudo modprobe intel_rapl_msr`; if already auto-loaded, `modprobe`
  will say "already loaded" — that's normal):
  ```
  sudo tee /etc/udev/rules.d/99-rapl.rules <<'EOF'
  SUBSYSTEM=="powercap", ACTION=="add", RUN+="/bin/chmod 0444 /sys%p/energy_uj"
  EOF
  sudo udevadm control --reload-rules && sudo udevadm trigger --subsystem-match=powercap --action=add
  ```
  With no source available, CPU power shows N/A (no crash) — all other
  sensors keep working.
- **Game mode (exe name in NOW PLAYING)**: requires an X11 session (native
  or XWayland) — on pure Wayland without XWayland only this part becomes N/A
  (nothing else is affected). See the notes in `src/foreground.rs`.

## What is displayed

- EQ bars (48 by default) from the 40Hz–16kHz audio spectrum, logarithmic
  scale, colored green→yellow→red by level, with auto-gain (automatically
  adapts to the volume being played) and attack/decay smoothing.
- Top info line: `CPU xx%   MEM used/total MB   UP [ND]HH:MM:SS   HH:MM:SS   YYYY-MM-DD`.
  Uptime (`UP ...`) uses `System::uptime()` from `sysinfo` — a static call
  that just reads an OS counter, not an expensive CPU/RAM snapshot, so it is
  computed every frame without following the `SYSINFO_REFRESH_INTERVAL` rule.
- Real-time CPU frequency (follows load/boost) in the info line, read via
  PDH (`% Processor Performance` × base clock) on Windows or sysfs cpufreq on
  Linux — the same figure as the "Speed" column in Task Manager.

## DeepCool Digital integration

trofeo-lcd can also drive a **DeepCool cooler/case display** (e.g.
AG300/400/500/620 DIGITAL, AK/K/Pro, LS, LQ, LD, LP, CH/CH-Gen2, CH510)
connected over USB HID — so **you don't need a separate DeepCool program**.
The temp/power/frequency shown are read from the same sensor instance as the
info line (shared with the DeepCool thread) and sent to the display on a
fixed interval.

Enabled by default. Options:

| Argument | Function | Default |
|---|---|---|
| `--no-deepcool` | Turn off the DeepCool integration entirely | enabled |
| `--deepcool-update-ms <N>` | Data send interval in ms (clamped to 100–2000) | `1000` |

The device is auto-detected (retry every few seconds until found). If the
CPU temp sensor is unavailable (e.g. the PawnIO driver is not installed on
Windows) the display shows 0 — everything else still works.

## Second Monitor Mode (`trofeo_screen`)

Besides the visualizer there is a separate binary **`trofeo_screen`** that
turns the Trofeo LCD into a **real second monitor**: the desktop is captured
in real time via DXGI Desktop Duplication and streamed to the LCD. You can
drag app windows (Spotify, browser, etc.) right onto the Trofeo screen.

It requires a *Virtual Display Driver* (VDD) at **1920×462** — the full guide
(VDD setup, `--fps`/`--quality`/`--rotate` options, and login autostart) is in
**[GUIDE_SECOND_MONITOR.md](./GUIDE_SECOND_MONITOR.md)**.

> ⚠️ `trofeo_lcd` and `trofeo_screen` use the same LCD — run **one of them**,
> never both at the same time.

## CPU optimizations

Several once expensive CPU points have been cleaned up (all pass
`cargo test`):

- **Per-bar FFT bin ranges** used to be recomputed every frame in
  `compute_bars` (including 2 `powf()` calls per bar = 96×/frame for 48
  bars). Now computed once up front (`precompute_bar_bins`) and reused.
- **`Framebuffer`** used to be allocated fresh (~2.66MB) every frame via
  `Framebuffer::new()` inside the main loop. Now allocated once outside the
  loop, only `clear()`ed each frame.
- **`Framebuffer::clear()` & `fill_rect()`** (used for EQ bars & text) used
  to write pixels one by one through `set_pixel` with per-pixel bounds
  checks. Now use the *doubling* technique (`copy_within`, memcpy of large
  blocks doubled at each step) — O(log n) copy operations instead of O(n)
  function calls per pixel, no extra heap allocation.
- **WASAPI audio capture** (`src/audio.rs`, Windows) used to lock the buffer
  ring mutex per SINGLE audio sample (potentially tens of thousands of
  times/second). Now samples are collected into a local buffer first, and the
  mutex is locked once for the whole batch.

If CPU usage is still high on real hardware, the two strongest levers are
lowering `--active-fps` (e.g. 15 → 10) — see the "Adaptive FPS" section
below — and/or `FFT_SIZE` in `main.rs`. JPEG encoding + USB transfer every
frame at 1920x462 remains the biggest inherent CPU cost of this feature, and
it scales directly with how many encodes happen per second — which is why the
idle FPS (2 by default) is far lower than the active FPS.

## Tuning

Constants at the top of `src/main.rs`:

| Constant | Purpose |
|---|---|
| `NUM_BARS` | Number of EQ bars |
| `FFT_SIZE` | FFT window size (resolution vs latency) |
| `FREQ_MIN` / `FREQ_MAX` | Frequency range mapped to bars |
| `SYSINFO_REFRESH_INTERVAL` | How often CPU/RAM are refreshed |

## Adaptive FPS (command-line args)

The send-to-screen FPS is **adaptive**: it drops to the idle FPS when no
sound is detected (huge CPU savings — 1920x462 JPEG encode + USB every frame
is this program's biggest CPU cost) and jumps back to the active FPS as soon
as sound appears. It's controlled by arguments, not constants, so you can
tune without rebuilding:

```bash
./target/release/trofeo_lcd --idle-fps 2 --active-fps 15 \
    --silence-threshold 0.005 --silence-timeout-ms 800
```

| Argument | Function | Default |
|---|---|---|
| `--idle-fps <N>` | FPS when idle | `2` |
| `--active-fps <N>` | FPS when there is sound | `15` |
| `--silence-threshold <N>` | Peak-amplitude threshold (0.0-1.0) of raw samples to be considered "silent" — raise it if background noise/DC-offset is still treated as "sound", lower it if quiet audio is not detected | `0.005` |
| `--silence-timeout-ms <N>` | How long it must stay silent in a row before dropping to `idle-fps` (going back up is ALWAYS immediate, no delay) | `800` |

Run with `--help` to see these options straight from the program.

Note: silence detection is only checked once per frame (using the same audio
window as the EQ bars), so while at `idle-fps` (e.g. 2 FPS) it takes up to
~1/idle-fps seconds to "notice" new sound and jump back to `active-fps` —
usually imperceptible for this kind of visualizer, but if you need more
responsiveness, bump `--idle-fps` a bit (e.g. 4–5) as a compromise.

## OpenRGB color sync

The EQ bar color can be synced (polled) to the color of a device you already
configured in **OpenRGB**:

```bash
./target/release/trofeo_lcd --openrgb-device "RAM" --openrgb-poll-ms 300
```

| Argument | Function | Default |
|---|---|---|
| `--openrgb-device <NAME>` | Partial-match an OpenRGB device name (case-insensitive) | (disabled) |
| `--openrgb-poll-ms <N>` | Re-read color from OpenRGB interval, ms | `300` |

Requirement: OpenRGB must be running with the **SDK Server** enabled
(Settings → SDK Server → Enable, default port `6742`). Until trofeo-lcd
connects or a matching device is found, the EQ bars fall back to `--color`
(or the default gradient if `--color` is not given) — the program runs
normally, the color just doesn't follow OpenRGB yet.

**This is one-way polling** (periodically reads a snapshot of an OpenRGB
device's color), **not** registering trofeo-lcd as an OpenRGB-controlled
device. Consequences:
- Good for source devices with a **static** color in OpenRGB.
- If the source device uses **animated effects** (rainbow, breathing, etc.),
  the LCD color still changes but "jerky", following `--openrgb-poll-ms`,
  not as smooth as the original animation.

## Hide the console window (`--hide-console`)

By default the program stays visible in the terminal (logs appear on
screen). To run it in the background (e.g. via a shortcut or Task Scheduler
at login, without a black window popping up), add:

```bash
trofeo_lcd.exe --hide-console
```

As soon as argument parsing is done, the console window is hidden
(`FreeConsole`) — standard output/error behave as usual and **no log file is
written**. Windows only; on non-Windows builds this option is ignored with a
warning.

## System dependency installation

`rusb` is used with the **`vendored`** feature — libusb is compiled directly
from the bundled source (via `cc`), so **you don't need to install libusb
separately on any OS**, including Windows. The only requirement is a C
compiler.

```bash
# Debian/Ubuntu
sudo apt install pkg-config build-essential

# Fedora
sudo dnf install pkgconf-pkg-config gcc

# Arch
sudo pacman -S pkgconf base-devel
```

### USB permissions on Linux (no sudo)

```bash
sudo tee /etc/udev/rules.d/99-trofeo-lcd.rules > /dev/null << 'RULES'
SUBSYSTEM=="usb", ATTR{idVendor}=="0416", ATTR{idProduct}=="5408", MODE="0666"
SUBSYSTEM=="usb", ATTR{idVendor}=="0416", ATTR{idProduct}=="5409", MODE="0666"
RULES
sudo udevadm control --reload-rules && sudo udevadm trigger
# unplug and re-plug the USB cable
```

## Running on Windows

### 1. Toolchain

- Install Rust via [rustup](https://rustup.rs) (default target
  `x86_64-pc-windows-msvc`).
- Since `vendored` needs a C compiler, install **Visual Studio Build Tools**
  (the "Desktop development with C++" workload). Alternative: the
  `x86_64-pc-windows-gnu` target + a MinGW-w64 toolchain.

### 2. USB driver (REQUIRED)

The device must be bound to the **WinUSB** driver first:

1. Plug in the Trofeo Vision LCD; don't run the official TRCC software.
2. Download [Zadig](https://zadig.akeo.io/) (portable).
3. Options → **List All Devices**.
4. Find the device with ID `0416 5408`, make sure the target driver is
   **WinUSB**, click **Replace Driver** / **Install Driver**.

> ⚠️ This replaces the driver for that device. To go back to the official
> TRCC software later, restore the driver through Device Manager, or unplug
> and replug the device so Windows installs the default driver again.

### 3. Build & run

```powershell
cd trofeo-lcd
cargo build --release
.\target\release\trofeo_lcd.exe
```

The EQ bars follow whatever audio Windows is currently playing (default
loopback device) — no extra setup needed, WASAPI loopback uses the system
default output device automatically.

> ℹ️ The DeepCool integration needs the PawnIO driver to read AMD CPU
> temp/power on Windows (`winget install namazso.PawnIO`) — without it the
> DeepCool display still runs but shows 0. The program also should be run as
> Administrator for driver access.

## Usage (Linux/macOS)

```bash
cargo build --release
./target/release/trofeo_lcd
```

On Linux/macOS the EQ bars use a synthetic source (not real audio) — see the
notes in `src/audio.rs`.

## Testing status

- The core logic (FFT, frequency bucketing into bars, auto-gain
  normalization, bitmap text drawing) was tested separately with synthetic
  signals (220/880/3000 Hz tones) — bar peaks appear exactly at the matching
  frequencies. The USB driver and chunking/JPEG unit tests pass via
  `cargo test` (`cargo check`/`cargo build --release` are clean on Windows).
- The Windows build runs on real hardware: a Trofeo Vision 9.16 LCD
  (`0416:5408`), plus DeepCool integration with an AG Series device
  (`VID_3633 PID_0008`) that is auto-detected.

## License

This code follows the license of the upstream project whose protocol it
references: GPL-3.0-or-later.