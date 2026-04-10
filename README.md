# th-linux-lcd

Native Linux controller for the **Thermaltake TH420V2 Ultra** LCD display (USB `264a:233c`).

Replaces the official Windows-only software with a lightweight Rust desktop application (~3 MB binary, ~150 ms startup). Protocol reverse-engineered from USB traces of the Windows app.

---

## Features

| Feature | Description |
|---------|-------------|
| **GIF streaming** | Stream animated GIFs to the LCD in real time (up to 60 fps) |
| **Standby image** | Upload a static JPEG/PNG/GIF as the always-on display image |
| **Boot animation** | Flash a custom GIF as the power-on boot animation |
| **Screen control** | Turn the LCD on or off |
| **Rotation** | Rotate any animation or boot image by 90 / 180 / 270° |
| **Gallery** | Per-category gallery with circle preview and thumbnail grid |

---

## Requirements

| Dependency | Purpose | Notes |
|-----------|---------|-------|
| `libusb` | USB access | Runtime dependency |
| `python3-pillow` | GIF/image processing | For frame extraction and JPEG encoding |
| `zenity` | File picker dialogs | Optional — required for GUI import |
| OpenGL | egui renderer | Present on virtually all Linux systems |

---

## Installation

### Arch Linux (AUR)

```bash
yay -S th-linux-lcd
```

### From source

```bash
git clone https://github.com/klatooine/th-linux-lcd.git
cd th-linux-lcd
cargo build --release
bash install.sh
```

`install.sh` places the binary in `~/.local/bin/` and installs the systemd user service.

### USB permissions (non-root)

Copy the udev rule so you can access the device without `sudo`:

```bash
sudo cp pkg/99-thermaltake-lcd.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules && sudo udevadm trigger
sudo usermod -aG plugdev $USER
# log out and back in
```

---

## Quick start

```bash
# Option 1 — launch the GUI (daemon starts automatically)
th-linux-lcd

# Option 2 — manage the daemon via systemd
systemctl --user enable --now th-linux-lcd
th-linux-lcd
```

The GUI auto-detects and connects to the daemon. If the device is not plugged in, the status bar shows **NO DEVICE** and retries automatically.

---

## Usage

### Animation tab

1. Click **+ ADD GIF** → select a GIF file
2. The app extracts up to 60 frames and creates thumbnails
3. Select the animation in the gallery
4. Click **SEND TO LCD** — the animation begins streaming immediately
5. Use **↻ / ↺** to rotate in 90° steps; the daemon re-applies rotation on the fly
6. Click **STOP** to stop streaming

### Standby tab

1. Click **+ ADD IMAGE** → select a JPEG, PNG, or GIF (first frame used)
2. The image is resized to 480×480 with correct JPEG subsampling
3. Click **UPLOAD TO LCD** — the image is written to the device's standby flash slot

### Boot tab

1. Click **+ ADD GIF** → select a GIF file
2. Up to 40 frames are extracted and packed into the firmware boot container
3. Click **↻ / ↺** to rotate; container is rebuilt from the source GIF
4. Click **UPLOAD TO LCD** — the animation is flashed to the boot slot
5. Power cycle the device to see the boot animation

### Screen control

Use the **ON** / **OFF** buttons in the header to control the LCD backlight.

---

## Architecture

```
┌──────────────────────────┐
│  th-linux-lcd (GUI)      │
│  egui/eframe main loop   │──── Unix socket ────┐
└──────────────────────────┘  (~/.run/th-lcd.sock)│
                                                   │
                              ┌────────────────────▼────────────────────┐
                              │  th-linux-lcd --daemon                  │
                              │                                         │
                              │  Socket thread  →  USB thread           │
                              │  (JSON IPC)         (owns rusb handle)  │
                              │                     frames + uploads    │
                              └─────────────────────────────────────────┘
```

The GUI and daemon are a **single binary**. The daemon is launched automatically by the GUI if the socket is not present. IPC uses newline-delimited JSON over a Unix domain socket.

See [docs/architecture.md](docs/architecture.md) for a detailed breakdown.

---

## Data directory

```
~/.local/share/th-linux-lcd/
├── videos.json          animation manifest
├── standby.json         standby image manifest
├── boot.json            boot animation manifest
├── frames/<uuid>/       extracted JPEG frames per GIF
├── standby/<uuid>.jpg   converted standby images
└── boot/<uuid>.bootanim boot animation containers
```

---

## Documentation

| Document | Description |
|---------|-------------|
| [Architecture](docs/architecture.md) | App + daemon design, IPC protocol |
| [Animation streaming](docs/animation-streaming.md) | GIF streaming via USB bulk transfer |
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
