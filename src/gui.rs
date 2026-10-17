use eframe::egui;
use egui::{Color32, FontId, Pos2, Rect, RichText, Rounding, Sense, Stroke, TextureHandle, TextureOptions, Vec2};
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
use th_linux_lcd::overlay::{self, Layer};
use th_linux_lcd::shared::{
    self, daemon_cmd, decode_thumbnail, frame_file, frames_dir, make_thumbnail, rotated_frames_dir, socket_path,
    BootAnim, Cmd, DaemonStatus, OnDevice, OverlaySettings, OverlayStyle, Resp, StandbyImage, Stat, Video,
    OVERLAY_BOTTOM, OVERLAY_SIZE_RANGE, OVERLAY_TOP, OVERLAY_VERTICAL_RANGE,
};
use uuid::Uuid;

// ── Palette ───────────────────────────────────────────────────────────────────

const GREEN: Color32 = Color32::from_rgb(0, 210, 55);
const GREEN_DIM: Color32 = Color32::from_rgb(0, 38, 10);
const GREEN_MID: Color32 = Color32::from_rgb(0, 90, 24);
const BG: Color32 = Color32::from_rgb(11, 11, 11);
const BG_BAR: Color32 = Color32::from_rgb(7, 7, 7);
const BG_PANEL: Color32 = Color32::from_rgb(15, 15, 15);
const BG_CARD: Color32 = Color32::from_rgb(20, 24, 20);
const BG_SEL: Color32 = Color32::from_rgb(10, 28, 13);
const BG_HOVER: Color32 = Color32::from_rgb(16, 22, 17);
const BORDER: Color32 = Color32::from_rgb(0, 48, 13);
const BORDER_SEL: Color32 = Color32::from_rgb(0, 160, 44);
const RED: Color32 = Color32::from_rgb(210, 45, 45);
const RED_DIM: Color32 = Color32::from_rgb(28, 7, 7);
const ORANGE: Color32 = Color32::from_rgb(180, 70, 0);
const AMBER: Color32 = Color32::from_rgb(200, 150, 0);
const TEXT_DIM: Color32 = Color32::from_rgb(0, 100, 28);
const TEXT_FAINT: Color32 = Color32::from_rgb(0, 64, 18);

const STATUS_POLL: Duration = Duration::from_secs(1);
const CARD_SIZE: f32 = 96.0;
const CARD_LABEL_H: f32 = 18.0;
const PREVIEW_SIZE: f32 = 190.0;
const RADIUS: f32 = 6.0;

// ── Theme ─────────────────────────────────────────────────────────────────────

fn setup_theme(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = BG_PANEL;
    v.window_stroke = Stroke::new(1.0_f32, BORDER);
    v.extreme_bg_color = Color32::from_rgb(6, 6, 6);
    v.faint_bg_color = BG_CARD;
    v.override_text_color = Some(GREEN);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT_DIM);
    v.widgets.inactive.bg_fill = BG_CARD;
    v.widgets.inactive.weak_bg_fill = BG_CARD;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, GREEN_MID);
    v.widgets.hovered.bg_fill = BG_HOVER;
    v.widgets.hovered.weak_bg_fill = BG_HOVER;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, GREEN_MID);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, GREEN);
    v.widgets.active.bg_fill = GREEN_DIM;
    v.widgets.active.weak_bg_fill = GREEN_DIM;
    v.widgets.active.bg_stroke = Stroke::new(1.0_f32, GREEN);
    v.widgets.active.fg_stroke = Stroke::new(1.0_f32, GREEN);
    for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active] {
        w.rounding = Rounding::same(4.0);
    }
    v.selection.bg_fill = GREEN_DIM;
    v.selection.stroke = Stroke::new(1.0_f32, GREEN);
    v.window_rounding = Rounding::same(RADIUS);
    v.window_shadow = eframe::epaint::Shadow::NONE;
    ctx.set_visuals(v);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = Vec2::new(8.0, 6.0);
    style.spacing.button_padding = Vec2::new(12.0, 6.0);
    style.spacing.window_margin = egui::Margin::same(0.0);
    style.spacing.scroll.bar_width = 5.0;
    ctx.set_style(style);
    ctx.set_pixels_per_point(1.1);
}

fn mono(text: impl Into<String>, size: f32, color: Color32) -> RichText {
    RichText::new(text).monospace().size(size).color(color)
}

// ── Widgets ───────────────────────────────────────────────────────────────────

/// Main call to action: filled green when enabled.
fn primary_button(ui: &mut egui::Ui, enabled: bool, label: &str) -> bool {
    ui.add_enabled(
        enabled,
        egui::Button::new(mono(label, 12.0, if enabled { GREEN } else { TEXT_FAINT }))
            .fill(if enabled { GREEN_DIM } else { BG_CARD })
            .stroke(Stroke::new(1.0_f32, if enabled { GREEN_MID } else { BORDER }))
            .min_size(Vec2::new(150.0, 32.0)),
    )
    .clicked()
}

fn danger_button(ui: &mut egui::Ui, label: &str) -> bool {
    ui.add(
        egui::Button::new(mono(label, 12.0, RED))
            .fill(RED_DIM)
            .stroke(Stroke::new(1.0_f32, RED))
            .min_size(Vec2::new(150.0, 32.0)),
    )
    .clicked()
}

/// Secondary action: outline only.
fn ghost_button(ui: &mut egui::Ui, enabled: bool, label: &str) -> bool {
    ui.add_enabled(
        enabled,
        egui::Button::new(mono(label, 10.5, if enabled { GREEN } else { TEXT_FAINT }))
            .fill(Color32::TRANSPARENT)
            .stroke(Stroke::new(1.0_f32, if enabled { GREEN_MID } else { BORDER })),
    )
    .clicked()
}

/// Small square button for a single glyph, with a hover hint.
fn icon_button(ui: &mut egui::Ui, enabled: bool, glyph: &str, hint: &str) -> bool {
    ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(glyph).size(15.0).color(if enabled { GREEN } else { TEXT_FAINT }))
            .fill(BG_CARD)
            .stroke(Stroke::new(1.0_f32, BORDER))
            .min_size(Vec2::new(32.0, 32.0)),
    )
    .on_hover_text(hint)
    .clicked()
}

/// Small on/off chip: filled green when `on`.
fn toggle_chip(ui: &mut egui::Ui, enabled: bool, label: &str, on: bool, min_width: f32) -> egui::Response {
    let lit = enabled && on;
    ui.add_enabled(
        enabled,
        egui::Button::new(mono(label, 9.5, if lit { GREEN } else { TEXT_FAINT }))
            .fill(if lit { GREEN_DIM } else { BG_CARD })
            .stroke(Stroke::new(1.0_f32, if lit { GREEN_MID } else { BORDER }))
            .min_size(Vec2::new(min_width, 24.0)),
    )
}

/// Rounded tag, e.g. "ON LCD".
fn badge(ui: &mut egui::Ui, text: &str, color: Color32) {
    egui::Frame::none()
        .stroke(Stroke::new(1.0_f32, color))
        .rounding(Rounding::same(8.0))
        .inner_margin(egui::Margin::symmetric(7.0, 2.0))
        .show(ui, |ui| ui.label(mono(text, 9.5, color)));
}

fn status_pill(ui: &mut egui::Ui, text: &str, color: Color32) {
    egui::Frame::none()
        .fill(Color32::from_black_alpha(120))
        .stroke(Stroke::new(1.0_f32, color.gamma_multiply(0.5)))
        .rounding(Rounding::same(12.0))
        .inner_margin(egui::Margin::symmetric(10.0, 4.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let (r, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                ui.painter().circle_filled(r.center(), 3.5, color);
                ui.label(mono(text, 10.5, color));
            });
        });
}

fn card_frame() -> egui::Frame {
    egui::Frame::none()
        .fill(BG_PANEL)
        .stroke(Stroke::new(1.0_f32, BORDER))
        .rounding(Rounding::same(RADIUS))
        .inner_margin(egui::Margin::same(16.0))
}

fn draw_circle_image(painter: &egui::Painter, center: Pos2, radius: f32, tex_id: egui::TextureId) {
    let n = 64u32;
    let mut mesh = egui::Mesh::with_texture(tex_id);
    let push = |mesh: &mut egui::Mesh, pos: Pos2, uv: Pos2| {
        mesh.vertices.push(eframe::epaint::Vertex { pos, uv, color: Color32::WHITE });
    };
    push(&mut mesh, center, egui::pos2(0.5, 0.5));
    for i in 0..=n {
        let angle = i as f32 / n as f32 * std::f32::consts::TAU;
        let (sin, cos) = angle.sin_cos();
        push(&mut mesh, center + Vec2::new(cos, sin) * radius, egui::pos2(0.5 + cos * 0.5, 0.5 + sin * 0.5));
        if i > 0 {
            mesh.indices.extend([0, i, i + 1]);
        }
    }
    painter.add(egui::Shape::mesh(mesh));
}

/// Round LCD-style preview; `active` lights the ring.
fn preview_circle(ui: &mut egui::Ui, tex: Option<&TextureHandle>, active: bool) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(PREVIEW_SIZE), Sense::hover());
    let center = rect.center();
    let r = PREVIEW_SIZE / 2.0;
    let p = ui.painter();
    p.circle_filled(center, r, Color32::from_rgb(4, 4, 4));
    match tex {
        Some(tex) => draw_circle_image(p, center, r - 3.0, tex.id()),
        None => {
            p.text(center, egui::Align2::CENTER_CENTER, "NO SIGNAL", FontId::monospace(10.0), TEXT_FAINT);
        }
    }
    p.circle_stroke(center, r - 1.0, Stroke::new(2.0_f32, if active { GREEN } else { BORDER }));
}

/// One gallery tile. Returns (clicked, delete clicked).
fn gallery_card(
    ui: &mut egui::Ui,
    id: &str,
    name: &str,
    tex: Option<egui::TextureId>,
    selected: bool,
    marker: Option<Color32>,
) -> (bool, bool) {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(CARD_SIZE, CARD_SIZE + CARD_LABEL_H), Sense::click());
    let del_rect = Rect::from_center_size(rect.right_top() + Vec2::new(-12.0, 12.0), Vec2::splat(18.0));
    let del_resp = ui.interact(del_rect, egui::Id::new(("del", id)), Sense::click());
    let hovered = resp.hovered() || del_resp.hovered();
    let p = ui.painter();

    let fill = if selected {
        BG_SEL
    } else if hovered {
        BG_HOVER
    } else {
        BG_CARD
    };
    let stroke = if selected { Stroke::new(1.5_f32, BORDER_SEL) } else { Stroke::new(1.0_f32, BORDER) };
    p.rect_filled(rect, RADIUS, fill);

    let img = Rect::from_min_size(rect.min, Vec2::splat(CARD_SIZE)).shrink(5.0);
    match tex {
        Some(tid) => {
            p.image(tid, img, Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
        }
        None => {
            p.rect_filled(img, 3.0, Color32::from_rgb(6, 6, 6));
        }
    }
    p.rect_stroke(rect, RADIUS, stroke);

    let label: String = if name.chars().count() > 13 {
        format!("{}…", name.chars().take(12).collect::<String>())
    } else {
        name.to_string()
    };
    let label_pos = egui::pos2(rect.center().x, rect.max.y - CARD_LABEL_H / 2.0 - 1.0);
    p.text(
        label_pos,
        egui::Align2::CENTER_CENTER,
        label,
        FontId::monospace(9.0),
        if selected { GREEN } else { TEXT_DIM },
    );

    if let Some(color) = marker {
        let c = img.left_top() + Vec2::new(8.0, 8.0);
        p.circle_filled(c, 5.0, Color32::from_black_alpha(200));
        p.circle_filled(c, 3.5, color);
    }
    if hovered {
        let bg = if del_resp.hovered() { RED } else { Color32::from_rgba_premultiplied(40, 6, 6, 230) };
        p.circle_filled(del_rect.center(), 9.0, bg);
        p.text(
            del_rect.center(),
            egui::Align2::CENTER_CENTER,
            "×",
            FontId::proportional(13.0),
            if del_resp.hovered() { Color32::WHITE } else { RED },
        );
    }

    let del_clicked = del_resp.clicked();
    (resp.clicked() && !del_clicked, del_clicked)
}

// ── State ─────────────────────────────────────────────────────────────────────

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Animation,
    Standby,
    Boot,
}

impl Tab {
    const ALL: [Tab; 3] = [Tab::Animation, Tab::Standby, Tab::Boot];

    fn title(self) -> &'static str {
        match self {
            Tab::Animation => "ANIMATION",
            Tab::Standby => "STANDBY",
            Tab::Boot => "BOOT",
        }
    }

    fn subtitle(self) -> &'static str {
        match self {
            Tab::Animation => "live stream",
            Tab::Standby => "idle image",
            Tab::Boot => "power-on",
        }
    }
}

/// Which flash slot an upload targets, and the library item written.
#[derive(Clone)]
enum UploadTarget {
    Standby(String),
    Boot(String),
}

/// Result of work done off the UI thread. `Ok(None)` means the user cancelled.
enum BgDone {
    Uploaded(Result<(), String>, &'static str, UploadTarget),
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

/// An action awaiting confirmation in a dialog.
enum Confirm {
    Upload { cmd: Cmd, what: &'static str, done_msg: &'static str, target: UploadTarget },
    Delete { tab: Tab, id: String, name: String },
}

pub struct App {
    data_dir: PathBuf,
    videos: Vec<Video>,
    standby: Vec<StandbyImage>,
    boot_anims: Vec<BootAnim>,
    on_device: OnDevice,
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
    confirm: Option<Confirm>,
    instance: GuiInstance,
    /// Overlay layer for the animation preview, keyed by its text.
    preview_layer: (String, Option<Layer>),
    /// Overlay settings being edited but not yet confirmed by the daemon
    /// (e.g. while a slider is dragged), and when they last changed.
    overlay_edit: Option<(OverlaySettings, Instant)>,
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

// ── App: state & actions ──────────────────────────────────────────────────────

impl App {
    fn new(cc: &eframe::CreationContext, instance: GuiInstance) -> Self {
        setup_theme(&cc.egui_ctx);
        let _ = instance.ctx.set(cc.egui_ctx.clone());
        let data = shared::data_dir();
        let (videos, e1) = shared::load_videos(&data);
        let (standby, e2) = shared::load_standby(&data);
        let (boot_anims, e3) = shared::load_boot(&data);
        let warnings: Vec<String> = [e1, e2, e3].into_iter().flatten().collect();

        let mut app = App {
            on_device: shared::load_on_device(&data),
            selected_video: videos.first().map(|v| v.id.clone()),
            selected_standby: standby.first().map(|i| i.id.clone()),
            selected_boot: boot_anims.first().map(|a| a.id.clone()),
            data_dir: data,
            videos,
            standby,
            boot_anims,
            tab: Tab::Animation,
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
            preview_layer: (String::new(), None),
            overlay_edit: None,
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
            // Own process group, so Ctrl-C in the GUI's terminal does not kill it.
            match Command::new(shared::app_exe()).arg("--daemon").stdin(Stdio::null()).process_group(0).spawn() {
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

    fn get_thumb(&mut self, ctx: &egui::Context, id: &str, b64: &str) -> Option<egui::TextureId> {
        if !self.thumb_cache.contains_key(id) {
            if let Some((pixels, size)) = decode_thumbnail(b64) {
                let ci = egui::ColorImage::from_rgba_unmultiplied(size, &pixels);
                self.thumb_cache.insert(id.to_string(), ctx.load_texture(id, ci, TextureOptions::LINEAR));
            }
        }
        self.thumb_cache.get(id).map(|t| t.id())
    }

    /// The stats overlay as the daemon would draw it right now (None when off).
    fn current_overlay(&mut self) -> Option<&Layer> {
        let settings = self.overlay_settings();
        if !settings.enabled {
            return None;
        }
        let rows = overlay::rows(&self.daemon_status.metrics, &settings);
        let key = overlay::key(&rows, &settings);
        if key != self.preview_layer.0 {
            self.preview_layer = (key, Layer::render(&rows, &settings));
        }
        self.preview_layer.1.as_ref()
    }

    /// Overlay settings to show: a pending edit if any, else what the daemon reports.
    fn overlay_settings(&self) -> OverlaySettings {
        self.overlay_edit.map_or(self.daemon_status.overlay, |(s, _)| s)
    }

    fn set_preview_from_bytes(&mut self, ctx: &egui::Context, name: &str, bytes: &[u8]) {
        if let Ok(img) = image::load_from_memory(bytes) {
            let mut rgba = img.to_rgba8();
            // Show the animation exactly as the LCD will: with the stats overlay.
            if self.tab == Tab::Animation && rgba.width() == shared::LCD_SIZE && rgba.height() == shared::LCD_SIZE {
                if let Some(layer) = self.current_overlay() {
                    layer.apply(&mut rgba, 4);
                }
            }
            let ci = egui::ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], &rgba);
            self.preview_texture = Some(ctx.load_texture(name, ci, TextureOptions::LINEAR));
        }
    }

    fn tick_anim_preview(&mut self, ctx: &egui::Context) {
        let Some(id) = self.selected_video.clone() else { return };
        let Some(video) = self.videos.iter().find(|v| v.id == id) else { return };
        if video.frame_count == 0 {
            return;
        }
        let frame_count = video.frame_count;
        let frame_dur = Duration::from_secs_f32(1.0 / video.fps.max(1.0));
        if self.preview_texture.is_none() || self.last_frame_time.elapsed() >= frame_dur {
            self.preview_frame_idx = (self.preview_frame_idx + 1) % frame_count;
            self.last_frame_time = Instant::now();
            let path = frame_file(&frames_dir(&self.data_dir, &id), self.preview_frame_idx + 1);
            if let Ok(bytes) = std::fs::read(&path) {
                self.set_preview_from_bytes(ctx, "preview", &bytes);
            }
        }
        ctx.request_repaint_after(frame_dur);
    }

    fn load_static_preview(&mut self, ctx: &egui::Context) {
        if self.preview_texture.is_some() {
            return;
        }
        match self.tab {
            Tab::Standby => {
                let Some(id) = &self.selected_standby else { return };
                let Some(path) = self.standby.iter().find(|i| &i.id == id).map(|i| i.path.clone()) else { return };
                if let Ok(bytes) = std::fs::read(path) {
                    self.set_preview_from_bytes(ctx, "preview_sb", &bytes);
                }
            }
            Tab::Boot => {
                let Some(id) = &self.selected_boot else { return };
                let Some(thumb) = self.boot_anims.iter().find(|a| &a.id == id).map(|a| a.thumbnail.clone()) else {
                    return;
                };
                if let Some((pixels, size)) = decode_thumbnail(&thumb) {
                    let ci = egui::ColorImage::from_rgba_unmultiplied(size, &pixels);
                    self.preview_texture = Some(ctx.load_texture("preview_boot", ci, TextureOptions::LINEAR));
                }
            }
            Tab::Animation => {}
        }
    }

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
            // Drop a pending overlay edit once the daemon has it (or it went stale).
            if let Some((edit, at)) = self.overlay_edit {
                if edit == s.overlay || at.elapsed() > Duration::from_secs(3) {
                    self.overlay_edit = None;
                }
            }
            self.daemon_status = s;
            self.daemon_connected = true;
        } else {
            self.daemon_status = DaemonStatus::default();
            self.daemon_connected = false;
        }
    }

    fn busy(&self) -> bool {
        self.bg_task.is_some() || self.daemon_status.flash_busy
    }

    fn can_write(&self) -> bool {
        !self.busy() && self.daemon_connected && self.daemon_status.device_found
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
            BgDone::Uploaded(Ok(()), msg, target) => {
                self.status_msg = msg.into();
                match target {
                    UploadTarget::Standby(id) => self.on_device.standby = Some(id),
                    UploadTarget::Boot(id) => self.on_device.boot = Some(id),
                }
                if let Err(e) = shared::save_on_device(&self.data_dir, &self.on_device) {
                    self.status_msg = format!("{msg} (could not record it: {e})");
                }
            }
            BgDone::Uploaded(Err(e), _, _) => self.status_msg = format!("Error: {e}"),

            BgDone::VideoImported(Ok(Some(v))) => {
                self.status_msg = format!("Imported {} frames @ {:.0} fps", v.frame_count, v.fps);
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
                self.status_msg = format!("Boot animation: {} frames @ {:.1} fps", anim.frame_count, anim.fps);
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
                // The rebuilt container is no longer what is on the device.
                if self.on_device.boot.as_deref() == Some(&anim.id) {
                    self.on_device.boot = None;
                    let _ = shared::save_on_device(&self.data_dir, &self.on_device);
                }
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

    fn add_item(&mut self) {
        let data = self.data_dir.clone();
        match self.tab {
            Tab::Animation => {
                self.spawn_bg("Importing GIF...", move || BgDone::VideoImported(import_video_task(&data)));
            }
            Tab::Standby => {
                self.spawn_bg("Converting image...", move || BgDone::StandbyImported(import_standby_task(&data)));
            }
            Tab::Boot => {
                self.spawn_bg("Building boot animation...", move || BgDone::BootImported(import_boot_task(&data)));
            }
        }
    }

    fn rotate_video(&mut self, id: &str, delta: i32) {
        let Some(v) = self.videos.iter_mut().find(|v| v.id == id) else { return };
        v.rotation = (v.rotation + delta).rem_euclid(360);
        let new_rot = v.rotation;
        // Invalidate cached rotated frames so the next play regenerates them
        if shared::is_valid_id(id) {
            let _ = std::fs::remove_dir_all(rotated_frames_dir(&self.data_dir, id));
        }
        self.save_videos();
        self.status_msg = format!("Rotation {new_rot}° (applied when sent to the LCD)");
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
            rotation: v.rotation,
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

    fn start_upload(&mut self, cmd: Cmd, what: &'static str, done_msg: &'static str, target: UploadTarget) {
        let label = format!("Writing {what} to device flash. Do not unplug or power off...");
        self.status_msg.clear();
        self.spawn_bg(label, move || {
            let result = match daemon_cmd(&cmd) {
                Ok(Resp::Ok) => Ok(()),
                Ok(Resp::Err { msg }) => Err(msg),
                Ok(Resp::Status(_)) => Err("unexpected reply from daemon".into()),
                Err(e) => Err(e),
            };
            BgDone::Uploaded(result, done_msg, target)
        });
    }

    fn delete_item(&mut self, tab: Tab, id: &str) {
        match tab {
            Tab::Animation => {
                if self.daemon_status.playing_id.as_deref() == Some(id) {
                    self.send(Cmd::Stop);
                }
                if shared::is_valid_id(id) {
                    let _ = std::fs::remove_dir_all(frames_dir(&self.data_dir, id));
                    let _ = std::fs::remove_dir_all(rotated_frames_dir(&self.data_dir, id));
                }
                self.videos.retain(|v| v.id != id);
                self.save_videos();
                if self.selected_video.as_deref() == Some(id) {
                    self.selected_video = self.videos.first().map(|v| v.id.clone());
                }
            }
            Tab::Standby => {
                if let Some(img) = self.standby.iter().find(|i| i.id == id) {
                    let _ = std::fs::remove_file(&img.path);
                }
                self.standby.retain(|i| i.id != id);
                self.save_standby();
                if self.selected_standby.as_deref() == Some(id) {
                    self.selected_standby = self.standby.first().map(|i| i.id.clone());
                }
            }
            Tab::Boot => {
                if let Some(anim) = self.boot_anims.iter().find(|a| a.id == id) {
                    let _ = std::fs::remove_file(&anim.path);
                }
                self.boot_anims.retain(|a| a.id != id);
                self.save_boot();
                if self.selected_boot.as_deref() == Some(id) {
                    self.selected_boot = self.boot_anims.first().map(|a| a.id.clone());
                }
            }
        }
        self.thumb_cache.remove(id);
        self.preview_texture = None;
    }

    fn select(&mut self, id: String) {
        let slot = match self.tab {
            Tab::Animation => &mut self.selected_video,
            Tab::Standby => &mut self.selected_standby,
            Tab::Boot => &mut self.selected_boot,
        };
        if slot.as_deref() != Some(&id) {
            *slot = Some(id);
            self.preview_frame_idx = 0;
            self.preview_texture = None;
        }
    }

    fn switch_tab(&mut self, tab: Tab) {
        if self.tab != tab {
            self.tab = tab;
            self.preview_texture = None;
        }
    }
}

// ── App: UI ───────────────────────────────────────────────────────────────────

/// (id, name, base64 thumbnail)
type GalleryItem = (String, String, String);

/// What the hero card shows for the selected item.
struct Hero {
    name: String,
    meta: String,
    badges: Vec<(&'static str, Color32)>,
    rotation: Option<i32>,
    live: bool,
}

impl App {
    fn device_state(&self) -> (&'static str, Color32) {
        if !self.daemon_connected {
            ("NO DAEMON", RED)
        } else if !self.daemon_status.device_found {
            ("NO DEVICE", ORANGE)
        } else if self.daemon_status.flash_busy {
            ("WRITING FLASH", AMBER)
        } else if self.daemon_status.streaming {
            ("STREAMING", GREEN)
        } else {
            ("IDLE", AMBER)
        }
    }

    fn ui_top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("topbar")
            .frame(
                egui::Frame::none()
                    .fill(BG_BAR)
                    .stroke(Stroke::new(1.0_f32, BORDER))
                    .inner_margin(egui::Margin::symmetric(16.0, 9.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(mono("TH LINUX LCD", 14.0, GREEN).strong());
                    ui.label(mono("TH420V2 Ultra", 10.0, TEXT_FAINT));

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // Screen on/off as one joined control
                        let enabled = self.daemon_connected && self.daemon_status.device_found;
                        ui.spacing_mut().item_spacing.x = 0.0;
                        let seg = |label: &str, color: Color32, rounding: Rounding| {
                            egui::Button::new(mono(label, 10.5, if enabled { color } else { TEXT_FAINT }))
                                .fill(BG_CARD)
                                .stroke(Stroke::new(1.0_f32, BORDER))
                                .rounding(rounding)
                                .min_size(Vec2::new(40.0, 26.0))
                        };
                        let right = Rounding { ne: 4.0, se: 4.0, ..Default::default() };
                        let left = Rounding { nw: 4.0, sw: 4.0, ..Default::default() };
                        if ui.add_enabled(enabled, seg("OFF", RED, right)).on_hover_text("Turn the LCD off").clicked() {
                            self.send(Cmd::ScreenOff);
                        }
                        if ui.add_enabled(enabled, seg("ON", GREEN, left)).on_hover_text("Turn the LCD on").clicked() {
                            self.send(Cmd::ScreenOn);
                        }
                        ui.add_space(6.0);
                        ui.label(mono("SCREEN", 9.5, TEXT_FAINT));
                        ui.add_space(14.0);
                        ui.spacing_mut().item_spacing.x = 8.0;

                        if !self.daemon_connected && ghost_button(ui, true, "START DAEMON") {
                            self.ensure_daemon();
                        }
                        let (text, color) = self.device_state();
                        status_pill(ui, text, color);
                    });
                });
            });
    }

    fn ui_status_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("statusbar")
            .frame(
                egui::Frame::none()
                    .fill(BG_BAR)
                    .stroke(Stroke::new(1.0_f32, BORDER))
                    .inner_margin(egui::Margin::symmetric(16.0, 6.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if let Some(task) = &self.bg_task {
                        ui.add(egui::Spinner::new().size(11.0).color(AMBER));
                        ui.label(mono(&task.label, 10.0, AMBER));
                    } else if self.daemon_status.flash_busy {
                        ui.add(egui::Spinner::new().size(11.0).color(AMBER));
                        ui.label(mono("Device is writing flash. Do not unplug or power off...", 10.0, AMBER));
                    } else if !self.status_msg.is_empty() {
                        let color =
                            if self.status_msg.starts_with("Error") || self.status_msg.starts_with("Import failed") {
                                RED
                            } else {
                                TEXT_DIM
                            };
                        ui.label(mono(&self.status_msg, 10.0, color));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.daemon_status.streaming && self.daemon_status.fps > 0.0 {
                            ui.label(mono(format!("{:.0} fps", self.daemon_status.fps), 10.0, TEXT_FAINT));
                        }
                        let summary = self.daemon_status.metrics.summary();
                        if self.daemon_connected && !summary.is_empty() {
                            ui.label(mono(summary, 10.0, TEXT_DIM));
                        }
                    });
                });
            });
    }

    fn ui_sidebar(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("sidebar")
            .resizable(false)
            .exact_width(132.0)
            .frame(egui::Frame::none().fill(BG_PANEL).stroke(Stroke::new(1.0_f32, BORDER)).inner_margin(egui::Margin {
                left: 0.0,
                right: 8.0,
                top: 14.0,
                bottom: 12.0,
            }))
            .show(ctx, |ui| {
                for (i, tab) in Tab::ALL.into_iter().enumerate() {
                    let active = self.tab == tab;
                    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 42.0), Sense::click());
                    let p = ui.painter();
                    let row = rect.with_min_x(rect.min.x + 8.0);
                    if active {
                        p.rect_filled(row, 4.0, BG_SEL);
                        p.rect_filled(Rect::from_min_size(rect.min, Vec2::new(3.0, rect.height())), 0.0, GREEN);
                    } else if resp.hovered() {
                        p.rect_filled(row, 4.0, BG_HOVER);
                    }
                    let x = row.min.x + 12.0;
                    p.text(
                        egui::pos2(x, row.center().y - 6.0),
                        egui::Align2::LEFT_CENTER,
                        tab.title(),
                        FontId::monospace(11.0),
                        if active { GREEN } else { TEXT_DIM },
                    );
                    p.text(
                        egui::pos2(x, row.center().y + 8.0),
                        egui::Align2::LEFT_CENTER,
                        tab.subtitle(),
                        FontId::monospace(8.5),
                        if active { TEXT_DIM } else { TEXT_FAINT },
                    );
                    if resp.on_hover_text(format!("Ctrl+{}", i + 1)).clicked() {
                        self.switch_tab(tab);
                    }
                    ui.add_space(4.0);
                }
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                    ui.horizontal(|ui| {
                        ui.add_space(16.0);
                        ui.label(mono(concat!("v", env!("CARGO_PKG_VERSION")), 9.0, TEXT_FAINT));
                    });
                });
            });
    }

    fn hero(&self) -> Option<Hero> {
        match self.tab {
            Tab::Animation => {
                let v = self.videos.iter().find(|v| Some(&v.id) == self.selected_video.as_ref())?;
                let live = self.daemon_status.streaming && self.daemon_status.playing_id.as_deref() == Some(&v.id);
                Some(Hero {
                    name: v.name.clone(),
                    meta: format!("{} frames · {:.0} fps · {}°", v.frame_count, v.fps, v.rotation),
                    badges: if live { vec![("STREAMING", GREEN)] } else { vec![] },
                    rotation: Some(v.rotation),
                    live,
                })
            }
            Tab::Standby => {
                let img = self.standby.iter().find(|i| Some(&i.id) == self.selected_standby.as_ref())?;
                let size = std::fs::metadata(&img.path).map(|m| m.len()).unwrap_or(0);
                let on = self.on_device.standby.as_deref() == Some(&img.id);
                Some(Hero {
                    name: img.name.clone(),
                    meta: format!("480×480 JPEG · {} KB", size.div_ceil(1024)),
                    badges: if on { vec![("ON LCD", GREEN)] } else { vec![] },
                    rotation: None,
                    live: on,
                })
            }
            Tab::Boot => {
                let a = self.boot_anims.iter().find(|a| Some(&a.id) == self.selected_boot.as_ref())?;
                let on = self.on_device.boot.as_deref() == Some(&a.id);
                Some(Hero {
                    name: a.name.clone(),
                    meta: format!("{} frames · {:.1} fps · {}°", a.frame_count, a.fps, a.rotation),
                    badges: if on { vec![("ON LCD", GREEN)] } else { vec![] },
                    rotation: Some(a.rotation),
                    live: on,
                })
            }
        }
    }

    fn ui_hero(&mut self, ui: &mut egui::Ui) {
        let hero = self.hero();
        card_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                preview_circle(ui, self.preview_texture.as_ref(), hero.as_ref().is_some_and(|h| h.live));
                ui.add_space(20.0);
                ui.vertical(|ui| {
                    ui.add_space(14.0);
                    let Some(hero) = hero else {
                        ui.label(mono("Nothing selected", 15.0, TEXT_DIM));
                        ui.add_space(4.0);
                        ui.label(mono("Pick one below, or add a new one.", 10.5, TEXT_FAINT));
                        return;
                    };
                    ui.horizontal(|ui| {
                        ui.add(egui::Label::new(mono(&hero.name, 15.0, GREEN).strong()).truncate());
                        for (text, color) in &hero.badges {
                            badge(ui, text, *color);
                        }
                    });
                    ui.add_space(2.0);
                    ui.label(mono(&hero.meta, 10.5, TEXT_DIM));
                    ui.add_space(16.0);

                    ui.horizontal(|ui| {
                        if let Some(rot) = hero.rotation {
                            self.ui_rotate(ui, rot);
                            ui.add_space(10.0);
                        }
                        self.ui_primary_action(ui, hero.live);
                    });
                    ui.add_space(8.0);
                    let (hint, color) = match self.tab {
                        Tab::Animation => ("Streams over USB. Nothing is written to the device.", TEXT_FAINT),
                        Tab::Standby => ("⚠ Writes to device flash.", AMBER),
                        Tab::Boot => ("⚠ Writes to device flash. Power-cycle the cooler to see it.", AMBER),
                    };
                    ui.label(mono(hint, 9.5, color));
                });
            });
        });
    }

    fn ui_rotate(&mut self, ui: &mut egui::Ui, rotation: i32) {
        let enabled = self.bg_task.is_none();
        let hint = match self.tab {
            Tab::Boot => "re-encodes the animation",
            _ => "applied when sent to the LCD",
        };
        let mut delta = 0;
        if icon_button(ui, enabled, "↺", &format!("Rotate left ({hint})")) {
            delta = -90;
        }
        ui.label(mono(format!("{rotation:>3}°"), 10.5, TEXT_DIM));
        if icon_button(ui, enabled, "↻", &format!("Rotate right ({hint})")) {
            delta = 90;
        }
        if delta == 0 {
            return;
        }
        match self.tab {
            Tab::Animation => {
                if let Some(id) = self.selected_video.clone() {
                    self.rotate_video(&id, delta);
                }
            }
            Tab::Boot => {
                if let Some(id) = self.selected_boot.clone() {
                    self.rotate_boot(&id, delta);
                }
            }
            Tab::Standby => {}
        }
    }

    fn ui_primary_action(&mut self, ui: &mut egui::Ui, live: bool) {
        match self.tab {
            Tab::Animation => {
                if live {
                    if danger_button(ui, "■  STOP") && self.send(Cmd::Stop) {
                        self.status_msg = "Stopped".into();
                    }
                } else if primary_button(ui, self.bg_task.is_none() && self.daemon_connected, "▶  SEND TO LCD") {
                    self.play_selected();
                }
            }
            Tab::Standby => {
                if primary_button(ui, self.can_write(), "WRITE TO LCD") {
                    if let Some(img) = self.standby.iter().find(|i| Some(&i.id) == self.selected_standby.as_ref()) {
                        self.confirm = Some(Confirm::Upload {
                            cmd: Cmd::UploadStandby { path: img.path.clone() },
                            what: "standby image",
                            done_msg: "Standby image written.",
                            target: UploadTarget::Standby(img.id.clone()),
                        });
                    }
                }
            }
            Tab::Boot => {
                if primary_button(ui, self.can_write(), "WRITE TO LCD") {
                    if let Some(a) = self.boot_anims.iter().find(|a| Some(&a.id) == self.selected_boot.as_ref()) {
                        self.confirm = Some(Confirm::Upload {
                            cmd: Cmd::UploadBoot { path: a.path.clone() },
                            what: "boot animation",
                            done_msg: "Boot animation written. Power-cycle the cooler to see it.",
                            target: UploadTarget::Boot(a.id.clone()),
                        });
                    }
                }
            }
        }
    }

    /// "Stats on LCD" card: switch, style, size, position, and one chip per
    /// stat laid out like the two rows on the display.
    fn ui_overlay_settings(&mut self, ui: &mut egui::Ui) {
        let current = self.overlay_settings();
        let mut next = current;
        let mut commit = false; // send to the daemon now (vs. preview only while dragging)
        let enabled = self.daemon_connected;
        let on = enabled && next.enabled;
        card_frame().inner_margin(egui::Margin::symmetric(16.0, 12.0)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                ui.label(mono("STATS ON LCD", 10.5, TEXT_DIM));
                if toggle_chip(ui, enabled, if next.enabled { "ON" } else { "OFF" }, next.enabled, 48.0).clicked() {
                    next.enabled = !next.enabled;
                    commit = true;
                }
                ui.add_space(10.0);
                ui.label(mono("STYLE", 9.5, TEXT_FAINT));
                for style in OverlayStyle::ALL {
                    if toggle_chip(ui, on, style.name(), next.style == style, 70.0).clicked() {
                        next.style = style;
                        commit = true;
                    }
                }
            });
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().slider_width = 110.0;
                ui.label(mono("SIZE", 9.5, TEXT_FAINT));
                let size = ui.add_enabled(on, egui::Slider::new(&mut next.size, OVERLAY_SIZE_RANGE).show_value(false));
                ui.label(mono(format!("{:>3.0}%", next.size * 100.0), 9.5, TEXT_DIM));
                ui.add_space(14.0);
                ui.label(mono("POSITION", 9.5, TEXT_FAINT));
                let pos =
                    ui.add_enabled(on, egui::Slider::new(&mut next.vertical, OVERLAY_VERTICAL_RANGE).show_value(false));
                for (name, v) in [("TOP", OVERLAY_TOP), ("BOTTOM", OVERLAY_BOTTOM)] {
                    if toggle_chip(ui, on, name, (next.vertical - v).abs() < 0.005, 56.0).clicked() {
                        next.vertical = v;
                        commit = true;
                    }
                }
                // Sliders: preview while dragging, send when released.
                for r in [&size, &pos] {
                    if r.drag_stopped() || (r.changed() && !r.dragged()) {
                        commit = true;
                    }
                }
            });
            ui.add_space(6.0);
            let m = self.daemon_status.metrics;
            for row in Stat::ROWS {
                ui.horizontal(|ui| {
                    for stat in row {
                        // Same short form as on the LCD; full name on hover.
                        let value = stat.value(&m).unwrap_or_else(|| "--".into());
                        let label = format!("{} {value}", stat.label());
                        let chip = toggle_chip(ui, on, &label, next.shows(stat), 92.0);
                        if chip.on_hover_text(stat.name()).clicked() {
                            next.toggle(stat);
                            commit = true;
                        }
                    }
                });
            }
        });
        if next != current || commit {
            self.overlay_edit = Some((next, Instant::now()));
            self.preview_texture = None;
        }
        if commit {
            self.send(Cmd::SetOverlay { settings: next });
        }
    }

    fn ui_gallery(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let (title, add_label, empty) = match self.tab {
            Tab::Animation => ("ANIMATIONS", "+ ADD GIF", "No animations yet. Add a GIF to stream it to the LCD."),
            Tab::Standby => ("STANDBY IMAGES", "+ ADD IMAGE", "No standby images yet. Add a JPEG, PNG or GIF."),
            Tab::Boot => ("BOOT ANIMATIONS", "+ ADD GIF", "No boot animations yet. Add a GIF to build one."),
        };
        let (items, selected, marked): (Vec<GalleryItem>, Option<String>, Option<String>) = match self.tab {
            Tab::Animation => (
                self.videos.iter().map(|v| (v.id.clone(), v.name.clone(), v.thumbnail.clone())).collect(),
                self.selected_video.clone(),
                self.daemon_status.playing_id.clone().filter(|_| self.daemon_status.streaming),
            ),
            Tab::Standby => (
                self.standby.iter().map(|i| (i.id.clone(), i.name.clone(), i.thumbnail.clone())).collect(),
                self.selected_standby.clone(),
                self.on_device.standby.clone(),
            ),
            Tab::Boot => (
                self.boot_anims.iter().map(|a| (a.id.clone(), a.name.clone(), a.thumbnail.clone())).collect(),
                self.selected_boot.clone(),
                self.on_device.boot.clone(),
            ),
        };

        ui.horizontal(|ui| {
            ui.label(mono(title, 10.5, TEXT_DIM));
            ui.label(mono(items.len().to_string(), 10.5, TEXT_FAINT));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ghost_button(ui, self.bg_task.is_none(), add_label) {
                    self.add_item();
                }
            });
        });
        ui.add_space(8.0);

        if items.is_empty() {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| ui.label(mono(empty, 10.5, TEXT_FAINT)));
            return;
        }

        let mut clicked = None;
        let mut delete = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::splat(10.0);
                for (id, name, b64) in &items {
                    let tex = self.get_thumb(ctx, id, b64);
                    let marker = (marked.as_deref() == Some(id.as_str())).then_some(GREEN);
                    let (c, d) = gallery_card(ui, id, name, tex, selected.as_deref() == Some(id), marker);
                    if c {
                        clicked = Some(id.clone());
                    }
                    if d {
                        delete = Some((id.clone(), name.clone()));
                    }
                }
            });
        });

        if let Some(id) = clicked {
            self.select(id);
        }
        if let Some((id, name)) = delete {
            self.confirm = Some(Confirm::Delete { tab: self.tab, id, name });
        }
    }

    fn ui_confirm(&mut self, ctx: &egui::Context) {
        let Some(confirm) = &self.confirm else { return };
        let (title, body, ok_label, ok_color) = match confirm {
            Confirm::Upload { what, .. } => (
                "Write to device flash?",
                format!(
                    "This writes the {what} to the cooler's flash memory.\n\n\
                     Do not unplug the USB cable or power off the PC until the status bar says it is done \
                     (up to about a minute).\n\n\
                     This is unofficial software. Use at your own risk."
                ),
                "WRITE",
                GREEN,
            ),
            Confirm::Delete { name, .. } => {
                ("Delete from library?", format!("Remove \"{name}\" from your library?"), "DELETE", RED)
            }
        };

        // Dim the app behind the dialog.
        let screen = ctx.screen_rect();
        egui::Area::new(egui::Id::new("confirm_backdrop")).order(egui::Order::Middle).fixed_pos(screen.min).show(
            ctx,
            |ui| {
                ui.painter().rect_filled(screen, 0.0, Color32::from_black_alpha(160));
                ui.allocate_rect(screen, Sense::click());
            },
        );

        let mut decision = None;
        egui::Window::new(title)
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .frame(card_frame().inner_margin(egui::Margin::same(20.0)))
            .show(ctx, |ui| {
                ui.set_max_width(360.0);
                ui.label(mono(title, 13.0, ok_color).strong());
                ui.add_space(10.0);
                ui.label(mono(body, 10.5, GREEN));
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    let fill = if ok_color == RED { RED_DIM } else { GREEN_DIM };
                    if ui
                        .add(
                            egui::Button::new(mono(ok_label, 11.0, ok_color))
                                .fill(fill)
                                .stroke(Stroke::new(1.0_f32, ok_color))
                                .min_size(Vec2::new(90.0, 28.0)),
                        )
                        .clicked()
                    {
                        decision = Some(true);
                    }
                    if ghost_button(ui, true, "CANCEL") {
                        decision = Some(false);
                    }
                });
            });
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            decision = Some(false);
        }

        match decision {
            Some(true) => match self.confirm.take() {
                Some(Confirm::Upload { cmd, what, done_msg, target }) => self.start_upload(cmd, what, done_msg, target),
                Some(Confirm::Delete { tab, id, .. }) => self.delete_item(tab, &id),
                None => {}
            },
            Some(false) => self.confirm = None,
            None => {}
        }
    }
}

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
        ctx.request_repaint_after(if self.bg_task.is_some() { Duration::from_millis(100) } else { STATUS_POLL });

        if self.confirm.is_none() {
            let pressed = ctx.input(|i| {
                [egui::Key::Num1, egui::Key::Num2, egui::Key::Num3]
                    .iter()
                    .position(|&k| i.modifiers.command && i.key_pressed(k))
            });
            if let Some(i) = pressed {
                self.switch_tab(Tab::ALL[i]);
            }
        }

        match self.tab {
            Tab::Animation => self.tick_anim_preview(ctx),
            _ => self.load_static_preview(ctx),
        }

        self.ui_top_bar(ctx);
        self.ui_status_bar(ctx);
        self.ui_sidebar(ctx);
        egui::CentralPanel::default().frame(egui::Frame::none().fill(BG).inner_margin(egui::Margin::same(18.0))).show(
            ctx,
            |ui| {
                self.ui_hero(ui);
                if self.tab == Tab::Animation {
                    ui.add_space(12.0);
                    self.ui_overlay_settings(ui);
                }
                ui.add_space(18.0);
                self.ui_gallery(ui, ctx);
            },
        );
        self.ui_confirm(ctx);
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
            .with_inner_size([720.0, 560.0])
            .with_min_inner_size([560.0, 440.0])
            .with_resizable(true),
        ..Default::default()
    };
    if let Err(e) = eframe::run_native("TH Linux LCD", options, Box::new(|cc| Ok(Box::new(App::new(cc, instance))))) {
        eprintln!("th-linux-lcd: cannot start the GUI: {e}");
        std::process::exit(1);
    }
}
