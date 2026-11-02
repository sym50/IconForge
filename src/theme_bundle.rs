//! Portable, bounded icon themes: images and styles only, never executable paths or credentials.
use crate::{lnk::ShortcutInfo, presets::StylePreset};
use anyhow::{ensure, Context};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    io::{Cursor, Read},
    path::{Path, PathBuf},
};
const MAX_FILE: u64 = 48 * 1024 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThemeIcon {
    pub name: String,
    pub png: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Theme {
    pub format: String,
    pub version: u32,
    pub name: String,
    pub icons: Vec<ThemeIcon>,
    pub presets: Vec<StylePreset>,
}
fn name_valid(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    !name.trim().is_empty()
        && name.chars().count() <= 100
        && !name.ends_with(['.', ' '])
        && !name
            .chars()
            .any(|c| c.is_control() || r#"<>:"/\|?*"#.contains(c))
        && !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        && !(stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'))
}
fn decode(icon: &ThemeIcon) -> anyhow::Result<Vec<u8>> {
    ensure!(name_valid(&icon.name), "图标名称不安全");
    ensure!(icon.png.len() <= 2 * 1024 * 1024, "单个图标过大");
    let bytes = STANDARD.decode(&icon.png).context("图标编码无效")?;
    let mut reader = image::ImageReader::with_format(Cursor::new(&bytes), image::ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(1024);
    limits.max_image_height = Some(1024);
    limits.max_alloc = Some(16 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode().context("图标 PNG 无效或尺寸过大")?;
    ensure!(image.width() > 0 && image.height() > 0, "空图标");
    Ok(bytes)
}
impl Theme {
    pub fn validate(&self) -> anyhow::Result<Vec<Vec<u8>>> {
        ensure!(
            self.format == "IconForgeTheme" && self.version == 1,
            "不支持的主题套装格式或版本"
        );
        ensure!(
            !self.name.trim().is_empty() && self.name.chars().count() <= 64,
            "主题名称需为 1–64 个字"
        );
        ensure!(
            !self.icons.is_empty() && self.icons.len() <= 512,
            "主题需包含 1–512 个图标"
        );
        ensure!(self.presets.len() <= 100, "样式预设过多");
        let mut seen = HashSet::new();
        let mut bytes = Vec::new();
        let mut total = 0;
        for icon in &self.icons {
            ensure!(seen.insert(icon.name.to_lowercase()), "存在重名图标");
            let data = decode(icon)?;
            total += data.len();
            ensure!(total <= 32 * 1024 * 1024, "主题图像总大小过大");
            bytes.push(data);
        }
        let mut seen = HashSet::new();
        for p in &self.presets {
            p.validate()?;
            ensure!(seen.insert(p.name.trim().to_lowercase()), "存在重名预设");
        }
        Ok(bytes)
    }
}
pub fn read(path: &Path) -> anyhow::Result<Theme> {
    ensure!(
        std::fs::metadata(path)?.len() <= MAX_FILE,
        "主题文件超过 48 MiB"
    );
    let mut data = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_FILE + 1)
        .read_to_end(&mut data)?;
    ensure!(data.len() as u64 <= MAX_FILE, "主题文件过大");
    let theme: Theme = serde_json::from_slice(&data).context("主题套装文件无效")?;
    theme.validate()?;
    Ok(theme)
}
pub fn write(path: &Path, theme: &Theme) -> anyhow::Result<()> {
    ensure!(
        path.extension()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.eq_ignore_ascii_case("iftheme")),
        "请使用 .iftheme 文件名"
    );
    theme.validate()?;
    let bytes = serde_json::to_vec(theme)?;
    ensure!(bytes.len() as u64 <= MAX_FILE, "主题文件超过 48 MiB");
    crate::webdav::atomic_write(path, &bytes)
}
/// Capture what the shortcut displays now. Do not unwrap already customized icons.
pub fn capture(
    name: String,
    items: Vec<ShortcutInfo>,
    presets: Vec<StylePreset>,
) -> anyhow::Result<(Theme, Vec<String>)> {
    ensure!(
        items.len() <= 512,
        "每个主题最多导出 512 个快捷方式，请缩小目录范围"
    );
    let mut icons = Vec::new();
    let mut skipped = Vec::new();
    let mut seen = HashSet::new();
    for s in items {
        let file = if s.icon_path.is_empty() {
            &s.target
        } else {
            &s.icon_path
        };
        let index = if s.icon_path.is_empty() {
            0
        } else {
            s.icon_index
        };
        let Some(png) = crate::lnk::extract_icon_png(file, index, 256) else {
            skipped.push(s.name);
            continue;
        };
        let mut label = s.name.clone();
        let mut n = 2;
        if !name_valid(&label) {
            skipped.push(s.name);
            continue;
        }
        while !seen.insert(label.to_lowercase()) {
            label = format!("{} ({n})", s.name.chars().take(90).collect::<String>());
            n += 1;
        }
        icons.push(ThemeIcon {
            name: label,
            png: STANDARD.encode(png),
        });
    }
    let theme = Theme {
        format: "IconForgeTheme".into(),
        version: 1,
        name,
        icons,
        presets,
    };
    theme.validate()?;
    Ok((theme, skipped))
}
/// Install into an exclusively created directory. No archive-supplied paths are used.
pub fn install(theme: &Theme, root: &Path) -> anyhow::Result<PathBuf> {
    let bytes = theme.validate()?;
    std::fs::create_dir_all(root)?;
    let id = format!(
        "theme-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    );
    let dir = root.join(id);
    std::fs::create_dir(&dir)?;
    let result = (|| -> anyhow::Result<()> {
        for (icon, data) in theme.icons.iter().zip(bytes) {
            crate::webdav::atomic_write(&dir.join(format!("{}.png", icon.name)), &data)?;
        }
        crate::webdav::atomic_write(
            &dir.join("theme.json"),
            &serde_json::to_vec(
                &serde_json::json!({"format":"IconForgeThemeLibrary","name":theme.name,"presets":theme.presets,"icons":theme.icons.iter().map(|i|&i.name).collect::<Vec<_>>()}),
            )?,
        )?;
        Ok(())
    })();
    if result.is_err() {
        let canonical = dir.canonicalize()?;
        ensure!(
            canonical.parent() == Some(root.canonicalize()?.as_path()),
            "主题目录超出范围"
        );
        let _ = std::fs::remove_dir_all(&canonical);
    }
    result?;
    Ok(dir)
}
/// Keep local presets, reuse identical styles, and suffix conflicting imported names.
pub fn merge_presets(local: &mut Vec<StylePreset>, incoming: &[StylePreset]) -> anyhow::Result<()> {
    for preset in incoming {
        preset.validate()?;
        if local
            .iter()
            .any(|p| p.name.eq_ignore_ascii_case(&preset.name) && p.same_style(preset))
        {
            continue;
        }
        let mut next = preset.clone();
        let mut i = 2;
        while local
            .iter()
            .any(|p| p.name.trim().eq_ignore_ascii_case(next.name.trim()))
        {
            next.name = format!("{} ({i})", preset.name.chars().take(24).collect::<String>());
            i += 1;
        }
        next.validate()?;
        local.push(next);
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    fn sample() -> Theme {
        let image = image::RgbaImage::from_pixel(16, 16, image::Rgba([12, 24, 36, 255]));
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        Theme {
            format: "IconForgeTheme".into(),
            version: 1,
            name: "我的主题".into(),
            icons: vec![ThemeIcon {
                name: "微信".into(),
                png: STANDARD.encode(bytes.into_inner()),
            }],
            presets: vec![StylePreset::default()],
        }
    }
    fn temp() -> PathBuf {
        std::env::temp_dir().join(format!(
            "iconforge-theme-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
    #[test]
    fn portable_theme_roundtrip_and_conflicting_presets_keep_local_data() {
        let p = temp();
        std::fs::create_dir_all(&p).unwrap();
        let t = sample();
        write(&p.join("sample.iftheme"), &t).unwrap();
        let t = read(&p.join("sample.iftheme")).unwrap();
        let dir = install(&t, &p).unwrap();
        assert!(dir.join("微信.png").is_file());
        let json = std::fs::read_to_string(p.join("sample.iftheme")).unwrap();
        assert!(!json.contains("lnk_path"));
        assert!(!json.contains("password"));
        let mut local = t.presets.clone();
        let mut imported = local[0].clone();
        imported.base = "pure".into();
        merge_presets(&mut local, &[imported]).unwrap();
        assert_eq!(local.len(), 2);
        assert_eq!(local[0].base, "white");
        assert_ne!(local[0].name, local[1].name);
        std::fs::remove_dir_all(p).unwrap();
    }
    #[test]
    fn untrusted_names_images_and_versions_are_rejected_before_install() {
        let p = temp();
        let mut t = sample();
        for name in ["../outside", "C:\\escape", "CON", "a.", "a/b"] {
            t.icons[0].name = name.into();
            assert!(install(&t, &p).is_err());
            assert!(!p.exists());
        }
        t = sample();
        t.icons[0].png = STANDARD.encode(b"not a PNG");
        assert!(install(&t, &p).is_err());
        assert!(!p.exists());
        t = sample();
        t.version = 2;
        assert!(t.validate().is_err());
        t = sample();
        t.icons.push(t.icons[0].clone());
        assert!(t.validate().is_err());
    }
}
