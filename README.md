# th-linux-lcd

Linux controller for the Thermaltake TH420V2 Ultra LCD cooler. Lets you stream GIF animations and set standby images on the 480×480 circular LCD display.

![status](https://img.shields.io/badge/status-working-brightgreen)

## Features

- Stream GIF animations to the LCD (up to 60 frames, 5–20 fps)
- Upload standby images (JPEG/PNG, auto-converted to 480×480)
- Screen on/off control
- Frame rotation (90°/180°/270°)
- Dark terminal-style UI, no browser engine required
- ~3MB binary, ~150ms startup

## Requirements

- Linux (x86_64)
- `libusb`
- `python-pillow` (for GIF/image conversion)
- `zenity` (optional — for file picker dialogs)
- OpenGL (for the GUI)

## Installation

### Arch Linux (AUR)

```bash
# With an AUR helper
yay -S th-linux-lcd

# Or manually
git clone https://aur.archlinux.org/th-linux-lcd.git
cd th-linux-lcd
makepkg -si
```

### From source

```bash
git clone https://github.com/klatooine/th-linux-lcd
cd th-linux-lcd
cargo build --release
```

Then install the udev rule manually (required for non-root USB access):

```bash
sudo install -Dm644 pkg/99-thermaltake-lcd.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules && sudo udevadm trigger
sudo usermod -aG plugdev $USER   # log out and back in after this
```

## Usage

Launch the app:

```bash
th-linux-lcd
```

The daemon starts automatically in the background. You can also manage it manually:

```bash
# Enable on login
systemctl --user enable --now th-linux-lcd

# Check status
systemctl --user status th-linux-lcd
```

### Animation tab

1. Click **+ ADD GIF** to import a GIF — frames are extracted and converted to 480×480 JPEG
2. Select an animation from the gallery
3. Click **SEND TO LCD** to start streaming
4. Use **CCW / CW** to rotate if needed
5. Click **STOP** to stop streaming

### Standby tab

1. Click **+ ADD IMAGE** to import a JPEG or PNG
2. Select an image from the gallery
3. Click **UPLOAD TO LCD** to set it as the standby image

The screen on/off buttons (**ON** / **OFF**) are in the top-right corner.

## Architecture

The app runs as two processes:

```
th-linux-lcd (GUI)  ←──Unix socket──→  th-linux-lcd --daemon
```

The GUI auto-launches the daemon on startup. The daemon owns the USB device handle and streams frames continuously. IPC is newline-delimited JSON over `$XDG_RUNTIME_DIR/th-lcd.sock`.

## Data directory

```
~/.local/share/th-linux-lcd/
├── videos.json          GIF manifest
├── standby.json         standby image manifest
├── frames/<uuid>/       extracted JPEG frames
└── standby/<uuid>.jpg   converted standby images
```

## Device

- USB VID `0x264a` PID `0x233c`
- Protocol reverse-engineered from Windows USB traces
- JPEG frames: 480×480, 4:4:4 subsampling, ≤20KB each
- Standby images: 480×480, 4:4:4 subsampling, ≤5MB

## License

MIT
