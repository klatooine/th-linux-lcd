use rusb::DeviceHandle;
use std::time::Duration;

const VID: u16 = 0x264a;
const PID: u16 = 0x233c;
const EP_OUT: u8 = 0x03;   // interface 1 — animation frames
const EP_IN: u8 = 0x84;    // interface 1 — drain after each frame
const EP_CTRL: u8 = 0x01;  // interface 0 — control commands
const EP_CTRL_IN: u8 = 0x82; // interface 0 — ACK responses
const PKT: usize = 1024;
const PAYLOAD: usize = PKT - 4;
const CTRL: usize = 440;

pub fn open() -> Option<DeviceHandle<rusb::GlobalContext>> {
    let dev = match rusb::open_device_with_vid_pid(VID, PID) {
        Some(d) => d,
        None => {
            eprintln!("[usb] device {:04x}:{:04x} not found", VID, PID);
            return None;
        }
    };
    for iface in 0..=1u8 {
        match dev.kernel_driver_active(iface) {
            Ok(true) => {
                if let Err(e) = dev.detach_kernel_driver(iface) {
                    eprintln!("[usb] detach iface {}: {}", iface, e);
                    return None;
                }
            }
            Ok(false) => {}
            Err(e) => eprintln!("[usb] kernel_driver_active iface {}: {}", iface, e),
        }
        if let Err(e) = dev.claim_interface(iface) {
            eprintln!("[usb] claim iface {}: {}", iface, e);
            return None;
        }
    }
    eprintln!("[usb] device opened ok");
    Some(dev)
}

pub fn send_frame(dev: &DeviceHandle<rusb::GlobalContext>, data: &[u8]) -> bool {
    let n = (data.len() + PAYLOAD - 1) / PAYLOAD;
    for j in 0..n {
        let off = j * PAYLOAD;
        let chunk = &data[off..(off + PAYLOAD).min(data.len())];
        let mut pkt = vec![0u8; PKT];
        pkt[0] = 0x08;
        pkt[1] = if j == 0 { n as u8 } else { ((j - 1) % 0x15) as u8 + 1 };
        pkt[3] = if j == 0 { 0x80 } else { 0x00 };
        pkt[4..4 + chunk.len()].copy_from_slice(chunk);
        if dev.write_interrupt(EP_OUT, &pkt, Duration::from_secs(2)).is_err() {
            return false;
        }
    }
    // drain response (required or device locks up)
    let mut r = [0u8; 16];
    let _ = dev.read_interrupt(EP_IN, &mut r, Duration::from_millis(1));
    true
}

pub fn screen_on(dev: &DeviceHandle<rusb::GlobalContext>) {
    let mut p = vec![0u8; CTRL];
    p[0] = 0x12; p[1] = 0x01; p[2] = 0x00; p[3] = 0x80; p[4] = 0x64;
    let _ = dev.write_interrupt(EP_CTRL, &p, Duration::from_secs(2));
}

pub fn screen_off(dev: &DeviceHandle<rusb::GlobalContext>) {
    let mut p = vec![0u8; CTRL];
    p[0] = 0x12; p[1] = 0x01; p[2] = 0x00; p[3] = 0x80; p[4] = 0x00;
    let _ = dev.write_interrupt(EP_CTRL, &p, Duration::from_secs(2));
}

pub fn upload_standby(dev: &DeviceHandle<rusb::GlobalContext>, jpeg: &[u8]) -> Result<(), String> {
    const CHUNK: usize = 10240;
    const HDR_DATA: usize = 424;
    const PKT_DATA: usize = 436;
    let sz = jpeg.len();

    // Pre-finalize
    let mut p = vec![0u8; CTRL];
    p[0] = 0x82; p[1] = 0x01; p[2] = 0x00; p[3] = 0x80;
    let _ = dev.write_interrupt(EP_CTRL, &p, Duration::from_secs(2));
    let mut r = vec![0u8; CTRL];
    let _ = dev.read_interrupt(EP_CTRL_IN, &mut r, Duration::from_millis(200));

    // INIT
    let mut p = vec![0u8; CTRL];
    p[0] = 0x0a; p[1] = 0x01; p[2] = 0x00; p[3] = 0x80;
    p[8..12].copy_from_slice(&(sz as u32).to_le_bytes());
    dev.write_interrupt(EP_CTRL, &p, Duration::from_secs(2)).map_err(|e| e.to_string())?;
    let mut r = vec![0u8; CTRL];
    let _ = dev.read_interrupt(EP_CTRL_IN, &mut r, Duration::from_millis(500));

    // Chunks
    let mut offset = 0usize;
    while offset < sz {
        let this_chunk = CHUNK.min(sz - offset);
        let copy_len = HDR_DATA.min(this_chunk);
        let data_pkts = if this_chunk > HDR_DATA {
            (this_chunk - HDR_DATA + PKT_DATA - 1) / PKT_DATA
        } else { 0 };
        let npkt = (1 + data_pkts) as u8;
        let progress = (((offset + this_chunk) as u64 * 100) / sz as u64) as u32;

        let mut p = vec![0u8; CTRL];
        p[0] = 0x0b; p[1] = npkt; p[2] = 0x00; p[3] = 0x80;
        p[4..8].copy_from_slice(&(offset as u32).to_le_bytes());
        p[8..12].copy_from_slice(&(this_chunk as u32).to_le_bytes());
        p[12..16].copy_from_slice(&progress.to_le_bytes());
        p[16..16 + copy_len].copy_from_slice(&jpeg[offset..offset + copy_len]);
        dev.write_interrupt(EP_CTRL, &p, Duration::from_secs(2)).map_err(|e| e.to_string())?;

        let mut data_off = offset + copy_len;
        let mut remaining = this_chunk - copy_len;
        let mut seq = 1u8;
        while remaining > 0 {
            let n = PKT_DATA.min(remaining);
            let mut p = vec![0u8; CTRL];
            p[0] = 0x0b; p[1] = seq; p[2] = 0x00; p[3] = 0x00;
            p[4..4 + n].copy_from_slice(&jpeg[data_off..data_off + n]);
            dev.write_interrupt(EP_CTRL, &p, Duration::from_secs(2)).map_err(|e| e.to_string())?;
            data_off += n;
            remaining -= n;
            seq += 1;
        }

        let mut r = vec![0u8; CTRL];
        let _ = dev.read_interrupt(EP_CTRL_IN, &mut r, Duration::from_millis(500));
        offset += this_chunk;
    }

    // Finalize
    let mut p = vec![0u8; CTRL];
    p[0] = 0x82; p[1] = 0x01; p[2] = 0x00; p[3] = 0x80;
    dev.write_interrupt(EP_CTRL, &p, Duration::from_secs(2)).map_err(|e| e.to_string())?;
    let mut r = vec![0u8; CTRL];
    let _ = dev.read_interrupt(EP_CTRL_IN, &mut r, Duration::from_millis(500));
    Ok(())
}
