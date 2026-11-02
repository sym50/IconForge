//! Backup snapshots: the manifest that makes undo/restore exact.
//!
//! LinkEcho's restore pitfall: it points the shortcut back at the target
//! file, losing the original standalone ICO/DLL icon. We record the
//! ORIGINAL IconLocation per shortcut and restore that value.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotEntry {
    /// Shortcut path, e.g. `C:\Users\umie\Desktop\WeChat.lnk`.
    pub lnk_path: String,
    /// Shortcut target (EXE / file the shortcut points to).
    pub target_path: String,
    /// Original IconLocation path (empty = default target icon).
    pub original_icon_path: String,
    /// Original IconLocation index.
    pub original_icon_index: i32,
    /// New ICO we wrote (for cleanup / audit).
    pub new_icon_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotManifest {
    /// ISO-8601-ish timestamp, e.g. `2026-10-18T23-30-00`.
    pub created_at: String,
    pub entries: Vec<SnapshotEntry>,
}

/// Write a snapshot manifest under `snapshots_dir/{created_at}/manifest.json`.
/// Returns the manifest path.
pub fn write_snapshot(
    snapshots_dir: &Path,
    manifest: &SnapshotManifest,
) -> anyhow::Result<PathBuf> {
    let dir = snapshots_dir.join(&manifest.created_at);
    fs::create_dir_all(&dir)?;
    let path = dir.join("manifest.json");
    let json = serde_json::to_string_pretty(manifest)?;
    fs::write(&path, json)?;
    Ok(path)
}

pub fn read_snapshot(path: &Path) -> anyhow::Result<SnapshotManifest> {
    let json = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&json)?)
}

/// Copy originals before publishing the manifest. A failed backup must
/// never authorize a shortcut write. Numbered names avoid collisions
/// between identically named shortcuts in different folders.
pub fn write_verified_backup(root: &Path, manifest: &SnapshotManifest) -> anyhow::Result<PathBuf> {
    let dir = root.join(&manifest.created_at);
    fs::create_dir_all(root)?;
    fs::create_dir(&dir)?;
    for (i, entry) in manifest.entries.iter().enumerate() {
        fs::copy(&entry.lnk_path, dir.join(format!("original-{i}.lnk")))?;
    }
    write_snapshot(root, manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_copies_originals_and_refuses_missing_sources() {
        let root = std::env::temp_dir().join(format!("iconforge-backup-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let source = root.join("source.lnk");
        fs::write(&source, b"original shortcut").unwrap();
        let mut m = SnapshotManifest {
            created_at: "complete".into(),
            entries: vec![SnapshotEntry {
                lnk_path: source.to_string_lossy().into_owned(),
                target_path: String::new(),
                original_icon_path: "original.ico".into(),
                original_icon_index: 3,
                new_icon_path: "new.ico".into(),
            }],
        };
        write_verified_backup(&root, &m).unwrap();
        fs::write(&source, b"changed shortcut").unwrap();
        assert_eq!(
            fs::read(root.join("complete/original-0.lnk")).unwrap(),
            b"original shortcut"
        );
        // Existing snapshots cannot be overwritten.
        assert!(write_verified_backup(&root, &m).is_err());
        m.created_at = "incomplete".into();
        m.entries[0].lnk_path = root.join("missing.lnk").to_string_lossy().into_owned();
        assert!(write_verified_backup(&root, &m).is_err());
        assert!(!root.join("incomplete/manifest.json").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn manifest_roundtrip() {
        let m = SnapshotManifest {
            created_at: "2026-10-18T23-30-00".into(),
            entries: vec![SnapshotEntry {
                lnk_path: r"C:\Users\umie\Desktop\WeChat.lnk".into(),
                target_path: r"C:\Program Files\WeChat\WeChat.exe".into(),
                original_icon_path: r"C:\Program Files\WeChat\WeChat.exe".into(),
                original_icon_index: 0,
                new_icon_path: r"C:\Users\umie\AppData\Local\IconForge\icons\wechat.ico".into(),
            }],
        };
        let dir = std::env::temp_dir().join("iconforge-snapshot-test");
        let path = write_snapshot(&dir, &m).unwrap();
        let back = read_snapshot(&path).unwrap();
        assert_eq!(back.entries.len(), 1);
        assert_eq!(back.entries[0].original_icon_index, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
