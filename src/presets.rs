//! Portable named compositing settings. No source paths or credentials.
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StylePreset {
    pub name: String,
    pub base: String,
    pub glyph_scale: f32,
    pub icon_radius: f32,
    pub base_size: f32,
    pub base_exponent: f32,
}
impl StylePreset {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.name.trim().is_empty() && self.name.trim().chars().count() <= 32,
            "预设名称需为 1–32 个字"
        );
        anyhow::ensure!(
            matches!(self.base.as_str(), "white" | "pure" | "pick")
                || crate::gradient::parse_custom(&self.base).is_some()
                || crate::gradient::parse_linear(&self.base).is_some(),
            "底板颜色无效"
        );
        for (value, min, max) in [
            (self.glyph_scale, 0.5, 1.2),
            (self.icon_radius, 0.0, 0.5),
            (self.base_size, 0.6, 1.0),
            (self.base_exponent, 2.0, 8.0),
        ] {
            anyhow::ensure!(
                value.is_finite() && (min..=max).contains(&value),
                "预设参数超出范围"
            );
        }
        Ok(())
    }
    pub fn same_style(&self, other: &Self) -> bool {
        self.base.eq_ignore_ascii_case(&other.base)
            && [
                (self.glyph_scale, other.glyph_scale),
                (self.icon_radius, other.icon_radius),
                (self.base_size, other.base_size),
                (self.base_exponent, other.base_exponent),
            ]
            .iter()
            .all(|(a, b)| (a - b).abs() < 0.0001)
    }
}
impl Default for StylePreset {
    fn default() -> Self {
        Self {
            name: "默认".into(),
            base: "white".into(),
            glyph_scale: crate::composite::DEFAULT_GLYPH_SCALE,
            icon_radius: crate::composite::DEFAULT_ICON_RADIUS,
            base_size: crate::composite::DEFAULT_BASE_SIZE,
            base_exponent: crate::composite::DEFAULT_BASE_EXPONENT,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_invalid_presets_before_persisting() {
        let mut p = StylePreset::default();
        assert!(p.validate().is_ok());
        p.glyph_scale = f32::NAN;
        assert!(p.validate().is_err());
        p = StylePreset::default();
        p.base = "custom:#oops".into();
        assert!(p.validate().is_err());
        p = StylePreset::default();
        p.name = " ".into();
        assert!(p.validate().is_err());
        p = StylePreset::default();
        p.base_exponent = 12.0;
        assert!(p.validate().is_err());
    }
    #[test]
    fn presets_roundtrip_inside_configuration_and_keep_old_config_compatible() {
        let old: crate::server::AppConfig = serde_json::from_str(r#"{"dark_mode":true}"#).unwrap();
        assert!(old.style_presets.is_empty());
        let mut cfg = old;
        let mut p = StylePreset::default();
        p.name = "我的蓝底".into();
        p.base = "custom:#2299FF".into();
        p.glyph_scale = 1.1;
        cfg.style_presets.push(p.clone());
        let restored: crate::server::AppConfig =
            serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(restored.style_presets[0], p);
    }
}
