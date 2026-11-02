//! Compositing engine: source icon -> iOS-style 512 master.
//!
//! Two branches, decided by the source's alpha behaviour:
//! - Glyph mode (has transparency / separable foreground): extract the
//!   alpha-foreground bbox, scale it to `glyph_scale` of the canvas,
//!   centre it on a base plate, soft shadow, superellipse mask.
//! - Mask-only mode (opaque photo / full-bleed art): centre-crop to
//!   square, resize, superellipse mask. No base plate.

use image::{Rgba, RgbaImage};

use crate::gradient::{render_base, BaseTemplate};
use crate::mask::{
    apply_mask, superellipse_mask, superellipse_mask_rect, DEFAULT_EXPONENT, MASTER_SIZE,
};

/// Glyph outer box as a fraction of the canvas. YouTube Music reference
/// 0.76: measured from its official artwork (red circle bbox 61-450 on
/// a 512 canvas = 0.762, ~11.9% white margin per side). The old PSD
/// 0.58 left too much white base around the glyph.
pub const DEFAULT_GLYPH_SCALE: f32 = 0.76;

/// Default icon corner radius (fraction of glyph max side): 0 = keep
/// the source glyph's own corners.
pub const DEFAULT_ICON_RADIUS: f32 = 0.0;

/// Default base-plate size (fraction of canvas): 1.0 = fills canvas.
pub const DEFAULT_BASE_SIZE: f32 = 1.0;

/// Default base-plate superellipse exponent (5.2 = iOS squircle).
pub const DEFAULT_BASE_EXPONENT: f32 = 5.2;

/// Map a "底板圆角" slider value (0–100%: 0 = square, 100 = round)
/// to the superellipse exponent (8.0–2.0).
pub fn radius_pct_to_exponent(pct: f32) -> f32 {
    8.0 - (pct.clamp(0.0, 100.0) / 100.0) * 6.0
}

/// Inverse of `radius_pct_to_exponent`: exponent -> slider %.
pub fn exponent_to_radius_pct(exp: f32) -> f32 {
    (8.0 - exp.clamp(2.0, 8.0)) / 6.0 * 100.0
}

/// Shadow: black, alpha 0.28, +10y offset at 512 scale, blur radius 8.
pub const SHADOW_ALPHA: f32 = 0.28;
pub const SHADOW_OFFSET_Y: i32 = 10;
pub const SHADOW_BLUR_RADIUS: u32 = 8;

/// Which branch was taken (surfaced for preview / AI audit).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Branch {
    Glyph,
    MaskOnly,
}

pub struct Composited {
    pub image: RgbaImage,
    pub branch: Branch,
    /// Foreground bbox of the source, in source pixels.
    pub glyph_bbox: Option<(u32, u32, u32, u32)>,
    /// Fraction of source pixels with alpha > 10.
    pub foreground_fraction: f32,
}

pub struct Options {
    pub base: BaseTemplate,
    pub glyph_scale: f32,
    pub shadow: bool,
    /// Glyph corner radius, fraction of glyph max side (0–0.5).
    /// LinkEcho-basis "图标圆角".
    pub icon_radius: f32,
    /// Base-plate size, fraction of canvas (0.6–1.0).
    /// LinkEcho-basis "底板大小".
    pub base_size: f32,
    /// Base-plate superellipse exponent (2.0–8.0): 2 = round,
    /// 8 = near-square. LinkEcho-basis "底板圆角".
    pub base_exponent: f32,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            base: BaseTemplate::WHITE,
            glyph_scale: DEFAULT_GLYPH_SCALE,
            shadow: true,
            icon_radius: DEFAULT_ICON_RADIUS,
            base_size: DEFAULT_BASE_SIZE,
            base_exponent: DEFAULT_BASE_EXPONENT,
        }
    }
}

/// Alpha-foreground bbox (alpha > 10) of an image. None if fully transparent.
pub fn glyph_bbox(img: &RgbaImage) -> Option<(u32, u32, u32, u32)> {
    let (w, h) = img.dimensions();
    let mut min_x = w;
    let mut min_y = h;
    let mut max_x = 0u32;
    let mut max_y = 0u32;
    let mut found = false;
    for y in 0..h {
        for x in 0..w {
            if img.get_pixel(x, y)[3] > 10 {
                found = true;
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
        }
    }
    if found {
        Some((min_x, min_y, max_x + 1, max_y + 1))
    } else {
        None
    }
}

/// Fraction of pixels with alpha > 10.
pub fn foreground_fraction(img: &RgbaImage) -> f32 {
    let (w, h) = img.dimensions();
    let total = (w * h) as f32;
    if total == 0.0 {
        return 0.0;
    }
    let mut n = 0u32;
    for px in img.pixels() {
        if px[3] > 10 {
            n += 1;
        }
    }
    n as f32 / total
}

/// Compose a source image into an iOS-style 512 master.
pub fn composite(src: &RgbaImage, opts: &Options) -> Composited {
    composite_inner(src, opts, false)
}

/// Explicit local-image design: retain the source as a layer even when opaque.
pub fn customize(src: &RgbaImage, opts: &Options) -> Composited {
    composite_inner(src, opts, true)
}

fn composite_inner(src: &RgbaImage, opts: &Options, force_layer: bool) -> Composited {
    let fg = foreground_fraction(src);
    if fg > 0.995 && !force_layer {
        // Opaque: photo / full-bleed art -> mask only.
        let squared = center_crop_square(src);
        let resized = image::imageops::resize(
            &squared,
            MASTER_SIZE,
            MASTER_SIZE,
            image::imageops::FilterType::CatmullRom,
        );
        let mut out = resized;
        let mask = superellipse_mask(MASTER_SIZE, DEFAULT_EXPONENT, 2);
        apply_mask(&mut out, &mask);
        return Composited {
            image: out,
            branch: Branch::MaskOnly,
            glyph_bbox: None,
            foreground_fraction: fg,
        };
    }

    // Glyph mode. Base plate: render at base_size, cut to a
    // squircle with base_exponent, centre on a transparent canvas
    // (so a smaller plate floats inside the icon, corners rounded).
    let bs = ((MASTER_SIZE as f32 * opts.base_size).round() as u32).max(1);
    let mut base_img = render_base(bs, &opts.base);
    let base_mask = superellipse_mask(bs, opts.base_exponent, 2);
    apply_mask(&mut base_img, &base_mask);
    let mut canvas = RgbaImage::new(MASTER_SIZE, MASTER_SIZE);
    paste(
        &mut canvas,
        &base_img,
        ((MASTER_SIZE - bs) / 2) as i32,
        ((MASTER_SIZE - bs) / 2) as i32,
    );
    let bbox = glyph_bbox(src);
    if let Some((x0, y0, x1, y1)) = bbox {
        let gw = x1 - x0;
        let gh = y1 - y0;
        let glyph = crop(src, x0, y0, gw, gh);
        let target = ((MASTER_SIZE as f32 * opts.glyph_scale).round() as u32).max(1);
        let scale = target as f32 / gw.max(gh) as f32;
        let nw = ((gw as f32 * scale).round() as u32).max(1);
        let nh = ((gh as f32 * scale).round() as u32).max(1);
        let mut resized =
            image::imageops::resize(&glyph, nw, nh, image::imageops::FilterType::CatmullRom);
        // Curvature uniformity: cut the glyph itself to the same
        // superellipse (base_exponent) as the base plate, so a source
        // glyph that is a rounded square (Adobe's) doesn't keep its
        // own non-Apple corner radius inside the Apple squircle.
        // Applied before the user's extra icon-radius rounding (which
        // then rounds further on top of the squircle).
        let glyph_mask = superellipse_mask_rect(nw, nh, opts.base_exponent, 2);
        apply_mask(&mut resized, &glyph_mask);
        // LinkEcho-basis 图标圆角: round the glyph's own corners
        // (radius = fraction of glyph max side).
        if opts.icon_radius > 0.0 {
            let r = (opts.icon_radius * nw.max(nh) as f32).round() as u32;
            round_corners(&mut resized, r);
        }
        // i32 arithmetic: at glyph_scale > 1.0 the glyph is bigger
        // than the canvas (dx/dy go negative = centre-cropped by
        // paste); u32 subtraction would underflow here.
        let dx = (MASTER_SIZE as i32 - nw as i32) / 2;
        let dy = (MASTER_SIZE as i32 - nh as i32) / 2;

        if opts.shadow {
            let shadow = make_shadow(&resized);
            paste(&mut canvas, &shadow, dx, dy + SHADOW_OFFSET_Y);
        }
        paste(&mut canvas, &resized, dx, dy);
    }
    // Outer shape follows base_exponent too: at base_size 1.0 the
    // plate IS the icon outline.
    let mask = superellipse_mask(MASTER_SIZE, opts.base_exponent, 2);
    apply_mask(&mut canvas, &mask);
    Composited {
        image: canvas,
        branch: Branch::Glyph,
        glyph_bbox: bbox,
        foreground_fraction: fg,
    }
}

/// Multiply an image's alpha by a rounded-rect mask (radius `r` px,
/// 1px feather). Used for the glyph's own corners.
fn round_corners(img: &mut RgbaImage, r: u32) {
    let (w, h) = img.dimensions();
    let rf = r as f32;
    let cx = w as f32 / 2.0;
    let cy = h as f32 / 2.0;
    let hx = cx - rf;
    let hy = cy - rf;
    for y in 0..h {
        for x in 0..w {
            let px = img.get_pixel_mut(x, y);
            if px[3] == 0 {
                continue;
            }
            // Signed distance to the rounded rect (negative inside).
            let dx = (x as f32 + 0.5 - cx).abs() - hx;
            let dy = (y as f32 + 0.5 - cy).abs() - hy;
            let ax = dx.max(0.0);
            let ay = dy.max(0.0);
            let dist = (ax * ax + ay * ay).sqrt() + dx.min(0.0).max(dy.min(0.0)) - rf;
            let keep = if dist <= 0.0 {
                1.0
            } else if dist >= 1.0 {
                0.0
            } else {
                1.0 - dist
            };
            px[3] = (px[3] as f32 * keep).round() as u8;
        }
    }
}

/// Re-mask an already-iOS-style image (e.g. iTunes artwork) to our
/// superellipse. No base plate, no rescale of the glyph.
pub fn remask(src: &RgbaImage) -> RgbaImage {
    let squared = center_crop_square(src);
    let resized = image::imageops::resize(
        &squared,
        MASTER_SIZE,
        MASTER_SIZE,
        image::imageops::FilterType::CatmullRom,
    );
    let mut out = resized;
    let mask = superellipse_mask(MASTER_SIZE, DEFAULT_EXPONENT, 2);
    apply_mask(&mut out, &mask);
    out
}

fn crop(img: &RgbaImage, x: u32, y: u32, w: u32, h: u32) -> RgbaImage {
    let mut out = RgbaImage::new(w, h);
    for yy in 0..h {
        for xx in 0..w {
            out.put_pixel(xx, yy, *img.get_pixel(x + xx, y + yy));
        }
    }
    out
}

fn center_crop_square(img: &RgbaImage) -> RgbaImage {
    let (w, h) = img.dimensions();
    let side = w.min(h);
    let x = (w - side) / 2;
    let y = (h - side) / 2;
    crop(img, x, y, side, side)
}

/// Paste `src` onto `dst` at (dx, dy), clipping at the edges.
/// Straight alpha-over (src is treated as already-premultiplied by its
/// own alpha for the shadow case; glyph pixels composite normally).
fn paste(dst: &mut RgbaImage, src: &RgbaImage, dx: i32, dy: i32) {
    let (dw, dh) = dst.dimensions();
    for y in 0..src.height() {
        let ty = dy + y as i32;
        if ty < 0 || ty >= dh as i32 {
            continue;
        }
        for x in 0..src.width() {
            let tx = dx + x as i32;
            if tx < 0 || tx >= dw as i32 {
                continue;
            }
            let s = src.get_pixel(x, y);
            let sa = s[3] as f32 / 255.0;
            if sa <= 0.0 {
                continue;
            }
            let d = dst.get_pixel(tx as u32, ty as u32);
            let da = d[3] as f32 / 255.0;
            let out_a = sa + da * (1.0 - sa);
            let mut out = [0u8; 4];
            for i in 0..3 {
                let v = if out_a > 0.0 {
                    (s[i] as f32 * sa + d[i] as f32 * da * (1.0 - sa)) / out_a
                } else {
                    0.0
                };
                out[i] = v.round().clamp(0.0, 255.0) as u8;
            }
            out[3] = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
            dst.put_pixel(tx as u32, ty as u32, Rgba(out));
        }
    }
}

/// Black shadow with the glyph's alpha * SHADOW_ALPHA, box-blurred.
fn make_shadow(glyph: &RgbaImage) -> RgbaImage {
    let (w, h) = glyph.dimensions();
    let mut shadow = RgbaImage::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let a = glyph.get_pixel(x, y)[3] as f32 * SHADOW_ALPHA;
            shadow.put_pixel(x, y, Rgba([0, 0, 0, a.round() as u8]));
        }
    }
    // 3-pass box blur approximates a Gaussian.
    for _ in 0..3 {
        shadow = box_blur(&shadow, SHADOW_BLUR_RADIUS);
    }
    shadow
}

/// Separable box blur (radius in pixels, edge-clamped).
fn box_blur(img: &RgbaImage, radius: u32) -> RgbaImage {
    let (w, h) = img.dimensions();
    let r = radius as i32;
    let mut tmp = RgbaImage::new(w, h);
    let mut out = RgbaImage::new(w, h);
    // Horizontal pass.
    for y in 0..h {
        let mut acc = [0f32; 4];
        for x in 0..w {
            let xi = x as i32;
            if x == 0 {
                for k in -r..=r {
                    let px = img.get_pixel(clamp_i(xi + k, w), y);
                    for i in 0..4 {
                        acc[i] += px[i] as f32;
                    }
                }
            } else {
                let add = img.get_pixel(clamp_i(xi + r, w), y);
                let sub = img.get_pixel(clamp_i(xi - r - 1, w), y);
                for i in 0..4 {
                    acc[i] += add[i] as f32 - sub[i] as f32;
                }
            }
            let n = (2 * r + 1) as f32;
            let mut p = [0u8; 4];
            for i in 0..4 {
                p[i] = (acc[i] / n).round().clamp(0.0, 255.0) as u8;
            }
            tmp.put_pixel(x, y, Rgba(p));
        }
    }
    // Vertical pass.
    for x in 0..w {
        let mut acc = [0f32; 4];
        for y in 0..h {
            let yi = y as i32;
            if y == 0 {
                for k in -r..=r {
                    let px = tmp.get_pixel(x, clamp_i(yi + k, h));
                    for i in 0..4 {
                        acc[i] += px[i] as f32;
                    }
                }
            } else {
                let add = tmp.get_pixel(x, clamp_i(yi + r, h));
                let sub = tmp.get_pixel(x, clamp_i(yi - r - 1, h));
                for i in 0..4 {
                    acc[i] += add[i] as f32 - sub[i] as f32;
                }
            }
            let n = (2 * r + 1) as f32;
            let mut p = [0u8; 4];
            for i in 0..4 {
                p[i] = (acc[i] / n).round().clamp(0.0, 255.0) as u8;
            }
            out.put_pixel(x, y, Rgba(p));
        }
    }
    out
}

fn clamp_i(v: i32, max: u32) -> u32 {
    v.clamp(0, max as i32 - 1) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_rect(w: u32, h: u32, rect: (u32, u32, u32, u32), alpha: u8) -> RgbaImage {
        let mut img = RgbaImage::new(w, h);
        for y in rect.1..rect.3 {
            for x in rect.0..rect.2 {
                img.put_pixel(x, y, Rgba([255, 0, 0, alpha]));
            }
        }
        img
    }

    #[test]
    fn bbox_finds_rect() {
        let img = solid_rect(100, 100, (10, 20, 60, 80), 255);
        assert_eq!(glyph_bbox(&img), Some((10, 20, 60, 80)));
    }

    #[test]
    fn glyph_mode_centres_on_base() {
        let img = solid_rect(100, 100, (10, 20, 60, 80), 255);
        let out = composite(&img, &Options::default());
        assert_eq!(out.branch, Branch::Glyph);
        // Centre pixel should be the red glyph (shadow is offset +10y,
        // centre of the glyph sits at canvas centre).
        let c = out.image.get_pixel(256, 256);
        assert!(c[0] > 200, "centre should be glyph red, got {:?}", c);
        // Corner pixel must be masked out.
        assert_eq!(out.image.get_pixel(0, 0)[3], 0);
    }

    #[test]
    fn opaque_image_goes_mask_only() {
        let mut img = RgbaImage::new(64, 64);
        for px in img.pixels_mut() {
            *px = Rgba([10, 200, 90, 255]);
        }
        let out = composite(&img, &Options::default());
        assert_eq!(out.branch, Branch::MaskOnly);
        // Centre keeps the original colour (no base plate).
        let c = out.image.get_pixel(256, 256);
        assert_eq!([c[0], c[1], c[2]], [10, 200, 90]);
    }

    #[test]
    fn glyph_corners_follow_base_squircle() {
        // An opaque square glyph: without the glyph squircle mask its
        // corners would stay square (a non-Apple radius inside the
        // Apple squircle base, e.g. Adobe's rounded square).
        let mut img = RgbaImage::new(200, 200);
        for y in 50..150 {
            for x in 50..150 {
                img.put_pixel(x, y, Rgba([200, 30, 30, 255]));
            }
        }
        let out = composite(&img, &Options::default());
        assert_eq!(out.branch, Branch::Glyph);
        // Glyph centre is red.
        let c = out.image.get_pixel(256, 256);
        assert!(
            c[0] > 150 && c[1] < 100,
            "centre should be red, got {:?}",
            c
        );
        // Glyph corner (top-left of the 389px glyph at 61,61): the
        // squircle mask cuts it, so the white base shows through
        // (G channel high = white, not red). The shadow may darken it
        // slightly, so only require "not red".
        let corner = out.image.get_pixel(62, 62);
        assert!(
            corner[1] > 150,
            "corner should be base white (squircle-cut), got {:?}",
            corner
        );
        // Glyph top edge midpoint stays red (squircle touches midpoints).
        let top = out.image.get_pixel(256, 62);
        assert!(
            top[0] > 150 && top[1] < 100,
            "top midpoint should be red, got {:?}",
            top
        );
    }

    #[test]
    fn glyph_scale_above_one_clips() {
        // 120% zoom: the glyph is bigger than the canvas; paste clips
        // it (dx/dy go negative) instead of underflowing.
        let img = solid_rect(100, 100, (10, 20, 60, 80), 255);
        let mut opts = Options::default();
        opts.glyph_scale = 1.2;
        let out = composite(&img, &opts);
        assert_eq!(out.branch, Branch::Glyph);
        let c = out.image.get_pixel(256, 256);
        assert!(c[0] > 200, "centre should be glyph red, got {:?}", c);
    }

    #[test]
    fn remask_keeps_centre_colour() {
        let mut img = RgbaImage::new(64, 64);
        for px in img.pixels_mut() {
            *px = Rgba([200, 30, 30, 255]);
        }
        let out = remask(&img);
        assert_eq!(out.get_pixel(256, 256)[0], 200);
        assert_eq!(out.get_pixel(0, 0)[3], 0);
    }
}
