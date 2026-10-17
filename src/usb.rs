//! Low-level USB protocol for the TH420V2 Ultra LCD (VID 0x264a, PID 0x233c).
//!
//! Frame streaming is volatile. Only two device-flash slots are ever written:
//! standby (kind 0) and boot animation (kind 1). There is no firmware-update,
//! DFU or erase command anywhere in this crate.

use rusb::{DeviceHandle, GlobalContext};
use std::time::Duration;

pub type Handle = DeviceHandle<GlobalContext>;

pub const VID: u16 = 0x264a;
pub const PID: u16 = 0x233c;

const EP_FRAME_OUT: u8 = 0x03; // interface 1 — animation frames
const EP_FRAME_IN: u8 = 0x84; // interface 1 — drain after each frame
const EP_CTRL_OUT: u8 = 0x01; // interface 0 — control commands
const EP_CTRL_IN: u8 = 0x82; // interface 0 — replies (command byte echoed at [0])

const FRAME_PKT: usize = 1024;
const FRAME_PAYLOAD: usize = FRAME_PKT - 4;
const CTRL_PKT: usize = 440;

const CMD_FRAME: u8 = 0x08;
const CMD_INIT: u8 = 0x0a;
const CMD_CHUNK: u8 = 0x0b;
const CMD_SCREEN: u8 = 0x12;
const CMD_COMMIT: u8 = 0x14;
const CMD_FINALIZE: u8 = 0x82; // also the status poll the Windows app sends every 2 s

const FLAG_FIRST: u8 = 0x80;
const COMMIT_ARG: u8 = 0x62; // byte[4] of CMD_COMMIT, as sent by the Windows app
const BRIGHTNESS_ON: u8 = 0x64;

const UPLOAD_CHUNK: usize = 10240;
const CHUNK_HDR_DATA: usize = 424;
const CHUNK_PKT_DATA: usize = 436;

const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const ACK_TIMEOUT: Duration = Duration::from_secs(3);
const QUERY_TIMEOUT: Duration = Duration::from_millis(250);

/// Largest frame `send_frame` can address (the packet count is a u8).
pub const MAX_FRAME_BYTES: usize = 255 * FRAME_PAYLOAD;

/// Device flash slot selected by the INIT command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    Standby = 0,
    Boot = 1,
}

#[derive(Debug)]
pub struct Error {
    pub msg: String,
    /// The handle is unusable (unplugged / I/O error) and should be dropped.
    pub device_gone: bool,
}

impl Error {
    fn other(msg: impl Into<String>) -> Self {
        Self { msg: msg.into(), device_gone: false }
    }
}

impl From<rusb::Error> for Error {
    fn from(e: rusb::Error) -> Self {
        let device_gone = matches!(e, rusb::Error::NoDevice | rusb::Error::Io | rusb::Error::NotFound);
        Self { msg: e.to_string(), device_gone }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Find, open and claim the device.
pub fn open() -> std::result::Result<Handle, String> {
    let devices = rusb::devices().map_err(|e| format!("cannot list USB devices: {e}"))?;
    let device = devices
        .iter()
        .find(|d| d.device_descriptor().is_ok_and(|desc| desc.vendor_id() == VID && desc.product_id() == PID))
        .ok_or_else(|| format!("device {VID:04x}:{PID:04x} not found"))?;
    let dev = device.open().map_err(|e| match e {
        rusb::Error::Access => {
            format!("permission denied opening {VID:04x}:{PID:04x} — is the udev rule installed? (see README)")
        }
        e => format!("open {VID:04x}:{PID:04x}: {e}"),
    })?;

    // Let libusb detach the kernel HID driver on claim and re-attach it on release.
    let auto_detach = dev.set_auto_detach_kernel_driver(true).is_ok();
    for iface in 0..=1u8 {
        if !auto_detach && dev.kernel_driver_active(iface).unwrap_or(false) {
            dev.detach_kernel_driver(iface).map_err(|e| format!("detach iface {iface}: {e}"))?;
        }
        dev.claim_interface(iface).map_err(|e| format!("claim iface {iface}: {e}"))?;
    }
    Ok(dev)
}

/// Whether the device behind `dev` is still enumerated on the bus.
pub fn still_connected(dev: &Handle) -> bool {
    let d = dev.device();
    rusb::devices()
        .map(|list| list.iter().any(|x| x.bus_number() == d.bus_number() && x.address() == d.address()))
        .unwrap_or(true)
}

fn write_all(dev: &Handle, ep: u8, buf: &[u8]) -> Result<()> {
    let n = dev.write_interrupt(ep, buf, WRITE_TIMEOUT)?;
    if n != buf.len() {
        return Err(Error::other(format!("short write on ep 0x{ep:02x}: {n}/{} bytes", buf.len())));
    }
    Ok(())
}

fn ctrl_packet(cmd: u8, seq: u8, flags: u8) -> [u8; CTRL_PKT] {
    let mut p = [0u8; CTRL_PKT];
    p[0] = cmd;
    p[1] = seq;
    p[3] = flags;
    p
}

/// Read one reply and require it to echo `cmd`.
fn expect_ack(dev: &Handle, cmd: u8) -> Result<()> {
    let mut r = [0u8; CTRL_PKT];
    match dev.read_interrupt(EP_CTRL_IN, &mut r, ACK_TIMEOUT) {
        Ok(n) if n > 0 && r[0] == cmd => Ok(()),
        Ok(n) => Err(Error::other(format!("unexpected reply to command 0x{cmd:02x}: {:02x?}", &r[..n.min(4)]))),
        Err(rusb::Error::Timeout) => Err(Error::other(format!("no reply to command 0x{cmd:02x}"))),
        Err(e) => Err(e.into()),
    }
}

/// Discard stale replies (e.g. from earlier screen on/off commands) so they
/// are not mistaken for the replies of the upload that follows.
fn drain_ctrl_replies(dev: &Handle) {
    let mut r = [0u8; CTRL_PKT];
    for _ in 0..8 {
        if dev.read_interrupt(EP_CTRL_IN, &mut r, Duration::from_millis(20)).is_err() {
            break;
        }
    }
}

/// Stream one JPEG frame (volatile — nothing is written to flash).
pub fn send_frame(dev: &Handle, jpeg: &[u8]) -> Result<()> {
    if jpeg.is_empty() || jpeg.len() > MAX_FRAME_BYTES {
        return Err(Error::other(format!("frame size {} out of range", jpeg.len())));
    }
    let n = jpeg.len().div_ceil(FRAME_PAYLOAD);
    let mut pkt = [0u8; FRAME_PKT];
    for (j, chunk) in jpeg.chunks(FRAME_PAYLOAD).enumerate() {
        pkt.fill(0);
        pkt[0] = CMD_FRAME;
        pkt[1] = if j == 0 { n as u8 } else { ((j - 1) % 0x15) as u8 + 1 };
        pkt[3] = if j == 0 { FLAG_FIRST } else { 0 };
        pkt[4..4 + chunk.len()].copy_from_slice(chunk);
        write_all(dev, EP_FRAME_OUT, &pkt)?;
    }
    // Drain the per-frame response; the device locks up if this is skipped.
    let mut r = [0u8; 16];
    let _ = dev.read_interrupt(EP_FRAME_IN, &mut r, Duration::from_millis(1));
    Ok(())
}

/// Turn the backlight on or off (not persisted across power cycles).
pub fn set_screen(dev: &Handle, on: bool) -> Result<()> {
    let mut p = ctrl_packet(CMD_SCREEN, 1, FLAG_FIRST);
    p[4] = if on { BRIGHTNESS_ON } else { 0 };
    write_all(dev, EP_CTRL_OUT, &p)
}

/// Send a read-only query (`cmd 01 00 80`) and return the reply that echoes
/// `cmd`, skipping stale replies to earlier commands.
fn query(dev: &Handle, cmd: u8) -> Result<[u8; CTRL_PKT]> {
    write_all(dev, EP_CTRL_OUT, &ctrl_packet(cmd, 1, FLAG_FIRST))?;
    let mut r = [0u8; CTRL_PKT];
    for _ in 0..4 {
        match dev.read_interrupt(EP_CTRL_IN, &mut r, QUERY_TIMEOUT) {
            Ok(n) if n > 0 && r[0] == cmd => return Ok(r),
            Ok(_) => continue,
            Err(rusb::Error::Timeout) => break,
            Err(e) => return Err(e.into()),
        }
    }
    Err(Error::other(format!("no reply to query 0x{cmd:02x}")))
}

/// Coolant temperature in °C, from the reply to the 0x82 status poll the
/// Windows app sends every 2 s: bytes 4–5, little-endian, hundredths of a
/// degree (e.g. `c6 0a` = 27.58 °C). Also returns the first 16 reply bytes for
/// diagnostics. `None` if the value is implausible.
pub fn read_coolant_temp(dev: &Handle) -> Result<(Option<f32>, [u8; 16])> {
    let r = query(dev, CMD_FINALIZE)?;
    let mut head = [0u8; 16];
    head.copy_from_slice(&r[..16]);
    let t = f32::from(u16::from_le_bytes([r[4], r[5]])) / 100.0;
    Ok(((1.0..100.0).contains(&t).then_some(t), head))
}

/// Write `data` to a flash slot.
///
/// The caller must validate `data` first (`shared::validate_standby_jpeg` /
/// `shared::validate_boot_container`). Every step up to the last chunk must be
/// acknowledged by the device; on any failure the upload stops and the boot
/// commit is never sent. For the boot slot the device keeps writing flash for
/// roughly `shared::BOOT_FLASH_SETTLE` after this returns.
pub fn upload(dev: &Handle, slot: Slot, data: &[u8]) -> Result<()> {
    let size = u32::try_from(data.len()).map_err(|_| Error::other("payload too large"))?;
    if size == 0 {
        return Err(Error::other("empty payload"));
    }

    drain_ctrl_replies(dev);

    // Pre-finalize: closes any transfer an interrupted upload left open.
    write_all(dev, EP_CTRL_OUT, &ctrl_packet(CMD_FINALIZE, 1, FLAG_FIRST))?;
    expect_ack(dev, CMD_FINALIZE)?;

    let mut init = ctrl_packet(CMD_INIT, 1, FLAG_FIRST);
    init[4..8].copy_from_slice(&(slot as u32).to_le_bytes());
    init[8..12].copy_from_slice(&size.to_le_bytes());
    write_all(dev, EP_CTRL_OUT, &init)?;
    expect_ack(dev, CMD_INIT)?;

    for (i, chunk) in data.chunks(UPLOAD_CHUNK).enumerate() {
        let offset = i * UPLOAD_CHUNK;
        let (head, rest) = chunk.split_at(CHUNK_HDR_DATA.min(chunk.len()));
        let npkt = 1 + rest.len().div_ceil(CHUNK_PKT_DATA); // ≤ 25
        let progress = ((offset + chunk.len()) as u64 * 100 / u64::from(size)) as u32;

        let mut p = ctrl_packet(CMD_CHUNK, npkt as u8, FLAG_FIRST);
        p[4..8].copy_from_slice(&(offset as u32).to_le_bytes());
        p[8..12].copy_from_slice(&(chunk.len() as u32).to_le_bytes());
        p[12..16].copy_from_slice(&progress.to_le_bytes());
        p[16..16 + head.len()].copy_from_slice(head);
        write_all(dev, EP_CTRL_OUT, &p)?;

        for (k, part) in rest.chunks(CHUNK_PKT_DATA).enumerate() {
            let mut p = ctrl_packet(CMD_CHUNK, (k + 1) as u8, 0);
            p[4..4 + part.len()].copy_from_slice(part);
            write_all(dev, EP_CTRL_OUT, &p)?;
        }

        expect_ack(dev, CMD_CHUNK).map_err(|e| Error { msg: format!("chunk at offset {offset}: {}", e.msg), ..e })?;
    }

    if slot == Slot::Boot {
        // Commit: the device burns the uploaded data to the boot slot.
        // Fire-and-forget — the device sends no reply.
        let mut p = ctrl_packet(CMD_COMMIT, 1, FLAG_FIRST);
        p[4] = COMMIT_ARG;
        write_all(dev, EP_CTRL_OUT, &p)?;
        let mut r = [0u8; CTRL_PKT];
        let _ = dev.read_interrupt(EP_CTRL_IN, &mut r, Duration::from_millis(500));
    }

    write_all(dev, EP_CTRL_OUT, &ctrl_packet(CMD_FINALIZE, 1, FLAG_FIRST))?;
    if let Err(e) = expect_ack(dev, CMD_FINALIZE) {
        eprintln!("[usb] finalize: {e} (data already sent)");
    }
    Ok(())
}
