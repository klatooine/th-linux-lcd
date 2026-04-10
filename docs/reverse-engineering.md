# Reverse Engineering

This document describes how the USB protocol for the Thermaltake TH420V2 Ultra LCD (USB `264a:233c`) was discovered and validated.

---

## Methodology

### 1. USB capture on Windows

The official Thermaltake software was run on a Windows machine while capturing USB traffic with [Wireshark](https://www.wireshark.org/) + [USBPcap](https://desowin.org/usbpcap/).

Captures were taken for each operation:
- Streaming a GIF animation
- Uploading a standby image
- Uploading a boot animation
- Turning the screen on/off

### 2. Trace analysis

Captures were analyzed with Wireshark and `tshark`. Key steps:

```bash
# List all USB packets to/from the device
tshark -r capture.pcapng -Y "usb.idVendor==0x264a" -T fields \
  -e frame.number -e usb.endpoint_address -e usb.transfer_type \
  -e usb.data_len -e usb.capdata

# Extract raw payload of a specific frame
tshark -r capture.pcapng -Y "frame.number==42" -T fields -e usb.capdata
```

### 3. Protocol reconstruction

Packet payloads were decoded manually and by comparing multiple captures. Patterns in the first few bytes identified command types (`0x08` = frame, `0x0a` = INIT, `0x0b` = chunk, `0x12` = screen, `0x14` = commit, `0x82` = finalize).

### 4. Validation

Each discovered protocol element was validated by reimplementing it in Rust and testing against the real device. The `src/bin/test_boot.rs` tool was used extensively for boot animation protocol validation.

---

## Endpoints

| Interface | Endpoint | Dir | Size | Purpose |
|-----------|---------|-----|------|---------|
| 1 | `0x03` | OUT | 1024 B | Animation frame packets |
| 1 | `0x84` | IN | 16 B | Frame drain (required after each frame) |
| 0 | `0x01` | OUT | 440 B | Control commands |
| 0 | `0x82` | IN | 440 B | ACK (echo of command byte at `[0]`) |
| 0 | `0x81` | IN | 13 B | Status (not used) |

---

## Animation streaming protocol

Animation frames are sent as 1024-byte interrupt packets on EP `0x03`. Each JPEG is split across multiple packets:

```
First packet:
  [0x08, total_packets, 0x00, 0x80, <first 1020 bytes of JPEG>]

Subsequent packets:
  [0x08, seq_mod_0x15, 0x00, 0x00, <next 1020 bytes of JPEG>]
```

After all packets for a single frame, EP `0x84` must be drained with a 1 ms timeout read. Failure to drain causes the device to lock up and stop responding.

---

## Upload protocol (standby & boot)

Both standby images (kind=0) and boot animations (kind=1) use the same chunked protocol over the control interface.

### Pre-finalize

Always sent before INIT. Clears any in-progress transfer state:

```
[0x82, 0x01, 0x00, 0x80, 0x00…]  → ACK on EP 0x82
```

### INIT

Declares the transfer kind and total size:

```
[0x0a, 0x01, 0x00, 0x80, kind(4 LE), total_size(4 LE), 0x00…]  → ACK
```

- `kind=0`: standby image slot
- `kind=1`: boot animation slot

### Chunk header + data packets

Data is sent in 10240-byte chunks. Each chunk begins with a header packet containing the first 424 bytes of data, followed by 436-byte continuation packets:

```
Header:  [0x0b, npkt, 0x00, 0x80, offset(4), chunk_size(4), progress%(4), data[0:424]]
Data:    [0x0b, seq,  0x00, 0x00, data[424:860]]
Data:    [0x0b, seq,  0x00, 0x00, data[860:1296]]
…
         read EP 0x82  → ACK after each complete chunk
```

Where:
```
npkt     = 1 + ceil((chunk_size - 424) / 436)
progress = floor((offset + chunk_size) * 100 / total_size)
```

### Commit (boot only — CMD 0x14)

After the last chunk, boot animation uploads send a commit command that triggers the device to write the data to flash:

```
[0x14, 0x01, 0x00, 0x80, 0x62, 0x00…]
```

This is **fire-and-forget** — EP `0x82` returns 0 bytes. Confirmed from traces and empirically: no ACK is generated. FINALIZE is sent immediately after without waiting.

The byte `0x62` at position `[4]` was confirmed by iterating values 0x60–0x90 with a probe tool and observing which value matched the Windows trace.

### Finalize

Completes the transaction:

```
[0x82, 0x01, 0x00, 0x80, 0x00…]  → ACK on EP 0x82
```

---

## Boot animation container format

The `Update_Boot_GIF` container was discovered by extracting the raw payload of the USB upload transfer from a Windows trace and parsing it manually.

### Discovery process

```bash
# Extract all OUT packets to EP 0x01 from the boot animation capture
tshark -r upload_boot.pcapng \
  -Y "usb.endpoint_address==0x01 && usb.transfer_type==1" \
  -T fields -e usb.capdata > chunks.txt

# Reassemble the data payload (skip 16-byte chunk headers)
python3 reassemble.py chunks.txt > container.bin

# Inspect the header
xxd container.bin | head -4
```

The first 32 bytes revealed:
- Bytes `[0:4]`: a signed int32 (looked like a checksum — negative value)
- Bytes `[4:8]`: content size ≈ `container.len() - 16`
- Bytes `[8:12]`: `n_frames * 16` (matched frame count × 16)
- Bytes `[12]`: `0x10`
- Bytes `[14]`: frame count (`40`)
- Bytes `[15]`: timing in ms (`80` = 12.5 fps)
- Bytes `[16:32]`: `"Update_Boot_GIF\0"` (ASCII magic)

The frame table starting at byte 32 was decoded by cross-referencing offsets against actual JPEG magic bytes (`FF D8 FF`) in the payload.

### Checksum formula

Multiple containers with different frame counts were compared to derive the formula:

```
checksum = -(full_size + n_frames * 17) as i32
```

The `× 17` factor was confirmed empirically: `17 = 1 (frame count byte) + 16 (frame table entry size)`.

### Firmware FPS limit

During testing, containers with `timing < 80` (>12.5 fps) were uploaded successfully (all chunk ACKs received) but the device showed no boot animation after power cycle. Containers with `timing = 80` (12.5 fps) worked. This suggests the firmware validates the timing byte and silently rejects containers that exceed the maximum playback speed.

---

## Key findings summary

| Finding | Value | Confirmed by |
|---------|-------|-------------|
| Animation EP | `0x03` OUT, `0x84` IN | Wireshark trace |
| Control EP | `0x01` OUT, `0x82` IN | Wireshark trace |
| Frame drain required | Yes, 1 ms read on `0x84` | Lockup without it |
| Standby subsampling | 4:4:4 | Display corruption test |
| Boot subsampling | 4:2:0 | Trace JPEG inspection |
| Boot max fps | 12.5 fps (timing ≥ 80) | Silent rejection test |
| CMD 0x14 ACK | None (fire-and-forget) | EP scan, trace |
| kind=0 | Standby slot | Experimental |
| kind=1 | Boot animation slot | Experimental |
| Boot trailer | `[0x00 × 15, 0x10]` | Trace extraction |
| Checksum formula | `-(size + n × 17)` | Multi-container comparison |
