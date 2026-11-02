//! ICO output: 6 sizes, PNG frames, CatmullRom downscale.
//! Same size table as LinkEcho: 16/32/48/64/128/256.

#[cfg(test)]
use std::fs::File;
use std::path::Path;

use image::codecs::ico::{IcoEncoder, IcoFrame};
use image::{ColorType, DynamicImage, RgbaImage};

/// ICO frame sizes (no 512 frame: legacy app compatibility).
pub const ICO_SIZES: [u32; 6] = [16, 32, 48, 64, 128, 256];

/// Save a 512 master as a multi-size ICO.
pub fn save_ico(master: &RgbaImage, path: &Path) -> anyhow::Result<()> {
    crate::webdav::atomic_write(path, &encode_ico(master)?)
}

pub fn encode_ico(master: &RgbaImage) -> anyhow::Result<Vec<u8>> {
    let dyn_img = DynamicImage::ImageRgba8(master.clone());
    let mut frames = Vec::with_capacity(ICO_SIZES.len());
    for &size in &ICO_SIZES {
        let resized = image::imageops::resize(
            &dyn_img,
            size,
            size,
            image::imageops::FilterType::CatmullRom,
        );
        let frame = IcoFrame::as_png(resized.as_raw(), size, size, ColorType::Rgba8.into())?;
        frames.push(frame);
    }
    let mut bytes = Vec::new();
    IcoEncoder::new(&mut bytes).encode_images(&frames)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn ico_header_claims_six_images() {
        let img = RgbaImage::from_pixel(512, 512, image::Rgba([1, 2, 3, 255]));
        let dir = std::env::temp_dir().join("iconforge-test.ico");
        save_ico(&img, &dir).unwrap();
        let mut buf = Vec::new();
        File::open(&dir).unwrap().read_to_end(&mut buf).unwrap();
        // ICO header: reserved(0) + type(1) + count(6), little-endian.
        assert_eq!(&buf[0..6], &[0, 0, 1, 0, 6, 0]);
        assert!(buf.len() > 1000, "ICO should carry real PNG frames");
        let _ = std::fs::remove_file(&dir);
    }
}
