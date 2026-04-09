use crate::shared::{
    boot_dir, build_boot_container, daemon_cmd, data_dir, decode_thumbnail, frames_dir,
    load_boot, load_standby, load_videos, make_thumbnail, save_boot, save_standby, save_videos,
    socket_path, standby_dir, BootAnim, Cmd, DaemonStatus, Resp, StandbyImage, Video,
};
use eframe::egui;
use egui::{Color32, FontId, Pos2, RichText, Stroke, TextureHandle, TextureOptions, Vec2};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};
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

// ── Theme ─────────────────────────────────────────────────────────────────────

fn setup_theme(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = BG;
    v.extreme_bg_color = Color32::from_rgb(6, 6, 6);
    v.faint_bg_color = BG_CARD;
    v.override_text_color = Some(GREEN);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT_DIM);
    v.widgets.inactive.bg_fill = BG_CARD;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, GREEN_MID);
    v.widgets.hovered.bg_fill = Color32::from_rgb(18, 32, 20);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, GREEN_MID);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, GREEN);
    v.widgets.active.bg_fill = GREEN_DIM;
    v.widgets.active.bg_stroke = Stroke::new(1.0, GREEN);
    v.widgets.active.fg_stroke = Stroke::new(1.0, GREEN);
    v.selection.bg_fill = GREEN_DIM;
    v.selection.stroke = Stroke::new(1.0, GREEN);
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
enum Tab { Animation, Standby, Boot }

struct BgTask {
    rx: Receiver<Result<(), String>>,
    label: String,
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
    last_status_poll: Instant,
    daemon_connected: bool,
}

impl App {
    fn new(cc: &eframe::CreationContext) -> Self {
        setup_theme(&cc.egui_ctx);
        let data = data_dir();
        let videos = load_videos(&data);
        let standby = load_standby(&data);
        let boot_anims = load_boot(&data);
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
            last_status_poll: Instant::now() - Duration::from_secs(10),
            daemon_connected: false,
        };

        app.ensure_daemon();
        app
    }

    fn ensure_daemon(&mut self) {
        // Try connecting to an existing daemon first (socket may exist but
        // daemon_connected is still false on first startup)
        if std::os::unix::net::UnixStream::connect(socket_path()).is_ok() {
            return; // socket is alive, a daemon is already running
        }
        // Socket not reachable — remove stale file if present and spawn
        let _ = std::fs::remove_file(socket_path());
        let exe = std::env::current_exe().unwrap_or_default();
        match std::process::Command::new(exe).arg("--daemon").spawn() {
            Ok(_) => {
                // Wait up to 2s for socket to appear
                for _ in 0..20 {
                    std::thread::sleep(Duration::from_millis(100));
                    if std::os::unix::net::UnixStream::connect(socket_path()).is_ok() { break }
                }
                self.status_msg = "Daemon started".into();
            }
            Err(e) => self.status_msg = format!("Failed to start daemon: {}", e),
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
        if video.frame_count == 0 { return }
        let frame_dur = Duration::from_secs_f32(1.0 / video.fps.max(1.0));
        if self.last_frame_time.elapsed() >= frame_dur {
            self.preview_frame_idx = (self.preview_frame_idx + 1) % video.frame_count;
            self.last_frame_time = Instant::now();
            let path = frames_dir(&self.data_dir, id)
                .join(format!("f{:05}.jpg", self.preview_frame_idx + 1));
            if let Ok(bytes) = std::fs::read(&path) {
                if let Ok(img) = image::load_from_memory(&bytes) {
                    let rgba = img.to_rgba8();
                    let ci = egui::ColorImage::from_rgba_unmultiplied(
                        [rgba.width() as usize, rgba.height() as usize], &rgba);
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
                let ci = egui::ColorImage::from_rgba_unmultiplied(
                    [rgba.width() as usize, rgba.height() as usize], &rgba);
                self.preview_texture = Some(ctx.load_texture("preview_sb", ci, TextureOptions::LINEAR));
            }
        }
    }

    fn load_boot_preview(&mut self, ctx: &egui::Context) {
        let Some(ref id) = self.selected_boot.clone() else { return };
        let Some(anim) = self.boot_anims.iter().find(|a| a.id == *id) else { return };
        if anim.thumbnail.is_empty() { return }
        if let Some((pixels, size)) = decode_thumbnail(&anim.thumbnail) {
            let ci = egui::ColorImage::from_rgba_unmultiplied(size, &pixels);
            self.preview_texture = Some(ctx.load_texture("preview_boot", ci, TextureOptions::LINEAR));
        }
    }

    // ── Daemon ────────────────────────────────────────────────────────────────

    fn poll_status(&mut self) {
        if self.last_status_poll.elapsed() < Duration::from_secs(2) { return }
        self.last_status_poll = Instant::now();
        if let Ok(Resp::Status(s)) = daemon_cmd(&Cmd::Status) {
            self.daemon_status = s;
            self.daemon_connected = true;
        } else {
            self.daemon_status = DaemonStatus::default();
            self.daemon_connected = false;
        }
    }

    fn daemon_up(&self) -> bool { self.daemon_connected }

    fn send(&mut self, cmd: Cmd) {
        match daemon_cmd(&cmd) {
            Ok(Resp::Ok) | Ok(Resp::Status(_)) => {}
            Ok(Resp::Err { msg }) => self.status_msg = format!("Error: {}", msg),
            Err(e) => self.status_msg = format!("Daemon: {}", e),
        }
    }

    fn send_bg(&mut self, cmd: Cmd, lbl: &str) {
        if self.bg_task.is_some() { return }
        let (tx, rx) = channel();
        let lbl = lbl.to_string();
        std::thread::spawn(move || {
            let r = match daemon_cmd(&cmd) {
                Ok(Resp::Ok) => Ok(()),
                Ok(Resp::Err { msg }) => Err(msg),
                Err(e) => Err(e),
                _ => Ok(()),
            };
            let _ = tx.send(r);
        });
        self.bg_task = Some(BgTask { rx, label: lbl });
    }

    // ── Import ────────────────────────────────────────────────────────────────

    fn rotate_video(&mut self, id: &str, delta: i32) {
        let Some(v) = self.videos.iter_mut().find(|v| v.id == id) else { return };
        v.rotation = (v.rotation + delta).rem_euclid(360);
        let new_rot = v.rotation;
        // Invalidate cached rotated frames so next play regenerates them
        let rotated_dir = frames_dir(&self.data_dir, id).with_file_name(format!("{}-rotated", id));
        let _ = std::fs::remove_dir_all(&rotated_dir);
        save_videos(&self.data_dir, &self.videos);
        self.status_msg = format!("Rotation: {}°", new_rot);
    }

    /// Returns the frames dir to use for playback — original if rotation=0,
    /// otherwise generates rotated copies into a sibling "-rotated" dir.
    fn prepare_play_dir(&mut self, id: &str) -> Option<(PathBuf, usize)> {
        let v = self.videos.iter().find(|v| v.id == id)?;
        let rotation = v.rotation;
        let count = v.frame_count;
        let src_dir = frames_dir(&self.data_dir, id);

        if rotation == 0 {
            return Some((src_dir, count));
        }

        let dst_dir = src_dir.with_file_name(format!("{}-rotated", id));
        let src_s = src_dir.display().to_string();
        let dst_s = dst_dir.display().to_string();

        self.status_msg = "Applying rotation...".into();

        let script = format!(r#"
from PIL import Image
import io, os
src={src:?}; dst={dst:?}
os.makedirs(dst, exist_ok=True)
for i in range(1, {count}+1):
    p = os.path.join(src, f'f{{i:05}}.jpg')
    if not os.path.exists(p): continue
    img = Image.open(p).rotate(-{rot}, expand=False)
    q = 25
    while q >= 5:
        b = io.BytesIO()
        img.save(b, 'JPEG', quality=q, subsampling='4:4:4', dpi=(96,96))
        if len(b.getvalue()) <= 20480 or q <= 5:
            open(os.path.join(dst, f'f{{i:05}}.jpg'), 'wb').write(b.getvalue())
            break
        q -= 5
"#, src = src_s, dst = dst_s, count = count, rot = rotation);

        match std::process::Command::new("python3").args(["-c", &script]).output() {
            Ok(o) if o.status.success() => {
                self.status_msg = "Streaming".into();
                Some((dst_dir, count))
            }
            Ok(o) => {
                self.status_msg = format!("Rotate failed: {}", String::from_utf8_lossy(&o.stderr));
                None
            }
            Err(e) => {
                self.status_msg = format!("Error: {}", e);
                None
            }
        }
    }

    fn add_video(&mut self) {
        if self.bg_task.is_some() { return }
        let out = std::process::Command::new("zenity")
            .args(["--file-selection", "--title=Select GIF", "--file-filter=GIF | *.gif", "--modal"])
            .output();
        let Ok(out) = out else { return };
        if !out.status.success() { return }
        let src = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
        if src.extension().map_or(true, |e| e != "gif") { return }

        let name = src.file_stem().unwrap_or_default().to_string_lossy().to_string();
        let id = Uuid::new_v4().to_string();
        let out_dir = frames_dir(&self.data_dir, &id).display().to_string();
        let src_s = src.display().to_string();
        let src_s2 = src_s.clone();
        self.status_msg = "Importing...".into();
        let (tx, rx) = channel::<Result<(usize, f32), String>>();

        std::thread::spawn(move || {
            let script = format!(r#"
from PIL import Image
import io, os, json
src={src:?}; out={out:?}
os.makedirs(out, exist_ok=True)
img=Image.open(src); frames=[]
try:
    while True:
        frames.append(img.copy().convert('RGB')); img.seek(img.tell()+1)
except EOFError: pass
n=min(len(frames),60); step=max(len(frames)//n,1); sel=frames[::step][:n]
total_ms=getattr(img,'info',{{}}).get('duration',50)*len(frames)
fps=min(max(1000.0*len(frames)/max(total_ms,1),5.0),20.0)
for i,frame in enumerate(sel):
    r=frame.resize((480,480),Image.LANCZOS); q=25
    while q>=5:
        b=io.BytesIO(); r.save(b,'JPEG',quality=q,subsampling='4:4:4',dpi=(96,96))
        if len(b.getvalue())<=20480 or q<=5:
            open(os.path.join(out,f'f{{i+1:05}}.jpg'),'wb').write(b.getvalue()); break
        q-=5
print(json.dumps({{"n":len(sel),"fps":fps}}))
"#, src = src_s, out = out_dir);
            match std::process::Command::new("python3").args(["-c",&script]).output() {
                Ok(o) if o.status.success() => {
                    let v: serde_json::Value = serde_json::from_str(String::from_utf8_lossy(&o.stdout).trim()).unwrap_or_default();
                    let _ = tx.send(Ok((v["n"].as_u64().unwrap_or(1) as usize, v["fps"].as_f64().unwrap_or(15.0) as f32)));
                }
                Ok(o) => { let _ = tx.send(Err(String::from_utf8_lossy(&o.stderr).to_string())); }
                Err(e) => { let _ = tx.send(Err(e.to_string())); }
            }
        });

        match rx.recv_timeout(Duration::from_secs(60)) {
            Ok(Ok((n, fps))) => {
                let thumb = make_thumbnail(&frames_dir(&self.data_dir, &id).join("f00001.jpg"));
                let v = Video { id: id.clone(), name, source_path: src_s2, frame_count: n, fps, rotation: 0, thumbnail: thumb };
                self.selected_video = Some(id.clone());
                self.videos.push(v);
                save_videos(&self.data_dir, &self.videos);
                self.preview_frame_idx = 0;
                self.preview_texture = None;
                self.status_msg = format!("Imported {} frames @ {:.0}fps", n, fps);
            }
            Ok(Err(e)) => self.status_msg = format!("Import failed: {}", e),
            Err(_) => self.status_msg = "Timed out".into(),
        }
    }

    fn add_standby(&mut self) {
        let out = std::process::Command::new("zenity")
            .args(["--file-selection", "--title=Select Image", "--file-filter=Images | *.jpg *.jpeg *.png", "--modal"])
            .output();
        let Ok(out) = out else { return };
        if !out.status.success() { return }
        let src = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
        let name = src.file_stem().unwrap_or_default().to_string_lossy().to_string();
        let id = Uuid::new_v4().to_string();
        let dest = standby_dir(&self.data_dir).join(format!("{}.jpg", id));
        let src_s = src.display().to_string();
        let dest_s = dest.display().to_string();
        self.status_msg = "Converting...".into();
        let script = format!(r#"
from PIL import Image; import io
img=Image.open({src:?}).convert('RGB').resize((480,480),Image.LANCZOS); q=85
while q>=20:
    b=io.BytesIO(); img.save(b,'JPEG',quality=q,subsampling='4:4:4',dpi=(96,96))
    if len(b.getvalue())<=5*1024*1024 or q<=20:
        open({dest:?},'wb').write(b.getvalue()); break
    q-=5
"#, src = src_s, dest = dest_s);
        match std::process::Command::new("python3").args(["-c",&script]).output() {
            Ok(o) if o.status.success() => {
                let img = StandbyImage { id: id.clone(), name, path: dest_s, thumbnail: make_thumbnail(&dest) };
                self.standby.push(img);
                save_standby(&self.data_dir, &self.standby);
                self.selected_standby = Some(id);
                self.preview_texture = None;
                self.status_msg = "Image added".into();
            }
            Ok(o) => self.status_msg = format!("Failed: {}", String::from_utf8_lossy(&o.stderr)),
            Err(e) => self.status_msg = format!("Error: {}", e),
        }
    }

    fn add_boot(&mut self) {
        if self.bg_task.is_some() { return }
        let out = std::process::Command::new("zenity")
            .args(["--file-selection", "--title=Select GIF for Boot Animation",
                   "--file-filter=GIF | *.gif", "--modal"])
            .output();
        let Ok(out) = out else { return };
        if !out.status.success() { return }
        let src = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
        if src.extension().map_or(true, |e| e != "gif") { return }

        let name = src.file_stem().unwrap_or_default().to_string_lossy().to_string();
        let id = uuid::Uuid::new_v4().to_string();
        let boot_d = boot_dir(&self.data_dir);
        let out_path = boot_d.join(format!("{}.bootanim", id));
        let src_s = src.display().to_string();
        let out_s = out_path.display().to_string();

        self.status_msg = "Importing boot animation...".into();
        let (tx, rx) = channel::<Result<(usize, f32, String), String>>();

        // Temp dir for extracted frames
        let tmp_dir = boot_d.join(format!("{}_frames", id));
        let tmp_s = tmp_dir.display().to_string();

        std::thread::spawn(move || {
            // Extract frames via Pillow (up to 40, quality ~65 for ~30KB/frame)
            let script = format!(r#"
from PIL import Image
import io, os, json
src={src:?}; tmp={tmp:?}
os.makedirs(tmp, exist_ok=True)
img=Image.open(src); frames=[]
try:
    while True:
        frames.append(img.copy().convert('RGB')); img.seek(img.tell()+1)
except EOFError: pass
n=min(len(frames),40); step=max(len(frames)//n,1); sel=frames[::step][:n]
while len(sel)<40: sel.append(sel[-1].copy())
total_ms=getattr(img,'info',{{}}).get('duration',80)*len(frames)
fps=min(max(1000.0*len(frames)/max(total_ms,1),5.0),12.5)
jpeg_frames=[]
for i,frame in enumerate(sel):
    r=frame.resize((480,480),Image.LANCZOS)
    b=io.BytesIO(); r.save(b,'JPEG',quality=75,subsampling='4:2:0',dpi=(96,96))
    jpeg_frames.append(b.getvalue())
    open(os.path.join(tmp,f'f{{i:05}}.jpg'),'wb').write(b.getvalue())
print(json.dumps({{"n":len(sel),"fps":fps}}))
"#, src = src_s, tmp = tmp_s);
            let r = std::process::Command::new("python3").args(["-c", &script]).output();
            match r {
                Ok(o) if o.status.success() => {
                    let v: serde_json::Value = serde_json::from_str(
                        String::from_utf8_lossy(&o.stdout).trim()).unwrap_or_default();
                    let n   = v["n"].as_u64().unwrap_or(1) as usize;
                    let fps = v["fps"].as_f64().unwrap_or(12.5) as f32;
                    // Build container
                    let mut jpeg_frames: Vec<Vec<u8>> = Vec::with_capacity(n);
                    for i in 0..n {
                        let p = format!("{}/f{:05}.jpg", tmp_s, i);
                        match std::fs::read(&p) {
                            Ok(d) => jpeg_frames.push(d),
                            Err(e) => { let _ = tx.send(Err(e.to_string())); return; }
                        }
                    }
                    let container = build_boot_container(&jpeg_frames, fps);
                    if let Err(e) = std::fs::write(&out_s, &container) {
                        let _ = tx.send(Err(e.to_string())); return;
                    }
                    // Cleanup temp frames
                    let _ = std::fs::remove_dir_all(&tmp_s);
                    let _ = tx.send(Ok((n, fps, out_s)));
                }
                Ok(o) => { let _ = tx.send(Err(String::from_utf8_lossy(&o.stderr).into())); }
                Err(e) => { let _ = tx.send(Err(e.to_string())); }
            }
        });

        match rx.recv_timeout(Duration::from_secs(120)) {
            Ok(Ok((n, fps, path))) => {
                // Use first frame for thumbnail — read directly from container
                let thumb = if let Ok(data) = std::fs::read(&path) {
                    // First frame JPEG starts at offset 32 + n*16 + 8 (after sub-header)
                    let tbl_size = (n as u32) * 16;
                    let frame0_off = (32 + tbl_size + 8) as usize;
                    if data.len() > frame0_off {
                        use base64::Engine;
                        let frame_size = u32::from_le_bytes(
                            data[32+4..32+8].try_into().unwrap_or([0;4])) as usize;
                        let end = (frame0_off + frame_size).min(data.len());
                        if let Ok(img) = image::load_from_memory(&data[frame0_off..end]) {
                            let small = img.resize(96, 96, image::imageops::FilterType::Lanczos3);
                            let mut buf = Vec::new();
                            small.write_to(&mut std::io::Cursor::new(&mut buf),
                                image::ImageFormat::Jpeg).ok();
                            base64::engine::general_purpose::STANDARD.encode(&buf)
                        } else { String::new() }
                    } else { String::new() }
                } else { String::new() };

                let anim = BootAnim { id: id.clone(), name, path, frame_count: n, fps, thumbnail: thumb };
                self.boot_anims.push(anim);
                save_boot(&self.data_dir, &self.boot_anims);
                self.selected_boot = Some(id);
                self.preview_texture = None;
                self.status_msg = format!("Boot animation: {} frames @ {:.0}fps", n, fps);
            }
            Ok(Err(e)) => self.status_msg = format!("Import failed: {}", e),
            Err(_) => self.status_msg = "Timed out".into(),
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
    p.rect_stroke(rect, 4.0, Stroke::new(if selected { 1.5 } else { 1.0 },
        if selected { BORDER_SEL } else { BORDER }));

    if let Some(tid) = tex {
        let img_r = rect.shrink(4.0).with_max_y(rect.max.y - 17.0);
        p.image(tid, img_r,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            Color32::WHITE);
    }

    let name_r = egui::Rect::from_min_max(egui::pos2(rect.min.x, rect.max.y - 17.0), rect.max);
    p.rect_filled(name_r, egui::Rounding { sw: 4.0, se: 4.0, ..Default::default() },
        Color32::from_black_alpha(210));
    let label: std::borrow::Cow<str> = if name.chars().count() > 13 {
        format!("{}…", name.chars().take(12).collect::<String>()).into()
    } else {
        name.into()
    };
    p.text(name_r.center(), egui::Align2::CENTER_CENTER, label.as_ref(),
        FontId::monospace(8.0), if selected { GREEN } else { TEXT_DIM });

    if hovered {
        p.rect_filled(del_rect, egui::Rounding { ne: 4.0, ..Default::default() },
            Color32::from_rgba_premultiplied(90, 10, 10, 230));
        p.text(del_rect.center(), egui::Align2::CENTER_CENTER, "x",
            FontId::proportional(10.0), RED);
    }

    let del_clicked = del_resp.clicked();
    (resp.clicked() && !del_clicked, del_clicked)
}

// ── eframe::App ───────────────────────────────────────────────────────────────

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Drain bg task
        if let Some(ref task) = self.bg_task {
            if let Ok(r) = task.rx.try_recv() {
                self.status_msg = match r { Ok(()) => "Done".into(), Err(e) => format!("Error: {}", e) };
                self.bg_task = None;
                self.last_status_poll = Instant::now() - Duration::from_secs(10);
            }
        }

        self.poll_status();

        if self.tab == Tab::Animation { self.tick_anim_preview(ctx); }
        if self.tab == Tab::Standby && self.preview_texture.is_none() {
            self.load_standby_preview(ctx);
        }
        if self.tab == Tab::Boot && self.preview_texture.is_none() {
            self.load_boot_preview(ctx);
        }

        // ── Top bar ───────────────────────────────────────────────────────────
        egui::TopBottomPanel::top("topbar")
            .frame(egui::Frame::none()
                .fill(Color32::from_rgb(7, 7, 7))
                .inner_margin(egui::Margin { left: 14.0, right: 14.0, top: 7.0, bottom: 7.0 }))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("TH LINUX LCD").color(GREEN).monospace().size(14.0).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // OFF button
                        if ui.add(egui::Button::new(
                            RichText::new("OFF").color(Color32::from_rgb(170,35,35)).monospace().size(11.0))
                            .fill(Color32::from_rgb(28,7,7))
                            .stroke(Stroke::new(1.0, Color32::from_rgb(90,18,18)))
                            .min_size(Vec2::new(40.0, 24.0))).clicked()
                        { self.send(Cmd::ScreenOff); }

                        // ON button
                        if ui.add(egui::Button::new(
                            RichText::new("ON").color(GREEN).monospace().size(11.0))
                            .fill(GREEN_DIM)
                            .stroke(Stroke::new(1.0, GREEN_MID))
                            .min_size(Vec2::new(40.0, 24.0))).clicked()
                        { self.send(Cmd::ScreenOn); }

                        ui.add_space(6.0);

                        // Status dot + label
                        let (dot, text, text_color) = if !self.daemon_up() {
                            (RED, "NO DAEMON", RED)
                        } else if !self.daemon_status.device_found {
                            (Color32::from_rgb(180,70,0), "NO DEVICE", Color32::from_rgb(180,70,0))
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
                            if ui.add(egui::Button::new(
                                RichText::new("RETRY").color(Color32::from_rgb(180,70,0)).monospace().size(10.0))
                                .fill(Color32::from_rgb(28, 14, 0))
                                .stroke(Stroke::new(1.0, Color32::from_rgb(120, 50, 0)))
                                .min_size(Vec2::new(44.0, 22.0))).clicked()
                            {
                                self.last_status_poll = Instant::now() - Duration::from_secs(10);
                            }
                        }
                    });
                });
            });

        // ── Bottom status bar ─────────────────────────────────────────────────
        egui::TopBottomPanel::bottom("statusbar")
            .frame(egui::Frame::none()
                .fill(Color32::from_rgb(7, 7, 7))
                .inner_margin(egui::Margin { left: 14.0, right: 14.0, top: 5.0, bottom: 5.0 }))
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
            .frame(egui::Frame::none()
                .fill(BG_PANEL)
                .inner_margin(egui::Margin { left: 8.0, right: 8.0, top: 12.0, bottom: 12.0 }))
            .show(ctx, |ui| {
                ui.add_space(4.0);
                for (t, lbl) in [(Tab::Animation, "ANIMATION"), (Tab::Standby, "STANDBY"), (Tab::Boot, "BOOT")] {
                    let active = self.tab == t;
                    let btn = egui::Button::new(
                        RichText::new(lbl)
                            .color(if active { GREEN } else { TEXT_DIM })
                            .monospace().size(11.0))
                        .fill(if active { BG_SEL } else { Color32::TRANSPARENT })
                        .stroke(Stroke::new(
                            if active { 1.0 } else { 0.0 },
                            if active { BORDER_SEL } else { Color32::TRANSPARENT }))
                        .min_size(Vec2::new(ui.available_width(), 30.0));
                    if ui.add(btn).clicked() && self.tab != t {
                        self.tab = t;
                        self.preview_texture = None;
                    }
                    ui.add_space(2.0);
                }
            });

        // ── Main content ──────────────────────────────────────────────────────
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(BG).inner_margin(egui::Margin::same(14.0)))
            .show(ctx, |ui| {
                match self.tab {
                    Tab::Animation => self.ui_animation(ui, ctx),
                    Tab::Standby => self.ui_standby(ui, ctx),
                    Tab::Boot => self.ui_boot(ui, ctx),
                }
            });
    }
}

impl App {
    fn ui_animation(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // Top section: preview left, info+button right
        ui.horizontal(|ui| {
            // Circular preview
            let sz = 180.0f32;
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(sz), egui::Sense::hover());
            let center = rect.center();
            let r = sz / 2.0;
            let p = ui.painter();
            p.circle_filled(center, r, Color32::from_rgb(5, 5, 5));
            if let Some(ref tex) = self.preview_texture {
                draw_circle_image(p, center, r - 1.0, tex.id());
            } else {
                p.text(center, egui::Align2::CENTER_CENTER, "NO SIGNAL",
                    FontId::monospace(10.0), TEXT_DIM);
            }
            let ring = if self.daemon_status.streaming { Color32::from_rgb(0,150,38) } else { BORDER };
            p.circle_stroke(center, r - 1.0, Stroke::new(2.0, ring));

            ui.add_space(16.0);

            // Info + controls
            ui.vertical(|ui| {
                ui.add_space(8.0);
                if let Some(ref id) = self.selected_video.clone() {
                    if let Some(v) = self.videos.iter().find(|v| v.id == *id) {
                        ui.label(RichText::new(&v.name).color(GREEN).monospace().size(12.0));
                        ui.add_space(2.0);
                        ui.label(RichText::new(format!("{} frames  {:.0} fps", v.frame_count, v.fps))
                            .color(TEXT_DIM).monospace().size(10.0));
                        ui.add_space(6.0);

                        // Rotation controls
                        let rot = v.rotation;
                        let vid_id = id.clone();
                        ui.horizontal(|ui| {
                            let btn_size = Vec2::new(28.0, 24.0);
                            let small_btn = |label: &str| {
                                egui::Button::new(RichText::new(label).color(TEXT_DIM).monospace().size(11.0))
                                    .fill(BG_CARD)
                                    .stroke(Stroke::new(1.0, BORDER))
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
                let is_playing = self.daemon_status.streaming
                    && self.daemon_status.playing_id == self.selected_video;

                if is_playing {
                    if ui.add(egui::Button::new(
                        RichText::new("STOP").color(RED).monospace().size(12.0))
                        .fill(Color32::from_rgb(28, 6, 6))
                        .stroke(Stroke::new(1.0, RED))
                        .min_size(Vec2::new(140.0, 30.0))).clicked()
                    {
                        self.send(Cmd::Stop);
                        self.status_msg = "Stopped".into();
                    }
                } else {
                    if ui.add_enabled(has_sel, egui::Button::new(
                        RichText::new("SEND TO LCD").color(if has_sel { GREEN } else { TEXT_DIM }).monospace().size(12.0))
                        .fill(if has_sel { GREEN_DIM } else { BG_CARD })
                        .stroke(Stroke::new(1.0, if has_sel { GREEN_MID } else { BORDER }))
                        .min_size(Vec2::new(140.0, 30.0))).clicked()
                    {
                        if let Some(id) = self.selected_video.clone() {
                            let fps = self.videos.iter().find(|v| v.id == id).map(|v| v.fps).unwrap_or(15.0);
                            if let Some((play_dir, count)) = self.prepare_play_dir(&id) {
                                self.send(Cmd::Play {
                                    frames_dir: play_dir.display().to_string(),
                                    fps, frame_count: count, id: id.clone(),
                                });
                            }
                        }
                    }
                }
            });
        });

        ui.add_space(14.0);
        ui.add(egui::Separator::default().horizontal());
        ui.add_space(6.0);

        // Gallery header
        ui.horizontal(|ui| {
            ui.label(RichText::new("ANIMATIONS").color(TEXT_DIM).monospace().size(10.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(RichText::new("+ ADD GIF").color(TEXT_DIM).monospace().size(10.0)).clicked() {
                    self.add_video();
                }
            });
        });
        ui.add_space(6.0);

        if self.videos.is_empty() {
            ui.label(RichText::new("No animations — import a GIF to get started").color(TEXT_DIM).monospace().size(11.0));
            return;
        }

        let items: Vec<(String, String, String)> = self.videos.iter()
            .map(|v| (v.id.clone(), v.name.clone(), v.thumbnail.clone())).collect();
        let card = 88.0;
        let mut sel = None;
        let mut del = None;

        egui::ScrollArea::vertical().id_salt("anim_scroll").show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.style_mut().spacing.item_spacing = Vec2::splat(5.0);
                for (id, name, b64) in &items {
                    let selected = self.selected_video.as_deref() == Some(id);
                    let tex = self.get_thumb(ctx, id, b64);
                    let (clicked, deleted) = gallery_card(ui, id, name, tex, selected, card);
                    if clicked { sel = Some(id.clone()); }
                    if deleted { del = Some(id.clone()); }
                }
            });
        });

        if let Some(id) = sel {
            if self.selected_video.as_deref() != Some(&id) {
                self.selected_video = Some(id);
                self.preview_frame_idx = 0;
                self.preview_texture = None;
            }
        }
        if let Some(id) = del {
            if self.daemon_status.playing_id.as_deref() == Some(&id) { self.send(Cmd::Stop); }
            let _ = std::fs::remove_dir_all(frames_dir(&self.data_dir, &id));
            self.videos.retain(|v| v.id != id);
            save_videos(&self.data_dir, &self.videos);
            self.thumb_cache.remove(&id);
            if self.selected_video.as_deref() == Some(&id) {
                self.selected_video = self.videos.first().map(|v| v.id.clone());
                self.preview_texture = None;
            }
        }
    }

    fn ui_standby(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            let sz = 180.0f32;
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(sz), egui::Sense::hover());
            let center = rect.center();
            let r = sz / 2.0;
            let p = ui.painter();
            p.circle_filled(center, r, Color32::from_rgb(5, 5, 5));
            if let Some(ref tex) = self.preview_texture {
                draw_circle_image(p, center, r - 1.0, tex.id());
            } else {
                p.text(center, egui::Align2::CENTER_CENTER, "NO SIGNAL",
                    FontId::monospace(10.0), TEXT_DIM);
            }
            p.circle_stroke(center, r - 1.0, Stroke::new(2.0, BORDER));

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

                let has_sel = self.selected_standby.is_some();
                let busy = self.bg_task.is_some();
                let enabled = has_sel && !busy;
                let lbl = if busy { "UPLOADING..." } else { "UPLOAD TO LCD" };

                if ui.add_enabled(enabled, egui::Button::new(
                    RichText::new(lbl).color(if enabled { GREEN } else { TEXT_DIM }).monospace().size(12.0))
                    .fill(if enabled { GREEN_DIM } else { BG_CARD })
                    .stroke(Stroke::new(1.0, if enabled { GREEN_MID } else { BORDER }))
                    .min_size(Vec2::new(140.0, 30.0))).clicked()
                {
                    if let Some(id) = &self.selected_standby.clone() {
                        if let Some(img) = self.standby.iter().find(|i| i.id == *id) {
                            let path = img.path.clone();
                            self.send_bg(Cmd::UploadStandby { path }, "Uploading...");
                            self.status_msg = "Uploading...".into();
                        }
                    }
                }
            });
        });

        ui.add_space(14.0);
        ui.add(egui::Separator::default().horizontal());
        ui.add_space(6.0);

        ui.horizontal(|ui| {
            ui.label(RichText::new("STANDBY IMAGES").color(TEXT_DIM).monospace().size(10.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(RichText::new("+ ADD IMAGE").color(TEXT_DIM).monospace().size(10.0)).clicked() {
                    self.add_standby();
                }
            });
        });
        ui.add_space(6.0);

        if self.standby.is_empty() {
            ui.label(RichText::new("No standby images — import one to get started").color(TEXT_DIM).monospace().size(11.0));
            return;
        }

        let items: Vec<(String, String, String)> = self.standby.iter()
            .map(|i| (i.id.clone(), i.name.clone(), i.thumbnail.clone())).collect();
        let card = 88.0;
        let mut sel = None;
        let mut del = None;

        egui::ScrollArea::vertical().id_salt("sb_scroll").show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.style_mut().spacing.item_spacing = Vec2::splat(5.0);
                for (id, name, b64) in &items {
                    let selected = self.selected_standby.as_deref() == Some(id);
                    let tex = self.get_thumb(ctx, id, b64);
                    let (clicked, deleted) = gallery_card(ui, id, name, tex, selected, card);
                    if clicked { sel = Some(id.clone()); }
                    if deleted { del = Some(id.clone()); }
                }
            });
        });

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
            save_standby(&self.data_dir, &self.standby);
            self.thumb_cache.remove(&id);
            if self.selected_standby.as_deref() == Some(&id) {
                self.selected_standby = None;
                self.preview_texture = None;
            }
        }
    }

    fn ui_boot(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            let sz = 180.0f32;
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(sz), egui::Sense::hover());
            let center = rect.center();
            let r = sz / 2.0;
            let p = ui.painter();
            p.circle_filled(center, r, Color32::from_rgb(5, 5, 5));
            if let Some(ref tex) = self.preview_texture {
                draw_circle_image(p, center, r - 1.0, tex.id());
            } else {
                p.text(center, egui::Align2::CENTER_CENTER, "NO SIGNAL",
                    FontId::monospace(10.0), TEXT_DIM);
            }
            p.circle_stroke(center, r - 1.0, Stroke::new(2.0, BORDER));

            ui.add_space(16.0);

            ui.vertical(|ui| {
                ui.add_space(8.0);
                if let Some(ref id) = self.selected_boot.clone() {
                    if let Some(anim) = self.boot_anims.iter().find(|a| a.id == *id) {
                        ui.label(RichText::new(&anim.name).color(GREEN).monospace().size(12.0));
                        ui.add_space(2.0);
                        ui.label(RichText::new(format!("{} frames  {:.0} fps", anim.frame_count, anim.fps))
                            .color(TEXT_DIM).monospace().size(10.0));
                    }
                } else {
                    ui.label(RichText::new("no boot animation selected").color(TEXT_DIM).monospace().size(11.0));
                }

                ui.add_space(12.0);

                let has_sel = self.selected_boot.is_some();
                let busy = self.bg_task.is_some();
                let enabled = has_sel && !busy;
                let lbl = if busy { "UPLOADING..." } else { "UPLOAD TO LCD" };

                if ui.add_enabled(enabled, egui::Button::new(
                    RichText::new(lbl).color(if enabled { GREEN } else { TEXT_DIM }).monospace().size(12.0))
                    .fill(if enabled { GREEN_DIM } else { BG_CARD })
                    .stroke(Stroke::new(1.0, if enabled { GREEN_MID } else { BORDER }))
                    .min_size(Vec2::new(140.0, 30.0))).clicked()
                {
                    if let Some(id) = &self.selected_boot.clone() {
                        if let Some(anim) = self.boot_anims.iter().find(|a| a.id == *id) {
                            let path = anim.path.clone();
                            self.send_bg(Cmd::UploadBoot { path }, "Uploading boot...");
                            self.status_msg = "Uploading boot animation...".into();
                        }
                    }
                }
            });
        });

        ui.add_space(14.0);
        ui.add(egui::Separator::default().horizontal());
        ui.add_space(6.0);

        ui.horizontal(|ui| {
            ui.label(RichText::new("BOOT ANIMATIONS").color(TEXT_DIM).monospace().size(10.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(RichText::new("+ ADD GIF").color(TEXT_DIM).monospace().size(10.0)).clicked() {
                    self.add_boot();
                }
            });
        });
        ui.add_space(6.0);

        if self.boot_anims.is_empty() {
            ui.label(RichText::new("No boot animations — import a GIF to get started").color(TEXT_DIM).monospace().size(11.0));
            return;
        }

        let items: Vec<(String, String, String)> = self.boot_anims.iter()
            .map(|a| (a.id.clone(), a.name.clone(), a.thumbnail.clone())).collect();
        let card = 88.0;
        let mut sel = None;
        let mut del = None;

        egui::ScrollArea::vertical().id_salt("boot_scroll").show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.style_mut().spacing.item_spacing = Vec2::splat(5.0);
                for (id, name, b64) in &items {
                    let selected = self.selected_boot.as_deref() == Some(id);
                    let tex = self.get_thumb(ctx, id, b64);
                    let (clicked, deleted) = gallery_card(ui, id, name, tex, selected, card);
                    if clicked { sel = Some(id.clone()); }
                    if deleted { del = Some(id.clone()); }
                }
            });
        });

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
            save_boot(&self.data_dir, &self.boot_anims);
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
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("TH Linux LCD")
            .with_inner_size([640.0, 520.0])
            .with_min_inner_size([500.0, 400.0])
            .with_resizable(true),
        ..Default::default()
    };
    eframe::run_native("TH Linux LCD", options, Box::new(|cc| Ok(Box::new(App::new(cc))))).unwrap();
}
