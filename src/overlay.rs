//! Stats overlay drawn onto streamed frames: up to two rows of outlined text
//! near the bottom (or top) of the round 480×480 LCD.
//!
//! Row 1: CPU temp · coolant temp · GPU temp. Row 2: CPU load · GPU load · RAM.
//! Each enabled stat is drawn as a green label and a white value, with a dark
//! outline so it stays readable over any animation.

use crate::shared::{Metrics, OverlayPosition, OverlaySettings, Stat, LCD_SIZE, STREAM_FRAME_BUDGET};
use ab_glyph::{point, Font, FontRef, PxScale, ScaleFont};
use std::sync::OnceLock;

const SIZE: usize = LCD_SIZE as usize;
const LABEL_COLOR: [u8; 3] = [0, 210, 55];
const VALUE_COLOR: [u8; 3] = [255, 255, 255];
const FONT_PX: f32 = 27.0;
/// Outline thickness in pixels.
const OUTLINE: f32 = 3.0;
const OUTLINE_STRENGTH: f32 = 0.9;
const BOLD_OFFSET: f32 = 1.0;
/// Row centres (y) for the two rows, chosen so both fit inside the circle.
const ROWS_BOTTOM: [f32; 2] = [352.0, 394.0];
const ROWS_TOP: [f32; 2] = [86.0, 128.0];
/// Keep text this far inside the round edge.
const EDGE_MARGIN: f32 = 26.0;
const QUALITY_START: u8 = 60;

type Span = (String, [u8; 3]);

fn font() -> &'static FontRef<'static> {
    static FONT: OnceLock<FontRef<'static>> = OnceLock::new();
    FONT.get_or_init(|| FontRef::try_from_slice(epaint_default_fonts::HACK_REGULAR).expect("bundled font is valid"))
}

/// Text rows for the enabled stats that have a value. Empty rows are dropped.
pub fn rows(metrics: &Metrics, settings: &OverlaySettings) -> Vec<Vec<Span>> {
    Stat::ROWS
        .iter()
        .map(|row| {
            let mut spans: Vec<Span> = Vec::new();
            for &stat in row {
                let Some(value) = settings.shows(stat).then(|| stat.value(metrics)).flatten() else { continue };
                if !spans.is_empty() {
                    spans.push(("  ".into(), VALUE_COLOR));
                }
                spans.push((format!("{} ", stat.label()), LABEL_COLOR));
                spans.push((value, VALUE_COLOR));
            }
            spans
        })
        .filter(|r| !r.is_empty())
        .collect()
}

/// Cache key: the overlay only needs re-rendering when this changes.
pub fn key(rows: &[Vec<Span>], position: OverlayPosition) -> String {
    let text: Vec<String> = rows.iter().map(|r| r.iter().map(|(t, _)| t.as_str()).collect()).collect();
    format!("{position:?}|{}", text.join("\n"))
}

/// Pre-rendered overlay: per-pixel outline/fill coverage and colour for a
/// horizontal band of the frame.
pub struct Layer {
    y0: usize,
    h: usize,
    outline: Vec<u8>,
    fill: Vec<u8>,
    color: Vec<[u8; 3]>,
}

impl Layer {
    /// Render `rows` (at most two are used). None if there is nothing to draw.
    pub fn render(rows: &[Vec<Span>], position: OverlayPosition) -> Option<Self> {
        let rows = &rows[..rows.len().min(2)];
        if rows.is_empty() {
            return None;
        }
        let slots = match position {
            OverlayPosition::Bottom => ROWS_BOTTOM,
            OverlayPosition::Top => ROWS_TOP,
        };
        // One row sits in the slot nearest the edge.
        let centers: Vec<f32> = match (rows.len(), position) {
            (1, OverlayPosition::Bottom) => vec![slots[1]],
            (1, OverlayPosition::Top) => vec![slots[0]],
            _ => slots.to_vec(),
        };

        let pad = (FONT_PX + OUTLINE * 2.0) as usize;
        let y0 = (centers[0] as usize).saturating_sub(pad);
        let y1 = ((centers[centers.len() - 1] as usize) + pad).min(SIZE);
        let h = y1 - y0;
        let mut mask = vec![0f32; SIZE * h];
        let mut color = vec![[0u8; 3]; SIZE * h];

        let font = font();
        for (spans, &cy) in rows.iter().zip(&centers) {
            // Fit the row inside the circle's chord at this height.
            let r = SIZE as f32 / 2.0;
            let dy = (cy - r).abs() + FONT_PX / 2.0;
            let chord = 2.0 * (r * r - dy * dy).max(0.0).sqrt() - 2.0 * EDGE_MARGIN;
            let text_width = |scale: PxScale| -> f32 {
                let sf = font.as_scaled(scale);
                spans.iter().flat_map(|(t, _)| t.chars()).map(|c| sf.h_advance(font.glyph_id(c))).sum()
            };
            let mut scale = PxScale::from(FONT_PX);
            let w = text_width(scale);
            if w > chord && chord > 0.0 {
                scale = PxScale::from(FONT_PX * chord / w);
            }
            let sf = font.as_scaled(scale);
            let width = text_width(scale);
            let baseline = cy + (sf.ascent() + sf.descent()) / 2.0;
            let mut x = r - width / 2.0;

            for (text, rgb) in spans {
                for c in text.chars() {
                    let id = font.glyph_id(c);
                    // Faux bold: draw each glyph twice, the second time 1 px to the right.
                    for bold_dx in [0.0, BOLD_OFFSET] {
                        let glyph = id.with_scale_and_position(scale, point(x + bold_dx, baseline));
                        let Some(outlined) = font.outline_glyph(glyph) else { continue };
                        let b = outlined.px_bounds();
                        outlined.draw(|gx, gy, cov| {
                            let px = b.min.x as i64 + gx as i64;
                            let py = b.min.y as i64 + gy as i64 - y0 as i64;
                            if px < 0 || py < 0 || px >= SIZE as i64 || py >= h as i64 {
                                return;
                            }
                            let i = py as usize * SIZE + px as usize;
                            if cov > mask[i] {
                                mask[i] = cov;
                                color[i] = *rgb;
                            }
                        });
                    }
                    x += sf.h_advance(id);
                }
            }
        }

        // Outline: dilate the glyph coverage with an anti-aliased disc.
        let rad = OUTLINE.ceil() as i64;
        let mut outline = vec![0u8; SIZE * h];
        for y in 0..h as i64 {
            for x in 0..SIZE as i64 {
                let mut best = 0f32;
                for dy in -rad..=rad {
                    for dx in -rad..=rad {
                        let (sx, sy) = (x + dx, y + dy);
                        if sx < 0 || sy < 0 || sx >= SIZE as i64 || sy >= h as i64 {
                            continue;
                        }
                        let m = mask[sy as usize * SIZE + sx as usize];
                        if m == 0.0 {
                            continue;
                        }
                        let falloff = (OUTLINE + 0.5 - ((dx * dx + dy * dy) as f32).sqrt()).clamp(0.0, 1.0);
                        best = best.max(m * falloff);
                    }
                }
                outline[y as usize * SIZE + x as usize] = (best * OUTLINE_STRENGTH * 255.0).round() as u8;
            }
        }
        let fill = mask.iter().map(|&m| (m * 255.0).round() as u8).collect();
        Some(Self { y0, h, outline, fill, color })
    }

    /// Blend onto a 480×480 image buffer with `channels` bytes per pixel (3 = RGB, 4 = RGBA).
    pub fn apply(&self, buf: &mut [u8], channels: usize) {
        if buf.len() < SIZE * SIZE * channels {
            return;
        }
        for row in 0..self.h {
            for x in 0..SIZE {
                let i = row * SIZE + x;
                let (o, f) = (self.outline[i] as u32, self.fill[i] as u32);
                if o == 0 && f == 0 {
                    continue;
                }
                let p = ((self.y0 + row) * SIZE + x) * channels;
                for (c, &fg) in self.color[i].iter().enumerate() {
                    let v = buf[p + c] as u32 * (255 - o) / 255; // darken toward black
                    buf[p + c] = ((v * (255 - f) + fg as u32 * f) / 255) as u8;
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

    #[test]
    fn rows_follow_layout_and_settings() {
        let all = OverlaySettings { enabled: true, ..Default::default() };
        let r = rows(&metrics(), &all);
        let text: Vec<String> = r.iter().map(|row| row.iter().map(|(t, _)| t.as_str()).collect()).collect();
        assert_eq!(text, vec!["CPU 52°  LIQ 31.5°  GPU 45°", "CPU 23%  GPU 100%  RAM 41%"]);

        let only_loads = OverlaySettings { cpu_temp: false, coolant: false, gpu_temp: false, ..all };
        assert_eq!(rows(&metrics(), &only_loads).len(), 1);

        let none = Metrics::default();
        assert!(rows(&none, &all).is_empty());
        assert!(Layer::render(&rows(&none, &all), OverlayPosition::Bottom).is_none());
    }

    #[test]
    fn overlay_round_trips_as_444_jpeg_within_budget() {
        let all = OverlaySettings { enabled: true, ..Default::default() };
        let layer = Layer::render(&rows(&metrics(), &all), OverlayPosition::Bottom).unwrap();

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
