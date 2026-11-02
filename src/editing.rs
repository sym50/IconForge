//! Editing history and a portable, image-free local studio draft.
use crate::presets::StylePreset;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EditState {
    pub image: String,
    pub customize: bool,
    pub style: StylePreset,
}
impl Default for EditState {
    fn default() -> Self {
        Self {
            image: String::new(),
            customize: true,
            style: StylePreset::default(),
        }
    }
}
impl EditState {
    pub fn equivalent(&self, other: &Self) -> bool {
        self.image == other.image
            && self.customize == other.customize
            && self.style.same_style(&other.style)
    }
}

pub struct History {
    current: EditState,
    past: Vec<EditState>,
    future: Vec<EditState>,
    gesture: bool,
}
impl History {
    pub fn new(current: EditState) -> Self {
        Self {
            current,
            past: Vec::new(),
            future: Vec::new(),
            gesture: false,
        }
    }
    pub fn observe(&mut self, next: EditState, dragging: bool) {
        if !self.current.equivalent(&next) {
            if !self.gesture {
                self.past.push(self.current.clone());
                if self.past.len() > 100 {
                    self.past.remove(0);
                }
            }
            self.current = next;
            self.future.clear();
            self.gesture = dragging;
        }
        if !dragging {
            self.gesture = false;
        }
    }
    pub fn can_undo(&self) -> bool {
        !self.past.is_empty()
    }
    pub fn can_redo(&self) -> bool {
        !self.future.is_empty()
    }
    pub fn undo(&mut self) -> Option<EditState> {
        let previous = self.past.pop()?;
        self.future.push(self.current.clone());
        self.current = previous.clone();
        self.gesture = false;
        Some(previous)
    }
    pub fn redo(&mut self) -> Option<EditState> {
        let next = self.future.pop()?;
        self.past.push(self.current.clone());
        self.current = next.clone();
        self.gesture = false;
        Some(next)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StudioSession {
    pub state: EditState,
    pub export_directory: Option<PathBuf>,
    pub last_export: Option<PathBuf>,
    pub resume_studio: bool,
}
impl StudioSession {
    pub fn load(path: &Path) -> anyhow::Result<Option<Self>> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let session: Self = serde_json::from_slice(&bytes)?;
        session.state.style.validate()?;
        Ok(Some(session))
    }
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        self.state.style.validate()?;
        crate::webdav::atomic_write(path, &serde_json::to_vec_pretty(self)?)
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Feedback {
    Info,
    Success,
    Error,
}
pub fn feedback(message: &str) -> Feedback {
    let check = message.replace("失败 0", "").replace("失败0", "");
    if ["失败", "错误", "损坏", "无法", "中断"]
        .iter()
        .any(|word| check.contains(word))
    {
        Feedback::Error
    } else if message.starts_with("已") || message.contains("完成") {
        Feedback::Success
    } else {
        Feedback::Info
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn state(scale: f32) -> EditState {
        EditState {
            style: StylePreset {
                glyph_scale: scale,
                ..Default::default()
            },
            ..Default::default()
        }
    }
    #[test]
    fn drag_is_one_step_and_new_edits_discard_redo() {
        let mut history = History::new(state(0.76));
        history.observe(state(0.8), true);
        history.observe(state(0.9), true);
        history.observe(state(1.0), false);
        assert_eq!(history.undo().unwrap().style.glyph_scale, 0.76);
        assert!(!history.can_undo());
        assert_eq!(history.redo().unwrap().style.glyph_scale, 1.0);
        history.undo();
        history.observe(state(0.6), false);
        assert!(!history.can_redo());
        assert_eq!(history.undo().unwrap().style.glyph_scale, 0.76);
    }
    #[test]
    fn history_retains_image_mode_and_is_bounded() {
        let original = state(0.76);
        let mut next = original.clone();
        next.image = "test.png".into();
        next.customize = false;
        let mut history = History::new(original);
        history.observe(next, false);
        assert!(history.undo().unwrap().image.is_empty());
        assert_eq!(history.redo().unwrap().image, "test.png");
        for i in 0..150 {
            history.observe(state(0.5 + (i as f32 / 1000.0)), false);
        }
        let mut count = 0;
        while history.undo().is_some() {
            count += 1;
        }
        assert_eq!(count, 100);
    }
    #[test]
    fn draft_roundtrip_preserves_unicode_paths_and_never_embeds_image_bytes() {
        let root = std::env::temp_dir().join(format!(
            "iconforge-draft-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = root.join("studio-session.json");
        assert!(StudioSession::load(&path).unwrap().is_none());
        let mut session = StudioSession::default();
        session.state.image = "C:\\图片\\我的图标.png".into();
        session.resume_studio = true;
        session.export_directory = Some(PathBuf::from("D:\\图标导出"));
        session.state.style.glyph_scale = 1.1;
        session.save(&path).unwrap();
        let restored = StudioSession::load(&path).unwrap().unwrap();
        assert!(restored.state.equivalent(&session.state));
        assert_eq!(restored.export_directory, session.export_directory);
        assert!(restored.resume_studio);
        std::fs::write(&path, b"corrupt").unwrap();
        assert!(StudioSession::load(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"corrupt");
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn zero_failure_counts_are_not_error_feedback() {
        assert_eq!(feedback("应用完成：成功 1，失败 0"), Feedback::Success);
        assert_eq!(feedback("应用完成：成功 1，失败 2"), Feedback::Error);
        assert_eq!(feedback("导出失败：文件被占用"), Feedback::Error);
        assert_eq!(feedback("已导出：我的图标.png"), Feedback::Success);
        assert_eq!(feedback("正在生成预览…"), Feedback::Info);
    }
}
