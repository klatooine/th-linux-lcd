# Standby Image

The standby image is a static JPEG stored in the device's flash memory. It is displayed whenever the device is powered on but no animation is actively being streamed.

---

## How it works

1. **Import**: An image (JPEG, PNG, or GIF) is selected. Python/Pillow resizes it to 480×480 and encodes it as a JPEG with the correct subsampling.
2. **Upload**: The GUI sends an `UploadStandby` command to the daemon with the path to the encoded JPEG.
3. **Validate**: The daemon checks the file is a 480×480 JPEG no larger than 256 KiB. Anything else is refused before any USB traffic.
4. **Flash write**: The daemon transmits it to the device using the upload protocol (kind=0). Every step must be acknowledged by the device; on any failure the upload stops.

---

## JPEG requirements

| Property | Value |
|---------|-------|
| Dimensions | 480×480 px |
| Chroma subsampling | **4:4:4** (`subsampling=0` in Pillow) |
| JFIF version | 1.1 |
| DPI | 96×96 |
| Max file size | 256 KiB (conservative app limit; the real slot size is unknown) |
| Starting quality | 85, stepped down to 20 until ≤ 256 KiB |

> **Critical**: the display corrupts the lower half of the image if subsampling is not 4:4:4. This is different from boot animation frames which require 4:2:0.

The encoding script:

```python
img = Image.open(src).convert('RGB').resize((480, 480), Image.LANCZOS)
quality = 85
while True:
    buf = io.BytesIO()
    img.save(buf, 'JPEG', quality=quality, subsampling=0, dpi=(96, 96))
    if len(buf.getvalue()) <= 256 * 1024:
        break
    if quality <= 20:
        raise SystemExit('image too detailed')
    quality -= 5
```

---

## USB protocol

Standby uses the **control interface** (Interface 0):

| Endpoint | Direction | Size | Purpose |
|---------|-----------|------|---------|
| `0x01` | OUT | 440 B | Control commands |
| `0x82` | IN | 440 B | ACK (command byte echoed at `[0]`) |

### Upload sequence

```
1. Pre-finalize:  CMD 0x82  [0x82, 0x01, 0x00, 0x80, 0x00…]  → ACK
2. INIT:          CMD 0x0a  [0x0a, 0x01, 0x00, 0x80,
                              kind=0 (u32 LE),
                              total_size (u32 LE), 0x00…]      → ACK
3. For each 10240-byte chunk:
   Header packet: CMD 0x0b  [0x0b, npkt, 0x00, 0x80,
                              offset (u32 LE),
                              chunk_size (u32 LE),
                              progress% (u32 LE),
                              first 424 bytes of chunk data]   → (no wait)
   Data packets:  CMD 0x0b  [0x0b, seq, 0x00, 0x00,
                              next 436 bytes of chunk data]    → (no wait)
   ACK:           read EP 0x82                                 → ACK
4. Finalize:      CMD 0x82  [0x82, 0x01, 0x00, 0x80, 0x00…]  → ACK
```

`kind=0` places the data in the standby image slot. `kind=1` is used for boot animations.

### Chunk parameters

```
npkt   = 1 + ceil((chunk_size - 424) / 436)
progress = floor((offset + chunk_size) * 100 / total_size)
```

---

## Storage

Converted images are stored at `~/.local/share/th-linux-lcd/standby/<uuid>.jpg` and referenced in `standby.json`.
