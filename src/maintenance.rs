//! Read-only shortcut checks and conservative, explicitly selected snapshot cleanup.
use crate::{lnk, snapshot};
use anyhow::{ensure, Context};
use std::{
    collections::{HashMap, HashSet},
    path::{Component, Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TargetHealth {
    Ready,
    Missing,
    Unknown(String),
    Skipped,
}
pub fn check_target(s: &lnk::ShortcutInfo) -> TargetHealth {
    if s.kind == "broken" {
        return TargetHealth::Unknown("快捷方式无法解析".into());
    }
    if s.kind == "uwp" || s.target.is_empty() {
        return TargetHealth::Skipped;
    }
    let mut target = s.target.clone();
    // Resolve environment variables without launching the target or asking Shell to resolve it.
    while let Some(start) = target.find('%') {
        let Some(end) = target[start + 1..].find('%').map(|n| start + 1 + n) else {
            break;
        };
        let Ok(value) = std::env::var(&target[start + 1..end]) else {
            return TargetHealth::Unknown("环境变量无法解析".into());
        };
        if value.contains('%') {
            return TargetHealth::Unknown("环境变量包含嵌套引用".into());
        }
        target.replace_range(start..=end, &value);
    }
    if target.starts_with(r"\\") || target.contains("://") {
        return TargetHealth::Skipped;
    }
    let path = Path::new(&target);
    if !path.is_absolute() {
        return TargetHealth::Skipped;
    }
    #[cfg(windows)]
    {
        let bytes = target.as_bytes();
        if bytes.get(1) != Some(&b':') {
            return TargetHealth::Skipped;
        }
        let drive: Vec<u16> = format!("{}:\\", bytes[0] as char)
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let kind = unsafe {
            windows::Win32::Storage::FileSystem::GetDriveTypeW(windows::core::PCWSTR(
                drive.as_ptr(),
            ))
        };
        // Only fixed local disks: offline remote/removable destinations are not broken links.
        if kind != 3 {
            return TargetHealth::Skipped;
        }
    }
    match std::fs::metadata(path) {
        Ok(_) => TargetHealth::Ready,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => TargetHealth::Missing,
        Err(e) => TargetHealth::Unknown(format!("无法确认目标：{e}")),
    }
}
pub fn check_shortcuts(items: &[lnk::ShortcutInfo]) -> HashMap<String, TargetHealth> {
    items
        .iter()
        .map(|s| (lnk::path_key(&s.path), check_target(s)))
        .collect()
}

#[derive(Clone, Debug)]
pub struct BackupRow {
    pub id: String,
    pub bytes: u64,
    pub count: usize,
    pub protected: Option<String>,
}
#[derive(Clone, Debug, Default)]
pub struct Inventory {
    pub rows: Vec<BackupRow>,
    pub bytes: u64,
}
fn safe_metadata(path: &Path) -> anyhow::Result<std::fs::Metadata> {
    let m = std::fs::symlink_metadata(path)?;
    ensure!(!m.file_type().is_symlink(), "备份中含符号链接，停止清理");
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            m.file_attributes() & 0x400 == 0,
            "备份中含重解析点，停止清理"
        );
    }
    Ok(m)
}
fn tree_size(path: &Path) -> anyhow::Result<u64> {
    let m = safe_metadata(path)?;
    if m.is_file() {
        return Ok(m.len());
    }
    ensure!(m.is_dir(), "未知备份文件类型");
    let mut bytes = 0u64;
    for item in std::fs::read_dir(path)? {
        bytes = bytes
            .checked_add(tree_size(&item?.path())?)
            .context("备份大小溢出")?;
    }
    Ok(bytes)
}
fn child(root: &Path, id: &str) -> anyhow::Result<PathBuf> {
    let mut parts = Path::new(id).components();
    ensure!(
        matches!(parts.next(), Some(Component::Normal(_)))
            && parts.next().is_none()
            && !id.contains(['/', '\\', ':']),
        "无效快照名称"
    );
    let root = root.canonicalize()?;
    let p = root.join(id);
    ensure!(safe_metadata(&p)?.is_dir(), "快照不是目录");
    let resolved = p.canonicalize()?;
    ensure!(
        resolved.parent() == Some(root.as_path()),
        "快照超出备份目录"
    );
    Ok(resolved)
}
pub fn inventory(root: &Path) -> anyhow::Result<Inventory> {
    match safe_metadata(root) {
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(Inventory::default())
        }
        Err(e) => return Err(e),
        Ok(m) => ensure!(m.is_dir(), "备份根路径不是目录"),
    }
    let mut rows = Vec::new();
    let mut manifests = Vec::new();
    let mut incomplete = false;
    for e in std::fs::read_dir(root)? {
        let e = e?;
        if !safe_metadata(&e.path())?.is_dir() {
            continue;
        }
        let id = e
            .file_name()
            .to_str()
            .context("快照名称不是 UTF-8")?
            .to_owned();
        let p = child(root, &id)?;
        let bytes = tree_size(&p)?;
        let manifest = snapshot::read_snapshot(&p.join("manifest.json")).ok();
        incomplete |= manifest.is_none();
        rows.push(BackupRow {
            id,
            bytes,
            count: manifest.as_ref().map_or(0, |m| m.entries.len()),
            protected: None,
        });
        manifests.push(manifest);
    }
    let mut order: Vec<usize> = (0..rows.len()).collect();
    order.sort_by(|&a, &b| rows[a].id.cmp(&rows[b].id));
    let mut seen = HashSet::new();
    for &i in &order {
        if let Some(m) = &manifests[i] {
            if m.entries
                .iter()
                .any(|e| seen.insert(lnk::path_key(&e.lnk_path)))
            {
                rows[i].protected = Some("保留最早原始记录".into());
            }
            // any() short-circuits; register every entry before inspecting the next snapshot.
            for e in &m.entries {
                seen.insert(lnk::path_key(&e.lnk_path));
            }
        }
    }
    if let Some(&last) = order.last() {
        rows[last].protected = Some("保留最新快照".into());
    }
    if incomplete {
        for r in &mut rows {
            r.protected = Some("存在不完整快照，暂不清理".into());
        }
    }
    rows.sort_by(|a, b| b.id.cmp(&a.id));
    let bytes = rows.iter().map(|r| r.bytes).sum();
    Ok(Inventory { rows, bytes })
}
#[derive(Default)]
pub struct CleanupReport {
    pub removed: usize,
    pub bytes: u64,
    pub errors: Vec<String>,
}
pub fn cleanup(root: &Path, ids: &[String]) -> anyhow::Result<CleanupReport> {
    ensure!(!ids.is_empty(), "未选择备份");
    // Preflight the ENTIRE selection before deleting anything, and recalculate retention.
    let inventory = inventory(root)?;
    let mut paths = Vec::new();
    let mut seen = HashSet::new();
    for id in ids {
        ensure!(seen.insert(id), "重复快照名称");
        let row = inventory
            .rows
            .iter()
            .find(|r| &r.id == id)
            .context("快照已变化，请刷新")?;
        ensure!(row.protected.is_none(), "快照 {} 需要保留", id);
        let p = child(root, id)?;
        tree_size(&p)?;
        paths.push((p, row.bytes));
    }
    let mut result = CleanupReport::default();
    for (p, bytes) in paths {
        // Only snapshot copies are removed. Never follow manifest target/icon/lnk paths.
        match std::fs::remove_dir_all(&p) {
            Ok(()) => {
                result.removed += 1;
                result.bytes += bytes;
            }
            Err(e) => result.errors.push(format!("{}：{e}", p.display())),
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn root(label: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "iconforge-maintenance-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
    fn snap(root: &Path, id: &str, paths: &[&str]) {
        snapshot::write_snapshot(
            root,
            &snapshot::SnapshotManifest {
                created_at: id.into(),
                entries: paths
                    .iter()
                    .map(|p| snapshot::SnapshotEntry {
                        lnk_path: (*p).into(),
                        target_path: String::new(),
                        original_icon_path: String::new(),
                        original_icon_index: 0,
                        new_icon_path: String::new(),
                    })
                    .collect(),
            },
        )
        .unwrap();
        std::fs::write(root.join(id).join("original-0.lnk"), b"backup").unwrap();
    }
    #[test]
    fn cleanup_retains_originals_latest_and_unrelated_files() {
        let p = root("retention");
        snap(&p, "snap-1", &["A.lnk"]);
        snap(&p, "snap-2", &["a.lnk"]);
        snap(&p, "snap-3", &["A.lnk"]);
        let inv = inventory(&p).unwrap();
        assert_eq!(inv.rows.len(), 3);
        assert!(inv.bytes > 0);
        assert!(cleanup(&p, &["snap-2".into(), "snap-1".into()]).is_err());
        assert!(p.join("snap-2").exists());
        assert_eq!(cleanup(&p, &["snap-2".into()]).unwrap().removed, 1);
        assert!(p.join("snap-1/original-0.lnk").exists());
        assert!(p.join("snap-3").exists());
        std::fs::remove_dir_all(p).unwrap();
    }
    #[test]
    fn retains_first_record_for_every_shortcut_and_blocks_incomplete_sets() {
        let p = root("each");
        snap(&p, "snap-1", &["A.lnk"]);
        snap(&p, "snap-2", &["A.lnk", "B.lnk"]);
        snap(&p, "snap-3", &["B.lnk"]);
        assert!(cleanup(&p, &["snap-2".into()]).is_err());
        std::fs::write(p.join("snap-2/manifest.json"), b"bad").unwrap();
        assert!(inventory(&p)
            .unwrap()
            .rows
            .iter()
            .all(|r| r.protected.is_some()));
        assert!(child(&p, "../outside").is_err());
        assert!(child(&p, "snap-1/sub").is_err());
        std::fs::remove_dir_all(p).unwrap();
    }
    #[test]
    fn missing_targets_are_distinct_from_unknown_or_special_targets() {
        let p = root("target");
        let mut s = lnk::ShortcutInfo {
            path: "test.lnk".into(),
            name: "test".into(),
            target: p.join("missing.exe").to_string_lossy().into(),
            icon_path: String::new(),
            icon_index: 0,
            kind: "normal".into(),
        };
        assert_eq!(check_target(&s), TargetHealth::Missing);
        std::fs::write(&s.target, b"present").unwrap();
        assert_eq!(check_target(&s), TargetHealth::Ready);
        s.kind = "uwp".into();
        assert_eq!(check_target(&s), TargetHealth::Skipped);
        s.kind = "broken".into();
        assert!(matches!(check_target(&s), TargetHealth::Unknown(_)));
        s.kind = "normal".into();
        s.target = r"\\server\offline\app.exe".into();
        assert_eq!(check_target(&s), TargetHealth::Skipped);
        std::fs::remove_dir_all(p).unwrap();
    }
}
