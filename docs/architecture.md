# Architecture

## Overview

`th-linux-lcd` is a single binary that runs in two modes:

| Mode | Command | Role |
|------|---------|------|
| GUI | `th-linux-lcd` | Renders the UI, sends commands over Unix socket |
| Daemon | `th-linux-lcd --daemon` | Owns the USB device, streams frames, handles uploads |
| Tray | `th-linux-lcd --tray` | StatusNotifierItem icon: shows status, opens the GUI, quick actions |

Each mode is single-instance per user, enforced with a lock file in `$XDG_RUNTIME_DIR` (`th-lcd.lock`, `th-lcd-gui.lock`, `th-lcd-tray.lock`). A second GUI launch connects to `th-lcd-gui.sock`, asks the running window to come forward, and exits. The tray polls the daemon's `Status` every 2 s and never touches USB itself.

The GUI starts the daemon on startup if it is not reachable — via `systemctl --user start th-linux-lcd` when the service is installed, otherwise by spawning `th-linux-lcd --daemon`. Only one daemon can run per user: it holds an exclusive lock on `$XDG_RUNTIME_DIR/th-lcd.lock`.

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

| Command | GUI waits | Daemon waits for USB thread |
|---------|-----------|-----------------------------|
| Most commands | 10 s | 10 s |
| `UploadStandby` / `UploadBoot` | 210 s | 180 s (transfer + 30 s flash settle for boot) |

Requests are capped at 64 KiB per connection and 16 concurrent connections. Commands are queued with `try_send`; a full queue returns "busy" instead of blocking. An upload that cannot start within 10 s is cancelled rather than written to flash later.

### Validation

The daemon validates everything it receives before touching the device: ids must be UUIDs, fps is clamped to 1–30, `frame_count` to 1–60, frame files are size-limited, and upload files are size-checked before being read and then fully validated (`shared::validate_standby_jpeg`, `shared::validate_boot_container`). The socket lives in the per-user `$XDG_RUNTIME_DIR` (mode 0700) with mode 0600; there is no `/tmp` fallback.

---

## USB thread

The USB thread owns the device handle and runs an event loop:

1. If no device: try `usb::open()` every 2 s (errors are logged only when they change). While idle, check every 5 s that the device is still on the bus. A libusb hotplug callback (on its own `hotplug` thread) wakes the loop immediately when the cooler is plugged in or unplugged, so both are noticed at once; polling remains the fallback. An active stream is kept across an unplug and resumes when the device returns.
2. Publish `DaemonStatus` when it changes.
3. Block on the command channel until the next frame is due, the flash-settle period ends, or the next reconnect attempt — whichever is first.
4. If streaming and a frame is due: `usb::send_frame()`; on error drop the handle and reconnect.

After a boot upload commits, the thread enters a 30 s settle period: no frames, no screen commands, no new uploads; the upload reply is sent when it ends.

On SIGINT/SIGTERM the thread finishes any upload and settle period, then exits; dropping the handle releases the interfaces and re-attaches the kernel driver. If the USB thread panics the process exits non-zero so systemd restarts it.

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

Imports (file picker + Pillow), rotation re-encoding and uploads run on a background thread and post a `BgDone` result back via a `channel`; the UI thread never blocks on them. Uploads to flash require confirmation in a dialog.

---

## Module map

| File | Responsibility |
|------|---------------|
| `src/main.rs` | Entry point — routes to `daemon::run()` or `gui::run()` |
| `src/daemon.rs` | Unix socket server, validation, USB thread (streaming, uploads, reconnect) |
| `src/gui.rs` | egui UI: all three tabs, gallery, circle preview, background tasks, single-instance guard |
| `src/tray.rs` | System tray icon (`ksni`, D-Bus StatusNotifierItem) |
| `src/lib.rs` | Library crate shared by the app and the example |
| `src/usb.rs` | Low-level USB protocol: open, send_frame, set_screen, upload |
| `src/shared.rs` | IPC types, data model, atomic persistence, boot container build/validate, unit tests |
| `src/import.rs` | python3/Pillow import scripts (paths passed as argv) |
| `examples/test_boot.rs` | Standalone upload tool for debugging the boot animation protocol |

---

## Data persistence

All state is stored in `~/.local/share/th-linux-lcd/`:

```
videos.json      Vec<Video>      — imported GIF manifest
standby.json     Vec<StandbyImage>
boot.json        Vec<BootAnim>
autoplay.json    id of the video to resume when the daemon starts
on_device.json   ids of the standby image / boot animation last written by the app (the device has no read-back command)
frames/<id>/     f00001.jpg … fNNNNN.jpg  (extracted animation frames)
frames/<id>-rotated/                      (cached rotated frames)
standby/<id>.jpg                          (converted standby images)
boot/<id>.bootanim                        (Update_Boot_GIF container)
```

Manifests store base64-encoded 96×96 JPEG thumbnails inline to avoid extra disk reads on startup. They are written atomically (temp file + fsync + rename). A manifest that fails to parse is copied to `*.json.corrupt` and reported in the status bar instead of being silently overwritten.
