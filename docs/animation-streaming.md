# Animation Streaming

Real-time GIF streaming to the LCD display using USB interrupt transfers.

---

## How it works

1. **Import**: A GIF is selected via the file picker. Python/Pillow extracts up to 60 frames, resizes each to 480×480, and encodes them as JPEG files in `~/.local/share/th-linux-lcd/frames/<uuid>/`.
2. **Play**: The GUI sends a `Play` command to the daemon with the frames directory, frame count, and FPS.
3. **Streaming loop**: The USB thread loads all frames into memory and sends them one by one on a wall-clock schedule.
4. **Stop**: The GUI sends a `Stop` command; the daemon clears the frame buffer.

---

## JPEG requirements

Each frame must be:

| Property | Value |
|---------|-------|
| Dimensions | 480×480 px |
| Chroma subsampling | **4:4:4** (YCbCr — any other value corrupts the lower half of the display) |
| JFIF version | 1.1 |
| DPI | 96×96 |
| Max size | ~20 KB (quality ~25) for 60 fps headroom |

The extraction script uses Pillow:

```python
frame.resize((480, 480), Image.LANCZOS)
frame.save(buf, 'JPEG', quality=25, subsampling=0, dpi=(96, 96))
```

`subsampling=0` is Pillow's constant for 4:4:4.

---

## USB protocol

Animation uses **Interface 1**:

| Endpoint | Direction | Size | Purpose |
|---------|-----------|------|---------|
| `0x03` | OUT | 1024 B | Frame data packets |
| `0x84` | IN | 16 B | Drain after each frame (required — device locks up otherwise) |

### Packet format

Each JPEG is split into 1020-byte chunks and wrapped in 1024-byte packets:

```
Byte 0:  0x08                     — command byte
Byte 1:  total_packets (first)    — or seq (1-based mod 0x15) for subsequent
Byte 2:  0x00
Byte 3:  0x80 (first) / 0x00 (subsequent)
Byte 4…: JPEG data (up to 1020 bytes)
```

After all packets for a single frame are sent, read 16 bytes from EP `0x84` with a 1 ms timeout. This drain is mandatory.

### Timing

The daemon schedules frames using `Instant::now()`:

```rust
if now >= state.next_frame {
    usb::send_frame(dev, &frames[idx]);
    state.next_frame = now + Duration::from_secs_f32(1.0 / fps);
}
```

The loop sleeps up to `min(time_to_next_frame, 5ms)` so USB commands remain responsive during playback.

---

## Frame extraction pipeline

```
GIF file
  └─ Pillow: seek each frame, convert RGB
       └─ resize 480×480 (LANCZOS)
            └─ JPEG encode (quality=25, subsampling=0, dpi=96)
                 └─ save f00001.jpg … f00060.jpg
```

Frame count is capped at 60. If the GIF has more frames, they are sampled uniformly:

```python
step = max(len(frames) // 60, 1)
sel = frames[::step][:60]
```

FPS is derived from the GIF's frame durations:

```python
total_ms = sum(f.info.get('duration', 100) for f in frames)
fps = min(max(1000.0 * len(frames) / total_ms, 5.0), 60.0)
```

---

## Rotation

Rotation is stored on the `Video` struct (`rotation: i32`, multiples of 90°). When the user changes rotation, the daemon re-sends frames from a `-rotated` copy of the frames directory. Rotated copies are generated on demand via Pillow:

```python
frame.rotate(-rotation, expand=False)
```

Rotated frames are cached in `frames/<uuid>-rotated/` so subsequent plays of the same rotation are instant.
