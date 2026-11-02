//! Base plates: vertical gradients + optional top highlight.
//!
//! Built-in templates, all measured from real references:
//! - Blue: PSD fit, top #65A1F8 -> bottom #2E58EA, 14px white highlight.
//! - White: ChatGPT-style, top #FFFFFF -> bottom #E9E9EB, no stroke.
//!   (Plain #FFFFFF flat fill looked plasticky and was rejected as the
//!   default white — but the user asked for it as a separate option.)
//! - Pure white: flat #FFFFFF, no gradient, no highlight (user-added).

use image::{Rgba, RgbaImage};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Parse `#RRGGBB` (leading `#` optional).
    pub fn from_hex(hex: &str) -> anyhow::Result<Self> {
        let h = hex.trim_start_matches('#');
        anyhow::ensure!(h.len() == 6, "expected #RRGGBB, got {hex}");
        let v = u32::from_str_radix(h, 16)?;
        Ok(Self {
            r: ((v >> 16) & 0xff) as u8,
            g: ((v >> 8) & 0xff) as u8,
            b: (v & 0xff) as u8,
        })
    }
}

/// A base-plate template: vertical linear gradient + optional top highlight.
#[derive(Debug, Clone, Copy)]
pub struct BaseTemplate {
    pub angle_deg: f32,
    pub top: Rgb,
    pub bottom: Rgb,
    /// White highlight at the top, alpha 0.30 -> 0 over `highlight_px`
    /// (measured at 512px, scaled with canvas size).
    pub highlight_px: u32,
    pub highlight_alpha: f32,
}

impl BaseTemplate {
    /// PSD blue: #65A1F8 -> #2E58EA + 14px highlight.
    pub const BLUE: Self = Self {
        angle_deg: 0.0,
        top: Rgb::new(101, 161, 248),
        bottom: Rgb::new(46, 88, 234),
        highlight_px: 14,
        highlight_alpha: 0.30,
    };

    /// ChatGPT-style white: #FFFFFF -> #E9E9EB, no highlight, no stroke.
    pub const WHITE: Self = Self {
        angle_deg: 0.0,
        top: Rgb::new(255, 255, 255),
        bottom: Rgb::new(233, 233, 235),
        highlight_px: 0,
        highlight_alpha: 0.0,
    };

    /// Pure white: flat #FFFFFF, no gradient, no highlight.
    pub const PURE_WHITE: Self = Self {
        angle_deg: 0.0,
        top: Rgb::new(255, 255, 255),
        bottom: Rgb::new(255, 255, 255),
        highlight_px: 0,
        highlight_alpha: 0.0,
    };

    pub fn custom(top: Rgb, bottom: Rgb) -> Self {
        Self {
            angle_deg: 0.0,
            top,
            bottom,
            highlight_px: 0,
            highlight_alpha: 0.0,
        }
    }
}

/// Pick the base color from a source icon: the dominant opaque color
/// (solid pixels, alpha > 200), averaged within its quantization bucket.
///
/// Returns None when the foreground is effectively monochromatic
/// (no second color with >= 8% share and a luminance gap > 0.2): a
/// picked base would swallow the glyph whole (e.g. a single-hue logo
/// like FlClash). Callers fall back to white in that case.
pub fn pick_color(img: &RgbaImage) -> Option<Rgb> {
    use std::collections::HashMap;
    // bucket -> (count, sum_r, sum_g, sum_b)
    let mut buckets: HashMap<(u8, u8, u8), (u32, u32, u32, u32)> = HashMap::new();
    for px in img.pixels() {
        if px[3] <= 200 {
            continue;
        }
        let key = (px[0] >> 5, px[1] >> 5, px[2] >> 5);
        let e = buckets.entry(key).or_default();
        e.0 += 1;
        e.1 += px[0] as u32;
        e.2 += px[1] as u32;
        e.3 += px[2] as u32;
    }
    let total: u32 = buckets.values().map(|e| e.0).sum();
    if total == 0 {
        return None;
    }
    let mut sorted: Vec<_> = buckets.iter().collect();
    sorted.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
    let (key1, c1) = sorted[0];
    let avg = Rgb::new(
        (c1.1 / c1.0) as u8,
        (c1.2 / c1.0) as u8,
        (c1.3 / c1.0) as u8,
    );
    if sorted.len() < 2 {
        return None;
    }
    let (key2, c2) = sorted[1];
    if (c2.0 as f32) / (total as f32) < 0.08 {
        return None;
    }
    let lum = |k: &(u8, u8, u8)| -> f32 {
        let r = (k.0 * 32 + 16) as f32 / 255.0;
        let g = (k.1 * 32 + 16) as f32 / 255.0;
        let b = (k.2 * 32 + 16) as f32 / 255.0;
        0.2126 * r + 0.7152 * g + 0.0722 * b
    };
    if (lum(key1) - lum(key2)).abs() < 0.2 {
        return None;
    }
    Some(avg)
}

/// Resolve a base name against a source image.
/// "pick" -> dominant-color gradient (white fallback when monochromatic).
/// "custom:#RRGGBB" -> gradient from the given color (same treatment
/// as pick). "blue" kept as a legacy mapping for old decisions; the
/// UI no longer offers it. Returns (template, actually_picked).
pub fn resolve_base(name: &str, src: &RgbaImage) -> (BaseTemplate, bool) {
    if name == "pick" {
        match pick_color(src) {
            Some(c) => (gradient_from_color(c), true),
            None => (BaseTemplate::WHITE, false),
        }
    } else if let Some(g) = parse_linear(name) {
        (g.template(), false)
    } else if let Some(c) = parse_custom(name) {
        (gradient_from_color(c), false)
    } else {
        let t = match name {
            "blue" => BaseTemplate::BLUE,
            "pure" => BaseTemplate::PURE_WHITE,
            _ => BaseTemplate::WHITE,
        };
        (t, false)
    }
}

/// Encode a custom base color as a base name: `custom:#RRGGBB`.
pub fn custom_name(c: Rgb) -> String {
    format!("custom:#{:02X}{:02X}{:02X}", c.r, c.g, c.b)
}

/// Parse a `custom:#RRGGBB` base name.
pub fn parse_custom(name: &str) -> Option<Rgb> {
    name.strip_prefix("custom:")
        .and_then(|h| Rgb::from_hex(h).ok())
}

/// Gradient from a single color: top lightened 6% toward white,
/// bottom darkened 8% — the same treatment as the pick base.
pub fn gradient_from_color(c: Rgb) -> BaseTemplate {
    let top = Rgb::new(
        lerp(c.r, 255, 0.06).round() as u8,
        lerp(c.g, 255, 0.06).round() as u8,
        lerp(c.b, 255, 0.06).round() as u8,
    );
    let bottom = Rgb::new(
        (c.r as f32 * 0.92).round() as u8,
        (c.g as f32 * 0.92).round() as u8,
        (c.b as f32 * 0.92).round() as u8,
    );
    BaseTemplate::custom(top, bottom)
}

/// Render a base plate at `size` x `size`.
pub fn render_base(size: u32, template: &BaseTemplate) -> RgbaImage {
    let mut img = RgbaImage::new(size, size);
    let scale = size as f32 / 512.0;
    let hl = (template.highlight_px as f32 * scale).max(0.0);
    let angle = template.angle_deg.to_radians();
    let (dx, dy) = (angle.sin(), angle.cos());
    let span = dx.abs() + dy.abs();
    for y in 0..size {
        for x in 0..size {
            let t = if size <= 1 {
                0.0
            } else {
                let px = x as f32 / (size - 1) as f32 - 0.5;
                let py = y as f32 / (size - 1) as f32 - 0.5;
                ((px * dx + py * dy) / span + 0.5).clamp(0.0, 1.0)
            };
            let mut r = lerp(template.top.r, template.bottom.r, t);
            let mut g = lerp(template.top.g, template.bottom.g, t);
            let mut b = lerp(template.top.b, template.bottom.b, t);
            if hl > 0.0 && (y as f32) < hl {
                let a = template.highlight_alpha * (1.0 - y as f32 / hl);
                r = r * (1.0 - a) + 255.0 * a;
                g = g * (1.0 - a) + 255.0 * a;
                b = b * (1.0 - a) + 255.0 * a;
            }
            img.put_pixel(
                x,
                y,
                Rgba([r.round() as u8, g.round() as u8, b.round() as u8, 255]),
            );
        }
    }
    img
}

fn lerp(a: u8, b: u8, t: f32) -> f32 {
    a as f32 + (b as f32 - a as f32) * t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gradient_endpoints_match() {
        let img = render_base(512, &BaseTemplate::BLUE);
        // Top row: gradient top + full highlight (alpha 0.30 towards white).
        let top = img.get_pixel(256, 0);
        let expected = |c: u8| (c as f32 * 0.7 + 255.0 * 0.3).round() as u8;
        assert_eq!(top[0], expected(101));
        assert_eq!(top[1], expected(161));
        assert_eq!(top[2], expected(248));
        // Bottom row: pure gradient bottom, no highlight.
        let bot = img.get_pixel(256, 511);
        assert_eq!([bot[0], bot[1], bot[2]], [46, 88, 234]);
    }

    #[test]
    fn white_gradient_has_no_highlight() {
        let img = render_base(512, &BaseTemplate::WHITE);
        assert_eq!(img.get_pixel(256, 0).0, [255, 255, 255, 255]);
        assert_eq!(img.get_pixel(256, 511).0, [233, 233, 235, 255]);
    }

    #[test]
    fn pure_white_is_flat() {
        let img = render_base(512, &BaseTemplate::PURE_WHITE);
        assert_eq!(img.get_pixel(256, 0).0, [255, 255, 255, 255]);
        assert_eq!(img.get_pixel(256, 511).0, [255, 255, 255, 255]);
        let (tpl, picked) = resolve_base("pure", &img);
        assert_eq!(
            (tpl.top, tpl.bottom),
            (Rgb::new(255, 255, 255), Rgb::new(255, 255, 255))
        );
        assert!(!picked);
    }

    #[test]
    fn resolve_base_custom_hex() {
        let src = RgbaImage::new(8, 8);
        let (tpl, picked) = resolve_base("custom:#3B82F6", &src);
        assert!(!picked);
        // Same lighten/darken treatment as pick: top toward white,
        // bottom * 0.92. (#3B82F6 -> top (71,138,247), bottom (54,120,226))
        assert_eq!(tpl.top, Rgb::new(71, 138, 247));
        assert_eq!(tpl.bottom, Rgb::new(54, 120, 226));
        assert_eq!(custom_name(Rgb::new(59, 130, 246)), "custom:#3B82F6");
        assert_eq!(parse_custom("custom:#3B82F6"), Some(Rgb::new(59, 130, 246)));
        // Malformed custom names fall back to gradient white.
        assert_eq!(
            resolve_base("custom:zzz", &src).0.top,
            Rgb::new(255, 255, 255)
        );
        assert_eq!(parse_custom("custom:zzz"), None);
    }

    #[test]
    fn hex_parse() {
        assert_eq!(Rgb::from_hex("#65A1F8").unwrap(), Rgb::new(101, 161, 248));
        assert!(Rgb::from_hex("xyz").is_err());
    }

    #[test]
    fn pick_color_dark_circle_white_glyph() {
        // Bettbox-like: dark disc + white slashes -> picks the dark color.
        let mut img = RgbaImage::new(100, 100);
        for y in 0..100 {
            for x in 0..100 {
                let dx = x as i32 - 50;
                let dy = y as i32 - 50;
                if dx * dx + dy * dy < 45 * 45 {
                    img.put_pixel(x, y, Rgba([58, 69, 86, 255]));
                } else {
                    img.put_pixel(x, y, Rgba([0, 0, 0, 0]));
                }
            }
        }
        for y in 40..60 {
            for x in 20..80 {
                img.put_pixel(x, y, Rgba([255, 255, 255, 255]));
            }
        }
        let c = pick_color(&img).expect("should pick the dark disc color");
        assert!((c.r as i32 - 58).abs() <= 2);
        assert!((c.g as i32 - 69).abs() <= 2);
        assert!((c.b as i32 - 86).abs() <= 2);
    }

    #[test]
    fn pick_color_monochrome_falls_back() {
        let mut img = RgbaImage::new(64, 64);
        for px in img.pixels_mut() {
            *px = Rgba([90, 100, 240, 255]);
        }
        assert!(pick_color(&img).is_none());
    }
}

/// Portable two-stop gradient; 0 degrees is top-to-bottom, 90 left-to-right.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LinearGradient {
    pub start: Rgb,
    pub end: Rgb,
    pub angle: f32,
}
impl Default for LinearGradient {
    fn default() -> Self {
        Self {
            start: Rgb::new(101, 161, 248),
            end: Rgb::new(46, 88, 234),
            angle: 0.0,
        }
    }
}
impl LinearGradient {
    pub fn template(self) -> BaseTemplate {
        let mut t = BaseTemplate::custom(self.start, self.end);
        t.angle_deg = self.angle;
        t
    }
    pub fn name(self) -> String {
        format!(
            "linear:#{:02X}{:02X}{:02X}:#{:02X}{:02X}{:02X}:{:.1}",
            self.start.r,
            self.start.g,
            self.start.b,
            self.end.r,
            self.end.g,
            self.end.b,
            self.angle
        )
    }
}
pub fn parse_linear(name: &str) -> Option<LinearGradient> {
    let parts: Vec<_> = name.strip_prefix("linear:")?.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let angle = parts[2].parse::<f32>().ok()?;
    if !angle.is_finite() || !(0.0..=360.0).contains(&angle) {
        return None;
    }
    Some(LinearGradient {
        start: Rgb::from_hex(parts[0]).ok()?,
        end: Rgb::from_hex(parts[1]).ok()?,
        angle,
    })
}
#[cfg(test)]
mod linear_tests {
    use super::*;
    #[test]
    fn gradient_direction_and_portable_validation() {
        let mut g = LinearGradient {
            start: Rgb::new(255, 0, 0),
            end: Rgb::new(0, 0, 255),
            angle: 0.0,
        };
        let image = render_base(9, &g.template());
        assert_eq!(image.get_pixel(4, 0).0, [255, 0, 0, 255]);
        assert_eq!(image.get_pixel(4, 8).0, [0, 0, 255, 255]);
        g.angle = 90.0;
        let image = render_base(9, &g.template());
        assert_eq!(image.get_pixel(0, 4).0, [255, 0, 0, 255]);
        assert_eq!(image.get_pixel(8, 4).0, [0, 0, 255, 255]);
        assert_eq!(parse_linear(&g.name()), Some(g));
        for invalid in [
            "linear:#FF0000:#0000FF:NaN",
            "linear:#FF0000:#0000FF:361",
            "linear:#FF0000:bad:90",
        ] {
            assert!(parse_linear(invalid).is_none());
        }
        let mut style = crate::presets::StylePreset::default();
        style.base = g.name();
        assert!(style.validate().is_ok());
    }
}
