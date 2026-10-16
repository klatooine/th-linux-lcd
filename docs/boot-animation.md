# Boot Animation

The boot animation plays automatically when the device is powered on, before the host computer connects. It is stored in a dedicated flash slot and uses a proprietary binary container format called `Update_Boot_GIF`.

---

## How it works

1. **Import**: A GIF is selected. Python/Pillow extracts up to 40 frames and encodes them as 480×480 JPEGs with 4:2:0 subsampling.
2. **Build container**: The frames are packed into the `Update_Boot_GIF` binary format with a checksum header, frame table, and trailer.
3. **Validate**: The daemon checks the container byte-for-byte (magic, header fields, checksum, 40 frames, timing ≥ 80 ms, every frame table entry, every frame is a JPEG, trailer) and that it is at most 1.25 MiB. An invalid container is refused before any USB traffic.
4. **Upload**: The daemon transmits it using the kind=1 upload protocol. INIT and every chunk must be acknowledged; on any failure the upload stops and the commit is **not** sent.
5. **Flash**: CMD `0x14` triggers the device to burn the data to the boot flash slot, then `Finalize` (CMD `0x82`). The daemon then waits 30 s without talking to the device (status **WRITING FLASH**) before reporting success. Stopping the daemon during this time is deferred until it finishes.
6. **Result**: Power cycle the device — the animation plays before the OS connects.

---

## JPEG requirements

| Property | Value |
|---------|-------|
| Dimensions | 480×480 px |
| Chroma subsampling | **4:2:0** (`subsampling=2` in Pillow) |
| JFIF version | 1.1 |
| DPI | 96×96 |
| Encoding quality | 75, stepped down (to 20) until each frame ≤ 31 KiB |
| Frames | exactly 40 (shorter GIFs are padded by repeating the last frame) |
| Max container size | 1.25 MiB (app limit; known-good container is 1,238,420 bytes) |
| Max FPS | **12.5 fps** (timing byte ≥ 80 ms — firmware rejects faster containers) |

> The subsampling is opposite to standby images. Boot frames must use 4:2:0; standby images must use 4:4:4.

Frame extraction:

```python
img.resize((480, 480), Image.LANCZOS)
img.save(buf, 'JPEG', quality=75, subsampling=2, dpi=(96, 96))
```

---

## Container format (`Update_Boot_GIF`)

The `.bootanim` file is a proprietary binary container:

```
Offset  Size  Field
──────  ────  ─────────────────────────────────────────────────────────────
 0       4    checksum  = -(full_size + n_frames × 17) as int32 (LE)
 4       4    content_size = full_size - 16
 8       4    tbl_size = n_frames × 16
12       1    0x10
13       1    0x00
14       1    n_frames
15       1    timing = round(1000 / fps)  in ms/frame (min 80)
16      16    magic: "Update_Boot_GIF\0"

── Frame table (n_frames × 16 bytes) ────────────────────────────────────
32 + i×16   4    flags  = -(offset + 8 + frame_size) as int32 (LE)
32 + i×16+4 4    offset = byte offset of this frame's sub-header
32 + i×16+8 4    size   = JPEG byte count (excluding the 8-byte sub-header)
32+i×16+12  4    0x40000008  (constant observed in all traces)

── Frame data (n_frames entries) ────────────────────────────────────────
offset[i]   8    sub-header: "NNN.jpg\0" (null-padded to 8 bytes)
offset[i]+8 size JPEG data

── Trailer ───────────────────────────────────────────────────────────────
last 16     15×0x00 + 0x10
```

### Checksum formula

```
full_size    = container.len()
checksum     = -(full_size + n_frames * 17) as i32
content_size = full_size - 16
```

This was confirmed by comparing the formula against a known-good container extracted from a Windows USB trace:

```
full_size = 1,238,420   n_frames = 40
checksum  = -(1,238,420 + 680) = -1,239,100 = 0xFFED17C4  ✓
```

---

## USB upload protocol (kind=1)

The boot upload uses the same chunked protocol as standby (kind=0) with two differences:

1. **INIT byte[4-7]** = `0x01 0x00 0x00 0x00` (kind=1 instead of kind=0)
2. **CMD 0x14** is sent after the last chunk (before FINALIZE):

```
1. Pre-finalize:  CMD 0x82  → ACK on EP 0x82
2. INIT kind=1:   CMD 0x0a  [0x0a, 0x01, 0x00, 0x80,
                              0x01, 0x00, 0x00, 0x00,   ← kind=1
                              size_u32_le, 0x00…]        → ACK
3. Chunks:        CMD 0x0b  (same as standby)            → ACK per chunk
4. Commit:        CMD 0x14  [0x14, 0x01, 0x00, 0x80, 0x62, 0x00…]
                             fire-and-forget — no ACK response
5. Finalize:      CMD 0x82  → ACK on EP 0x82
```

> CMD `0x14` is **fire-and-forget**. The device returns 0 bytes on EP `0x82`. FINALIZE follows immediately (as in the Windows trace); the device then needs ~15–30 s to finish writing flash. **Do not unplug or power off during that time.**

---

## Rotation

Rotation is re-encoded at import time. The source GIF path is stored in `boot.json` (`source_gif` field). When the user rotates, the container is rebuilt from the original GIF with the new angle applied during Pillow resizing:

```python
frame.rotate(-rotation, expand=False)
```

---

## Testing tool

`examples/test_boot.rs` is a standalone upload tool that bypasses the daemon (stop it first with `systemctl --user stop th-linux-lcd`). It is not built or installed by default:

```bash
cargo run --example test_boot -- animation.gif --dry-run
cargo run --example test_boot -- animation.gif
cargo run --example test_boot -- --binary container.bootanim
```

It uses the same container builder, validation and upload code as the app, prints the container structure, and asks for confirmation (`--yes` to skip) before writing.

---

## Storage

Containers are stored at `~/.local/share/th-linux-lcd/boot/<uuid>.bootanim`. The source GIF path is kept in `boot.json` for rotation re-encoding.
