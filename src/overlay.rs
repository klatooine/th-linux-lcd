//! Stats overlay drawn onto streamed frames: up to two rows of text on the
//! round 480×480 LCD, in one of several styles.
//!
//! Row 1: CPU temp · coolant temp · GPU temp. Row 2: CPU load · GPU load · RAM.
//! Rendering happens once per change of the displayed text into a `Layer`
//! (premultiplied RGBA for a horizontal band of the screen), which is then
//! blended onto each frame.

use crate::shared::{Metrics, OverlaySettings, OverlayStyle, Stat, LCD_SIZE, STREAM_FRAME_BUDGET};
use ab_glyph::{point, Font, FontRef, PxScale, ScaleFont};
use std::sync::OnceLock;

const SIZE: usize = LCD_SIZE as usize;
/// Text height at size 1.0.
const BASE_PX: f32 = 27.0;
/// Keep text this far inside the round edge.
const EDGE_MARGIN: f32 = 22.0;
const QUALITY_START: u8 = 60;

fn font() -> &'static FontRef<'static> {
    static FONT: OnceLock<FontRef<'static>> = OnceLock::new();
    FONT.get_or_init(|| FontRef::try_from_slice(epaint_default_fonts::HACK_REGULAR).expect("bundled font is valid"))
}

/// One stat on the overlay, e.g. label "CPU", value "52°".
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub label: &'static str,
    pub value: String,
}

/// Enabled stats that have a value, in display rows. Empty rows are dropped.
pub fn rows(metrics: &Metrics, settings: &OverlaySettings) -> Vec<Vec<Item>> {
    Stat::ROWS
        .iter()
        .map(|row| {
            row.iter()
                .filter(|&&s| settings.shows(s))
                .filter_map(|&s| s.value(metrics).map(|value| Item { label: s.label(), value }))
                .collect::<Vec<_>>()
        })
        .filter(|r| !r.is_empty())
        .collect()
}

/// Cache key: the overlay only needs re-rendering when this changes.
pub fn key(rows: &[Vec<Item>], s: &OverlaySettings) -> String {
    let text: Vec<String> = rows
        .iter()
        .map(|r| r.iter().map(|i| format!("{} {}", i.label, i.value)).collect::<Vec<_>>().join("  "))
        .collect();
    format!("{:?}|{:.3}|{:.3}|{}", s.style, s.size, s.vertical, text.join("\n"))
}

// ── Coverage masks ────────────────────────────────────────────────────────────

/// Per-pixel coverage (0..1) for a horizontal band of the screen.
#[derive(Clone)]
struct Mask {
    h: usize,
    y0: f32,
    data: Vec<f32>,
}

impl Mask {
    fn new(y0: usize, h: usize) -> Self {
        Self { h, y0: y0 as f32, data: vec![0.0; SIZE * h] }
    }

    /// Max-combine coverage at screen coordinates.
    fn plot(&mut self, x: i64, y: i64, v: f32) {
        let row = y - self.y0 as i64;
        if x < 0 || row < 0 || x >= SIZE as i64 || row >= self.h as i64 || v <= 0.0 {
            return;
        }
        let i = row as usize * SIZE + x as usize;
        if v > self.data[i] {
            self.data[i] = v.min(1.0);
        }
    }

    /// Coverage of any shape described by a signed-distance function (screen coords),
    /// evaluated inside the given bounding box.
    fn fill_sdf(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, sdf: impl Fn(f32, f32) -> f32) {
        for y in (y0.floor() as i64 - 1)..=(y1.ceil() as i64 + 1) {
            for x in (x0.floor() as i64 - 1)..=(x1.ceil() as i64 + 1) {
                let d = sdf(x as f32 + 0.5, y as f32 + 0.5);
                self.plot(x, y, (0.5 - d).clamp(0.0, 1.0));
            }
        }
    }

    fn max_with(&self, other: &Mask) -> Mask {
        let mut m = self.clone();
        for (a, b) in m.data.iter_mut().zip(&other.data) {
            *a = a.max(*b);
        }
        m
    }

    /// Grow by `radius` px with an anti-aliased disc (for outlines).
    fn dilate(&self, radius: f32) -> Mask {
        let r = radius.ceil() as i64;
        let offsets: Vec<(i64, i64, f32)> = (-r..=r)
            .flat_map(|dy| (-r..=r).map(move |dx| (dx, dy)))
            .filter_map(|(dx, dy)| {
                let w = (radius + 0.5 - ((dx * dx + dy * dy) as f32).sqrt()).clamp(0.0, 1.0);
                (w > 0.0).then_some((dx, dy, w))
            })
            .collect();
        let mut out = Mask { h: self.h, y0: self.y0, data: vec![0.0; self.data.len()] };
        for y in 0..self.h as i64 {
            for x in 0..SIZE as i64 {
                let src = self.data[y as usize * SIZE + x as usize];
                if src == 0.0 {
                    continue;
                }
                for &(dx, dy, w) in &offsets {
                    let (tx, ty) = (x + dx, y + dy);
                    if tx < 0 || ty < 0 || tx >= SIZE as i64 || ty >= self.h as i64 {
                        continue;
                    }
                    let i = ty as usize * SIZE + tx as usize;
                    out.data[i] = out.data[i].max(src * w);
                }
            }
        }
        out
    }
}

fn sd_capsule(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32, r: f32) -> f32 {
    let (pax, pay, bax, bay) = (px - ax, py - ay, bx - ax, by - ay);
    let h = ((pax * bax + pay * bay) / (bax * bax + bay * bay).max(1e-6)).clamp(0.0, 1.0);
    ((pax - bax * h).powi(2) + (pay - bay * h).powi(2)).sqrt() - r
}

fn sd_round_rect(px: f32, py: f32, cx: f32, cy: f32, hw: f32, hh: f32, r: f32) -> f32 {
    let qx = (px - cx).abs() - (hw - r);
    let qy = (py - cy).abs() - (hh - r);
    (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r
}

// ── Text and seven-segment drawing ────────────────────────────────────────────

/// Draw `text` with the bundled font; `bold` lists extra horizontal offsets
/// (faux bold). Returns the advance width.
fn draw_text(mask: &mut Mask, text: &str, px: f32, x: f32, cy: f32, bold: &[f32]) -> f32 {
    let f = font();
    let scale = PxScale::from(px);
    let sf = f.as_scaled(scale);
    let baseline = cy + (sf.ascent() + sf.descent()) / 2.0;
    let mut pen = x;
    for c in text.chars() {
        let id = f.glyph_id(c);
        for &dx in bold {
            let glyph = id.with_scale_and_position(scale, point(pen + dx, baseline));
            if let Some(outlined) = f.outline_glyph(glyph) {
                let b = outlined.px_bounds();
                outlined.draw(|gx, gy, cov| mask.plot(b.min.x as i64 + gx as i64, b.min.y as i64 + gy as i64, cov));
            }
        }
        pen += sf.h_advance(id);
    }
    pen - x
}

fn text_width(text: &str, px: f32) -> f32 {
    let f = font();
    let sf = f.as_scaled(PxScale::from(px));
    text.chars().map(|c| sf.h_advance(f.glyph_id(c))).sum()
}

const SEG_SLANT: f32 = 0.08;

/// Segments a..g lit for each digit.
fn seg_digit(d: char) -> Option<[bool; 7]> {
    let s = |a, b, c, d, e, f, g| [a, b, c, d, e, f, g];
    Some(match d {
        '0' => s(true, true, true, true, true, true, false),
        '1' => s(false, true, true, false, false, false, false),
        '2' => s(true, true, false, true, true, false, true),
        '3' => s(true, true, true, true, false, false, true),
        '4' => s(false, true, true, false, false, true, true),
        '5' => s(true, false, true, true, false, true, true),
        '6' => s(true, false, true, true, true, true, true),
        '7' => s(true, true, true, false, false, false, false),
        '8' => s(true, true, true, true, true, true, true),
        '9' => s(true, true, true, true, false, true, true),
        '-' => s(false, false, false, false, false, false, true),
        _ => return None,
    })
}

fn seg_advance(c: char, h: f32) -> f32 {
    match c {
        '.' => 0.24 * h,
        '°' => 0.4 * h,
        _ if seg_digit(c).is_some() => 0.66 * h,
        _ => text_width(&c.to_string(), h * 0.9),
    }
}

/// Seven-segment rendering of a value like "52°" or "31.5°". Unlit segments
/// go into `ghost`. Returns the advance width.
fn draw_seg(lit: &mut Mask, ghost: &mut Mask, text: &str, h: f32, x: f32, cy: f32) -> f32 {
    let w = 0.5 * h;
    let t = 0.13 * h;
    let gap = 0.035 * h;
    let top = cy - h / 2.0;
    let (mid, bot) = (cy, cy + h / 2.0);
    let mut pen = x;
    for c in text.chars() {
        let x0 = pen + 0.06 * h;
        if let Some(on) = seg_digit(c) {
            let (l, r) = (x0 + t / 2.0, x0 + w - t / 2.0);
            let segs = [
                (l + gap, top + t / 2.0, r - gap, top + t / 2.0),
                (r, top + t / 2.0 + gap, r, mid - gap),
                (r, mid + gap, r, bot - t / 2.0 - gap),
                (l + gap, bot - t / 2.0, r - gap, bot - t / 2.0),
                (l, mid + gap, l, bot - t / 2.0 - gap),
                (l, top + t / 2.0 + gap, l, mid - gap),
                (l + gap, mid, r - gap, mid),
            ];
            for (lit_on, &(ax, ay, bx, by)) in on.iter().zip(&segs) {
                let target = if *lit_on { &mut *lit } else { &mut *ghost };
                let slant = h * SEG_SLANT;
                target.fill_sdf(ax.min(bx) - t, ay.min(by) - t, ax.max(bx) + t + slant, ay.max(by) + t, |px, py| {
                    // Lean the digit to the right like a classic clock display.
                    let ux = px - (mid - py) / h * slant;
                    sd_capsule(ux, py, ax, ay, bx, by, t / 2.0)
                });
            }
        } else if c == '.' {
            let (dx, dy) = (x0 + 0.04 * h, bot - t / 2.0);
            lit.fill_sdf(dx - t, dy - t, dx + t, dy + t, |px, py| {
                ((px - dx).powi(2) + (py - dy).powi(2)).sqrt() - t * 0.6
            });
        } else if c == '°' {
            let (dx, dy, rad) = (x0 + 0.16 * h, top + 0.17 * h, 0.11 * h);
            lit.fill_sdf(dx - rad - t, dy - rad - t, dx + rad + t, dy + rad + t, |px, py| {
                (((px - dx).powi(2) + (py - dy).powi(2)).sqrt() - rad).abs() - t * 0.38
            });
        } else {
            draw_text(lit, &c.to_string(), h * 0.9, pen, cy, &[0.0, 1.0]);
        }
        pen += seg_advance(c, h);
    }
    pen - x
}

// ── Layout ────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum Part {
    Label,
    Value,
}

enum Piece {
    Text { s: String, px: f32, part: Part },
    Seg { s: String, h: f32 },
    Gap(f32),
}

impl Piece {
    fn width(&self) -> f32 {
        match self {
            Piece::Text { s, px, .. } => text_width(s, *px),
            Piece::Seg { s, h } => s.chars().map(|c| seg_advance(c, *h)).sum(),
            Piece::Gap(w) => *w,
        }
    }

    fn scaled(self, k: f32) -> Piece {
        match self {
            Piece::Text { s, px, part } => Piece::Text { s, px: px * k, part },
            Piece::Seg { s, h } => Piece::Seg { s, h: h * k },
            Piece::Gap(w) => Piece::Gap(w * k),
        }
    }
}

/// Visual parameters per style.
struct Look {
    label_scale: f32,
    seg_values: bool,
    bold: &'static [f32],
    row_gap: f32,
    item_gap: f32,
}

fn look(style: OverlayStyle) -> Look {
    match style {
        OverlayStyle::Terminal => {
            Look { label_scale: 1.0, seg_values: false, bold: &[0.0, 1.0], row_gap: 1.55, item_gap: 0.75 }
        }
        OverlayStyle::RetroClock => {
            Look { label_scale: 0.55, seg_values: true, bold: &[0.0, 1.0], row_gap: 1.75, item_gap: 0.8 }
        }
        OverlayStyle::HeavyMetal => {
            Look { label_scale: 1.0, seg_values: false, bold: &[0.0, 0.8, 1.6], row_gap: 1.6, item_gap: 0.75 }
        }
        OverlayStyle::Kawaii => {
            Look { label_scale: 1.0, seg_values: false, bold: &[0.0, 0.8, 1.6], row_gap: 1.8, item_gap: 0.9 }
        }
    }
}

struct RowLayout {
    cy: f32,
    x: f32,
    width: f32,
    height: f32,
    pieces: Vec<Piece>,
}

fn layout_rows(rows: &[Vec<Item>], s: &OverlaySettings) -> Vec<RowLayout> {
    let lk = look(s.style);
    let base = BASE_PX * s.size;
    let vy = s.vertical * SIZE as f32;
    let gap = base * lk.row_gap;
    let centers: Vec<f32> = if rows.len() == 1 { vec![vy] } else { vec![vy - gap / 2.0, vy + gap / 2.0] };
    let r = SIZE as f32 / 2.0;

    rows.iter()
        .zip(centers)
        .map(|(items, cy)| {
            let mut pieces = Vec::new();
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    pieces.push(Piece::Gap(base * lk.item_gap));
                }
                let label_px = base * lk.label_scale;
                pieces.push(Piece::Text { s: item.label.to_string(), px: label_px, part: Part::Label });
                pieces.push(Piece::Gap(text_width(" ", label_px) * if lk.seg_values { 0.6 } else { 1.0 }));
                pieces.push(if lk.seg_values {
                    Piece::Seg { s: item.value.clone(), h: base }
                } else {
                    Piece::Text { s: item.value.clone(), px: base, part: Part::Value }
                });
            }
            // Shrink to fit the circle's chord at this height.
            let dy = (cy - r).abs() + base / 2.0;
            let chord = 2.0 * (r * r - dy * dy).max(0.0).sqrt() - 2.0 * EDGE_MARGIN;
            let w: f32 = pieces.iter().map(Piece::width).sum();
            let k = if w > chord && chord > 0.0 { chord / w } else { 1.0 };
            let pieces: Vec<Piece> = pieces.into_iter().map(|p| p.scaled(k)).collect();
            let width = pieces.iter().map(Piece::width).sum::<f32>();
            RowLayout { cy, x: r - width / 2.0, width, height: base * k, pieces }
        })
        .collect()
}

// ── Compositing ───────────────────────────────────────────────────────────────

/// Premultiplied RGBA canvas for the band.
struct Canvas {
    y0: usize,
    h: usize,
    px: Vec<[f32; 4]>,
}

impl Canvas {
    /// Paint `color(x, y)` through `mask` with `opacity` (source-over).
    fn over(&mut self, mask: &Mask, opacity: f32, color: impl Fn(usize, usize) -> [f32; 3]) {
        for (i, &m) in mask.data.iter().enumerate() {
            let a = m * opacity;
            if a <= 0.0 {
                continue;
            }
            let (x, y) = (i % SIZE, self.y0 + i / SIZE);
            let c = color(x, y);
            let p = &mut self.px[i];
            for ch in 0..3 {
                p[ch] = c[ch] * a + p[ch] * (1.0 - a);
            }
            p[3] = a + p[3] * (1.0 - a);
        }
    }
}

fn rgb(r: u8, g: u8, b: u8) -> [f32; 3] {
    [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0]
}

/// Piecewise-linear colour ramp; `stops` sorted by position in 0..1.
fn ramp(stops: &[(f32, [f32; 3])], t: f32) -> [f32; 3] {
    let t = t.clamp(0.0, 1.0);
    for w in stops.windows(2) {
        let ((t0, c0), (t1, c1)) = (w[0], w[1]);
        if t <= t1 {
            let f = if t1 > t0 { (t - t0) / (t1 - t0) } else { 0.0 };
            return [c0[0] + (c1[0] - c0[0]) * f, c0[1] + (c1[1] - c0[1]) * f, c0[2] + (c1[2] - c0[2]) * f];
        }
    }
    stops.last().map_or([1.0; 3], |s| s.1)
}

/// Pre-rendered overlay for a horizontal band of the frame (premultiplied RGBA).
pub struct Layer {
    y0: usize,
    h: usize,
    rgba: Vec<[u8; 4]>,
}

impl Layer {
    /// Render the overlay. None if there is nothing to draw.
    pub fn render(rows: &[Vec<Item>], settings: &OverlaySettings) -> Option<Self> {
        let s = settings.sanitized();
        let rows = &rows[..rows.len().min(2)];
        if rows.is_empty() {
            return None;
        }
        let lk = look(s.style);
        let laid = layout_rows(rows, &s);
        let base = BASE_PX * s.size;

        let pad = base * 0.9 + 14.0 * s.size;
        let top = laid.iter().map(|r| r.cy - r.height / 2.0).fold(f32::MAX, f32::min) - pad;
        let bottom = laid.iter().map(|r| r.cy + r.height / 2.0).fold(f32::MIN, f32::max) + pad;
        let y0 = top.max(0.0) as usize;
        let h = (bottom.min(SIZE as f32) as usize).saturating_sub(y0).max(1);

        let mut label = Mask::new(y0, h);
        let mut value = Mask::new(y0, h);
        let mut ghost = Mask::new(y0, h);
        for row in &laid {
            let mut x = row.x;
            for piece in &row.pieces {
                match piece {
                    Piece::Text { s: t, px, part } => {
                        let m = if *part == Part::Label { &mut label } else { &mut value };
                        x += draw_text(m, t, *px, x, row.cy, lk.bold);
                    }
                    Piece::Seg { s: t, h: sh } => x += draw_seg(&mut value, &mut ghost, t, *sh, x, row.cy),
                    Piece::Gap(w) => x += w,
                }
            }
        }

        let mut canvas = Canvas { y0, h, px: vec![[0.0; 4]; SIZE * h] };
        let both = label.max_with(&value);
        let k = s.size;
        // Row-relative vertical position (0 = top of text, 1 = bottom) for gradients.
        let row_t = |y: usize| -> f32 {
            let row = laid.iter().min_by(|a, b| (a.cy - y as f32).abs().total_cmp(&(b.cy - y as f32).abs()));
            row.map_or(0.5, |r| (y as f32 - (r.cy - r.height / 2.0)) / r.height.max(1.0))
        };

        match s.style {
            OverlayStyle::Terminal => {
                canvas.over(&both.dilate(3.0 * k), 0.9, |_, _| [0.0; 3]);
                canvas.over(&label, 1.0, |_, _| rgb(0, 210, 55));
                canvas.over(&value, 1.0, |_, _| rgb(255, 255, 255));
            }
            OverlayStyle::RetroClock => {
                // Black panel with a dark-yellow rim behind the whole block.
                let x0 = laid.iter().map(|r| r.x).fold(f32::MAX, f32::min) - 14.0 * k;
                let x1 = laid.iter().map(|r| r.x + r.width).fold(f32::MIN, f32::max) + 14.0 * k;
                let py0 = laid.iter().map(|r| r.cy - r.height / 2.0).fold(f32::MAX, f32::min) - 10.0 * k;
                let py1 = laid.iter().map(|r| r.cy + r.height / 2.0).fold(f32::MIN, f32::max) + 10.0 * k;
                let (cx, cy, hw, hh, rad) =
                    ((x0 + x1) / 2.0, (py0 + py1) / 2.0, (x1 - x0) / 2.0, (py1 - py0) / 2.0, 9.0 * k);
                let mut panel = Mask::new(y0, h);
                panel.fill_sdf(x0, py0, x1, py1, |px, py| sd_round_rect(px, py, cx, cy, hw, hh, rad));
                let mut rim = Mask::new(y0, h);
                rim.fill_sdf(x0, py0, x1, py1, |px, py| (sd_round_rect(px, py, cx, cy, hw, hh, rad) + 1.0).abs() - 1.0);
                canvas.over(&panel, 0.9, |_, _| rgb(6, 5, 0));
                canvas.over(&rim, 1.0, |_, _| rgb(110, 88, 0));
                canvas.over(&ghost, 0.12, |_, _| rgb(255, 210, 0));
                canvas.over(&value.dilate(2.5 * k), 0.22, |_, _| rgb(255, 190, 0)); // LED glow
                canvas.over(&label, 1.0, |_, _| rgb(205, 165, 0));
                canvas.over(&value, 1.0, |_, _| rgb(255, 214, 10));
            }
            OverlayStyle::HeavyMetal => {
                let chrome = [
                    (0.0, rgb(250, 251, 255)),
                    (0.42, rgb(160, 166, 178)),
                    (0.5, rgb(52, 55, 64)),
                    (0.58, rgb(120, 126, 138)),
                    (1.0, rgb(235, 238, 246)),
                ];
                canvas.over(&both.dilate(1.6 * k), 0.95, |_, _| rgb(14, 14, 18));
                canvas.over(&both, 1.0, |_, y| ramp(&chrome, row_t(y)));
            }
            OverlayStyle::Kawaii => {
                let pink = [(0.0, rgb(255, 170, 215)), (0.55, rgb(255, 95, 175)), (1.0, rgb(240, 60, 150))];
                canvas.over(&both.dilate(6.0 * k), 1.0, |_, _| rgb(90, 215, 160));
                canvas.over(&both.dilate(3.2 * k), 1.0, |_, _| rgb(255, 255, 255));
                canvas.over(&label, 1.0, |_, _| rgb(30, 165, 110));
                canvas.over(&value, 1.0, |_, y| ramp(&pink, row_t(y)));
            }
        }

        let rgba =
            canvas.px.iter().map(|p| [0, 1, 2, 3].map(|c| (p[c].clamp(0.0, 1.0) * 255.0).round() as u8)).collect();
        Some(Self { y0: canvas.y0, h: canvas.h, rgba })
    }

    /// Blend onto a 480×480 image buffer with `channels` bytes per pixel (3 = RGB, 4 = RGBA).
    pub fn apply(&self, buf: &mut [u8], channels: usize) {
        if buf.len() < SIZE * SIZE * channels {
            return;
        }
        for row in 0..self.h {
            for x in 0..SIZE {
                let [r, g, b, a] = self.rgba[row * SIZE + x];
                if a == 0 {
                    continue;
                }
                let p = ((self.y0 + row) * SIZE + x) * channels;
                let inv = 255 - a as u32;
                for (c, fg) in [r, g, b].into_iter().enumerate() {
                    buf[p + c] = (fg as u32 + buf[p + c] as u32 * inv / 255).min(255) as u8;
                }
            }
        }
    }

    /// Decode a 480×480 JPEG frame, draw the overlay and re-encode it as a
    /// 4:4:4 JPEG within the streaming size budget. `quality` carries the
    /// last good quality between frames so most frames encode only once.
    pub fn apply_to_jpeg(&self, jpeg: &[u8], quality: &mut u8) -> Result<Vec<u8>, String> {
        let img = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg).map_err(|e| e.to_string())?;
        if img.width() != LCD_SIZE || img.height() != LCD_SIZE {
            return Err(format!("frame is {}×{}, expected {LCD_SIZE}×{LCD_SIZE}", img.width(), img.height()));
        }
        let mut rgb = img.into_rgb8().into_raw();
        self.apply(&mut rgb, 3);

        // The budget is a target, not a hard limit: very detailed frames may
        // stay slightly above it even at the lowest quality.
        let mut q = (*quality).clamp(5, 90);
        loop {
            let out = encode(&rgb, q)?;
            if out.len() <= STREAM_FRAME_BUDGET || q <= 5 {
                // Drift back up when there is plenty of headroom.
                *quality = if out.len() < STREAM_FRAME_BUDGET * 6 / 10 { (q + 5).min(90) } else { q };
                return Ok(out);
            }
            q = q.saturating_sub(10).max(5);
        }
    }
}

/// Starting JPEG quality for `Layer::apply_to_jpeg`.
pub fn initial_quality() -> u8 {
    QUALITY_START
}

/// 480×480 RGB → baseline JPEG, 4:4:4, 96 dpi, JFIF 1.1 (what the LCD expects).
fn encode(rgb: &[u8], quality: u8) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(STREAM_FRAME_BUDGET);
    let mut enc = jpeg_encoder::Encoder::new(&mut out, quality);
    enc.set_sampling_factor(jpeg_encoder::SamplingFactor::F_1_1);
    enc.set_density(jpeg_encoder::Density::Inch { x: 96, y: 96 });
    enc.encode(rgb, LCD_SIZE as u16, LCD_SIZE as u16, jpeg_encoder::ColorType::Rgb).map_err(|e| e.to_string())?;
    // The encoder writes JFIF 1.2; known-good frames use 1.1.
    if out.len() > 13 && &out[6..11] == b"JFIF\0" {
        out[11] = 1;
        out[12] = 1;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::{OVERLAY_SIZE_RANGE, OVERLAY_TOP};

    fn metrics() -> Metrics {
        Metrics {
            cpu_temp: Some(52.4),
            coolant: Some(31.46),
            gpu_temp: Some(45.0),
            cpu_load: Some(23.0),
            gpu_load: Some(100.0),
            ram: Some(41.2),
        }
    }

    fn all_on() -> OverlaySettings {
        OverlaySettings { enabled: true, ..Default::default() }
    }

    #[test]
    fn rows_follow_layout_and_settings() {
        let r = rows(&metrics(), &all_on());
        let text: Vec<Vec<String>> =
            r.iter().map(|row| row.iter().map(|i| format!("{} {}", i.label, i.value)).collect()).collect();
        assert_eq!(text, vec![vec!["CPU 52°", "LIQ 31.5°", "GPU 45°"], vec!["CPU 23%", "GPU 100%", "RAM 41%"]]);

        let only_loads = OverlaySettings { cpu_temp: false, coolant: false, gpu_temp: false, ..all_on() };
        assert_eq!(rows(&metrics(), &only_loads).len(), 1);

        let none = Metrics::default();
        assert!(rows(&none, &all_on()).is_empty());
        assert!(Layer::render(&rows(&none, &all_on()), &all_on()).is_none());
    }

    #[test]
    fn every_style_size_and_position_renders_inside_the_screen() {
        let r = rows(&metrics(), &all_on());
        for style in OverlayStyle::ALL {
            for size in [*OVERLAY_SIZE_RANGE.start(), 1.0, *OVERLAY_SIZE_RANGE.end()] {
                for vertical in [0.0, OVERLAY_TOP, 0.5, 1.0] {
                    let s = OverlaySettings { style, size, vertical, ..all_on() };
                    let layer = Layer::render(&r, &s).expect("renders");
                    assert!(layer.y0 + layer.h <= SIZE);
                    assert!(layer.rgba.iter().any(|p| p[3] > 0), "{style:?} draws something");
                    // Premultiplied: colour never exceeds alpha.
                    assert!(layer.rgba.iter().all(|p| p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3]));
                }
            }
        }
    }

    #[test]
    fn key_changes_with_style_size_and_position() {
        let r = rows(&metrics(), &all_on());
        let a = key(&r, &all_on());
        assert_ne!(a, key(&r, &OverlaySettings { style: OverlayStyle::Kawaii, ..all_on() }));
        assert_ne!(a, key(&r, &OverlaySettings { size: 1.3, ..all_on() }));
        assert_ne!(a, key(&r, &OverlaySettings { vertical: 0.3, ..all_on() }));
    }

    #[test]
    fn overlay_round_trips_as_444_jpeg_within_budget() {
        let layer = Layer::render(&rows(&metrics(), &all_on()), &all_on()).unwrap();

        // A smooth, colourful frame, encoded the way the importer does it.
        let mut src = image::RgbImage::new(LCD_SIZE, LCD_SIZE);
        for (x, y, p) in src.enumerate_pixels_mut() {
            let v = (x as f32 / 40.0).sin() + (y as f32 / 30.0).cos();
            *p = image::Rgb([(128.0 + 60.0 * v) as u8, (x / 2) as u8, (y / 2) as u8]);
        }
        let jpeg = encode(src.as_raw(), 25).unwrap();

        let mut q = initial_quality();
        let out = layer.apply_to_jpeg(&jpeg, &mut q).unwrap();
        assert!(out.len() <= STREAM_FRAME_BUDGET, "{} bytes", out.len());
        assert_eq!(&out[..2], &[0xFF, 0xD8]);
        assert_eq!(&out[6..13], b"JFIF\0\x01\x01");
        // SOF0: 3 components, each sampling factor 1x1 (4:4:4).
        let sof = out.windows(2).position(|w| w == [0xFF, 0xC0]).unwrap();
        assert_eq!(out[sof + 9], 3);
        for c in 0..3 {
            assert_eq!(out[sof + 11 + c * 3], 0x11);
        }
        let decoded = image::load_from_memory(&out).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (LCD_SIZE, LCD_SIZE));
    }
}
