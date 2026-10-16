use eframe::egui;
use egui::{Color32, FontId, Pos2, RichText, Stroke, TextureHandle, TextureOptions, Vec2};
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use th_linux_lcd::import;
use th_linux_lcd::shared::{
    self, daemon_cmd, decode_thumbnail, frame_file, frames_dir, make_thumbnail, rotated_frames_dir, socket_path,
    BootAnim, Cmd, DaemonStatus, Resp, StandbyImage, Video,
};
use uuid::Uuid;

// ── Palette ───────────────────────────────────────────────────────────────────

const GREEN: Color32 = Color32::from_rgb(0, 210, 55);
const GREEN_DIM: Color32 = Color32::from_rgb(0, 38, 10);
const GREEN_MID: Color32 = Color32::from_rgb(0, 90, 24);
const BG: Color32 = Color32::from_rgb(11, 11, 11);
const BG_PANEL: Color32 = Color32::from_rgb(15, 15, 15);
const BG_CARD: Color32 = Color32::from_rgb(20, 24, 20);
const BG_SEL: Color32 = Color32::from_rgb(10, 28, 13);
const BORDER: Color32 = Color32::from_rgb(0, 48, 13);
const BORDER_SEL: Color32 = Color32::from_rgb(0, 160, 44);
const RED: Color32 = Color32::from_rgb(210, 45, 45);
const AMBER: Color32 = Color32::from_rgb(200, 150, 0);
const TEXT_DIM: Color32 = Color32::from_rgb(0, 100, 28);

const STATUS_POLL: Duration = Duration::from_secs(2);
const THUMB_CARD: f32 = 88.0;
const PREVIEW_SIZE: f32 = 180.0;

// ── Theme ─────────────────────────────────────────────────────────────────────

fn setup_theme(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = BG;
    v.extreme_bg_color = Color32::from_rgb(6, 6, 6);
    v.faint_bg_color = BG_CARD;
    v.override_text_color = Some(GREEN);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT_DIM);
    v.widgets.inactive.bg_fill = BG_CARD;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, GREEN_MID);
    v.widgets.hovered.bg_fill = Color32::from_rgb(18, 32, 20);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, GREEN_MID);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, GREEN);
    v.widgets.active.bg_fill = GREEN_DIM;
    v.widgets.active.bg_stroke = Stroke::new(1.0_f32, GREEN);
    v.widgets.active.fg_stroke = Stroke::new(1.0_f32, GREEN);
    v.selection.bg_fill = GREEN_DIM;
    v.selection.stroke = Stroke::new(1.0_f32, GREEN);
    v.window_rounding = egui::Rounding::same(4.0);
    v.window_shadow = eframe::epaint::Shadow::NONE;
    ctx.set_visuals(v);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = Vec2::new(6.0, 5.0);
    style.spacing.button_padding = Vec2::new(10.0, 6.0);
    style.spacing.window_margin = egui::Margin::same(0.0);
    style.spacing.scroll.bar_width = 5.0;
    ctx.set_style(style);
    ctx.set_pixels_per_point(1.1);
}

// ── State ─────────────────────────────────────────────────────────────────────

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Animation,
    Standby,
    Boot,
}

/// Result of work done off the UI thread. `Ok(None)` means the user cancelled.
enum BgDone {
    Daemon(Result<(), String>, &'static str),
    VideoImported(Result<Option<Video>, String>),
    StandbyImported(Result<Option<StandbyImage>, String>),
    BootImported(Result<Option<BootAnim>, String>),
    BootRotated(Result<BootAnim, String>),
    PlayReady(Result<Cmd, String>),
}

struct BgTask {
    rx: Receiver<BgDone>,
    label: String,
}

/// An upload to device flash awaiting user confirmation.
struct PendingUpload {
    cmd: Cmd,
    what: &'static str,
    done_msg: &'static str,
}

pub struct App {
    data_dir: PathBuf,
    videos: Vec<Video>,
    standby: Vec<StandbyImage>,
    boot_anims: Vec<BootAnim>,
    tab: Tab,
    selected_video: Option<String>,
    selected_standby: Option<String>,
    selected_boot: Option<String>,
    daemon_status: DaemonStatus,
    status_msg: String,
    preview_texture: Option<TextureHandle>,
    preview_frame_idx: usize,
    last_frame_time: Instant,
    thumb_cache: HashMap<String, TextureHandle>,
    bg_task: Option<BgTask>,
    last_status_poll: Option<Instant>,
    daemon_connected: bool,
    daemon_child: Option<Child>,
    confirm: Option<PendingUpload>,
    instance: GuiInstance,
}

fn daemon_reachable() -> bool {
    UnixStream::connect(socket_path()).is_ok()
}

fn file_stem(p: &Path) -> String {
    p.file_stem().unwrap_or_default().to_string_lossy().to_string()
}

/// Run a zenity file picker. `Ok(None)` if the user cancelled.
fn pick_file(title: &str, filter: &str) -> Result<Option<PathBuf>, String> {
    let out = Command::new("zenity")
        .args(["--file-selection", &format!("--title={title}"), &format!("--file-filter={filter}")])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => "zenity is not installed; it is needed for the file picker".to_string(),
            _ => format!("zenity: {e}"),
        })?;
    if !out.status.success() {
        return Ok(None);
    }
    let mut bytes = out.stdout;
    while bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    Ok((!bytes.is_empty()).then(|| PathBuf::from(OsString::from_vec(bytes))))
}

fn import_video_task(data: &Path) -> Result<Option<Video>, String> {
    let Some(src) = pick_file("Select GIF", "GIF | *.gif *.GIF")? else { return Ok(None) };
    let id = Uuid::new_v4().to_string();
    let out_dir = frames_dir(data, &id);
    let (n, fps) = import::import_video(&src, &out_dir)?;
    Ok(Some(Video {
        name: file_stem(&src),
        source_path: src.display().to_string(),
        frame_count: n,
        fps,
        rotation: 0,
        thumbnail: make_thumbnail(&frame_file(&out_dir, 1)),
        id,
    }))
}

fn import_standby_task(data: &Path) -> Result<Option<StandbyImage>, String> {
    let filter = "Images | *.jpg *.jpeg *.png *.gif *.JPG *.JPEG *.PNG *.GIF";
    let Some(src) = pick_file("Select Image", filter)? else { return Ok(None) };
    let dir = shared::standby_dir(data);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let id = Uuid::new_v4().to_string();
    let dest = dir.join(format!("{id}.jpg"));
    import::import_standby(&src, &dest)?;
    Ok(Some(StandbyImage {
        name: file_stem(&src),
        path: dest.display().to_string(),
        thumbnail: make_thumbnail(&dest),
        id,
    }))
}

fn import_boot_task(data: &Path) -> Result<Option<BootAnim>, String> {
    let Some(src) = pick_file("Select GIF for Boot Animation", "GIF | *.gif *.GIF")? else { return Ok(None) };
    let dir = shared::boot_dir(data);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let id = Uuid::new_v4().to_string();
    let out_path = dir.join(format!("{id}.bootanim"));
    let (n, fps, thumbnail) = import::build_boot_from_gif(&src, 0, &out_path)?;
    Ok(Some(BootAnim {
        name: file_stem(&src),
        path: out_path.display().to_string(),
        source_gif: src.display().to_string(),
        frame_count: n,
        fps,
        rotation: 0,
        thumbnail,
        id,
    }))
}

// ── Single instance ───────────────────────────────────────────────────────────

/// Single-instance guard for the GUI. A second launch (from the tray or the
/// app menu) asks the running window to come forward and then exits.
struct GuiInstance {
    _lock: Option<std::fs::File>,
    show_requested: Arc<AtomicBool>,
    ctx: Arc<OnceLock<egui::Context>>,
    socket: Option<PathBuf>,
}

fn request_show(sock: &Path) -> bool {
    UnixStream::connect(sock).and_then(|mut s| s.write_all(b"show\n")).is_ok()
}

impl GuiInstance {
    /// Returns `None` if another GUI is running (it has been asked to show itself).
    fn acquire() -> Option<Self> {
        let sock = shared::gui_socket_path();
        for _ in 0..20 {
            if request_show(&sock) {
                return None;
            }
            match shared::lock_instance(shared::GUI_LOCK) {
                Ok(Some(lock)) => return Some(Self::listen(Some(lock), sock)),
                Ok(None) => std::thread::sleep(Duration::from_millis(100)), // another GUI is starting
                Err(e) => {
                    eprintln!("[gui] {e}; running without single-instance support");
                    return Some(Self::listen(None, sock));
                }
            }
        }
        None
    }

    fn listen(lock: Option<std::fs::File>, sock: PathBuf) -> Self {
        let show_requested = Arc::new(AtomicBool::new(false));
        let ctx: Arc<OnceLock<egui::Context>> = Arc::new(OnceLock::new());
        let mut socket = None;
        if lock.is_some() {
            // We hold the lock, so any existing socket is stale.
            let _ = std::fs::remove_file(&sock);
            match UnixListener::bind(&sock) {
                Ok(listener) => {
                    let _ = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600));
                    let (flag, ctx) = (Arc::clone(&show_requested), Arc::clone(&ctx));
                    std::thread::spawn(move || {
                        for mut conn in listener.incoming().flatten() {
                            let mut buf = [0u8; 16];
                            let _ = conn.set_read_timeout(Some(Duration::from_secs(1)));
                            if conn.read(&mut buf).is_ok_and(|n| buf[..n].starts_with(b"show")) {
                                flag.store(true, Ordering::Relaxed);
                                if let Some(ctx) = ctx.get() {
                                    ctx.request_repaint();
                                }
                            }
                        }
                    });
                    socket = Some(sock);
                }
                Err(e) => eprintln!("[gui] cannot bind {}: {e}", sock.display()),
            }
        }
        Self { _lock: lock, show_requested, ctx, socket }
    }
}

impl Drop for GuiInstance {
    fn drop(&mut self) {
        if let Some(sock) = &self.socket {
            let _ = std::fs::remove_file(sock);
        }
    }
}

impl App {
    fn new(cc: &eframe::CreationContext, instance: GuiInstance) -> Self {
        setup_theme(&cc.egui_ctx);
        let _ = instance.ctx.set(cc.egui_ctx.clone());
        let data = shared::data_dir();
        let (videos, e1) = shared::load_videos(&data);
        let (standby, e2) = shared::load_standby(&data);
        let (boot_anims, e3) = shared::load_boot(&data);
        let warnings: Vec<String> = [e1, e2, e3].into_iter().flatten().collect();
        let selected_video = videos.first().map(|v| v.id.clone());

        let mut app = App {
            data_dir: data,
            videos,
            standby,
            boot_anims,
            tab: Tab::Animation,
            selected_video,
            selected_standby: None,
            selected_boot: None,
            daemon_status: DaemonStatus::default(),
            status_msg: String::new(),
            preview_texture: None,
            preview_frame_idx: 0,
            last_frame_time: Instant::now(),
            thumb_cache: HashMap::new(),
            bg_task: None,
            last_status_poll: None,
            daemon_connected: false,
            daemon_child: None,
            confirm: None,
            instance,
        };

        app.ensure_daemon();
        if !warnings.is_empty() {
            app.status_msg = format!("Warning: {}", warnings.join("; "));
        }
        app
    }

    fn ensure_daemon(&mut self) {
        if daemon_reachable() {
            return;
        }
        // Prefer the systemd user service when it is installed.
        let via_systemd = Command::new("systemctl")
            .args(["--user", "start", "th-linux-lcd.service"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !via_systemd {
            let exe = match std::env::current_exe() {
                Ok(e) => e,
                Err(e) => {
                    self.status_msg = format!("Failed to start daemon: {e}");
                    return;
                }
            };
            // Own process group, so Ctrl-C in the GUI's terminal does not kill it.
            match Command::new(exe).arg("--daemon").stdin(Stdio::null()).process_group(0).spawn() {
                Ok(child) => self.daemon_child = Some(child),
                Err(e) => {
                    self.status_msg = format!("Failed to start daemon: {e}");
                    return;
                }
            }
        }
        for _ in 0..30 {
            std::thread::sleep(Duration::from_millis(100));
            if daemon_reachable() {
                break;
            }
        }
        self.status_msg = if daemon_reachable() {
            "Daemon started".into()
        } else {
            "Daemon did not start; run `th-linux-lcd --daemon` in a terminal to see why".into()
        };
        self.last_status_poll = None;
    }

    // ── Persistence ───────────────────────────────────────────────────────────

    fn save_videos(&mut self) {
        if let Err(e) = shared::save_videos(&self.data_dir, &self.videos) {
            self.status_msg = format!("Error: {e}");
        }
    }

    fn save_standby(&mut self) {
        if let Err(e) = shared::save_standby(&self.data_dir, &self.standby) {
            self.status_msg = format!("Error: {e}");
        }
    }

    fn save_boot(&mut self) {
        if let Err(e) = shared::save_boot(&self.data_dir, &self.boot_anims) {
            self.status_msg = format!("Error: {e}");
        }
    }

    // ── Thumbnails ────────────────────────────────────────────────────────────

    fn get_thumb(&mut self, ctx: &egui::Context, id: &str, b64: &str) -> Option<egui::TextureId> {
        if !self.thumb_cache.contains_key(id) {
            if let Some((pixels, size)) = decode_thumbnail(b64) {
                let ci = egui::ColorImage::from_rgba_unmultiplied(size, &pixels);
                self.thumb_cache.insert(id.to_string(), ctx.load_texture(id, ci, TextureOptions::LINEAR));
            }
        }
        self.thumb_cache.get(id).map(|t| t.id())
    }

    // ── Preview ───────────────────────────────────────────────────────────────

    fn tick_anim_preview(&mut self, ctx: &egui::Context) {
        let Some(ref id) = self.selected_video.clone() else { return };
        let Some(video) = self.videos.iter().find(|v| v.id == *id) else { return };
        if video.frame_count == 0 {
            return;
        }
        let frame_dur = Duration::from_secs_f32(1.0 / video.fps.max(1.0));
        if self.last_frame_time.elapsed() >= frame_dur {
            self.preview_frame_idx = (self.preview_frame_idx + 1) % video.frame_count;
            self.last_frame_time = Instant::now();
            let path = frame_file(&frames_dir(&self.data_dir, id), self.preview_frame_idx + 1);
            if let Ok(bytes) = std::fs::read(&path) {
                if let Ok(img) = image::load_from_memory(&bytes) {
                    let rgba = img.to_rgba8();
                    let ci = egui::ColorImage::from_rgba_unmultiplied(
                        [rgba.width() as usize, rgba.height() as usize],
                        &rgba,
                    );
                    self.preview_texture = Some(ctx.load_texture("preview", ci, TextureOptions::LINEAR));
                }
            }
        }
        ctx.request_repaint_after(frame_dur);
    }

    fn load_standby_preview(&mut self, ctx: &egui::Context) {
        let Some(ref id) = self.selected_standby.clone() else { return };
        let Some(img) = self.standby.iter().find(|i| i.id == *id) else { return };
        if let Ok(bytes) = std::fs::read(&img.path) {
            if let Ok(decoded) = image::load_from_memory(&bytes) {
                let rgba = decoded.to_rgba8();
                let ci =
                    egui::ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], &rgba);
                self.preview_texture = Some(ctx.load_texture("preview_sb", ci, TextureOptions::LINEAR));
            }
        }
    }

    fn load_boot_preview(&mut self, ctx: &egui::Context) {
        let Some(ref id) = self.selected_boot.clone() else { return };
        let Some(anim) = self.boot_anims.iter().find(|a| a.id == *id) else { return };
        if anim.thumbnail.is_empty() {
            return;
        }
        if let Some((pixels, size)) = decode_thumbnail(&anim.thumbnail) {
            let ci = egui::ColorImage::from_rgba_unmultiplied(size, &pixels);
            self.preview_texture = Some(ctx.load_texture("preview_boot", ci, TextureOptions::LINEAR));
        }
    }

    // ── Daemon ────────────────────────────────────────────────────────────────

    fn poll_status(&mut self) {
        if self.last_status_poll.is_some_and(|t| t.elapsed() < STATUS_POLL) {
            return;
        }
        self.last_status_poll = Some(Instant::now());
        // Reap a daemon we spawned if it has exited.
        if let Some(child) = &mut self.daemon_child {
            if !matches!(child.try_wait(), Ok(None)) {
                self.daemon_child = None;
            }
        }
        if let Ok(Resp::Status(s)) = daemon_cmd(&Cmd::Status) {
            self.daemon_status = s;
            self.daemon_connected = true;
        } else {
            self.daemon_status = DaemonStatus::default();
            self.daemon_connected = false;
        }
    }

    fn daemon_up(&self) -> bool {
        self.daemon_connected
    }

    fn busy(&self) -> bool {
        self.bg_task.is_some() || self.daemon_status.flash_busy
    }

    /// Send a quick command synchronously; returns true on success.
    fn send(&mut self, cmd: Cmd) -> bool {
        let ok = match daemon_cmd(&cmd) {
            Ok(Resp::Ok) | Ok(Resp::Status(_)) => true,
            Ok(Resp::Err { msg }) => {
                self.status_msg = format!("Error: {msg}");
                false
            }
            Err(e) => {
                self.status_msg = format!("Daemon: {e}");
                false
            }
        };
        self.last_status_poll = None;
        ok
    }

    fn spawn_bg(&mut self, label: impl Into<String>, work: impl FnOnce() -> BgDone + Send + 'static) {
        if self.bg_task.is_some() {
            return;
        }
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let _ = tx.send(work());
        });
        self.bg_task = Some(BgTask { rx, label: label.into() });
    }

    fn poll_bg(&mut self) {
        let Some(task) = &self.bg_task else { return };
        match task.rx.try_recv() {
            Ok(done) => {
                self.bg_task = None;
                self.finish_bg(done);
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.bg_task = None;
                self.status_msg = "Error: background task crashed".into();
            }
        }
    }

    fn finish_bg(&mut self, done: BgDone) {
        self.last_status_poll = None;
        match done {
            BgDone::Daemon(Ok(()), msg) => self.status_msg = msg.into(),
            BgDone::Daemon(Err(e), _) => self.status_msg = format!("Error: {e}"),

            BgDone::VideoImported(Ok(Some(v))) => {
                self.status_msg = format!("Imported {} frames @ {:.0}fps", v.frame_count, v.fps);
                self.selected_video = Some(v.id.clone());
                self.videos.push(v);
                self.save_videos();
                self.preview_frame_idx = 0;
                self.preview_texture = None;
            }
            BgDone::StandbyImported(Ok(Some(img))) => {
                self.status_msg = "Image added".into();
                self.selected_standby = Some(img.id.clone());
                self.standby.push(img);
                self.save_standby();
                self.preview_texture = None;
            }
            BgDone::BootImported(Ok(Some(anim))) => {
                self.status_msg = format!("Boot animation: {} frames @ {:.1}fps", anim.frame_count, anim.fps);
                self.selected_boot = Some(anim.id.clone());
                self.boot_anims.push(anim);
                self.save_boot();
                self.preview_texture = None;
            }
            BgDone::VideoImported(Ok(None)) | BgDone::StandbyImported(Ok(None)) | BgDone::BootImported(Ok(None)) => {
                self.status_msg.clear();
            }
            BgDone::VideoImported(Err(e)) | BgDone::StandbyImported(Err(e)) | BgDone::BootImported(Err(e)) => {
                self.status_msg = format!("Import failed: {e}");
            }

            BgDone::BootRotated(Ok(anim)) => {
                self.status_msg = format!("Rotated to {}°", anim.rotation);
                self.thumb_cache.remove(&anim.id);
                if let Some(a) = self.boot_anims.iter_mut().find(|a| a.id == anim.id) {
                    *a = anim;
                }
                self.save_boot();
                self.preview_texture = None;
            }
            BgDone::BootRotated(Err(e)) => self.status_msg = format!("Rotation failed: {e}"),

            BgDone::PlayReady(Ok(cmd)) => {
                if self.send(cmd) {
                    self.status_msg = "Streaming".into();
                }
            }
            BgDone::PlayReady(Err(e)) => self.status_msg = format!("Rotate failed: {e}"),
        }
    }

    // ── Actions ───────────────────────────────────────────────────────────────

    fn add_video(&mut self) {
        let data = self.data_dir.clone();
        self.spawn_bg("Importing GIF...", move || BgDone::VideoImported(import_video_task(&data)));
    }

    fn add_standby(&mut self) {
        let data = self.data_dir.clone();
        self.spawn_bg("Converting image...", move || BgDone::StandbyImported(import_standby_task(&data)));
    }

    fn add_boot(&mut self) {
        let data = self.data_dir.clone();
        self.spawn_bg("Importing boot animation...", move || BgDone::BootImported(import_boot_task(&data)));
    }

    fn rotate_video(&mut self, id: &str, delta: i32) {
        let Some(v) = self.videos.iter_mut().find(|v| v.id == id) else { return };
        v.rotation = (v.rotation + delta).rem_euclid(360);
        let new_rot = v.rotation;
        // Invalidate cached rotated frames so next play regenerates them
        if shared::is_valid_id(id) {
            let _ = std::fs::remove_dir_all(rotated_frames_dir(&self.data_dir, id));
        }
        self.save_videos();
        self.status_msg = format!("Rotation: {new_rot}°");
    }

    fn rotate_boot(&mut self, id: &str, delta: i32) {
        let Some(anim) = self.boot_anims.iter().find(|a| a.id == id).cloned() else { return };
        let rotation = (anim.rotation + delta).rem_euclid(360);
        self.spawn_bg(format!("Re-encoding at {rotation}°..."), move || {
            let result = import::build_boot_from_gif(Path::new(&anim.source_gif), rotation, Path::new(&anim.path))
                .map(|(frame_count, fps, thumbnail)| BootAnim { rotation, frame_count, fps, thumbnail, ..anim });
            BgDone::BootRotated(result)
        });
    }

    fn play_selected(&mut self) {
        let Some(id) = self.selected_video.clone() else { return };
        let Some(v) = self.videos.iter().find(|v| v.id == id).cloned() else { return };
        let src = frames_dir(&self.data_dir, &v.id);
        let dst = rotated_frames_dir(&self.data_dir, &v.id);
        let (count, rotation) = (v.frame_count, v.rotation);
        let play = move |dir: &Path| Cmd::Play {
            frames_dir: dir.display().to_string(),
            fps: v.fps,
            frame_count: v.frame_count,
            id: v.id.clone(),
        };
        if rotation == 0 {
            if self.send(play(&src)) {
                self.status_msg = "Streaming".into();
            }
            return;
        }
        self.spawn_bg("Applying rotation...", move || {
            BgDone::PlayReady(import::rotate_frames(&src, &dst, count, rotation).map(|()| play(&dst)))
        });
    }

    fn start_upload(&mut self, p: PendingUpload) {
        let label = format!("Writing {} to device flash. Do not unplug or power off...", p.what);
        self.status_msg.clear();
        self.spawn_bg(label, move || {
            let result = match daemon_cmd(&p.cmd) {
                Ok(Resp::Ok) => Ok(()),
                Ok(Resp::Err { msg }) => Err(msg),
                Ok(Resp::Status(_)) => Err("unexpected reply from daemon".into()),
                Err(e) => Err(e),
            };
            BgDone::Daemon(result, p.done_msg)
        });
    }

    fn confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some(p) = &self.confirm else { return };
        let mut decision = None;
        egui::Window::new("Write to device flash?")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .frame(egui::Frame::window(&ctx.style()).fill(BG_PANEL).inner_margin(egui::Margin::same(14.0)))
            .show(ctx, |ui| {
                ui.set_max_width(340.0);
                ui.label(
                    RichText::new(format!(
                        "This writes the {} to the cooler's flash memory.\n\n\
                     Do not unplug the USB cable or power off the PC until the status bar \
                     says it is done (up to about a minute).\n\n\
                     This is unofficial software. Use at your own risk.",
                        p.what
                    ))
                    .color(GREEN)
                    .size(11.0),
                );
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(
                            egui::Button::new(RichText::new("WRITE").color(GREEN).monospace())
                                .fill(GREEN_DIM)
                                .stroke(Stroke::new(1.0_f32, GREEN_MID)),
                        )
                        .clicked()
                    {
                        decision = Some(true);
                    }
                    if ui.button(RichText::new("CANCEL").color(TEXT_DIM).monospace()).clicked() {
                        decision = Some(false);
                    }
                });
            });
        match decision {
            Some(true) => {
                if let Some(p) = self.confirm.take() {
                    self.start_upload(p);
                }
            }
            Some(false) => self.confirm = None,
            None => {}
        }
    }
}

// ── Circle image draw (mesh-based, no corner hacks) ───────────────────────────

fn draw_circle_image(painter: &egui::Painter, center: Pos2, radius: f32, tex_id: egui::TextureId) {
    let n = 64u32;
    let mut mesh = egui::Mesh::with_texture(tex_id);

    let push = |mesh: &mut egui::Mesh, pos: Pos2, uv: egui::Pos2| {
        mesh.vertices.push(eframe::epaint::Vertex { pos, uv, color: Color32::WHITE });
    };

    // Center vertex at UV (0.5, 0.5)
    push(&mut mesh, center, egui::pos2(0.5, 0.5));

    for i in 0..=n {
        let angle = i as f32 / n as f32 * std::f32::consts::TAU;
        let cos = angle.cos();
        let sin = angle.sin();
        let pos = center + Vec2::new(cos, sin) * radius;
        let uv = egui::pos2(0.5 + cos * 0.5, 0.5 + sin * 0.5);
        push(&mut mesh, pos, uv);
        if i > 0 {
            mesh.indices.push(0);
            mesh.indices.push(i);
            mesh.indices.push(i + 1);
        }
    }

    painter.add(egui::Shape::mesh(mesh));
}

/// Circular preview of the current texture; `ring` colours the outline.
fn preview_circle(ui: &mut egui::Ui, tex: Option<&TextureHandle>, ring: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(PREVIEW_SIZE), egui::Sense::hover());
    let center = rect.center();
    let r = PREVIEW_SIZE / 2.0;
    let p = ui.painter();
    p.circle_filled(center, r, Color32::from_rgb(5, 5, 5));
    if let Some(tex) = tex {
        draw_circle_image(p, center, r - 1.0, tex.id());
    } else {
        p.text(center, egui::Align2::CENTER_CENTER, "NO SIGNAL", FontId::monospace(10.0), TEXT_DIM);
    }
    p.circle_stroke(center, r - 1.0, Stroke::new(2.0_f32, ring));
}

/// Primary action button (green when enabled).
fn primary_button(ui: &mut egui::Ui, enabled: bool, label: &str) -> bool {
    ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(label).color(if enabled { GREEN } else { TEXT_DIM }).monospace().size(12.0))
            .fill(if enabled { GREEN_DIM } else { BG_CARD })
            .stroke(Stroke::new(1.0_f32, if enabled { GREEN_MID } else { BORDER }))
            .min_size(Vec2::new(140.0, 30.0)),
    )
    .clicked()
}

// ── Gallery card ──────────────────────────────────────────────────────────────

fn gallery_card(
    ui: &mut egui::Ui,
    id: &str,
    name: &str,
    tex: Option<egui::TextureId>,
    selected: bool,
    size: f32,
) -> (bool, bool) {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), egui::Sense::click());
    let del_rect = egui::Rect::from_min_size(egui::pos2(rect.max.x - 18.0, rect.min.y), Vec2::splat(18.0));

    // Always register delete interaction so egui tracks hover/click on it
    let del_resp = ui.interact(del_rect, egui::Id::new(("del", id)), egui::Sense::click());

    let hovered = resp.hovered() || del_resp.hovered();
    let p = ui.painter();

    p.rect_filled(rect, 4.0, if selected { BG_SEL } else { BG_CARD });
    p.rect_stroke(
        rect,
        4.0,
        Stroke::new(if selected { 1.5_f32 } else { 1.0_f32 }, if selected { BORDER_SEL } else { BORDER }),
    );

    if let Some(tid) = tex {
        let img_r = rect.shrink(4.0).with_max_y(rect.max.y - 17.0);
        p.image(tid, img_r, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
    }

    let name_r = egui::Rect::from_min_max(egui::pos2(rect.min.x, rect.max.y - 17.0), rect.max);
    p.rect_filled(name_r, egui::Rounding { sw: 4.0, se: 4.0, ..Default::default() }, Color32::from_black_alpha(210));
    let label: std::borrow::Cow<str> = if name.chars().count() > 13 {
        format!("{}…", name.chars().take(12).collect::<String>()).into()
    } else {
        name.into()
    };
    p.text(
        name_r.center(),
        egui::Align2::CENTER_CENTER,
        label.as_ref(),
        FontId::monospace(8.0),
        if selected { GREEN } else { TEXT_DIM },
    );

    if hovered {
        p.rect_filled(
            del_rect,
            egui::Rounding { ne: 4.0, ..Default::default() },
            Color32::from_rgba_premultiplied(90, 10, 10, 230),
        );
        p.text(del_rect.center(), egui::Align2::CENTER_CENTER, "x", FontId::proportional(10.0), RED);
    }

    let del_clicked = del_resp.clicked();
    (resp.clicked() && !del_clicked, del_clicked)
}

/// Gallery grid; returns (clicked id, delete-clicked id).
fn gallery(
    app: &mut App,
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    scroll_id: &str,
    items: Vec<(String, String, String)>,
    selected: Option<&str>,
) -> (Option<String>, Option<String>) {
    let mut sel = None;
    let mut del = None;
    egui::ScrollArea::vertical().id_salt(scroll_id).show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.style_mut().spacing.item_spacing = Vec2::splat(5.0);
            for (id, name, b64) in &items {
                let tex = app.get_thumb(ctx, id, b64);
                let (clicked, deleted) = gallery_card(ui, id, name, tex, selected == Some(id.as_str()), THUMB_CARD);
                if clicked {
                    sel = Some(id.clone());
                }
                if deleted {
                    del = Some(id.clone());
                }
            }
        });
    });
    (sel, del)
}

fn section_header(ui: &mut egui::Ui, title: &str, button: &str, enabled: bool) -> bool {
    let mut clicked = false;
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).color(TEXT_DIM).monospace().size(10.0));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            clicked = ui
                .add_enabled(enabled, egui::Button::new(RichText::new(button).color(TEXT_DIM).monospace().size(10.0)))
                .clicked();
        });
    });
    clicked
}

// ── eframe::App ───────────────────────────────────────────────────────────────

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.instance.show_requested.swap(false, Ordering::Relaxed) {
            // Best effort: Wayland compositors may only highlight the window instead.
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(egui::UserAttentionType::Informational));
        }
        self.poll_bg();
        self.poll_status();
        ctx.request_repaint_after(if self.bg_task.is_some() { Duration::from_millis(200) } else { STATUS_POLL });

        if self.tab == Tab::Animation {
            self.tick_anim_preview(ctx);
        }
        if self.tab == Tab::Standby && self.preview_texture.is_none() {
            self.load_standby_preview(ctx);
        }
        if self.tab == Tab::Boot && self.preview_texture.is_none() {
            self.load_boot_preview(ctx);
        }

        self.confirm_dialog(ctx);

        // ── Top bar ───────────────────────────────────────────────────────────
        egui::TopBottomPanel::top("topbar")
            .frame(egui::Frame::none().fill(Color32::from_rgb(7, 7, 7)).inner_margin(egui::Margin {
                left: 14.0,
                right: 14.0,
                top: 7.0,
                bottom: 7.0,
            }))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("TH LINUX LCD").color(GREEN).monospace().size(14.0).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // OFF button
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new("OFF").color(Color32::from_rgb(170, 35, 35)).monospace().size(11.0),
                                )
                                .fill(Color32::from_rgb(28, 7, 7))
                                .stroke(Stroke::new(1.0_f32, Color32::from_rgb(90, 18, 18)))
                                .min_size(Vec2::new(40.0, 24.0)),
                            )
                            .clicked()
                        {
                            self.send(Cmd::ScreenOff);
                        }

                        // ON button
                        if ui
                            .add(
                                egui::Button::new(RichText::new("ON").color(GREEN).monospace().size(11.0))
                                    .fill(GREEN_DIM)
                                    .stroke(Stroke::new(1.0_f32, GREEN_MID))
                                    .min_size(Vec2::new(40.0, 24.0)),
                            )
                            .clicked()
                        {
                            self.send(Cmd::ScreenOn);
                        }

                        ui.add_space(6.0);

                        // Status dot + label
                        let (dot, text, text_color) = if !self.daemon_up() {
                            (RED, "NO DAEMON", RED)
                        } else if !self.daemon_status.device_found {
                            (Color32::from_rgb(180, 70, 0), "NO DEVICE", Color32::from_rgb(180, 70, 0))
                        } else if self.daemon_status.flash_busy {
                            (AMBER, "WRITING FLASH", AMBER)
                        } else if self.daemon_status.streaming {
                            (GREEN, "STREAMING", GREEN)
                        } else {
                            (AMBER, "IDLE", AMBER)
                        };

                        ui.label(RichText::new(text).color(text_color).monospace().size(11.0));
                        let (r, _) = ui.allocate_exact_size(Vec2::splat(10.0), egui::Sense::hover());
                        ui.painter().circle_filled(r.center(), 4.0, dot);

                        if !self.daemon_up() {
                            ui.add_space(6.0);
                            if ui.button(RichText::new("START").color(TEXT_DIM).monospace().size(10.0)).clicked() {
                                self.ensure_daemon();
                            }
                        } else if !self.daemon_status.device_found {
                            ui.add_space(6.0);
                            if ui
                                .add(
                                    egui::Button::new(
                                        RichText::new("RETRY")
                                            .color(Color32::from_rgb(180, 70, 0))
                                            .monospace()
                                            .size(10.0),
                                    )
                                    .fill(Color32::from_rgb(28, 14, 0))
                                    .stroke(Stroke::new(1.0_f32, Color32::from_rgb(120, 50, 0)))
                                    .min_size(Vec2::new(44.0, 22.0)),
                                )
                                .clicked()
                            {
                                self.last_status_poll = None;
                            }
                        }
                    });
                });
            });

        // ── Bottom status bar ─────────────────────────────────────────────────
        egui::TopBottomPanel::bottom("statusbar")
            .frame(egui::Frame::none().fill(Color32::from_rgb(7, 7, 7)).inner_margin(egui::Margin {
                left: 14.0,
                right: 14.0,
                top: 5.0,
                bottom: 5.0,
            }))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if let Some(ref task) = self.bg_task {
                        let (r, _) = ui.allocate_exact_size(Vec2::splat(10.0), egui::Sense::hover());
                        ui.painter().circle_filled(r.center(), 4.0, AMBER);
                        ui.label(RichText::new(&task.label).color(AMBER).monospace().size(10.0));
                    } else if !self.status_msg.is_empty() {
                        ui.label(RichText::new(&self.status_msg).color(TEXT_DIM).monospace().size(10.0));
                    }
                });
            });

        // ── Left sidebar ──────────────────────────────────────────────────────
        egui::SidePanel::left("sidebar")
            .resizable(false)
            .exact_width(110.0)
            .frame(egui::Frame::none().fill(BG_PANEL).inner_margin(egui::Margin {
                left: 8.0,
                right: 8.0,
                top: 12.0,
                bottom: 12.0,
            }))
            .show(ctx, |ui| {
                ui.add_space(4.0);
                for (t, lbl) in [(Tab::Animation, "ANIMATION"), (Tab::Standby, "STANDBY"), (Tab::Boot, "BOOT")] {
                    let active = self.tab == t;
                    let btn = egui::Button::new(
                        RichText::new(lbl).color(if active { GREEN } else { TEXT_DIM }).monospace().size(11.0),
                    )
                    .fill(if active { BG_SEL } else { Color32::TRANSPARENT })
                    .stroke(Stroke::new(
                        if active { 1.0_f32 } else { 0.0_f32 },
                        if active { BORDER_SEL } else { Color32::TRANSPARENT },
                    ))
                    .min_size(Vec2::new(ui.available_width(), 30.0));
                    if ui.add(btn).clicked() && self.tab != t {
                        self.tab = t;
                        self.preview_texture = None;
                    }
                    ui.add_space(2.0);
                }
            });

        // ── Main content ──────────────────────────────────────────────────────
        egui::CentralPanel::default().frame(egui::Frame::none().fill(BG).inner_margin(egui::Margin::same(14.0))).show(
            ctx,
            |ui| match self.tab {
                Tab::Animation => self.ui_animation(ui, ctx),
                Tab::Standby => self.ui_standby(ui, ctx),
                Tab::Boot => self.ui_boot(ui, ctx),
            },
        );
    }
}

impl App {
    fn ui_animation(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // Top section: preview left, info+button right
        ui.horizontal(|ui| {
            let ring = if self.daemon_status.streaming { Color32::from_rgb(0, 150, 38) } else { BORDER };
            preview_circle(ui, self.preview_texture.as_ref(), ring);

            ui.add_space(16.0);

            // Info + controls
            ui.vertical(|ui| {
                ui.add_space(8.0);
                if let Some(ref id) = self.selected_video.clone() {
                    if let Some(v) = self.videos.iter().find(|v| v.id == *id) {
                        ui.label(RichText::new(&v.name).color(GREEN).monospace().size(12.0));
                        ui.add_space(2.0);
                        ui.label(
                            RichText::new(format!("{} frames  {:.0} fps", v.frame_count, v.fps))
                                .color(TEXT_DIM)
                                .monospace()
                                .size(10.0),
                        );
                        ui.add_space(6.0);

                        // Rotation controls
                        let rot = v.rotation;
                        let vid_id = id.clone();
                        ui.horizontal(|ui| {
                            let btn_size = Vec2::new(28.0, 24.0);
                            let small_btn = |label: &str| {
                                egui::Button::new(RichText::new(label).color(TEXT_DIM).monospace().size(11.0))
                                    .fill(BG_CARD)
                                    .stroke(Stroke::new(1.0_f32, BORDER))
                                    .min_size(btn_size)
                            };
                            if ui.add(small_btn("CCW")).clicked() {
                                self.rotate_video(&vid_id, -90);
                            }
                            ui.label(RichText::new(format!("{}°", rot)).color(TEXT_DIM).monospace().size(10.0));
                            if ui.add(small_btn("CW")).clicked() {
                                self.rotate_video(&vid_id, 90);
                            }
                        });
                    }
                } else {
                    ui.label(RichText::new("no animation selected").color(TEXT_DIM).monospace().size(11.0));
                }

                ui.add_space(8.0);

                let has_sel = self.selected_video.is_some();
                let is_playing = self.daemon_status.streaming && self.daemon_status.playing_id == self.selected_video;

                if is_playing {
                    if ui
                        .add(
                            egui::Button::new(RichText::new("STOP").color(RED).monospace().size(12.0))
                                .fill(Color32::from_rgb(28, 6, 6))
                                .stroke(Stroke::new(1.0_f32, RED))
                                .min_size(Vec2::new(140.0, 30.0)),
                        )
                        .clicked()
                        && self.send(Cmd::Stop)
                    {
                        self.status_msg = "Stopped".into();
                    }
                } else if primary_button(ui, has_sel && self.bg_task.is_none(), "SEND TO LCD") {
                    self.play_selected();
                }
            });
        });

        ui.add_space(14.0);
        ui.add(egui::Separator::default().horizontal());
        ui.add_space(6.0);

        if section_header(ui, "ANIMATIONS", "+ ADD GIF", self.bg_task.is_none()) {
            self.add_video();
        }
        ui.add_space(6.0);

        if self.videos.is_empty() {
            ui.label(
                RichText::new("No animations — import a GIF to get started").color(TEXT_DIM).monospace().size(11.0),
            );
            return;
        }

        let items = self.videos.iter().map(|v| (v.id.clone(), v.name.clone(), v.thumbnail.clone())).collect();
        let selected = self.selected_video.clone();
        let (sel, del) = gallery(self, ui, ctx, "anim_scroll", items, selected.as_deref());

        if let Some(id) = sel {
            if self.selected_video.as_deref() != Some(&id) {
                self.selected_video = Some(id);
                self.preview_frame_idx = 0;
                self.preview_texture = None;
            }
        }
        if let Some(id) = del {
            if self.daemon_status.playing_id.as_deref() == Some(&id) {
                self.send(Cmd::Stop);
            }
            if shared::is_valid_id(&id) {
                let _ = std::fs::remove_dir_all(frames_dir(&self.data_dir, &id));
                let _ = std::fs::remove_dir_all(rotated_frames_dir(&self.data_dir, &id));
            }
            self.videos.retain(|v| v.id != id);
            self.save_videos();
            self.thumb_cache.remove(&id);
            if self.selected_video.as_deref() == Some(&id) {
                self.selected_video = self.videos.first().map(|v| v.id.clone());
                self.preview_texture = None;
            }
        }
    }

    fn ui_standby(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            preview_circle(ui, self.preview_texture.as_ref(), BORDER);

            ui.add_space(16.0);

            ui.vertical(|ui| {
                ui.add_space(8.0);
                if let Some(ref id) = self.selected_standby.clone() {
                    if let Some(img) = self.standby.iter().find(|i| i.id == *id) {
                        ui.label(RichText::new(&img.name).color(GREEN).monospace().size(12.0));
                    }
                } else {
                    ui.label(RichText::new("no image selected").color(TEXT_DIM).monospace().size(11.0));
                }

                ui.add_space(12.0);

                let busy = self.busy();
                let enabled = self.selected_standby.is_some() && !busy && self.daemon_status.device_found;
                let lbl = if busy { "BUSY..." } else { "UPLOAD TO LCD" };

                if primary_button(ui, enabled, lbl) {
                    if let Some(id) = &self.selected_standby {
                        if let Some(img) = self.standby.iter().find(|i| i.id == *id) {
                            self.confirm = Some(PendingUpload {
                                cmd: Cmd::UploadStandby { path: img.path.clone() },
                                what: "standby image",
                                done_msg: "Standby image written.",
                            });
                        }
                    }
                }
            });
        });

        ui.add_space(14.0);
        ui.add(egui::Separator::default().horizontal());
        ui.add_space(6.0);

        if section_header(ui, "STANDBY IMAGES", "+ ADD IMAGE", self.bg_task.is_none()) {
            self.add_standby();
        }
        ui.add_space(6.0);

        if self.standby.is_empty() {
            ui.label(
                RichText::new("No standby images — import one to get started").color(TEXT_DIM).monospace().size(11.0),
            );
            return;
        }

        let items = self.standby.iter().map(|i| (i.id.clone(), i.name.clone(), i.thumbnail.clone())).collect();
        let selected = self.selected_standby.clone();
        let (sel, del) = gallery(self, ui, ctx, "sb_scroll", items, selected.as_deref());

        if let Some(id) = sel {
            if self.selected_standby.as_deref() != Some(&id) {
                self.selected_standby = Some(id);
                self.preview_texture = None;
            }
        }
        if let Some(id) = del {
            if let Some(img) = self.standby.iter().find(|i| i.id == id) {
                let _ = std::fs::remove_file(&img.path);
            }
            self.standby.retain(|i| i.id != id);
            self.save_standby();
            self.thumb_cache.remove(&id);
            if self.selected_standby.as_deref() == Some(&id) {
                self.selected_standby = None;
                self.preview_texture = None;
            }
        }
    }

    fn ui_boot(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            preview_circle(ui, self.preview_texture.as_ref(), BORDER);

            ui.add_space(16.0);

            ui.vertical(|ui| {
                ui.add_space(8.0);
                if let Some(ref id) = self.selected_boot.clone() {
                    if let Some(anim) = self.boot_anims.iter().find(|a| a.id == *id) {
                        ui.label(RichText::new(&anim.name).color(GREEN).monospace().size(12.0));
                        ui.add_space(2.0);
                        ui.label(
                            RichText::new(format!("{} frames  {:.1} fps", anim.frame_count, anim.fps))
                                .color(TEXT_DIM)
                                .monospace()
                                .size(10.0),
                        );
                    }
                } else {
                    ui.label(RichText::new("no boot animation selected").color(TEXT_DIM).monospace().size(11.0));
                }

                ui.add_space(12.0);

                let has_sel = self.selected_boot.is_some();
                let busy = self.busy();
                let enabled = has_sel && !busy && self.daemon_status.device_found;
                let lbl = if busy { "BUSY..." } else { "UPLOAD TO LCD" };

                if primary_button(ui, enabled, lbl) {
                    if let Some(id) = &self.selected_boot {
                        if let Some(anim) = self.boot_anims.iter().find(|a| a.id == *id) {
                            self.confirm = Some(PendingUpload {
                                cmd: Cmd::UploadBoot { path: anim.path.clone() },
                                what: "boot animation",
                                done_msg: "Boot animation written. Power-cycle the cooler to see it.",
                            });
                        }
                    }
                }

                ui.add_space(8.0);

                let rot_enabled = has_sel && !busy;
                let rot_lbl = if let Some(ref id) = self.selected_boot {
                    self.boot_anims.iter().find(|a| a.id == *id).map(|a| format!("{}°", a.rotation)).unwrap_or_default()
                } else {
                    String::new()
                };
                if !rot_lbl.is_empty() {
                    ui.label(RichText::new(&rot_lbl).color(TEXT_DIM).monospace().size(11.0));
                }
                if ui
                    .add_enabled(
                        rot_enabled,
                        egui::Button::new(RichText::new("↻ 90°").color(TEXT_DIM).monospace().size(11.0))
                            .min_size(Vec2::new(55.0, 26.0)),
                    )
                    .clicked()
                {
                    if let Some(id) = self.selected_boot.clone() {
                        self.rotate_boot(&id, 90);
                    }
                }
                if ui
                    .add_enabled(
                        rot_enabled,
                        egui::Button::new(RichText::new("↺ 90°").color(TEXT_DIM).monospace().size(11.0))
                            .min_size(Vec2::new(55.0, 26.0)),
                    )
                    .clicked()
                {
                    if let Some(id) = self.selected_boot.clone() {
                        self.rotate_boot(&id, -90);
                    }
                }
            });
        });

        ui.add_space(14.0);
        ui.add(egui::Separator::default().horizontal());
        ui.add_space(6.0);

        if section_header(ui, "BOOT ANIMATIONS", "+ ADD GIF", self.bg_task.is_none()) {
            self.add_boot();
        }
        ui.add_space(6.0);

        if self.boot_anims.is_empty() {
            ui.label(
                RichText::new("No boot animations — import a GIF to get started")
                    .color(TEXT_DIM)
                    .monospace()
                    .size(11.0),
            );
            return;
        }

        let items = self.boot_anims.iter().map(|a| (a.id.clone(), a.name.clone(), a.thumbnail.clone())).collect();
        let selected = self.selected_boot.clone();
        let (sel, del) = gallery(self, ui, ctx, "boot_scroll", items, selected.as_deref());

        if let Some(id) = sel {
            if self.selected_boot.as_deref() != Some(&id) {
                self.selected_boot = Some(id);
                self.preview_texture = None;
            }
        }
        if let Some(id) = del {
            if let Some(anim) = self.boot_anims.iter().find(|a| a.id == id) {
                let _ = std::fs::remove_file(&anim.path);
            }
            self.boot_anims.retain(|a| a.id != id);
            self.save_boot();
            self.thumb_cache.remove(&id);
            if self.selected_boot.as_deref() == Some(&id) {
                self.selected_boot = None;
                self.preview_texture = None;
            }
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn run() {
    let Some(instance) = GuiInstance::acquire() else {
        eprintln!("th-linux-lcd: already running; brought the existing window forward");
        return;
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("TH Linux LCD")
            .with_app_id("th-linux-lcd")
            .with_inner_size([640.0, 520.0])
            .with_min_inner_size([500.0, 400.0])
            .with_resizable(true),
        ..Default::default()
    };
    if let Err(e) = eframe::run_native("TH Linux LCD", options, Box::new(|cc| Ok(Box::new(App::new(cc, instance))))) {
        eprintln!("th-linux-lcd: cannot start the GUI: {e}");
        std::process::exit(1);
    }
}
