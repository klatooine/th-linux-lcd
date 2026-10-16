# th-linux-lcd

> [!WARNING]
> **Use at your own risk.**
> This is homemade, unofficial software — not affiliated with or endorsed by Thermaltake.
> The standby and boot-animation features write directly to the device's flash memory using a
> reverse-engineered protocol. There is **no guarantee** it will work on your hardware, and it could
> potentially brick your LCD screen or damage its firmware. It has been tested on a single device.
> No warranty, no support, no liability. **You accept full responsibility for any damage caused.**
> See [Safety](#safety) before writing anything to the device.

Native Linux controller for the **Thermaltake TH420V2 Ultra** LCD display (USB `264a:233c`).

Replaces the official Windows-only software with a lightweight Rust desktop application. Protocol reverse-engineered from USB traces of the Windows app.

---

## Features

| Feature | Description |
|---------|-------------|
| **GIF streaming** | Stream animated GIFs to the LCD in real time (up to 60 frames, 5–20 fps) |
| **Standby image** | Upload a static JPEG/PNG/GIF (first frame) as the always-on display image |
| **Boot animation** | Flash a custom GIF as the power-on boot animation (40 frames, up to 12.5 fps) |
| **Screen control** | Turn the LCD on or off |
| **Rotation** | Rotate any animation or boot image by 90 / 180 / 270° |
| **Gallery** | Per-category gallery with circle preview and thumbnail grid |
| **Autoplay** | The last streamed animation resumes when the daemon starts |

---

## Requirements

| Dependency | Purpose | Notes |
|-----------|---------|-------|
| `libusb` | USB access | Runtime dependency |
| `python3` + Pillow | GIF/image processing | Frame extraction and JPEG encoding |
| `zenity` | File picker dialogs | Needed to import files from the GUI |
| OpenGL | egui renderer | Present on virtually all Linux desktops |
| Rust ≥ 1.89 | Build only | The installer sets it up via rustup if your distro's is older |
| systemd-logind | USB permissions | The udev rule uses `uaccess` (standard on desktop distros) |

---

## Installation

```bash
git clone https://github.com/klatooine/th-linux-lcd.git
cd th-linux-lcd
bash install.sh
```

The installer asks for your sudo password and then:

- Installs system dependencies via your distro's package manager (Arch/CachyOS, Debian/Ubuntu, Fedora, openSUSE)
- Makes sure Rust ≥ 1.89 is available, installing a user-local toolchain with [rustup](https://rustup.rs) if needed
- Builds the release binary with `cargo`
- Installs the binary to `~/.local/bin/`, plus a desktop entry and icon
- Installs a udev rule that grants USB access to the user logged in at the seat (no group membership or re-login needed)
- Installs, enables and starts the systemd user service

Re-run it to upgrade. If the GUI shows **NO DEVICE** right after installing, unplug and re-plug the cooler's USB cable.

### Arch Linux (makepkg)

```bash
git clone https://github.com/klatooine/th-linux-lcd.git
cd th-linux-lcd
makepkg -si
systemctl --user enable --now th-linux-lcd
```

### Uninstall

```bash
bash install.sh --uninstall
```

This removes the binary, service, desktop entry and udev rule. Your library in `~/.local/share/th-linux-lcd/` is kept.

### Manual dependency install

| Dependency | Arch | Debian/Ubuntu | Fedora |
|-----------|------|---------------|--------|
| Build tools | `base-devel` | `build-essential pkg-config` | `gcc pkgconf-pkg-config` |
| libusb | `libusb` | `libusb-1.0-0-dev` | `libusb1-devel` |
| Pillow | `python-pillow` | `python3-pil` | `python3-pillow` |
| zenity | `zenity` | `zenity` | `zenity` |
| Rust ≥ 1.89 | `rust` or `rustup` | [rustup](https://rustup.rs) (apt's is too old on 24.04 / 13) | `cargo rust` |

---

## Quick start

```bash
# Option 1 — launch the GUI (starts the daemon automatically)
th-linux-lcd

# Option 2 — manage the daemon via systemd
systemctl --user enable --now th-linux-lcd
th-linux-lcd
```

The GUI auto-detects and connects to the daemon. If the device is not plugged in, the header shows **NO DEVICE** and the daemon keeps retrying.

Daemon logs: `journalctl --user -u th-linux-lcd -f`

---

## Usage

### Animation tab

1. Click **+ ADD GIF** → select a GIF file
2. The app extracts up to 60 frames and creates thumbnails
3. Select the animation in the gallery
4. Click **SEND TO LCD** — the animation begins streaming immediately (nothing is written to flash)
5. Use **CCW / CW** to rotate in 90° steps; rotated frames are generated the next time you send it
6. Click **STOP** to stop streaming

### Standby tab

1. Click **+ ADD IMAGE** → select a JPEG, PNG, or GIF (first frame used)
2. The image is resized to 480×480 with the subsampling the device requires
3. Click **UPLOAD TO LCD** and confirm — the image is written to the device's standby flash slot

### Boot tab

1. Click **+ ADD GIF** → select a GIF file
2. 40 frames are extracted and packed into the firmware boot container
3. Click **↻ / ↺** to rotate; the container is rebuilt from the source GIF
4. Click **UPLOAD TO LCD** and confirm — the animation is flashed to the boot slot
5. Wait until the status bar says it is done (about a minute; the header shows **WRITING FLASH**)
6. Power cycle the device to see the boot animation

### Screen control

Use the **ON** / **OFF** buttons in the header to control the LCD backlight.

---

## Safety

Streaming (Animation tab) and screen on/off are volatile — a power cycle undoes them. Standby and boot uploads are **persistent writes to device flash**.

What the app does to keep them safe:

- Only the standby and boot slots are ever written, with the same command sequence the Windows app uses. There is no firmware-update, bootloader or erase command anywhere in the code.
- Every upload is validated before any USB traffic: standby must be a 480×480 JPEG ≤ 256 KiB; the boot container is checked byte-for-byte (header, checksum, 40 frames, timing, frame table, JPEG markers, trailer) and capped at 1.25 MiB — close to the known-good container from the Windows app.
- Every step of an upload must be acknowledged by the device. If anything fails, the upload stops and the boot commit is never sent.
- After a boot commit the daemon waits 30 s for the flash write before reporting success, and defers shutdown (e.g. `systemctl --user stop`, logout) until it is done.

What you should do:

- **Do not unplug the cooler or power off the PC while an upload is in progress**, or until the status bar says it is done.
- If a boot animation or standby image does not show up (or looks corrupted) after a power cycle, upload it again — or re-upload one from the official Windows app. That is the expected recovery path, but it has not been tested against every possible failure.
- Unknowns: the real size of the flash slots, the meaning of some protocol bytes, and whether the firmware bounds-checks uploads. If you are not comfortable with that, use streaming only.

---

## Architecture

```
┌──────────────────────────┐
│  th-linux-lcd (GUI)      │
│  egui/eframe main loop   │──── Unix socket ─────────┐
└──────────────────────────┘  ($XDG_RUNTIME_DIR/th-lcd.sock)
                                                      │
                              ┌───────────────────────▼─────────────────┐
                              │  th-linux-lcd --daemon                  │
                              │                                         │
                              │  Socket threads  →  USB thread          │
                              │  (JSON IPC,          (owns rusb handle) │
                              │   validation)        frames + uploads   │
                              └─────────────────────────────────────────┘
```

The GUI and daemon are a **single binary**. The GUI starts the daemon automatically (via the systemd user service when installed). IPC uses newline-delimited JSON over a Unix domain socket that only your user can access.

See [docs/architecture.md](docs/architecture.md) for a detailed breakdown.

---

## Data directory

```
~/.local/share/th-linux-lcd/
├── videos.json            animation manifest
├── standby.json           standby image manifest
├── boot.json              boot animation manifest
├── autoplay.json          animation to resume on daemon start
├── frames/<uuid>/         extracted JPEG frames per GIF
├── frames/<uuid>-rotated/ cached rotated frames
├── standby/<uuid>.jpg     converted standby images
└── boot/<uuid>.bootanim   boot animation containers
```

---

## Development

```bash
cargo test                                   # unit tests (container format, validation, persistence)
cargo clippy --all-targets
cargo run -- --daemon                        # run the daemon in the foreground
cargo run --example test_boot -- anim.gif --dry-run   # build + validate a boot container
```

---

## Documentation

| Document | Description |
|---------|-------------|
| [Architecture](docs/architecture.md) | App + daemon design, IPC protocol |
| [Animation streaming](docs/animation-streaming.md) | GIF streaming via USB interrupt transfers |
| [Standby image](docs/standby-image.md) | Static image upload protocol |
| [Boot animation](docs/boot-animation.md) | Boot slot flash upload + container format |
| [Screen control](docs/screen-control.md) | LCD on/off control |
| [Reverse engineering](docs/reverse-engineering.md) | USB protocol discovery methodology and findings |

---

## Device

| Property | Value |
|---------|-------|
| USB VID:PID | `264a:233c` |
| Control EP | `0x01` OUT / `0x82` IN (440-byte interrupt) |
| Animation EP | `0x03` OUT / `0x84` IN (1024-byte interrupt) |
| Display | 480×480 circular LCD |

---

## License

MIT — see [LICENSE](LICENSE).
