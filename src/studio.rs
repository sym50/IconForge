//! Read-only local design and explicit atomic export. Never modifies shortcuts.
use crate::presets::StylePreset;
use image::RgbaImage;
use std::path::Path;

pub fn render(source: &RgbaImage, style: &StylePreset) -> anyhow::Result<RgbaImage> {
    style.validate()?;
    anyhow::ensure!(source.width() > 0 && source.height() > 0, "图片尺寸无效");
    let (base, _) = crate::gradient::resolve_base(&style.base, source);
    Ok(crate::composite::customize(
        source,
        &crate::composite::Options {
            base,
            glyph_scale: style.glyph_scale,
            icon_radius: style.icon_radius,
            base_size: style.base_size,
            base_exponent: style.base_exponent,
            shadow: true,
        },
    )
    .image)
}

pub fn export(master: &RgbaImage, output: &Path, source: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(
        output != source
            && !(output.exists()
                && std::fs::canonicalize(output)? == std::fs::canonicalize(source)?),
        "请另存为新文件，不能覆盖原始图片"
    );
    let extension = output
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let bytes = match extension.as_str() {
        "ico" => crate::ico::encode_ico(master)?,
        "png" => {
            let mut bytes = std::io::Cursor::new(Vec::new());
            master.write_to(&mut bytes, image::ImageFormat::Png)?;
            bytes.into_inner()
        }
        _ => anyhow::bail!("请选择 .ico 或 .png 文件名"),
    };
    crate::webdav::atomic_write(output, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn opaque_local_image_can_be_scaled_over_a_custom_base_without_changing_legacy_mode() {
        let source = RgbaImage::from_pixel(64, 64, image::Rgba([250, 10, 10, 255]));
        let style = StylePreset {
            base: "custom:#2299FF".into(),
            glyph_scale: 0.5,
            ..Default::default()
        };
        let result = render(&source, &style).unwrap();
        assert!(result.get_pixel(60, 256)[2] > 150);
        assert!(result.get_pixel(256, 256)[0] > 220);
        assert_eq!(result.get_pixel(0, 0)[3], 0);
        let old = crate::composite::composite(&source, &Default::default());
        assert_eq!(old.branch, crate::composite::Branch::MaskOnly);
        assert!(old.image.get_pixel(60, 256)[0] > 220);
    }
    #[test]
    fn exported_png_and_ico_match_the_preview_and_cannot_overwrite_source() {
        let root = std::env::temp_dir().join(format!(
            "iconforge-studio-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("source.png");
        let image = RgbaImage::from_pixel(64, 64, image::Rgba([10, 20, 200, 255]));
        image.save(&source).unwrap();
        let original = std::fs::read(&source).unwrap();
        let master = render(&image, &Default::default()).unwrap();
        let mut spec = crate::pipeline::SourceSpec {
            kind: "file_design".into(),
            path: source.to_string_lossy().into_owned(),
            artwork_url: String::new(),
            bundle_id: String::new(),
        };
        let style = StylePreset::default();
        let preview = crate::pipeline::compose_preview(
            &spec,
            "",
            &style.base,
            false,
            style.glyph_scale,
            style.icon_radius,
            style.base_size,
            style.base_exponent,
        )
        .unwrap()
        .0;
        assert_eq!(preview, master);
        spec.kind = "file".into();
        let legacy = crate::pipeline::compose_preview(
            &spec,
            "",
            &style.base,
            false,
            style.glyph_scale,
            style.icon_radius,
            style.base_size,
            style.base_exponent,
        )
        .unwrap()
        .0;
        assert_eq!(legacy, crate::composite::remask(&image));
        let png = root.join("export.png");
        let ico = root.join("export.ico");
        export(&master, &png, &source).unwrap();
        export(&master, &ico, &source).unwrap();
        assert_eq!(image::open(&png).unwrap().to_rgba8(), master);
        assert_eq!(&std::fs::read(&ico).unwrap()[..6], &[0, 0, 1, 0, 6, 0]);
        assert!(image::open(&ico).is_ok());
        assert!(export(&master, &source, &source).is_err());
        assert_eq!(std::fs::read(&source).unwrap(), original);
        let bad = root.join("bad.svg");
        assert!(export(&master, &bad, &source).is_err());
        assert!(!bad.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
