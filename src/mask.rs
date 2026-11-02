//! Superellipse mask: |x/a|^n + |y/a|^n = 1, default n = 5.2 at 512px.
//!
//! Fitted from the user's PSD base plate: superellipse MSE 0.0004 vs
//! rounded-rect (best r=115@512) MSE 4.3. Not a rounded rectangle.

/// Superellipse exponent. 4.5..6 is the usable range; 5.2 is the PSD fit.
pub const DEFAULT_EXPONENT: f32 = 5.2;

/// Canvas size for the master plate.
pub const MASTER_SIZE: u32 = 512;

/// Compute a superellipse alpha mask (0..255 per pixel) for a square canvas.
/// `supersample` = sub-samples per axis (2 => 2x2 = 4x AA).
pub fn superellipse_mask(size: u32, exponent: f32, supersample: u32) -> Vec<u8> {
    superellipse_mask_rect(size, size, exponent, supersample)
}

/// Rectangular superellipse mask: same exponent, inscribed in the
/// w x h rect (touches all four edge midpoints). Used to cut the
/// GLYPH to the same squircle curvature as the base plate, so the
/// inner glyph corners and the outer base corners are uniform
/// (a source glyph that is itself a rounded square — e.g. Adobe's
/// — would otherwise keep its own, non-Apple corner radius inside
/// the Apple squircle base).
pub fn superellipse_mask_rect(
    w: u32,
    h: u32,
    exponent: f32,
    supersample: u32,
) -> Vec<u8> {
    let mut mask = vec![0u8; (w * h) as usize];
    let hx = w as f32 / 2.0;
    let hy = h as f32 / 2.0;
    let ss = supersample.max(1) as f32;
    for y in 0..h {
        for x in 0..w {
            let mut inside = 0u32;
            for sy in 0..supersample {
                for sx in 0..supersample {
                    // Sub-pixel centre, normalized to [-1, 1] per axis.
                    let px = (x as f32 + (sx as f32 + 0.5) / ss - hx) / hx;
                    let py = (y as f32 + (sy as f32 + 0.5) / ss - hy) / hy;
                    let v = px.abs().powf(exponent) + py.abs().powf(exponent);
                    if v <= 1.0 {
                        inside += 1;
                    }
                }
            }
            let total = supersample * supersample;
            mask[(y * w + x) as usize] =
                ((inside as f32 / total as f32) * 255.0).round() as u8;
        }
    }
    mask
}

/// Apply an alpha mask to an RGBA image in place (alpha *= mask).
pub fn apply_mask(img: &mut image::RgbaImage, mask: &[u8]) {
    debug_assert_eq!(img.width() * img.height(), mask.len() as u32);
    for (px, m) in img.pixels_mut().zip(mask.iter()) {
        let a = px[3] as f32 * (*m as f32 / 255.0);
        px[3] = a.round() as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corners_are_out_center_is_in() {
        let m = superellipse_mask(512, DEFAULT_EXPONENT, 2);
        let idx = |x: u32, y: u32| m[(y * 512 + x) as usize];
        assert_eq!(idx(0, 0), 0, "top-left corner must be fully masked");
        assert_eq!(idx(511, 511), 0);
        assert_eq!(idx(256, 256), 255, "centre must be fully inside");
        // Edge midpoints sit on the boundary.
        assert_eq!(idx(256, 0), 255);
        assert_eq!(idx(0, 256), 255);
    }

    #[test]
    fn exponent_changes_shape() {
        // Higher exponent => squarer => more area covered.
        let low = superellipse_mask(64, 2.0, 1);
        let high = superellipse_mask(64, 5.2, 1);
        let covered = |m: &Vec<u8>| m.iter().filter(|&&a| a > 0).count();
        assert!(covered(&high) > covered(&low));
    }
}
