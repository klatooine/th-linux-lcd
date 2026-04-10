# Architecture

## Overview

`th-linux-lcd` is a single binary that runs in two modes:

| Mode | Command | Role |
|------|---------|------|
| GUI | `th-linux-lcd` | Renders the UI, sends commands over Unix socket |
| Daemon | `th-linux-lcd --daemon` | Owns the USB device, streams frames, handles uploads |

The GUI launches the daemon automatically on startup if the socket is not present.

---

## Process diagram

```
┌─────────────────────────────────────┐
│  GUI process (egui/eframe)          │
│                                     │
│  - renders all UI (immediate mode)  │
│  - polls daemon status every 2s     │
│  - sends commands over socket       │
│  - no USB access                    │
└──────────────┬──────────────────────┘
               │ Unix domain socket
               │ $XDG_RUNTIME_DIR/th-lcd.sock
               │ newline-delimited JSON
┌──────────────▼──────────────────────┐
│  Daemon process                     │
│                                     │
│  ┌─────────────────┐                │
│  │  Socket thread  │  one thread    │
│  │  per connection │  per client    │
│  └────────┬────────┘                │
│           │ sync_channel (cap 32)   │
│  ┌────────▼────────┐                │
│  │   USB thread    │  single thread │
│  │                 │  owns handle   │
│  │  - streams GIF  │                │
│  │  - uploads      │                │
│  │  - reconnects   │                │
│  └─────────────────┘                │
└─────────────────────────────────────┘
```

---

## IPC protocol

All messages are newline-delimited JSON. The GUI opens a fresh TCP-like connection for each command.

### Commands (GUI → daemon)

```json
{ "cmd": "Play",          "frames_dir": "/path/...", "fps": 15.0, "frame_count": 42, "id": "uuid" }
{ "cmd": "Stop" }
{ "cmd": "ScreenOn" }
{ "cmd": "ScreenOff" }
{ "cmd": "UploadStandby", "path": "/path/to/image.jpg" }
{ "cmd": "UploadBoot",    "path": "/path/to/anim.bootanim" }
{ "cmd": "Status" }
```

### Responses (daemon → GUI)

```json
{ "type": "Ok" }
{ "type": "Err", "msg": "Device not found" }
{ "type": "Status", "streaming": true, "playing_id": "uuid", "device_found": true, "fps": 15.0 }
```

### Timeouts

| Command | Socket read timeout |
|---------|-------------------|
| Most commands | 30 s |
| `UploadBoot` | 120 s (flash write takes ~15 s) |

---

## USB thread

The USB thread runs a tight loop:

1. If no device: call `usb::open()`, sleep 2 s on failure
2. Drain all pending `UsbCmd` messages (non-blocking `try_recv`)
3. Update shared `DaemonStatus` (Arc<Mutex<>>)
4. If streaming and frame is due: send frame via `usb::send_frame()`
5. Sleep up to next frame time (max 5 ms) or 50 ms if idle

Frame timing is wall-clock based (`Instant::now()`) so the loop never over-delivers frames.

---

## GUI state

The GUI uses egui's immediate-mode paradigm. Key state:

```rust
struct App {
    videos:      Vec<Video>,
    standby:     Vec<StandbyImage>,
    boot_anims:  Vec<BootAnim>,
    tab:         Tab,            // Animation | Standby | Boot
    bg_task:     Option<BgTask>, // running upload (non-blocking channel)
    daemon_status: DaemonStatus,
    preview_texture: Option<TextureHandle>,
    thumb_cache: HashMap<String, TextureHandle>,
}
```

Background tasks (uploads) run on a separate thread and post results back via a `channel`. The GUI polls the channel each frame and updates the status bar.

---

## Module map

| File | Responsibility |
|------|---------------|
| `src/main.rs` | Entry point — routes to `daemon::run()` or `gui::run()` |
| `src/daemon.rs` | Unix socket server, USB command dispatch, frame streaming loop |
| `src/usb.rs` | Low-level USB protocol: open, send_frame, screen_on/off, upload_impl |
| `src/shared.rs` | IPC types, data structures, persistence, boot container builder |
| `src/gui.rs` | egui UI: all three tabs, gallery, circle preview, import/rotate logic |
| `src/bin/test_boot.rs` | Standalone upload tool for testing the boot animation protocol |

---

## Data persistence

All state is stored in `~/.local/share/th-linux-lcd/`:

```
videos.json      Vec<Video>      — imported GIF manifest
standby.json     Vec<StandbyImage>
boot.json        Vec<BootAnim>
frames/<id>/     f00001.jpg … fNNNNN.jpg  (extracted animation frames)
standby/<id>.jpg                          (converted standby images)
boot/<id>.bootanim                        (Update_Boot_GIF container)
```

Manifests store base64-encoded 96×96 JPEG thumbnails inline to avoid extra disk reads on startup.
