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
