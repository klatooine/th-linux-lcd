/// Standalone boot animation upload test.
/// Usage: test_boot <input.gif> [--dry-run]
///
/// Builds the Update_Boot_GIF container from a GIF and uploads it directly
/// via USB, bypassing the daemon. Prints detailed diagnostic output at each
/// step so protocol issues can be spotted without running the full app.

use rusb::DeviceHandle;
use std::io::Write;
use std::process::Command;
use std::time::Duration;

const VID: u16 = 0x264a;
const PID: u16 = 0x233c;
const EP_OUT: u8 = 0x03;
const EP_IN: u8 = 0x84;
const EP_CTRL: u8 = 0x01;
const EP_CTRL_IN: u8 = 0x82; // real ACK endpoint (440-byte, one per completed command)
const CTRL: usize = 440;
const CHUNK: usize = 10240;
const HDR_DATA: usize = 424;
const PKT_DATA: usize = 436;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <input.gif|--binary file.bootanim> [--dry-run]", args[0]);
        std::process::exit(1);
    }
    let dry_run = args.iter().any(|a| a == "--dry-run");

    // --binary: skip all GIF processing, upload a pre-built .bootanim container directly
    if args[1] == "--binary" {
        let path = args.get(2).expect("--binary requires a file path");
        let container = std::fs::read(path).expect("failed to read binary");
        println!("[test_boot] Direct binary upload: {} ({} bytes)", path, container.len());
        let n     = container[14];
        let tbl   = u32::from_le_bytes(container[8..12].try_into().unwrap());
        let cksum = u32::from_le_bytes(container[0..4].try_into().unwrap());
        let timing = container[15];
        println!("    checksum : 0x{cksum:08x}");
        println!("    n_frames : {n}");
        println!("    tbl_size : {tbl} bytes");
        println!("    timing   : {timing} ms/frame → {:.1} fps", 1000.0 / timing as f32);
        if dry_run { println!("[DRY RUN] skipping USB"); return; }
        let dev = match rusb::open_device_with_vid_pid(VID, PID) {
            Some(d) => { println!("device found"); d }
            None => { eprintln!("device not found"); std::process::exit(1); }
        };
        for iface in 0..=1u8 {
            match dev.kernel_driver_active(iface) {
                Ok(true) => { dev.detach_kernel_driver(iface).ok(); }
                _ => {}
            }
            dev.claim_interface(iface).ok();
        }
        match upload(&dev, &container) {
            Ok(()) => println!("\n✓ Upload complete"),
            Err(e) => eprintln!("\n✗ Upload failed: {e}"),
        }
        return;
    }

    let gif_path = &args[1];

    if dry_run {
        println!("[test_boot] DRY RUN — no USB writes");
    }

    // ── Step 1: Extract frames via Pillow ─────────────────────────────────────
    println!("\n[1] Extracting frames from: {}", gif_path);
    let tmp_dir = "/tmp/test_boot_frames";
    std::fs::create_dir_all(tmp_dir).unwrap();

    let script = format!(r#"
from PIL import Image
import io, os, json
src={src:?}; tmp={tmp:?}
os.makedirs(tmp, exist_ok=True)
img = Image.open(src)
frames = []
try:
    while True:
        frames.append(img.copy().convert('RGB'))
        img.seek(img.tell() + 1)
except EOFError:
    pass
TARGET = 40
n = min(len(frames), TARGET)
step = max(len(frames) // n, 1)
sel = frames[::step][:n]
# Pad to exactly TARGET frames by repeating the last frame
while len(sel) < TARGET:
    sel.append(sel[-1].copy())
total_ms = getattr(img, 'info', {{}}).get('duration', 80) * len(frames)
fps = min(max(1000.0 * len(frames) / max(total_ms, 1), 5.0), 12.5)
sizes = []
for i, frame in enumerate(sel):
    r = frame.resize((480, 480), Image.LANCZOS)
    b = io.BytesIO()
    r.save(b, 'JPEG', quality=75, subsampling=2, dpi=(96, 96))
    data = b.getvalue()
    sizes.append(len(data))
    open(os.path.join(tmp, f'f{{i:03d}}.jpg'), 'wb').write(data)
print(json.dumps({{"n": len(sel), "fps": fps, "min_size": min(sizes), "max_size": max(sizes), "avg_size": sum(sizes)//len(sizes)}}))
"#, src = gif_path, tmp = tmp_dir);

    let out = Command::new("python3").args(["-c", &script]).output()
        .expect("python3 not found");
    if !out.status.success() {
        eprintln!("[ERROR] Frame extraction failed:\n{}", String::from_utf8_lossy(&out.stderr));
        std::process::exit(1);
    }
    let info: serde_json::Value = serde_json::from_str(
        String::from_utf8_lossy(&out.stdout).trim()).unwrap_or_default();
    let n_frames = info["n"].as_u64().unwrap_or(1) as usize;
    let fps      = info["fps"].as_f64().unwrap_or(12.5) as f32;
    println!("    frames={n_frames}  fps={fps:.1}  size min={} max={} avg={} bytes",
        info["min_size"], info["max_size"], info["avg_size"]);

    // ── Step 2: Load frames ───────────────────────────────────────────────────
    println!("\n[2] Loading JPEG frames");
    let mut jpeg_frames: Vec<Vec<u8>> = Vec::with_capacity(n_frames);
    for i in 0..n_frames {
        let p = format!("{}/f{:03}.jpg", tmp_dir, i);
        let data = std::fs::read(&p).unwrap_or_else(|e| { eprintln!("  [ERROR] {p}: {e}"); vec![] });
        println!("    f{i:03}.jpg  {} bytes", data.len());
        jpeg_frames.push(data);
    }

    // ── Step 3: Build container ───────────────────────────────────────────────
    println!("\n[3] Building Update_Boot_GIF container");
    let container = build_container(&jpeg_frames, fps);
    println!("    total size: {} bytes ({:.1} KB)", container.len(), container.len() as f64 / 1024.0);

    // Verify container structure
    let n     = container[14];
    let tbl   = u32::from_le_bytes(container[8..12].try_into().unwrap());
    let magic = std::str::from_utf8(&container[16..31]).unwrap_or("?");
    let timing = container[15];
    println!("    magic    : {:?}", magic);
    println!("    n_frames : {n}");
    println!("    tbl_size : {tbl} bytes = {} entries × 16", tbl / 16);
    println!("    timing   : {timing} ms/frame → {:.1} fps", 1000.0 / timing as f32);

    // Check first frame entry
    let entry_off = 32usize;
    let f0_flags = u32::from_le_bytes(container[entry_off..entry_off+4].try_into().unwrap());
    let f0_off   = u32::from_le_bytes(container[entry_off+4..entry_off+8].try_into().unwrap());
    let f0_size  = u32::from_le_bytes(container[entry_off+8..entry_off+12].try_into().unwrap());
    let f0_flags2= u32::from_le_bytes(container[entry_off+12..entry_off+16].try_into().unwrap());
    println!("    frame[0] : flags=0x{f0_flags:08x} offset={f0_off} size={f0_size} flags2=0x{f0_flags2:08x}");
    let sub_hdr = std::str::from_utf8(&container[f0_off as usize..f0_off as usize + 7])
        .unwrap_or("?");
    let jpeg_magic = &container[f0_off as usize + 8..f0_off as usize + 11];
    println!("    frame[0] sub-header : {:?}", sub_hdr);
    println!("    frame[0] JPEG magic : {:02x} {:02x} {:02x}  (should be ff d8 ff)",
        jpeg_magic[0], jpeg_magic[1], jpeg_magic[2]);

    // Save container to disk for inspection
    let out_path = "/tmp/test_boot.bootanim";
    std::fs::write(out_path, &container).unwrap();
    println!("    saved to: {out_path}");

    if dry_run {
        println!("\n[DRY RUN] Skipping USB upload.");
        return;
    }

    // ── Step 4: Open USB device ───────────────────────────────────────────────
    println!("\n[4] Opening USB device {:04x}:{:04x}", VID, PID);
    let dev = match rusb::open_device_with_vid_pid(VID, PID) {
        Some(d) => { println!("    found"); d }
        None    => { eprintln!("    [ERROR] device not found"); std::process::exit(1); }
    };
    for iface in 0..=1u8 {
        match dev.kernel_driver_active(iface) {
            Ok(true) => {
                dev.detach_kernel_driver(iface)
                    .unwrap_or_else(|e| eprintln!("    detach iface {iface}: {e}"));
            }
            _ => {}
        }
        dev.claim_interface(iface)
            .unwrap_or_else(|e| { eprintln!("    [ERROR] claim iface {iface}: {e}"); });
        println!("    claimed interface {iface}");
    }

    // ── Step 5: Upload ────────────────────────────────────────────────────────
    println!("\n[5] Uploading ({} bytes)", container.len());
    match upload(&dev, &container) {
        Ok(()) => println!("\n✓ Upload complete"),
        Err(e) => eprintln!("\n✗ Upload failed: {e}"),
    }
}

fn build_container(frames: &[Vec<u8>], fps: f32) -> Vec<u8> {
    let n = frames.len() as u8;
    let tbl_size = (n as u32) * 16;
    let timing = (1000.0 / fps.max(1.0)).round() as u8;
    const SUB_HDR: u32 = 8;
    const TBL_OFF: u32 = 32;

    let data_start = TBL_OFF + tbl_size;
    let mut offsets: Vec<u32> = Vec::with_capacity(n as usize);
    let mut cur = data_start;
    for frame in frames {
        offsets.push(cur);
        cur += SUB_HDR + frame.len() as u32;
    }
    // cur is the offset where the trailer will start.
    // Full container = cur + 16 (trailer). Content = full - 16 (outer header) = cur.
    let content_size = cur;
    let full_size    = cur + 16;

    // Checksum: -(full_container_size + n_frames * 17) as int32
    let checksum = (-(full_size as i64 + n as i64 * 17)) as i32;

    let mut buf = Vec::with_capacity(full_size as usize);

    // 16-byte outer header
    buf.extend_from_slice(&checksum.to_le_bytes());
    buf.extend_from_slice(&content_size.to_le_bytes());
    buf.extend_from_slice(&tbl_size.to_le_bytes());
    buf.push(0x10);
    buf.push(0x00);
    buf.push(n);
    buf.push(timing);

    // Magic
    let mut magic = [0u8; 16];
    magic[..15].copy_from_slice(b"Update_Boot_GIF");
    buf.extend_from_slice(&magic);

    // Frame table — flags[i] = -(offset[i] + 8 + frame_size[i])
    for (&off, frame) in offsets.iter().zip(frames.iter()) {
        let flags = (-(off as i64 + 8 + frame.len() as i64)) as i32;
        buf.extend_from_slice(&flags.to_le_bytes());
        buf.extend_from_slice(&off.to_le_bytes());
        buf.extend_from_slice(&(frame.len() as u32).to_le_bytes());
        buf.extend_from_slice(&0x40000008u32.to_le_bytes());
    }

    // Frame data
    for (i, frame) in frames.iter().enumerate() {
        let name = format!("{:03}.jpg", i);
        let mut sub = [0u8; 8];
        let nb = name.as_bytes();
        sub[..nb.len().min(7)].copy_from_slice(&nb[..nb.len().min(7)]);
        buf.extend_from_slice(&sub);
        buf.extend_from_slice(frame);
    }

    // 16-byte trailer (confirmed in Windows trace: 15 zero bytes + 0x10)
    buf.extend_from_slice(&[0u8; 15]);
    buf.push(0x10);

    buf
}

fn upload(dev: &DeviceHandle<rusb::GlobalContext>, data: &[u8]) -> Result<(), String> {
    let sz = data.len();
    let t2 = Duration::from_secs(2);

    // Pre-finalize
    print!("    pre-finalize ... ");
    std::io::stdout().flush().ok();
    let mut p = vec![0u8; CTRL];
    p[0] = 0x82; p[1] = 0x01; p[2] = 0x00; p[3] = 0x80;
    dev.write_interrupt(EP_CTRL, &p, t2).map_err(|e| e.to_string())?;
    let mut r = vec![0u8; CTRL];
    let n = dev.read_interrupt(EP_CTRL_IN, &mut r, Duration::from_millis(500))
        .unwrap_or(0);
    println!("ok (ACK {n}B: {:02x} {:02x} {:02x} {:02x})", r[0], r[1], r[2], r[3]);

    // INIT with kind=1 — boot animation slot (kind=0 is standby slot, confirmed)
    print!("    INIT (kind=1, size={sz}) ... ");
    std::io::stdout().flush().ok();
    let mut p = vec![0u8; CTRL];
    p[0] = 0x0a; p[1] = 0x01; p[2] = 0x00; p[3] = 0x80;
    p[4..8].copy_from_slice(&1u32.to_le_bytes());
    p[8..12].copy_from_slice(&(sz as u32).to_le_bytes());
    dev.write_interrupt(EP_CTRL, &p, t2).map_err(|e| e.to_string())?;
    let mut r = vec![0u8; CTRL];
    let n = dev.read_interrupt(EP_CTRL_IN, &mut r, Duration::from_millis(500))
        .unwrap_or(0);
    println!("ok (ACK {n}B: {:02x} {:02x} {:02x} {:02x})", r[0], r[1], r[2], r[3]);

    // Chunks
    let n_chunks = (sz + CHUNK - 1) / CHUNK;
    let mut offset = 0usize;
    while offset < sz {
        let this_chunk = CHUNK.min(sz - offset);
        let copy_len   = HDR_DATA.min(this_chunk);
        let remaining_after_hdr = this_chunk - copy_len;
        let data_pkts  = (remaining_after_hdr + PKT_DATA - 1) / PKT_DATA;
        let npkt       = (1 + data_pkts) as u8;
        let progress   = (((offset + this_chunk) as u64 * 100) / sz as u64) as u32;
        let chunk_idx  = offset / CHUNK + 1;

        print!("    chunk {chunk_idx}/{n_chunks} offset={offset} size={this_chunk} npkt={npkt} progress={progress}% ... ");
        std::io::stdout().flush().ok();

        let mut p = vec![0u8; CTRL];
        p[0] = 0x0b; p[1] = npkt; p[2] = 0x00; p[3] = 0x80;
        p[4..8].copy_from_slice(&(offset as u32).to_le_bytes());
        p[8..12].copy_from_slice(&(this_chunk as u32).to_le_bytes());
        p[12..16].copy_from_slice(&progress.to_le_bytes());
        p[16..16 + copy_len].copy_from_slice(&data[offset..offset + copy_len]);
        dev.write_interrupt(EP_CTRL, &p, t2).map_err(|e| e.to_string())?;

        let mut data_off  = offset + copy_len;
        let mut remaining = remaining_after_hdr;
        let mut seq = 1u8;
        while remaining > 0 {
            let n = PKT_DATA.min(remaining);
            let mut p = vec![0u8; CTRL];
            p[0] = 0x0b; p[1] = seq; p[2] = 0x00; p[3] = 0x00;
            p[4..4 + n].copy_from_slice(&data[data_off..data_off + n]);
            dev.write_interrupt(EP_CTRL, &p, t2).map_err(|e| e.to_string())?;
            data_off  += n;
            remaining -= n;
            seq += 1;
        }

        let mut r = vec![0u8; CTRL];
        let n = dev.read_interrupt(EP_CTRL_IN, &mut r, Duration::from_millis(500))
            .unwrap_or(0);
        println!("ACK {n}B: {:02x} {:02x} {:02x} {:02x}", r[0], r[1], r[2], r[3]);

        offset += this_chunk;
    }

    // CMD 0x14 — commit to flash (fire-and-forget; confirmed from pcap trace, no real ACK)
    print!("    CMD 0x14 commit ... ");
    std::io::stdout().flush().ok();
    {
        let mut p = vec![0u8; CTRL];
        p[0] = 0x14; p[1] = 0x01; p[2] = 0x00; p[3] = 0x80; p[4] = 0x62;
        match dev.write_interrupt(EP_CTRL, &p, t2) {
            Err(e) => println!("write failed: {e}"),
            Ok(_) => {
                let mut r = vec![0u8; CTRL];
                let n = dev.read_interrupt(EP_CTRL_IN, &mut r, Duration::from_millis(500)).unwrap_or(0);
                println!("done (ACK {n}B: {:02x} {:02x} {:02x} {:02x})", r[0], r[1], r[2], r[3]);
            }
        }
    }

    // Poll CMD 0x82 every 2s for 30s — keep USB alive during flash write.
    println!("    waiting for flash write (~30s) ...");
    for i in 0..15 {
        std::thread::sleep(Duration::from_secs(2));
        let mut p = vec![0u8; CTRL];
        p[0] = 0x82; p[1] = 0x01; p[2] = 0x00; p[3] = 0x80;
        let _ = dev.write_interrupt(EP_CTRL, &p, t2);
        let mut r = vec![0u8; CTRL];
        let n = dev.read_interrupt(EP_CTRL_IN, &mut r, Duration::from_millis(500)).unwrap_or(0);
        println!("      poll {}/15: ACK {n}B: {:02x} {:02x} {:02x} {:02x}", i+1, r[0], r[1], r[2], r[3]);
    }

    // Finalize
    print!("    finalize ... ");
    std::io::stdout().flush().ok();
    let mut p = vec![0u8; CTRL];
    p[0] = 0x82; p[1] = 0x01; p[2] = 0x00; p[3] = 0x80;
    dev.write_interrupt(EP_CTRL, &p, t2).map_err(|e| e.to_string())?;
    let mut r = vec![0u8; CTRL];
    let n = dev.read_interrupt(EP_CTRL_IN, &mut r, Duration::from_millis(500)).unwrap_or(0);
    println!("ok (ACK {n}B: {:02x} {:02x} {:02x} {:02x})", r[0], r[1], r[2], r[3]);

    Ok(())
}
