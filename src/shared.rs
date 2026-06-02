use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

// ── Paths ────────────────────────────────────────────────────────────────────

pub fn socket_path() -> PathBuf {
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(runtime).join("th-lcd.sock")
}

pub fn data_dir() -> PathBuf {
    let d = dirs::data_dir().unwrap_or_default().join("th-linux-lcd");
    fs::create_dir_all(&d).ok();
    d
}

pub fn frames_dir(data: &PathBuf, id: &str) -> PathBuf {
    let d = data.join("frames").join(id);
    fs::create_dir_all(&d).ok();
    d
}

pub fn standby_dir(data: &PathBuf) -> PathBuf {
    let d = data.join("standby");
    fs::create_dir_all(&d).ok();
    d
}

// ── IPC protocol ─────────────────────────────────────────────────────────────

/// Commands sent from GUI → daemon over Unix socket (newline-delimited JSON)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd")]
pub enum Cmd {
    Play { frames_dir: String, fps: f32, frame_count: usize, id: String },
    Stop,
    ScreenOn,
    ScreenOff,
    UploadStandby { path: String },
    UploadBoot { path: String },
    Status,
}

/// Responses sent from daemon → GUI
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Resp {
    Ok,
    Err { msg: String },
    Status(DaemonStatus),
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DaemonStatus {
    pub streaming: bool,
    pub playing_id: Option<String>,
    pub device_found: bool,
    pub fps: f32,
}

// ── Data types ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Video {
    pub id: String,
    pub name: String,
    pub source_path: String,
    pub frame_count: usize,
    pub fps: f32,
    pub rotation: i32,
    pub thumbnail: String, // base64-encoded 96×96 JPEG
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StandbyImage {
    pub id: String,
    pub name: String,
    pub path: String,
    pub thumbnail: String, // base64-encoded 96×96 JPEG
}

// ── Persistence ───────────────────────────────────────────────────────────────

pub fn load_videos(data: &PathBuf) -> Vec<Video> {
    let p = data.join("videos.json");
    fs::read_to_string(&p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_videos(data: &PathBuf, videos: &[Video]) {
    let _ = fs::write(data.join("videos.json"), serde_json::to_string_pretty(videos).unwrap());
}

pub fn load_standby(data: &PathBuf) -> Vec<StandbyImage> {
    let p = data.join("standby.json");
    fs::read_to_string(&p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_standby(data: &PathBuf, images: &[StandbyImage]) {
    let _ = fs::write(data.join("standby.json"), serde_json::to_string_pretty(images).unwrap());
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootAnim {
    pub id: String,
    pub name: String,
    pub path: String,        // path to .bootanim binary on disk
    pub source_gif: String,  // original GIF path (for re-encoding with rotation)
    pub frame_count: usize,
    pub fps: f32,
    pub rotation: i32,       // 0, 90, 180, 270
    pub thumbnail: String,   // base64-encoded 96×96 JPEG of first frame
}

pub fn load_boot(data: &PathBuf) -> Vec<BootAnim> {
    let p = data.join("boot.json");
    fs::read_to_string(&p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_boot(data: &PathBuf, anims: &[BootAnim]) {
    let _ = fs::write(data.join("boot.json"), serde_json::to_string_pretty(anims).unwrap());
}

// ── Autoplay persistence ──────────────────────────────────────────────────────

/// Returns the video ID that should auto-play on daemon start, if any.
pub fn load_autoplay(data: &PathBuf) -> Option<String> {
    let p = data.join("autoplay.json");
    fs::read_to_string(&p)
        .ok()
        .and_then(|s| serde_json::from_str::<String>(&s).ok())
}

/// Persist `id` as the autoplay target, or clear it if `id` is None.
pub fn save_autoplay(data: &PathBuf, id: Option<&str>) {
    let p = data.join("autoplay.json");
    match id {
        Some(id) => { let _ = fs::write(p, serde_json::to_string(id).unwrap()); }
        None     => { let _ = fs::remove_file(p); }
    }
}

pub fn boot_dir(data: &PathBuf) -> PathBuf {
    let d = data.join("boot");
    fs::create_dir_all(&d).ok();
    d
}

/// Build the Update_Boot_GIF binary container from a list of 480×480 JPEG frames.
/// `fps` controls the playback speed (timing byte = round(1000/fps)).
/// Returns the raw bytes to be written to disk and uploaded.
pub fn build_boot_container(frames: &[Vec<u8>], fps: f32) -> Vec<u8> {
    let n = frames.len() as u8;
    let tbl_size = (n as u32) * 16;
    // Firmware rejects timing < 80ms (> 12.5fps) — cap playback at 12.5fps
    let fps_capped = fps.min(12.5).max(1.0);
    let timing = (1000.0 / fps_capped).round() as u8;

    // Frame offsets — each frame entry is: 8-byte filename sub-header + JPEG bytes
    const TBL_OFF: u32 = 32;
    const SUB_HDR: u32 = 8;
    let data_start = TBL_OFF + tbl_size;
    let mut offsets: Vec<u32> = Vec::with_capacity(n as usize);
    let mut cur = data_start;
    for frame in frames {
        offsets.push(cur);
        cur += SUB_HDR + frame.len() as u32;
    }
    // cur = offset where trailer starts. Full container = cur + 16 (trailer).
    // content_size = full - outer_header(16) = cur. Checksum uses full container size.
    let content_size = cur;
    let full_size    = cur + 16;
    let checksum = (-(full_size as i64 + n as i64 * 17)) as i32;

    let mut buf = Vec::with_capacity(full_size as usize);

    // Outer header (16 bytes)
    buf.extend_from_slice(&checksum.to_le_bytes());
    buf.extend_from_slice(&content_size.to_le_bytes());
    buf.extend_from_slice(&tbl_size.to_le_bytes());
    buf.push(0x10);
    buf.push(0x00);
    buf.push(n);
    buf.push(timing);

    // Magic (16 bytes, null-padded)
    let mut magic = [0u8; 16];
    magic[..15].copy_from_slice(b"Update_Boot_GIF");
    buf.extend_from_slice(&magic);

    // Frame table — flags[i] = -(offset[i] + 8 + frame_size[i]) as int32
    // This encodes the negated start of the next frame (confirmed from traces)
    for (&off, frame) in offsets.iter().zip(frames.iter()) {
        let flags = (-(off as i64 + 8 + frame.len() as i64)) as i32;
        buf.extend_from_slice(&flags.to_le_bytes());
        buf.extend_from_slice(&off.to_le_bytes());
        buf.extend_from_slice(&(frame.len() as u32).to_le_bytes()); // JPEG size, excl. sub-header
        buf.extend_from_slice(&0x40000008u32.to_le_bytes()); // constant observed in traces
    }

    // Frame data
    for (i, frame) in frames.iter().enumerate() {
        // 8-byte null-padded filename: "NNN.jpg\0"
        let name = format!("{:03}.jpg", i);
        let mut sub = [0u8; 8];
        let nb = name.as_bytes();
        sub[..nb.len().min(7)].copy_from_slice(&nb[..nb.len().min(7)]);
        buf.extend_from_slice(&sub);
        buf.extend_from_slice(frame);
    }

    // 16-byte trailer required by firmware: 15 zero bytes + 0x10
    // Observed in both Windows trace sessions; absent → device rejects CMD 0x14
    buf.extend_from_slice(&[0u8; 15]);
    buf.push(0x10);

    buf
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Send a single command to the daemon and receive a response.
/// Each call opens a fresh connection.
pub fn daemon_cmd(cmd: &Cmd) -> Result<Resp, String> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let mut stream = UnixStream::connect(socket_path())
        .map_err(|_| "Daemon not running".to_string())?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(30))).ok();

    let line = serde_json::to_string(cmd).unwrap() + "\n";
    stream.write_all(line.as_bytes()).map_err(|e| e.to_string())?;

    let mut resp_line = String::new();
    BufReader::new(stream).read_line(&mut resp_line).map_err(|e| e.to_string())?;
    serde_json::from_str(resp_line.trim()).map_err(|e| e.to_string())
}

/// Thumbnail: decode base64 → raw RGBA pixels + [w, h]
pub fn decode_thumbnail(b64: &str) -> Option<(Vec<u8>, [usize; 2])> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    let img = image::load_from_memory(&bytes).ok()?.to_rgba8();
    let w = img.width() as usize;
    let h = img.height() as usize;
    Some((img.into_raw(), [w, h]))
}

/// Encode a JPEG file to a base64 thumbnail (96×96)
pub fn make_thumbnail(path: &PathBuf) -> String {
    use base64::Engine;
    image::open(path)
        .ok()
        .map(|i| {
            let small = i.resize(96, 96, image::imageops::FilterType::Lanczos3);
            let mut buf = Vec::new();
            small
                .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Jpeg)
                .ok();
            buf
        })
        .map(|b| base64::engine::general_purpose::STANDARD.encode(&b))
        .unwrap_or_default()
}
