use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

// ── Limits ───────────────────────────────────────────────────────────────────

/// Display resolution (square).
pub const LCD_SIZE: u32 = 480;

/// Most frames an imported animation keeps / the daemon will stream.
pub const MAX_STREAM_FRAMES: usize = 60;
/// Target size per streamed JPEG frame.
pub const STREAM_FRAME_BUDGET: usize = 20 * 1024;
pub const MIN_STREAM_FPS: f32 = 1.0;
pub const MAX_STREAM_FPS: f32 = 30.0;

/// Upper bound for a standby JPEG sent to flash. The real slot size is
/// unknown; this stays far below anything observed to work.
pub const MAX_STANDBY_BYTES: usize = 256 * 1024;

/// The firmware expects exactly this many frames in a boot container.
pub const BOOT_FRAMES: usize = 40;
/// Firmware silently rejects containers faster than this (timing < 80 ms).
pub const MIN_BOOT_TIMING_MS: u8 = 80;
pub const MAX_BOOT_FPS: f32 = 1000.0 / MIN_BOOT_TIMING_MS as f32;
/// Upper bound for a boot container. The known-good container from the
/// Windows app is 1,238,420 bytes; the slot size itself is unknown, so stay
/// close to that.
pub const MAX_BOOT_BYTES: usize = 1_310_720;
/// Per-frame JPEG budget that keeps a full container under `MAX_BOOT_BYTES`.
pub const BOOT_FRAME_BUDGET: usize = 31 * 1024;
/// How long the device keeps writing flash after a boot-animation commit.
pub const BOOT_FLASH_SETTLE: Duration = Duration::from_secs(30);

/// How long the daemon waits for the USB thread to finish an upload
/// (transfer + flash settle).
pub const DAEMON_UPLOAD_TIMEOUT: Duration = Duration::from_secs(180);
/// How long the GUI waits for any reply to an upload command.
pub const CLIENT_UPLOAD_TIMEOUT: Duration = Duration::from_secs(210);
/// How long the GUI waits for a reply to any other command.
pub const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);
/// Status polls happen on the GUI thread; keep them short so a stuck daemon
/// cannot freeze the window.
pub const STATUS_TIMEOUT: Duration = Duration::from_millis(1500);

/// Clamp a frame rate to the streaming range; rejects NaN/inf/≤0.
pub fn sanitize_fps(fps: f32) -> Option<f32> {
    (fps.is_finite() && fps > 0.0).then(|| fps.clamp(MIN_STREAM_FPS, MAX_STREAM_FPS))
}

// ── Paths ────────────────────────────────────────────────────────────────────

/// Per-user runtime directory for the socket and lock file.
/// Uses `$XDG_RUNTIME_DIR`; never falls back to the shared `/tmp`.
pub fn runtime_dir() -> PathBuf {
    if let Some(d) = dirs::runtime_dir() {
        return d;
    }
    let d = dirs::cache_dir().unwrap_or_else(|| PathBuf::from(".")).join("th-linux-lcd");
    let _ = fs::DirBuilder::new().recursive(true).mode(0o700).create(&d);
    d
}

/// Path of this program's binary, for launching the GUI or daemon.
///
/// After an upgrade replaces the binary, a long-running process (e.g. the
/// tray) sees `current_exe()` as "<path> (deleted)"; strip that so the new
/// binary at the same path is used, and fall back to `th-linux-lcd` on PATH.
pub fn app_exe() -> PathBuf {
    if let Ok(p) = std::env::current_exe() {
        let s = p.to_string_lossy();
        let path = PathBuf::from(s.strip_suffix(" (deleted)").unwrap_or(&s));
        if path.exists() {
            return path;
        }
    }
    PathBuf::from("th-linux-lcd")
}

pub fn socket_path() -> PathBuf {
    runtime_dir().join("th-lcd.sock")
}

/// Socket a running GUI listens on so a second launch can bring it forward.
pub fn gui_socket_path() -> PathBuf {
    runtime_dir().join("th-lcd-gui.sock")
}

pub const DAEMON_LOCK: &str = "th-lcd.lock";
pub const GUI_LOCK: &str = "th-lcd-gui.lock";
pub const TRAY_LOCK: &str = "th-lcd-tray.lock";

/// Take the per-user single-instance lock `name` (in the runtime dir) for the
/// lifetime of the returned file. `Ok(None)` if another process holds it.
pub fn lock_instance(name: &str) -> Result<Option<fs::File>, String> {
    let path = runtime_dir().join(name);
    let file = fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(fs::TryLockError::WouldBlock) => Ok(None),
        Err(fs::TryLockError::Error(e)) => Err(format!("cannot lock {}: {e}", path.display())),
    }
}

pub fn data_dir() -> PathBuf {
    let d = dirs::data_dir().unwrap_or_default().join("th-linux-lcd");
    let _ = fs::create_dir_all(&d);
    d
}

/// Library item ids are UUIDs; anything else is refused before being used
/// as a path component.
pub fn is_valid_id(id: &str) -> bool {
    uuid::Uuid::try_parse(id).is_ok()
}

pub fn frames_dir(data: &Path, id: &str) -> PathBuf {
    data.join("frames").join(id)
}

/// Cache of rotated frames for a video with non-zero rotation.
pub fn rotated_frames_dir(data: &Path, id: &str) -> PathBuf {
    data.join("frames").join(format!("{id}-rotated"))
}

pub fn frame_file(dir: &Path, index: usize) -> PathBuf {
    dir.join(format!("f{index:05}.jpg"))
}

pub fn standby_dir(data: &Path) -> PathBuf {
    data.join("standby")
}

pub fn boot_dir(data: &Path) -> PathBuf {
    data.join("boot")
}

// ── IPC protocol ─────────────────────────────────────────────────────────────

/// Commands sent from GUI → daemon over Unix socket (newline-delimited JSON)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd")]
pub enum Cmd {
    Play {
        frames_dir: String,
        fps: f32,
        frame_count: usize,
        id: String,
        /// Clockwise rotation (0/90/180/270) already applied to the frames;
        /// the stats overlay is rotated the same way.
        #[serde(default)]
        rotation: i32,
    },
    Stop,
    ScreenOn,
    ScreenOff,
    UploadStandby {
        path: String,
    },
    UploadBoot {
        path: String,
    },
    /// Configure the stats overlay drawn onto streamed frames.
    SetOverlay {
        settings: OverlaySettings,
    },
    Status,
}

impl Cmd {
    fn reply_timeout(&self) -> Duration {
        match self {
            Cmd::UploadStandby { .. } | Cmd::UploadBoot { .. } => CLIENT_UPLOAD_TIMEOUT,
            Cmd::Status => STATUS_TIMEOUT,
            _ => CLIENT_TIMEOUT,
        }
    }
}

/// Responses sent from daemon → GUI
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Resp {
    Ok,
    Err { msg: String },
    Status(DaemonStatus),
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct DaemonStatus {
    pub streaming: bool,
    pub playing_id: Option<String>,
    pub device_found: bool,
    pub fps: f32,
    /// The device is still writing a boot animation to flash.
    #[serde(default)]
    pub flash_busy: bool,
    #[serde(default)]
    pub metrics: Metrics,
    #[serde(default)]
    pub overlay: OverlaySettings,
}

/// Latest sensor readings (None = unavailable).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct Metrics {
    /// CPU package temperature, °C.
    pub cpu_temp: Option<f32>,
    /// Coolant (liquid) temperature reported by the pump, °C.
    pub coolant: Option<f32>,
    /// GPU temperature, °C.
    pub gpu_temp: Option<f32>,
    /// CPU load, %.
    pub cpu_load: Option<f32>,
    /// GPU load, %.
    pub gpu_load: Option<f32>,
    /// Memory in use, %.
    pub ram: Option<f32>,
}

/// One overlay item, in display order: row 1 temperatures, row 2 loads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stat {
    CpuTemp,
    Coolant,
    GpuTemp,
    CpuLoad,
    GpuLoad,
    Ram,
}

impl Stat {
    pub const ROWS: [[Stat; 3]; 2] =
        [[Stat::CpuTemp, Stat::Coolant, Stat::GpuTemp], [Stat::CpuLoad, Stat::GpuLoad, Stat::Ram]];

    pub fn label(self) -> &'static str {
        match self {
            Stat::CpuTemp | Stat::CpuLoad => "CPU",
            Stat::Coolant => "LIQ",
            Stat::GpuTemp | Stat::GpuLoad => "GPU",
            Stat::Ram => "RAM",
        }
    }

    /// Long name for settings UIs.
    pub fn name(self) -> &'static str {
        match self {
            Stat::CpuTemp => "CPU temp",
            Stat::Coolant => "Coolant temp",
            Stat::GpuTemp => "GPU temp",
            Stat::CpuLoad => "CPU load",
            Stat::GpuLoad => "GPU load",
            Stat::Ram => "RAM load",
        }
    }

    /// Formatted value, e.g. "52°" / "23%"; None if unavailable.
    pub fn value(self, m: &Metrics) -> Option<String> {
        let temp = |t: Option<f32>| t.map(|t| format!("{t:.0}°"));
        let pct = |p: Option<f32>| p.map(|p| format!("{p:.0}%"));
        match self {
            Stat::CpuTemp => temp(m.cpu_temp),
            Stat::Coolant => m.coolant.map(|t| format!("{t:.1}°")),
            Stat::GpuTemp => temp(m.gpu_temp),
            Stat::CpuLoad => pct(m.cpu_load),
            Stat::GpuLoad => pct(m.gpu_load),
            Stat::Ram => pct(m.ram),
        }
    }
}

impl Metrics {
    /// One-line summary for the status bar / tray, e.g. "CPU 52° · LIQ 31.5° · GPU 45° · …".
    pub fn summary(&self) -> String {
        Stat::ROWS
            .iter()
            .flatten()
            .filter_map(|s| s.value(self).map(|v| format!("{} {v}", s.label())))
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

/// Look of the stats overlay.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
pub enum OverlayStyle {
    /// Green labels, white values, dark outline.
    #[default]
    Terminal,
    /// Amber CRT terminal (VT323) on a dark screen with scanlines and glow.
    RetroClock,
    /// Blue-steel chrome in a heavy-metal font, bevelled and extruded.
    HeavyMetal,
    /// Puffy pink/blue bubble letters with gloss and a sticker outline.
    Kawaii,
    /// Bouncy anime-title lettering: gold/red gradient, thick black outline,
    /// and a dragon-ball degree sign.
    Saiyan,
}

impl OverlayStyle {
    pub const ALL: [OverlayStyle; 5] = [
        OverlayStyle::Terminal,
        OverlayStyle::RetroClock,
        OverlayStyle::HeavyMetal,
        OverlayStyle::Kawaii,
        OverlayStyle::Saiyan,
    ];

    pub fn name(self) -> &'static str {
        match self {
            OverlayStyle::Terminal => "TERMINAL",
            OverlayStyle::RetroClock => "RETRO",
            OverlayStyle::HeavyMetal => "METAL",
            OverlayStyle::Kawaii => "KAWAII",
            OverlayStyle::Saiyan => "SAIYAN",
        }
    }
}

/// Allowed text scale (1.0 = default size).
pub const OVERLAY_SIZE_RANGE: std::ops::RangeInclusive<f32> = 0.6..=1.6;
/// Allowed vertical position of the stats block centre (0 = top edge, 1 = bottom edge).
pub const OVERLAY_VERTICAL_RANGE: std::ops::RangeInclusive<f32> = 0.15..=0.85;
pub const OVERLAY_TOP: f32 = 0.22;
pub const OVERLAY_BOTTOM: f32 = 0.78;

/// Which stats are drawn onto streamed frames, and how.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct OverlaySettings {
    pub enabled: bool,
    pub style: OverlayStyle,
    /// Text scale, see `OVERLAY_SIZE_RANGE`.
    pub size: f32,
    /// Vertical centre of the stats block as a fraction of the screen height.
    pub vertical: f32,
    pub cpu_temp: bool,
    pub coolant: bool,
    pub gpu_temp: bool,
    pub cpu_load: bool,
    pub gpu_load: bool,
    pub ram: bool,
}

impl Default for OverlaySettings {
    fn default() -> Self {
        Self {
            enabled: false,
            style: OverlayStyle::Terminal,
            size: 1.0,
            vertical: OVERLAY_BOTTOM,
            cpu_temp: true,
            coolant: true,
            gpu_temp: true,
            cpu_load: true,
            gpu_load: true,
            ram: true,
        }
    }
}

impl OverlaySettings {
    /// Clamp size and position into their allowed ranges.
    pub fn sanitized(mut self) -> Self {
        let clamp = |v: f32, r: &std::ops::RangeInclusive<f32>, default: f32| {
            if v.is_finite() {
                v.clamp(*r.start(), *r.end())
            } else {
                default
            }
        };
        self.size = clamp(self.size, &OVERLAY_SIZE_RANGE, 1.0);
        self.vertical = clamp(self.vertical, &OVERLAY_VERTICAL_RANGE, OVERLAY_BOTTOM);
        self
    }

    pub fn shows(&self, stat: Stat) -> bool {
        match stat {
            Stat::CpuTemp => self.cpu_temp,
            Stat::Coolant => self.coolant,
            Stat::GpuTemp => self.gpu_temp,
            Stat::CpuLoad => self.cpu_load,
            Stat::GpuLoad => self.gpu_load,
            Stat::Ram => self.ram,
        }
    }

    pub fn toggle(&mut self, stat: Stat) {
        let flag = match stat {
            Stat::CpuTemp => &mut self.cpu_temp,
            Stat::Coolant => &mut self.coolant,
            Stat::GpuTemp => &mut self.gpu_temp,
            Stat::CpuLoad => &mut self.cpu_load,
            Stat::GpuLoad => &mut self.gpu_load,
            Stat::Ram => &mut self.ram,
        };
        *flag = !*flag;
    }
}

const OVERLAY_JSON: &str = "overlay.json";

pub fn load_overlay_settings(data: &Path) -> OverlaySettings {
    fs::read_to_string(data.join(OVERLAY_JSON))
        .ok()
        .and_then(|s| serde_json::from_str::<OverlaySettings>(&s).ok())
        .unwrap_or_default()
        .sanitized()
}

pub fn save_overlay_settings(data: &Path, settings: &OverlaySettings) -> Result<(), String> {
    let json = serde_json::to_vec_pretty(settings).map_err(|e| e.to_string())?;
    write_atomic(&data.join(OVERLAY_JSON), &json)
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootAnim {
    pub id: String,
    pub name: String,
    pub path: String,       // path to .bootanim binary on disk
    pub source_gif: String, // original GIF path (for re-encoding with rotation)
    pub frame_count: usize,
    pub fps: f32,
    pub rotation: i32,     // 0, 90, 180, 270
    pub thumbnail: String, // base64-encoded 96×96 JPEG of first frame
}

// ── Persistence ───────────────────────────────────────────────────────────────

const VIDEOS_JSON: &str = "videos.json";
const STANDBY_JSON: &str = "standby.json";
const BOOT_JSON: &str = "boot.json";
const AUTOPLAY_JSON: &str = "autoplay.json";

/// Write `bytes` to `path` via a temp file + fsync + rename, so a crash or
/// full disk never leaves a truncated file behind.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    let result = (|| {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    result.map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("could not write {}: {e}", path.display())
    })
}

/// Load a JSON list. A missing file is an empty list. A file that cannot be
/// read or parsed is backed up to `*.corrupt` (so a later save cannot destroy
/// it) and reported in the second tuple element.
fn load_list<T: DeserializeOwned>(path: &Path) -> (Vec<T>, Option<String>) {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Vec::new(), None),
        Err(e) => return (Vec::new(), Some(format!("could not read {}: {e}", path.display()))),
    };
    match serde_json::from_str(&text) {
        Ok(v) => (v, None),
        Err(e) => {
            let backup = path.with_extension("json.corrupt");
            let _ = fs::copy(path, &backup);
            let msg = format!("{} is corrupt ({e}); a copy was saved to {}", path.display(), backup.display());
            (Vec::new(), Some(msg))
        }
    }
}

fn save_list<T: Serialize>(path: &Path, items: &[T]) -> Result<(), String> {
    let json = serde_json::to_vec_pretty(items).map_err(|e| e.to_string())?;
    write_atomic(path, &json)
}

pub fn load_videos(data: &Path) -> (Vec<Video>, Option<String>) {
    load_list(&data.join(VIDEOS_JSON))
}

pub fn save_videos(data: &Path, videos: &[Video]) -> Result<(), String> {
    save_list(&data.join(VIDEOS_JSON), videos)
}

pub fn load_standby(data: &Path) -> (Vec<StandbyImage>, Option<String>) {
    load_list(&data.join(STANDBY_JSON))
}

pub fn save_standby(data: &Path, images: &[StandbyImage]) -> Result<(), String> {
    save_list(&data.join(STANDBY_JSON), images)
}

pub fn load_boot(data: &Path) -> (Vec<BootAnim>, Option<String>) {
    load_list(&data.join(BOOT_JSON))
}

pub fn save_boot(data: &Path, anims: &[BootAnim]) -> Result<(), String> {
    save_list(&data.join(BOOT_JSON), anims)
}

// ── Autoplay persistence ──────────────────────────────────────────────────────

/// Returns the video ID that should auto-play on daemon start, if any.
pub fn load_autoplay(data: &Path) -> Option<String> {
    fs::read_to_string(data.join(AUTOPLAY_JSON))
        .ok()
        .and_then(|s| serde_json::from_str::<String>(&s).ok())
        .filter(|id| is_valid_id(id))
}

/// Persist `id` as the autoplay target, or clear it if `id` is None.
pub fn save_autoplay(data: &Path, id: Option<&str>) -> Result<(), String> {
    let p = data.join(AUTOPLAY_JSON);
    match id {
        Some(id) => write_atomic(&p, serde_json::to_string(id).map_err(|e| e.to_string())?.as_bytes()),
        None => match fs::remove_file(&p) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
            _ => Ok(()),
        },
    }
}

// ── What is on the device ─────────────────────────────────────────────────────

const ON_DEVICE_JSON: &str = "on_device.json";

/// Library ids of the standby image / boot animation last written to the
/// device by this app. The protocol has no read-back command, so this is the
/// only record of what is in the flash slots.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct OnDevice {
    pub standby: Option<String>,
    pub boot: Option<String>,
}

pub fn load_on_device(data: &Path) -> OnDevice {
    fs::read_to_string(data.join(ON_DEVICE_JSON)).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

pub fn save_on_device(data: &Path, state: &OnDevice) -> Result<(), String> {
    let json = serde_json::to_vec_pretty(state).map_err(|e| e.to_string())?;
    write_atomic(&data.join(ON_DEVICE_JSON), &json)
}

// ── Boot container (Update_Boot_GIF) ──────────────────────────────────────────

const BOOT_MAGIC: &[u8; 15] = b"Update_Boot_GIF";
const BOOT_HDR: usize = 32; // 16-byte outer header + 16-byte magic
const BOOT_ENTRY: usize = 16;
const BOOT_SUB_HDR: usize = 8;
const BOOT_TRAILER: usize = 16;
const BOOT_ENTRY_CONST: u32 = 0x4000_0008; // constant observed in traces

/// Header checksum: -(full container size + n_frames × 17) as i32.
fn boot_checksum(full_size: usize, n_frames: usize) -> i32 {
    (-(full_size as i64 + n_frames as i64 * 17)) as i32
}

/// Frame-table flags: negated end offset of the frame entry.
fn boot_entry_flags(offset: usize, jpeg_len: usize) -> i32 {
    (-((offset + BOOT_SUB_HDR + jpeg_len) as i64)) as i32
}

/// Build the Update_Boot_GIF container from 480×480 JPEG frames.
/// `fps` is capped at `MAX_BOOT_FPS` (timing byte = round(1000/fps)).
pub fn build_boot_container(frames: &[Vec<u8>], fps: f32) -> Result<Vec<u8>, String> {
    let n = u8::try_from(frames.len())
        .ok()
        .filter(|&n| n > 0)
        .ok_or_else(|| format!("{} frames is out of range", frames.len()))?;
    let fps = if fps.is_finite() { fps.clamp(1.0, MAX_BOOT_FPS) } else { MAX_BOOT_FPS };
    let timing = (1000.0 / fps).round().min(255.0) as u8;

    let data_start = BOOT_HDR + frames.len() * BOOT_ENTRY;
    let mut offsets = Vec::with_capacity(frames.len());
    let mut cur = data_start;
    for frame in frames {
        offsets.push(cur);
        cur += BOOT_SUB_HDR + frame.len();
    }
    let full_size = cur + BOOT_TRAILER;
    let content_size = u32::try_from(full_size - 16).map_err(|_| "container too large".to_string())?;

    let mut buf = Vec::with_capacity(full_size);

    // Outer header (16 bytes)
    buf.extend_from_slice(&boot_checksum(full_size, frames.len()).to_le_bytes());
    buf.extend_from_slice(&content_size.to_le_bytes());
    buf.extend_from_slice(&(u32::from(n) * BOOT_ENTRY as u32).to_le_bytes());
    buf.extend_from_slice(&[0x10, 0x00, n, timing]);

    // Magic (16 bytes, null-padded)
    buf.extend_from_slice(BOOT_MAGIC);
    buf.push(0);

    // Frame table
    for (&off, frame) in offsets.iter().zip(frames) {
        buf.extend_from_slice(&boot_entry_flags(off, frame.len()).to_le_bytes());
        buf.extend_from_slice(&(off as u32).to_le_bytes());
        buf.extend_from_slice(&(frame.len() as u32).to_le_bytes()); // JPEG size, excl. sub-header
        buf.extend_from_slice(&BOOT_ENTRY_CONST.to_le_bytes());
    }

    // Frame data: 8-byte null-padded name "NNN.jpg\0" + JPEG
    for (i, frame) in frames.iter().enumerate() {
        let mut sub = [0u8; BOOT_SUB_HDR];
        let name = format!("{i:03}.jpg");
        sub[..7].copy_from_slice(&name.as_bytes()[..7]);
        buf.extend_from_slice(&sub);
        buf.extend_from_slice(frame);
    }

    // 16-byte trailer required by firmware: 15 zero bytes + 0x10
    buf.extend_from_slice(&[0u8; 15]);
    buf.push(0x10);

    debug_assert_eq!(buf.len(), full_size);
    Ok(buf)
}

fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// Check a boot container byte-for-byte against the known-good format before
/// it is written to flash.
pub fn validate_boot_container(data: &[u8]) -> Result<(), String> {
    let len = data.len();
    if len < BOOT_HDR + BOOT_TRAILER {
        return Err(format!("boot container too small ({len} bytes)"));
    }
    if len > MAX_BOOT_BYTES {
        return Err(format!("boot container is {len} bytes; the limit is {MAX_BOOT_BYTES}. Re-import the GIF."));
    }
    if &data[16..31] != BOOT_MAGIC || data[31] != 0 {
        return Err("not an Update_Boot_GIF container (bad magic)".into());
    }
    let n = usize::from(data[14]);
    let timing = data[15];
    if data[12] != 0x10 || data[13] != 0x00 {
        return Err("bad container header".into());
    }
    if n != BOOT_FRAMES {
        return Err(format!("container has {n} frames; the firmware expects {BOOT_FRAMES}"));
    }
    if timing < MIN_BOOT_TIMING_MS {
        return Err(format!("frame timing {timing} ms is below the firmware minimum of {MIN_BOOT_TIMING_MS} ms"));
    }
    if le_u32(&data[4..8]) as usize + 16 != len {
        return Err("content size field does not match file size".into());
    }
    if le_u32(&data[8..12]) as usize != n * BOOT_ENTRY {
        return Err("frame table size field is wrong".into());
    }
    if le_u32(&data[0..4]) as i32 != boot_checksum(len, n) {
        return Err("checksum mismatch".into());
    }
    let trailer_start = len - BOOT_TRAILER;
    if data[trailer_start..len - 1].iter().any(|&b| b != 0) || data[len - 1] != 0x10 {
        return Err("bad container trailer".into());
    }

    let mut expected = BOOT_HDR + n * BOOT_ENTRY;
    for i in 0..n {
        let e = &data[BOOT_HDR + i * BOOT_ENTRY..BOOT_HDR + (i + 1) * BOOT_ENTRY];
        let off = le_u32(&e[4..8]) as usize;
        let size = le_u32(&e[8..12]) as usize;
        if off != expected {
            return Err(format!("frame {i}: unexpected offset {off}"));
        }
        let end = off + BOOT_SUB_HDR + size;
        if size < 4 || end > trailer_start {
            return Err(format!("frame {i}: size {size} out of bounds"));
        }
        if le_u32(&e[0..4]) as i32 != boot_entry_flags(off, size) || le_u32(&e[12..16]) != BOOT_ENTRY_CONST {
            return Err(format!("frame {i}: bad table entry"));
        }
        if data[off + BOOT_SUB_HDR..off + BOOT_SUB_HDR + 2] != [0xFF, 0xD8] {
            return Err(format!("frame {i} is not a JPEG"));
        }
        expected = end;
    }
    if expected != trailer_start {
        return Err("frame data does not end at the trailer".into());
    }
    Ok(())
}

/// Check a standby image before it is written to flash.
pub fn validate_standby_jpeg(data: &[u8]) -> Result<(), String> {
    if data.is_empty() {
        return Err("standby image is empty".into());
    }
    if data.len() > MAX_STANDBY_BYTES {
        return Err(format!("standby image is {} bytes; the limit is {MAX_STANDBY_BYTES}. Re-import it.", data.len()));
    }
    if !data.starts_with(&[0xFF, 0xD8]) {
        return Err("standby image is not a JPEG".into());
    }
    let dims = image::ImageReader::with_format(std::io::Cursor::new(data), image::ImageFormat::Jpeg)
        .into_dimensions()
        .map_err(|e| format!("standby image is not a valid JPEG: {e}"))?;
    if dims != (LCD_SIZE, LCD_SIZE) {
        return Err(format!("standby image is {}×{}; it must be {LCD_SIZE}×{LCD_SIZE}", dims.0, dims.1));
    }
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Send a single command to the daemon and receive a response.
/// Each call opens a fresh connection.
pub fn daemon_cmd(cmd: &Cmd) -> Result<Resp, String> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let mut stream = UnixStream::connect(socket_path()).map_err(|_| "Daemon not running".to_string())?;
    let _ = stream.set_read_timeout(Some(cmd.reply_timeout()));
    let _ = stream.set_write_timeout(Some(CLIENT_TIMEOUT));

    let line = serde_json::to_string(cmd).map_err(|e| e.to_string())? + "\n";
    stream.write_all(line.as_bytes()).map_err(|e| e.to_string())?;

    let mut resp_line = String::new();
    BufReader::new(stream).read_line(&mut resp_line).map_err(|e| match e.kind() {
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => "no reply from daemon (timed out)".to_string(),
        _ => e.to_string(),
    })?;
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

/// Encode an image (JPEG/PNG bytes) to a base64 96×96 JPEG thumbnail.
pub fn thumbnail_from_bytes(bytes: &[u8]) -> String {
    use base64::Engine;
    let Ok(img) = image::load_from_memory(bytes) else { return String::new() };
    let small = img.resize(96, 96, image::imageops::FilterType::Lanczos3).to_rgb8();
    let mut buf = Vec::new();
    if small.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Jpeg).is_err() {
        return String::new();
    }
    base64::engine::general_purpose::STANDARD.encode(&buf)
}

/// Encode an image file to a base64 96×96 JPEG thumbnail.
pub fn make_thumbnail(path: &Path) -> String {
    fs::read(path).map(|b| thumbnail_from_bytes(&b)).unwrap_or_default()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_jpeg(len: usize) -> Vec<u8> {
        let mut v = vec![0xAB; len];
        v[0] = 0xFF;
        v[1] = 0xD8;
        v
    }

    fn container() -> Vec<u8> {
        let frames: Vec<_> = (0..BOOT_FRAMES).map(|i| fake_jpeg(1000 + i)).collect();
        build_boot_container(&frames, 12.5).unwrap()
    }

    #[test]
    fn checksum_matches_windows_trace() {
        // Known-good container captured from the Windows app.
        assert_eq!(boot_checksum(1_238_420, 40) as u32, 0xFFED_17C4);
    }

    #[test]
    fn built_container_validates() {
        let c = container();
        validate_boot_container(&c).unwrap();
        assert_eq!(c[14] as usize, BOOT_FRAMES);
        assert_eq!(c[15], MIN_BOOT_TIMING_MS);
        assert_eq!(&c[c.len() - 16..], &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x10]);
    }

    #[test]
    fn fps_is_capped_for_boot() {
        let frames: Vec<_> = (0..BOOT_FRAMES).map(|_| fake_jpeg(100)).collect();
        let c = build_boot_container(&frames, 60.0).unwrap();
        assert_eq!(c[15], MIN_BOOT_TIMING_MS);
        let c = build_boot_container(&frames, f32::NAN).unwrap();
        assert_eq!(c[15], MIN_BOOT_TIMING_MS);
    }

    #[test]
    fn rejects_corrupted_containers() {
        let good = container();

        assert!(validate_boot_container(&good[..good.len() - 1]).is_err(), "truncated");
        assert!(validate_boot_container(&[]).is_err(), "empty");

        let mut c = good.clone();
        c[20] ^= 1;
        assert!(validate_boot_container(&c).is_err(), "magic");

        let mut c = good.clone();
        c[15] = 50;
        assert!(validate_boot_container(&c).is_err(), "timing");

        let mut c = good.clone();
        c[0] ^= 1;
        assert!(validate_boot_container(&c).is_err(), "checksum");

        let mut c = good.clone();
        let f0 = BOOT_HDR + BOOT_FRAMES * BOOT_ENTRY + BOOT_SUB_HDR;
        c[f0] = 0;
        assert!(validate_boot_container(&c).is_err(), "frame not JPEG");

        let few: Vec<_> = (0..10).map(|_| fake_jpeg(100)).collect();
        let c = build_boot_container(&few, 12.5).unwrap();
        assert!(validate_boot_container(&c).is_err(), "wrong frame count");
    }

    #[test]
    fn rejects_oversized_container() {
        let frames: Vec<_> = (0..BOOT_FRAMES).map(|_| fake_jpeg(MAX_BOOT_BYTES / BOOT_FRAMES)).collect();
        let c = build_boot_container(&frames, 12.5).unwrap();
        assert!(validate_boot_container(&c).unwrap_err().contains("limit"));
    }

    #[test]
    fn frame_budget_fits_limit() {
        let worst = BOOT_HDR + BOOT_FRAMES * (BOOT_ENTRY + BOOT_SUB_HDR + BOOT_FRAME_BUDGET) + BOOT_TRAILER;
        assert!(worst <= MAX_BOOT_BYTES);
    }

    #[test]
    fn standby_validation() {
        let img = image::RgbImage::new(LCD_SIZE, LCD_SIZE);
        let mut jpeg = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut jpeg), image::ImageFormat::Jpeg).unwrap();
        validate_standby_jpeg(&jpeg).unwrap();

        let small = image::RgbImage::new(100, 100);
        let mut jpeg = Vec::new();
        small.write_to(&mut std::io::Cursor::new(&mut jpeg), image::ImageFormat::Jpeg).unwrap();
        assert!(validate_standby_jpeg(&jpeg).is_err());
        assert!(validate_standby_jpeg(&[]).is_err());
        assert!(validate_standby_jpeg(b"GIF89a").is_err());
    }

    #[test]
    fn fps_sanitizing() {
        assert_eq!(sanitize_fps(0.0), None);
        assert_eq!(sanitize_fps(-5.0), None);
        assert_eq!(sanitize_fps(f32::NAN), None);
        assert_eq!(sanitize_fps(f32::INFINITY), None);
        assert_eq!(sanitize_fps(0.01), Some(MIN_STREAM_FPS));
        assert_eq!(sanitize_fps(1000.0), Some(MAX_STREAM_FPS));
        assert_eq!(sanitize_fps(15.0), Some(15.0));
    }

    #[test]
    fn ids_must_be_uuids() {
        assert!(is_valid_id("6f1c1c1e-8a7e-4d43-9a43-0d6f0f6a1b2c"));
        assert!(!is_valid_id("../../.."));
        assert!(!is_valid_id(""));
    }

    #[test]
    fn atomic_write_and_corrupt_backup() {
        let dir = std::env::temp_dir().join(format!("th-lcd-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        save_videos(&dir, &[]).unwrap();
        let (v, err) = load_videos(&dir);
        assert!(v.is_empty() && err.is_none());
        assert!(!dir.join("videos.json.tmp").exists());
        fs::write(dir.join(VIDEOS_JSON), b"{ truncated").unwrap();
        let (v, err) = load_videos(&dir);
        assert!(v.is_empty());
        assert!(err.unwrap().contains("corrupt"));
        assert!(dir.join("videos.json.corrupt").exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}
