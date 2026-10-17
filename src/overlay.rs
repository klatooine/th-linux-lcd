//! Stats overlay drawn onto streamed frames: up to two rows of stats on the
//! round 480×480 LCD, laid out as an aligned grid, in one of several styles.
//!
//! Row 1: CPU temp · coolant temp · GPU temp. Row 2: CPU load · GPU load · RAM.
//! Rendering happens once per change of the displayed text into a `Layer`
//! (premultiplied RGBA for a horizontal band of the screen), which is then
//! blended onto each frame.
//!
//! Fonts: Hack (bundled with egui) for Terminal; Sniglet, VT323, Metal Mania
//! (SIL OFL) and Luckiest Guy (Apache 2.0) from `assets/fonts/` for Kawaii,
//! Retro, Heavy metal and Saiyan.

use crate::shared::{Metrics, OverlaySettings, OverlayStyle, Stat, LCD_SIZE, STREAM_FRAME_BUDGET};
use ab_glyph::{point, Font, FontRef, PxScale, ScaleFont};
use std::sync::OnceLock;

const SIZE: usize = LCD_SIZE as usize;
/// Text height at size 1.0.
const BASE_PX: f32 = 27.0;
/// Keep text this far inside the round edge.
const EDGE_MARGIN: f32 = 22.0;
/// The stats block stays at least this far from the top/bottom of the screen.
const SAFE_TOP: f32 = 58.0;
const QUALITY_START: u8 = 60;

// ── Fonts ─────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FontKind {
    Hack,
    Sniglet,
    Vt323,
    MetalMania,
    LuckiestGuy,
}

impl FontKind {
    fn font(self) -> &'static FontRef<'static> {
        fn load(cell: &'static OnceLock<FontRef<'static>>, bytes: &'static [u8]) -> &'static FontRef<'static> {
            cell.get_or_init(|| FontRef::try_from_slice(bytes).expect("bundled font is valid"))
        }
        static HACK: OnceLock<FontRef<'static>> = OnceLock::new();
        static SNIGLET: OnceLock<FontRef<'static>> = OnceLock::new();
        static VT323: OnceLock<FontRef<'static>> = OnceLock::new();
        static METAL: OnceLock<FontRef<'static>> = OnceLock::new();
        static LUCKIEST: OnceLock<FontRef<'static>> = OnceLock::new();
        match self {
            FontKind::Hack => load(&HACK, epaint_default_fonts::HACK_REGULAR),
            FontKind::Sniglet => load(&SNIGLET, include_bytes!("../assets/fonts/sniglet/Sniglet-ExtraBold.ttf")),
            FontKind::Vt323 => load(&VT323, include_bytes!("../assets/fonts/vt323/VT323-Regular.ttf")),
            FontKind::MetalMania => load(&METAL, include_bytes!("../assets/fonts/metalmania/MetalMania-Regular.ttf")),
            FontKind::LuckiestGuy => {
                load(&LUCKIEST, include_bytes!("../assets/fonts/luckiestguy/LuckiestGuy-Regular.ttf"))
            }
        }
    }

    /// The font that actually has `c` (falls back to Hack for missing glyphs).
    fn for_char(self, c: char) -> FontKind {
        if self.font().glyph_id(c).0 != 0 {
            self
        } else {
            FontKind::Hack
        }
    }
}

// ── Rows ──────────────────────────────────────────────────────────────────────

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
    y0: usize,
    data: Vec<f32>,
}

impl Mask {
    fn new(y0: usize, h: usize) -> Self {
        Self { h, y0, data: vec![0.0; SIZE * h] }
    }

    fn empty_like(&self) -> Self {
        Self::new(self.y0, self.h)
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

    /// Coverage of a shape given by a signed-distance function (screen
    /// coords), evaluated inside the bounding box.
    fn fill_sdf(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, sdf: impl Fn(f32, f32) -> f32) {
        for y in (y0.floor() as i64 - 1)..=(y1.ceil() as i64 + 1) {
            for x in (x0.floor() as i64 - 1)..=(x1.ceil() as i64 + 1) {
                let d = sdf(x as f32 + 0.5, y as f32 + 0.5);
                self.plot(x, y, (0.5 - d).clamp(0.0, 1.0));
            }
        }
    }

    fn union(&self, other: &Mask) -> Mask {
        let mut m = self.clone();
        for (a, b) in m.data.iter_mut().zip(&other.data) {
            *a = a.max(*b);
        }
        m
    }

    /// `self` where `other` is not (clamped difference).
    fn minus(&self, other: &Mask) -> Mask {
        let mut m = self.clone();
        for (a, b) in m.data.iter_mut().zip(&other.data) {
            *a = (*a - b).max(0.0);
        }
        m
    }

    /// `self` clipped to `other`.
    fn within(&self, other: &Mask) -> Mask {
        let mut m = self.clone();
        for (a, b) in m.data.iter_mut().zip(&other.data) {
            *a *= b;
        }
        m
    }

    /// Integer translation.
    fn shifted(&self, dx: i64, dy: i64) -> Mask {
        let mut out = self.empty_like();
        for y in 0..self.h as i64 {
            let sy = y - dy;
            if sy < 0 || sy >= self.h as i64 {
                continue;
            }
            for x in 0..SIZE as i64 {
                let sx = x - dx;
                if sx >= 0 && sx < SIZE as i64 {
                    out.data[y as usize * SIZE + x as usize] = self.data[sy as usize * SIZE + sx as usize];
                }
            }
        }
        out
    }

    /// Solid 3-D extrusion towards (dx, dy) per step, `depth` steps deep.
    fn extrude(&self, depth: i64, dx: f32, dy: f32) -> Mask {
        let mut out = self.empty_like();
        for t in 1..=depth.max(1) {
            out = out.union(&self.shifted((t as f32 * dx).round() as i64, (t as f32 * dy).round() as i64));
        }
        out
    }

    /// Grow by `radius` px with an anti-aliased disc (outlines, glow).
    fn dilate(&self, radius: f32) -> Mask {
        let r = radius.ceil() as i64;
        let offsets: Vec<(i64, i64, f32)> = (-r..=r)
            .flat_map(|dy| (-r..=r).map(move |dx| (dx, dy)))
            .filter_map(|(dx, dy)| {
                let w = (radius + 0.5 - ((dx * dx + dy * dy) as f32).sqrt()).clamp(0.0, 1.0);
                (w > 0.0).then_some((dx, dy, w))
            })
            .collect();
        let mut out = self.empty_like();
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

/// Five-pointed star centred at (cx, cy), point up; `r` outer radius, `rf`
/// inner/outer ratio (after Inigo Quilez's sdStar5).
fn sd_star5(px: f32, py: f32, cx: f32, cy: f32, r: f32, rf: f32) -> f32 {
    let (k1x, k1y) = (0.809_017_f32, -0.587_785_f32);
    let (k2x, k2y) = (-k1x, k1y);
    let (mut x, mut y) = ((px - cx).abs(), cy - py); // y up
    let d1 = (k1x * x + k1y * y).max(0.0);
    x -= 2.0 * d1 * k1x;
    y -= 2.0 * d1 * k1y;
    let d2 = (k2x * x + k2y * y).max(0.0);
    x -= 2.0 * d2 * k2x;
    y -= 2.0 * d2 * k2y;
    x = x.abs();
    y -= r;
    let (bax, bay) = (rf * -k1y, rf * k1x - 1.0);
    let t = ((x * bax + y * bay) / (bax * bax + bay * bay)).clamp(0.0, r);
    let (dx, dy) = (x - bax * t, y - bay * t);
    (dx * dx + dy * dy).sqrt() * (y * bax - x * bay).signum()
}

fn sd_round_rect(px: f32, py: f32, cx: f32, cy: f32, hw: f32, hh: f32, r: f32) -> f32 {
    let qx = (px - cx).abs() - (hw - r);
    let qy = (py - cy).abs() - (hh - r);
    (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r
}

// ── Text ──────────────────────────────────────────────────────────────────────

/// Pixel bounds of a drawn glyph: (x0, y0, x1, y1).
type GlyphBox = [f32; 4];

/// Per-letter tilt (degrees) and baseline lift (fraction of the text size)
/// for bouncy, hand-lettered titles (Saiyan style).
const BOUNCE: [(f32, f32); 6] = [(-6.0, 0.0), (4.5, -0.05), (-3.0, 0.035), (6.0, -0.03), (-4.5, 0.045), (2.5, -0.02)];

/// Bilinear sample of a coverage buffer (0 outside).
fn bilinear(buf: &[f32], w: usize, h: usize, x: f32, y: f32) -> f32 {
    let (x0, y0) = (x.floor(), y.floor());
    let (fx, fy) = (x - x0, y - y0);
    let at = |xi: f32, yi: f32| {
        if xi < 0.0 || yi < 0.0 || xi >= w as f32 || yi >= h as f32 {
            0.0
        } else {
            buf[yi as usize * w + xi as usize]
        }
    };
    let top = at(x0, y0) * (1.0 - fx) + at(x0 + 1.0, y0) * fx;
    let bottom = at(x0, y0 + 1.0) * (1.0 - fx) + at(x0 + 1.0, y0 + 1.0) * fx;
    top * (1.0 - fy) + bottom * fy
}

/// Draw a placed piece of text in the style's font; `lk.bold` lists extra
/// horizontal offsets (faux bold). Glyph bounds are appended to `boxes`.
/// In the Saiyan style letters bounce and tilt, and '°' becomes a dragon ball
/// (centre and radius appended to `balls`).
fn draw_text(mask: &mut Mask, lk: &Look, p: &Placed, boxes: &mut Vec<GlyphBox>, balls: &mut Vec<[f32; 3]>) {
    let scale = PxScale::from(p.px);
    let base = lk.font.font().as_scaled(scale);
    let baseline = p.cy + (base.ascent() + base.descent()) / 2.0;
    let mut pen = p.x;
    for (i, c) in p.text.chars().enumerate() {
        let f = lk.font.for_char(c).font();
        let id = f.glyph_id(c);
        let adv = f.as_scaled(scale).h_advance(id);
        if lk.saiyan && c == '°' {
            let r = 0.2 * p.px;
            let (bx, by) = (pen + r + 0.06 * p.px, p.cy - 0.24 * p.px);
            balls.push([bx, by, r]);
            mask.fill_sdf(bx - r, by - r, bx + r, by + r, |x, y| ((x - bx).powi(2) + (y - by).powi(2)).sqrt() - r);
            pen += adv;
            continue;
        }
        let (tilt, lift) = if lk.saiyan { BOUNCE[(i + p.text.len()) % BOUNCE.len()] } else { (0.0, 0.0) };
        for &dx in lk.bold {
            let glyph = id.with_scale_and_position(scale, point(pen + dx, baseline + lift * p.px));
            let Some(outlined) = f.outline_glyph(glyph) else { continue };
            let b = outlined.px_bounds();
            if dx == 0.0 {
                boxes.push([b.min.x, b.min.y, b.max.x, b.max.y]);
            }
            if tilt == 0.0 {
                outlined.draw(|gx, gy, cov| mask.plot(b.min.x as i64 + gx as i64, b.min.y as i64 + gy as i64, cov));
                continue;
            }
            // Rasterise upright, then rotate about the glyph centre.
            let (w, h) = ((b.width().ceil() as usize).max(1), (b.height().ceil() as usize).max(1));
            let mut buf = vec![0f32; w * h];
            outlined.draw(|gx, gy, cov| {
                let (gx, gy) = (gx as usize, gy as usize);
                if gx < w && gy < h {
                    buf[gy * w + gx] = cov;
                }
            });
            let (cx, cy) = ((b.min.x + b.max.x) / 2.0, (b.min.y + b.max.y) / 2.0);
            let (sin, cos) = tilt.to_radians().sin_cos();
            let reach = ((w * w + h * h) as f32).sqrt() / 2.0 + 1.0;
            for y in (cy - reach).floor() as i64..=(cy + reach).ceil() as i64 {
                for x in (cx - reach).floor() as i64..=(cx + reach).ceil() as i64 {
                    let (rx, ry) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
                    let sx = cos * rx + sin * ry + cx - b.min.x - 0.5;
                    let sy = -sin * rx + cos * ry + cy - b.min.y - 0.5;
                    mask.plot(x, y, bilinear(&buf, w, h, sx, sy));
                }
            }
        }
        pen += adv;
    }
}

fn text_width(fk: FontKind, text: &str, px: f32) -> f32 {
    text.chars()
        .map(|c| {
            let f = fk.for_char(c).font();
            f.as_scaled(PxScale::from(px)).h_advance(f.glyph_id(c))
        })
        .sum()
}

// ── Styles ────────────────────────────────────────────────────────────────────

/// Font and spacing per style (sizes relative to the base text height).
struct Look {
    font: FontKind,
    value_px: f32,
    label_px: f32,
    bold: &'static [f32],
    row_gap: f32,
    col_gap: f32,
    /// Retro terminal: "> " prompt before each row and a block cursor at the end.
    terminal: bool,
    /// Dashboard layout: small label centred above a big value in each column.
    stacked: bool,
    /// Bouncy letters and a dragon-ball degree sign.
    saiyan: bool,
}

fn look(style: OverlayStyle) -> Look {
    let l = |font, value_px, label_px, bold, row_gap, col_gap, terminal, stacked| Look {
        font,
        value_px,
        label_px,
        bold,
        row_gap,
        col_gap,
        terminal,
        stacked,
        saiyan: false,
    };
    match style {
        OverlayStyle::Terminal => l(FontKind::Hack, 1.0, 1.0, &[0.0, 1.0], 1.55, 0.75, false, false),
        OverlayStyle::RetroClock => l(FontKind::Vt323, 1.3, 1.3, &[0.0, 0.5], 1.25, 0.9, true, false),
        OverlayStyle::HeavyMetal => l(FontKind::MetalMania, 1.75, 1.0, &[0.0, 0.6], 0.12, 0.8, false, true),
        OverlayStyle::Kawaii => l(FontKind::Sniglet, 1.6, 0.9, &[0.0], 0.2, 0.75, false, true),
        OverlayStyle::Saiyan => {
            Look { saiyan: true, ..l(FontKind::LuckiestGuy, 1.6, 0.95, &[0.0], 0.22, 0.8, false, true) }
        }
    }
}

/// Where each piece of text goes, after grid alignment and fitting to the circle.
struct Placed {
    text: String,
    label: bool,
    x: f32,
    cy: f32,
    px: f32,
}

/// One grid row; `cy`/`height` describe its value line (used for gradients).
struct RowLayout {
    cy: f32,
    height: f32,
    x0: f32,
    x1: f32,
    pieces: Vec<Placed>,
}

/// Re-flow into fewer columns only if that makes the text at least this much bigger.
const REFLOW_GAIN: f32 = 1.15;

/// Lay out the stats. The default grid has temperatures over loads. Above the
/// default size, when the round screen would force that grid to shrink a lot,
/// the stats are re-flowed (temperatures | loads as two columns, or a single
/// column) if that shows them noticeably bigger.
fn layout(rows: &[Vec<Item>], s: &OverlaySettings, lk: &Look) -> Vec<RowLayout> {
    let mut best = layout_grid(rows, s, lk);
    // At the default size or smaller, always keep the default grid.
    if s.size <= 1.0 {
        return best.1;
    }
    let flat: Vec<Item> = rows.iter().flatten().cloned().collect();
    let mut candidates: Vec<Vec<Vec<Item>>> = Vec::new();
    if rows.len() == 2 {
        // Two columns: temperatures on the left, loads on the right.
        let n = rows.iter().map(Vec::len).max().unwrap_or(0);
        candidates.push((0..n).map(|i| rows.iter().filter_map(|r| r.get(i).cloned()).collect()).collect());
    } else if flat.len() > 2 {
        candidates.push(flat.chunks(2).map(<[Item]>::to_vec).collect());
    }
    if flat.len() > 1 {
        candidates.push(flat.chunks(1).map(<[Item]>::to_vec).collect());
    }
    for grid in candidates {
        let candidate = layout_grid(&grid, s, lk);
        if candidate.0 > best.0 * REFLOW_GAIN {
            best = candidate;
        }
    }
    best.1
}

/// Lay out one arrangement as a grid (cells in the same column line up across
/// rows), scaled to fit the circle. Returns the scale factor applied to the
/// requested size (1.0 = no shrinking) and the placed rows.
fn layout_grid(rows: &[Vec<Item>], s: &OverlaySettings, lk: &Look) -> (f32, Vec<RowLayout>) {
    let base = BASE_PX * s.size;
    let (vpx, lpx) = (base * lk.value_px, base * lk.label_px);
    let w = |t: &str, px: f32| text_width(lk.font, t, px);
    let space = w(" ", lpx);
    let prompt = if lk.terminal { w("> ", lpx) } else { 0.0 };
    let cursor = if lk.terminal { space * 1.4 } else { 0.0 };
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let cell_w = |it: &Item| {
        if lk.stacked {
            w(it.label, lpx).max(w(&it.value, vpx))
        } else {
            w(it.label, lpx) + space + w(&it.value, vpx)
        }
    };
    let col_w: Vec<f32> =
        (0..cols).map(|c| rows.iter().filter_map(|r| r.get(c)).map(cell_w).fold(0.0, f32::max)).collect();
    let gap = base * lk.col_gap;
    let total = prompt + col_w.iter().sum::<f32>() + gap * cols.saturating_sub(1) as f32 + cursor;

    // Vertical: each grid row is one line (inline) or label line + value line (stacked).
    let n = rows.len() as f32;
    let label_drop = if lk.stacked { lpx * 0.55 + vpx * 0.5 } else { 0.0 };
    let row_h = if lk.stacked { lpx + vpx } else { vpx };
    let pitch = if lk.stacked { row_h + vpx * lk.row_gap } else { vpx * lk.row_gap };
    let block_h = pitch * (n - 1.0) + row_h;

    // One scale for the whole grid so columns stay aligned. The width allowed
    // depends on where the rows end up, which depends on the scale, so iterate.
    let r = SIZE as f32 / 2.0;
    let fit_h = (SIZE as f32 - 2.0 * SAFE_TOP) / block_h;
    let mut k = 1.0f32;
    let mut centers = Vec::new();
    for _ in 0..4 {
        let block = block_h * k;
        // Keep the block away from the round edge, where the circle gets too narrow.
        let (min_c, max_c) = (SAFE_TOP + block / 2.0, SIZE as f32 - SAFE_TOP - block / 2.0);
        let vy = (s.vertical * SIZE as f32).clamp(min_c.min(max_c), max_c.max(min_c));
        let first = vy - pitch * k * (n - 1.0) / 2.0;
        // Value-line centres (stacked: the value sits below its label).
        centers = (0..rows.len()).map(|i| first + i as f32 * pitch * k + label_drop * k / 2.0).collect();
        let fit_w = centers
            .iter()
            .map(|&cy| {
                let dy = (cy - r).abs() + vpx * k / 2.0;
                let chord = 2.0 * (r * r - dy * dy).max(0.0).sqrt() - 2.0 * EDGE_MARGIN;
                (chord / total).max(0.05)
            })
            .fold(1.0, f32::min);
        k = fit_w.min(fit_h).min(1.0);
    }

    let x_start = r - total * k / 2.0;
    let laid = rows
        .iter()
        .zip(&centers)
        .map(|(items, &cy)| {
            let mut pieces = Vec::new();
            let mut x = x_start;
            if lk.terminal {
                pieces.push(Placed { text: ">".into(), label: true, x, cy, px: lpx * k });
                x += prompt * k;
            }
            for (c, it) in items.iter().enumerate() {
                let cell_x = x + (col_w[..c].iter().sum::<f32>() + gap * c as f32) * k;
                if lk.stacked {
                    let centre = |t: &str, px: f32| cell_x + (col_w[c] - w(t, px)) * k / 2.0;
                    let label_cy = cy - label_drop * k;
                    pieces.push(Placed {
                        text: it.label.into(),
                        label: true,
                        x: centre(it.label, lpx),
                        cy: label_cy,
                        px: lpx * k,
                    });
                    pieces.push(Placed {
                        text: it.value.clone(),
                        label: false,
                        x: centre(&it.value, vpx),
                        cy,
                        px: vpx * k,
                    });
                } else {
                    pieces.push(Placed { text: it.label.into(), label: true, x: cell_x, cy, px: lpx * k });
                    let vx = cell_x + (w(it.label, lpx) + space) * k;
                    pieces.push(Placed { text: it.value.clone(), label: false, x: vx, cy, px: vpx * k });
                }
            }
            RowLayout { cy, height: vpx * k, x0: x_start, x1: x_start + total * k, pieces }
        })
        .collect();
    (k, laid)
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

fn hex(c: u32) -> [f32; 3] {
    [((c >> 16) & 0xff) as f32 / 255.0, ((c >> 8) & 0xff) as f32 / 255.0, (c & 0xff) as f32 / 255.0]
}

/// Piecewise-linear colour ramp; `stops` sorted by position in 0..1.
fn ramp(stops: &[(f32, u32)], t: f32) -> [f32; 3] {
    let t = t.clamp(0.0, 1.0);
    for w in stops.windows(2) {
        let ((t0, c0), (t1, c1)) = ((w[0].0, hex(w[0].1)), (w[1].0, hex(w[1].1)));
        if t <= t1 {
            let f = if t1 > t0 { (t - t0) / (t1 - t0) } else { 0.0 };
            return [c0[0] + (c1[0] - c0[0]) * f, c0[1] + (c1[1] - c0[1]) * f, c0[2] + (c1[2] - c0[2]) * f];
        }
    }
    stops.last().map_or([1.0; 3], |s| hex(s.1))
}

/// Pre-rendered overlay for a horizontal band of the frame (premultiplied RGBA).
#[derive(Clone)]
pub struct Layer {
    x0: usize,
    y0: usize,
    w: usize,
    h: usize,
    rgba: Vec<[u8; 4]>,
}

impl Layer {
    /// Render the overlay. None if there is nothing to draw.
    pub fn render(rows: &[Vec<Item>], settings: &OverlaySettings) -> Option<Self> {
        let s = settings.sanitized();
        if rows.is_empty() {
            return None;
        }
        let lk = look(s.style);
        let laid = layout(rows, &s, &lk);
        let k = s.size;

        let pad = BASE_PX * k * 1.2 + 16.0 * k;
        let pieces = laid.iter().flat_map(|r| &r.pieces);
        let top = pieces.clone().map(|p| p.cy - p.px / 2.0).fold(f32::MAX, f32::min) - pad;
        let bottom = pieces.map(|p| p.cy + p.px / 2.0).fold(f32::MIN, f32::max) + pad;
        let y0 = top.max(0.0) as usize;
        let h = (bottom.min(SIZE as f32) as usize).saturating_sub(y0).max(1);

        let mut labels = Mask::new(y0, h);
        let mut values = Mask::new(y0, h);
        let mut boxes: Vec<GlyphBox> = Vec::new();
        let mut balls: Vec<[f32; 3]> = Vec::new();
        for row in &laid {
            for p in &row.pieces {
                let m = if p.label { &mut labels } else { &mut values };
                draw_text(m, &lk, p, &mut boxes, &mut balls);
            }
        }
        let text = labels.union(&values);

        let mut canvas = Canvas { y0, h, px: vec![[0.0; 4]; SIZE * h] };
        // Row index and row-relative position (0 = top of text, 1 = bottom) for gradients.
        let nearest_row = |y: usize| -> usize {
            (0..laid.len())
                .min_by(|&a, &b| (laid[a].cy - y as f32).abs().total_cmp(&(laid[b].cy - y as f32).abs()))
                .unwrap_or(0)
        };
        // Position within the nearest text line (label or value), 0 = top of the
        // letters, 1 = bottom, so every line gets the full gradient.
        let lines: Vec<(f32, f32)> = laid.iter().flat_map(|r| r.pieces.iter().map(|p| (p.cy, p.px))).collect();
        let line_t = |y: usize| -> f32 {
            let (cy, px) = lines
                .iter()
                .copied()
                .min_by(|a, b| (a.0 - y as f32).abs().total_cmp(&(b.0 - y as f32).abs()))
                .unwrap_or((y as f32, 1.0));
            (y as f32 - (cy - px * 0.42)) / (px * 0.84).max(1.0)
        };

        match s.style {
            OverlayStyle::Terminal => {
                canvas.over(&text.dilate(3.0 * k), 0.9, |_, _| [0.0; 3]);
                canvas.over(&labels, 1.0, |_, _| hex(0x00d237));
                canvas.over(&values, 1.0, |_, _| hex(0xffffff));
            }

            OverlayStyle::RetroClock => {
                // Amber CRT: dark rounded screen with vignette, scanlines and phosphor glow.
                let (x0, x1) = (laid[0].x0 - 16.0 * k, laid[0].x1 + 14.0 * k);
                let py0 = laid.iter().map(|r| r.cy - r.height / 2.0).fold(f32::MAX, f32::min) - 9.0 * k;
                let py1 = laid.iter().map(|r| r.cy + r.height / 2.0).fold(f32::MIN, f32::max) + 9.0 * k;
                let (cx, cy, hw, hh, rad) =
                    ((x0 + x1) / 2.0, (py0 + py1) / 2.0, (x1 - x0) / 2.0, (py1 - py0) / 2.0, 12.0 * k);
                let mut screen = Mask::new(y0, h);
                screen.fill_sdf(x0, py0, x1, py1, |px, py| sd_round_rect(px, py, cx, cy, hw, hh, rad));
                let mut bezel = Mask::new(y0, h);
                bezel.fill_sdf(x0 - 4.0, py0 - 4.0, x1 + 4.0, py1 + 4.0, |px, py| {
                    (sd_round_rect(px, py, cx, cy, hw, hh, rad) - 1.0).abs() - 1.6 * k
                });
                canvas.over(&screen, 0.94, |x, y| {
                    let d = (((x as f32 - cx) / hw).powi(2) + ((y as f32 - cy) / hh).powi(2)).sqrt();
                    ramp(&[(0.0, 0x241706), (0.7, 0x150d03), (1.0, 0x070401)], d)
                });
                canvas.over(&bezel, 1.0, |_, _| hex(0x4a3414));

                // Block cursor after the last value.
                let last = laid.last().unwrap();
                let end = last.pieces.last().map_or(last.x0, |p| p.x + text_width(lk.font, &p.text, p.px));
                let (cw, ch) = (last.height * 0.42, last.height * 0.62);
                let (ccx, ccy) = (end + last.height * 0.3 + cw / 2.0, last.cy + last.height * 0.04);
                let mut cursor = Mask::new(y0, h);
                cursor.fill_sdf(ccx - cw, ccy - ch, ccx + cw, ccy + ch, |px, py| {
                    sd_round_rect(px, py, ccx, ccy, cw / 2.0, ch / 2.0, 1.0)
                });

                let glyphs = text.union(&cursor);
                canvas.over(&glyphs.dilate(5.0 * k), 0.12, |_, _| hex(0xff8c00));
                canvas.over(&glyphs.dilate(2.0 * k), 0.28, |_, _| hex(0xff9d00));
                canvas.over(&labels, 1.0, |_, _| hex(0xd38b12));
                canvas.over(&values, 1.0, |_, _| hex(0xffc23a));
                canvas.over(&cursor, 1.0, |_, _| hex(0xffb31a));

                // Scanlines across the screen, text included.
                let mut lines = screen.clone();
                for (i, v) in lines.data.iter_mut().enumerate() {
                    if !(y0 + i / SIZE).is_multiple_of(3) {
                        *v = 0.0;
                    }
                }
                canvas.over(&lines, 0.35, |_, _| hex(0x000000));
            }

            OverlayStyle::HeavyMetal => {
                // Polished blue steel: drop shadow, extruded side face, chrome face, bevel.
                let depth = (4.0 * k).round() as i64;
                let side = text.extrude(depth, 0.55, 1.0).minus(&text);
                let solid = text.union(&side);
                let mut shadow = solid.shifted((2.0 * k) as i64, (4.0 * k) as i64).dilate(2.5 * k);
                shadow = shadow.minus(&solid);
                canvas.over(&shadow, 0.55, |_, _| hex(0x000000));
                canvas.over(&solid.dilate(1.2 * k), 0.9, |_, _| hex(0x0b0f16)); // thin border
                canvas.over(&side, 1.0, |_, y| ramp(&[(0.0, 0x3c4a5c), (1.0, 0x151b24)], line_t(y)));
                let chrome = [
                    (0.0, 0xffffff),
                    (0.28, 0xe3edf8),
                    (0.47, 0x9fb4ca),
                    (0.5, 0x2a3644),
                    (0.62, 0x4f6680),
                    (0.85, 0x9eb5cc),
                    (1.0, 0xdbe6f2),
                ];
                canvas.over(&text, 1.0, |_, y| ramp(&chrome, line_t(y)));
                // Bevel: light on top-left edges, dark on bottom-right edges.
                canvas.over(&text.minus(&text.shifted(1, 1)), 0.75, |_, _| hex(0xffffff));
                canvas.over(&text.minus(&text.shifted(-1, -1)), 0.6, |_, _| hex(0x10161f));
            }

            OverlayStyle::Kawaii => {
                // Bubble letters: row 1 bubble-gum pink, row 2 baby blue; 3-D shade,
                // gloss highlights, white sticker outline and a soft pink outer edge.
                let depth = (3.0 * k).round() as i64;
                let shade = text.extrude(depth, 0.6, 1.0).minus(&text);
                let solid = text.union(&shade);
                canvas.over(&solid.dilate(6.0 * k), 0.95, |_, _| hex(0xffc4e5));
                canvas.over(&solid.dilate(3.5 * k), 1.0, |_, _| hex(0xffffff));
                let pink = [(0.0, 0xffa8dc), (0.45, 0xff7cc8), (1.0, 0xff4fb0)];
                let blue = [(0.0, 0xa8e2ff), (0.45, 0x74c6f6), (1.0, 0x4aa2e8)];
                let row_of = |y: usize| nearest_row(y);
                canvas.over(&shade, 1.0, |_, y| if row_of(y) % 2 == 0 { hex(0xc8338a) } else { hex(0x2f6fc4) });
                canvas.over(&text, 1.0, |_, y| ramp(if row_of(y) % 2 == 0 { &pink } else { &blue }, line_t(y)));
                // Glossy highlight on each glyph, kept inside the letter.
                let mut gloss = Mask::new(y0, h);
                for &[gx0, gy0, gx1, gy1] in &boxes {
                    let (w, gh) = (gx1 - gx0, gy1 - gy0);
                    if w < 4.0 || gh < 6.0 {
                        continue;
                    }
                    let (ax, ay, bx, by, r) =
                        (gx0 + 0.3 * w, gy0 + 0.2 * gh, gx0 + 0.55 * w, gy0 + 0.15 * gh, 0.06 * gh);
                    gloss.fill_sdf(ax - r, by - r, bx + r, ay + r, |px, py| sd_capsule(px, py, ax, ay, bx, by, r));
                    let (dx, dy, dr) = (gx0 + 0.2 * w, gy0 + 0.33 * gh, 0.045 * gh);
                    gloss.fill_sdf(dx - dr, dy - dr, dx + dr, dy + dr, |px, py| {
                        ((px - dx).powi(2) + (py - dy).powi(2)).sqrt() - dr
                    });
                }
                canvas.over(&gloss.within(&text), 0.85, |_, _| hex(0xffffff));
            }

            OverlayStyle::Saiyan => {
                // Anime title lettering: drop shadow, thick black outline, gold values,
                // orange-red labels, and dragon balls for degree signs.
                let outline = text.dilate(3.2 * k);
                let shadow = text.shifted((2.5 * k) as i64, (3.5 * k) as i64).dilate(3.5 * k).minus(&outline);
                canvas.over(&shadow, 0.5, |_, _| hex(0x000000));
                canvas.over(&outline, 1.0, |_, _| hex(0x140800));
                let gold = [(0.0, 0xfff36b), (0.35, 0xffd21a), (0.75, 0xff9a00), (1.0, 0xff6a00)];
                let red = [(0.0, 0xffb43a), (0.45, 0xff5a1f), (1.0, 0xc4120f)];
                canvas.over(&labels, 1.0, |_, y| ramp(&red, line_t(y)));
                canvas.over(&values, 1.0, |_, y| ramp(&gold, line_t(y)));
                for &[bx, by, r] in &balls {
                    let mut sphere = Mask::new(y0, h);
                    sphere.fill_sdf(bx - r, by - r, bx + r, by + r, |x, y| {
                        ((x - bx).powi(2) + (y - by).powi(2)).sqrt() - r
                    });
                    // Lit from the upper left.
                    let (hx, hy) = (bx - 0.35 * r, by - 0.4 * r);
                    let orb = [(0.0, 0xfff3c4), (0.35, 0xffc23d), (0.75, 0xff8c0a), (1.0, 0xd9570a)];
                    canvas.over(&sphere, 1.0, |x, y| {
                        let d = ((x as f32 + 0.5 - hx).powi(2) + (y as f32 + 0.5 - hy).powi(2)).sqrt();
                        ramp(&orb, d / (1.5 * r))
                    });
                    let mut star = Mask::new(y0, h);
                    star.fill_sdf(bx - r, by - r, bx + r, by + r, |x, y| {
                        sd_star5(x, y, bx, by + 0.03 * r, 0.52 * r, 0.42)
                    });
                    canvas.over(&star, 1.0, |_, _| hex(0xd6181e));
                    let mut shine = Mask::new(y0, h);
                    let (sx, sy, sr) = (bx - 0.42 * r, by - 0.45 * r, 0.17 * r);
                    shine.fill_sdf(sx - sr, sy - sr, sx + sr, sy + sr, |x, y| {
                        ((x - sx).powi(2) + (y - sy).powi(2)).sqrt() - sr
                    });
                    canvas.over(&shine, 0.9, |_, _| hex(0xffffff));
                }
            }
        }

        let rgba =
            canvas.px.iter().map(|p| [0, 1, 2, 3].map(|c| (p[c].clamp(0.0, 1.0) * 255.0).round() as u8)).collect();
        Some(Self { x0: 0, y0: canvas.y0, w: SIZE, h: canvas.h, rgba })
    }

    /// Blend onto a 480×480 image buffer with `channels` bytes per pixel (3 = RGB, 4 = RGBA).
    pub fn apply(&self, buf: &mut [u8], channels: usize) {
        if buf.len() < SIZE * SIZE * channels {
            return;
        }
        for row in 0..self.h {
            for col in 0..self.w {
                let [r, g, b, a] = self.rgba[row * self.w + col];
                if a == 0 {
                    continue;
                }
                let p = ((self.y0 + row) * SIZE + self.x0 + col) * channels;
                let inv = 255 - a as u32;
                for (c, fg) in [r, g, b].into_iter().enumerate() {
                    buf[p + c] = (fg as u32 + buf[p + c] as u32 * inv / 255).min(255) as u8;
                }
            }
        }
    }
}

/// Prepare a streamed frame: decode the 480×480 JPEG, draw the overlay
/// (upright), rotate the whole frame clockwise by `rotation` to match how the
/// LCD is mounted, and re-encode as a 4:4:4 JPEG within the streaming size
/// budget. `quality` carries the last good quality between frames so most
/// frames encode only once.
pub fn compose(jpeg: &[u8], layer: Option<&Layer>, rotation: i32, quality: &mut u8) -> Result<Vec<u8>, String> {
    let img = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg).map_err(|e| e.to_string())?;
    if img.width() != LCD_SIZE || img.height() != LCD_SIZE {
        return Err(format!("frame is {}×{}, expected {LCD_SIZE}×{LCD_SIZE}", img.width(), img.height()));
    }
    let mut rgb = img.into_rgb8();
    if let Some(layer) = layer {
        layer.apply(&mut rgb, 3);
    }
    let rgb = crate::shared::rotate_cw(image::DynamicImage::ImageRgb8(rgb), rotation).into_rgb8().into_raw();

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

/// Starting JPEG quality for `compose`.
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
    fn bundled_fonts_load_and_cover_stats() {
        for fk in [FontKind::Hack, FontKind::Sniglet, FontKind::Vt323, FontKind::MetalMania, FontKind::LuckiestGuy] {
            for c in "0123456789.%CPULIQGRAM> ".chars() {
                assert_ne!(fk.font().glyph_id(c).0, 0, "{fk:?} lacks {c:?}");
            }
            // '°' may come from the Hack fallback, but must resolve somewhere.
            assert_ne!(fk.for_char('°').font().glyph_id('°').0, 0);
        }
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
    fn columns_line_up_across_rows() {
        let s = all_on();
        let laid = layout(&rows(&metrics(), &s), &s, &look(s.style));
        let label_x = |row: usize| -> Vec<f32> { laid[row].pieces.iter().filter(|p| p.label).map(|p| p.x).collect() };
        assert_eq!(label_x(0), label_x(1));
    }

    #[test]
    fn every_style_size_and_position_renders_inside_the_screen() {
        let r = rows(&metrics(), &all_on());
        for style in OverlayStyle::ALL {
            for size in [*OVERLAY_SIZE_RANGE.start(), 1.0, *OVERLAY_SIZE_RANGE.end()] {
                for vertical in [0.0, OVERLAY_TOP, 0.5, 1.0] {
                    let s = OverlaySettings { style, size, vertical, ..all_on() };
                    let layer = Layer::render(&r, &s).expect("renders");
                    assert!(layer.y0 + layer.h <= SIZE && layer.x0 + layer.w <= SIZE);
                    assert!(layer.rgba.iter().any(|p| p[3] > 0), "{style:?} draws something");
                    // Premultiplied: colour never exceeds alpha.
                    assert!(layer.rgba.iter().all(|p| p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3]));
                }
            }
        }
    }

    #[test]
    fn compose_rotates_frame_and_overlay_together() {
        // White square in the top-left corner of a black frame, stats at the bottom.
        let mut src = image::RgbImage::new(LCD_SIZE, LCD_SIZE);
        for y in 0..60 {
            for x in 0..60 {
                src.put_pixel(x, y, image::Rgb([255, 255, 255]));
            }
        }
        let jpeg = encode(src.as_raw(), 80).unwrap();
        let layer = Layer::render(&rows(&metrics(), &all_on()), &all_on()).unwrap();
        let mut q = initial_quality();
        let out = image::load_from_memory(&compose(&jpeg, Some(&layer), 90, &mut q).unwrap()).unwrap().to_rgb8();
        let bright = |x: u32, y: u32| out.get_pixel(x, y).0.iter().map(|&c| c as u32).sum::<u32>() > 600;
        // Clockwise 90°: the top-left square goes to the top-right corner.
        assert!(bright(450, 30) && !bright(30, 30));
        // The stats (drawn at the bottom) go to the left edge: count coloured pixels per half.
        let coloured = |x: u32, y: u32| {
            let [r, g, b] = out.get_pixel(x, y).0;
            r.max(g).max(b) - r.min(g).min(b) > 80
        };
        let left =
            (0..LCD_SIZE / 2).flat_map(|x| (0..LCD_SIZE).map(move |y| (x, y))).filter(|&(x, y)| coloured(x, y)).count();
        let right = (LCD_SIZE / 2..LCD_SIZE)
            .flat_map(|x| (0..LCD_SIZE).map(move |y| (x, y)))
            .filter(|&(x, y)| coloured(x, y))
            .count();
        assert!(left > 10 * right.max(1), "stats on the left: {left} vs {right}");
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
        let out = compose(&jpeg, Some(&layer), 0, &mut q).unwrap();
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
