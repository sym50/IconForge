//! Shared pipeline used by both the Web UI (`server.rs`) and the native
//! egui window (`gui.rs`): source loading, compositing, apply + snapshot,
//! restore. One implementation, two frontends.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::composite::{self, Options};
use crate::gradient::BaseTemplate;
use crate::ico;
use crate::itunes;
use crate::lnk::{self, ShortcutInfo};
use crate::snapshot::{self, SnapshotEntry, SnapshotManifest};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceSpec {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub artwork_url: String,
    #[serde(default)]
    pub bundle_id: String,
    #[serde(default)]
    pub path: String,
}

#[derive(Debug, Clone)]
pub struct ApplyItem {
    pub lnk_path: String,
    pub source: SourceSpec,
    pub base: String,
    /// Glyph scale (0.5–1.2): crop-to-zoom. The glyph is cropped to
    /// its opaque bbox, then scaled so the bbox fills this fraction
    /// of the 512 canvas. Rounded-corner icons look a ring smaller
    /// than full-bleed ones at the same scale, so the slider lets the
    /// user zoom them up to a suitable size.
    pub glyph_scale: f32,
    /// Glyph corner radius, fraction of glyph max side (0–0.5).
    /// LinkEcho-basis "图标圆角". Glyph branch only.
    pub icon_radius: f32,
    /// Base-plate size, fraction of canvas (0.6–1.0).
    /// LinkEcho-basis "底板大小". Glyph branch only.
    pub base_size: f32,
    /// Base-plate superellipse exponent (2.0–8.0).
    /// LinkEcho-basis "底板圆角". Glyph branch only.
    pub base_exponent: f32,
}

#[derive(Debug, Clone)]
pub struct ApplyResult {
    pub lnk_path: String,
    pub ok: bool,
    pub error: Option<String>,
    pub icon_path: Option<String>,
    /// The write failed with access denied: the .lnk lives in a protected
    /// location (e.g. Public Desktop) and needs an elevated process.
    pub needs_admin: bool,
}

/// True when `e` is an access-denied failure (HRESULT 0x80070005 or a
/// localized message). Used to offer the "restart as admin" path.
pub(crate) fn is_access_denied(e: &anyhow::Error) -> bool {
    #[cfg(windows)]
    {
        if let Some(we) = e.downcast_ref::<windows::core::Error>() {
            if we.code() == windows::Win32::Foundation::E_ACCESSDENIED {
                return true;
            }
        }
    }
    let msg = format!("{e:#}").to_lowercase();
    msg.contains("0x80070005")
        || msg.contains("80070005")
        || msg.contains("access is denied")
        || msg.contains("e_accessdenied")
        || msg.contains("拒绝访问")
}

#[derive(Debug, Clone)]
pub struct SnapshotRow {
    pub id: String,
    pub created_at: String,
    pub count: usize,
}

pub fn data_dir() -> PathBuf {
    if cfg!(windows) {
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            return PathBuf::from(local).join("IconForge");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".local/share/iconforge");
    }
    PathBuf::from("iconforge-data")
}

pub fn icons_dir() -> PathBuf {
    data_dir().join("icons")
}

pub fn snapshots_dir() -> PathBuf {
    data_dir().join("snapshots")
}

/// File stem of a shortcut path, splitting on both `/` and `\`
/// (mock shortcuts carry Windows paths even on Linux).
pub fn lnk_stem(lnk_path: &str) -> String {
    let last = lnk_path.rsplit(['/', '\\']).next().unwrap_or(lnk_path);
    let stem = last.strip_suffix(".lnk").unwrap_or(last);
    safe_stem(stem)
}

pub fn safe_stem(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        if ch.is_alphanumeric()
            || ch == '_'
            || ch == '-'
            || ch == '.'
            || ('\u{4e00}'..='\u{9fff}').contains(&ch)
        {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push_str("icon");
    }
    out
}

pub fn base_template(s: &str) -> BaseTemplate {
    match s {
        "blue" => BaseTemplate::BLUE,
        "pure" => BaseTemplate::PURE_WHITE,
        _ => crate::gradient::parse_custom(s)
            .map(crate::gradient::gradient_from_color)
            .unwrap_or(BaseTemplate::WHITE),
    }
}

/// Changed/Unchanged, on LinkEcho's basis (`link_info.rs`):
/// a shortcut counts as replaced when its icon does NOT come from
/// the target. Derived from the .lnk itself, so it survives restarts
/// (no session-only set) and matches LinkEcho's 已更换/未更换 counts.
/// When a snapshot recorded this shortcut's original, the comparison
/// is against that original instead of the target: this keeps a
/// restore-to-standalone-ICO (outside the target dir) correctly
/// Unchanged, where LinkEcho's target-only rule would mislabel it.
pub fn is_changed(
    icon_path: &str,
    target_path: &str,
    kind: &str,
    snapshot_original: Option<&str>,
) -> bool {
    if let Some(orig) = snapshot_original {
        return !icon_path.eq_ignore_ascii_case(orig);
    }
    if kind == "uwp" {
        // UWP: no target to compare against. Counts as changed only
        // when an explicit icon location is set (i.e. after a
        // single-apply — LinkEcho-basis: single-apply works on UWP
        // .lnk). Before apply the icon location is empty -> Unchanged.
        return !icon_path.is_empty() && !icon_path.starts_with('%');
    }
    if icon_path.is_empty() || icon_path.starts_with('%') {
        return false;
    }
    if icon_path.eq_ignore_ascii_case(target_path) {
        return false;
    }
    // Manual parent split: `Path::parent()` is platform-specific and
    // does not treat `\` as a separator on Linux (where tests run).
    let parent_dir = |p: &str| -> String {
        match p.rfind(['/', '\\']) {
            Some(i) => p[..i].to_string(),
            None => String::new(),
        }
    };
    let icon_dir = parent_dir(icon_path);
    let target_dir = parent_dir(target_path);
    if !icon_dir.is_empty() && icon_dir.eq_ignore_ascii_case(&target_dir) {
        return false;
    }
    true
}

/// Oldest recorded original icon per shortcut (first snapshot wins),
/// skipping entries with an empty original (those mean "no explicit
/// icon" and are covered by the LinkEcho rule instead).
pub fn oldest_originals() -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for e in all_snapshot_entries() {
        if !e.original_icon_path.is_empty() {
            map.entry(e.lnk_path).or_insert(e.original_icon_path);
        }
    }
    map
}

/// Pre-replacement icon for a shortcut: the oldest snapshot's recorded
/// original, else the target itself (LinkEcho's restore target).
fn original_for(lnk_path: &str, info: &ShortcutInfo) -> (String, i32) {
    let entry = all_snapshot_entries()
        .into_iter()
        .find(|e| e.lnk_path == lnk_path && !e.original_icon_path.is_empty());
    match entry {
        Some(e) => (e.original_icon_path, e.original_icon_index),
        None => (info.target.clone(), 0),
    }
}

/// True when the image already looks like a processed iOS-style icon:
/// a superellipse base (opaque, ~93% fill) with transparent corners.
/// A freeform glyph is well below 0.88 fill; a photo is ~1.0 with
/// opaque corners.
fn is_squircle(img: &image::RgbaImage) -> bool {
    let (w, h) = img.dimensions();
    if w < 8 || h < 8 {
        return false;
    }
    let corners = [
        img.get_pixel(0, 0)[3],
        img.get_pixel(w - 1, 0)[3],
        img.get_pixel(0, h - 1)[3],
        img.get_pixel(w - 1, h - 1)[3],
    ];
    if corners.iter().any(|&a| a > 10) {
        return false;
    }
    if img.get_pixel(w / 2, h / 2)[3] <= 200 {
        return false;
    }
    let fg = composite::foreground_fraction(img);
    // Upper bound just below 1.0: photos are excluded by the opaque-
    // corners check above, not by the fill band. Lower bound above a
    // circle (0.785) so freeform round glyphs are not mistaken.
    (0.85..=0.995).contains(&fg)
}

/// True when a path lives under a tool's generated-icons directory
/// (ours, or LinkEcho's): the icon is a tool output, not the app's own.
fn is_tool_output(icon_path: &str) -> bool {
    let lower = icon_path.to_lowercase();
    let ours = icons_dir().to_string_lossy().to_lowercase();
    let linkecho = if cfg!(windows) {
        std::env::var("LOCALAPPDATA")
            .map(|l| format!("{}\\linkecho\\icons", l).to_lowercase())
            .unwrap_or_default()
    } else {
        String::new()
    };
    (!ours.is_empty() && lower.starts_with(&ours))
        || (!linkecho.is_empty() && lower.starts_with(&linkecho))
}

fn extract_icon(file: &str, idx: i32) -> anyhow::Result<image::RgbaImage> {
    match lnk::extract_icon_png(file, idx, 256) {
        Some(png) => Ok(image::load_from_memory(&png)?.to_rgba8()),
        None => anyhow::bail!("无法提取当前图标"),
    }
}

/// Load the source image for a preview/apply step.
/// Returns (image, resolved_to_original): the flag is true when the
/// "current" source turned out to be an already-processed icon and was
/// unwrapped to the pre-replacement original. Without this, re-applying
/// 兜底 to an already-iOS-ified icon stacks squircle inside squircle
/// (LinkEcho never has this bug: it never uses the current icon as
/// a source).
pub fn load_source_image(
    src: &SourceSpec,
    lnk_path: &str,
) -> anyhow::Result<(image::RgbaImage, bool)> {
    match src.kind.as_str() {
        "itunes" => Ok((itunes::download_image(&src.artwork_url)?, false)),
        "file" | "file_design" => {
            let img = image::open(&src.path)?;
            Ok((img.to_rgba8(), false))
        }
        "current" => {
            let info = lnk::parse(Path::new(lnk_path))?;
            let (mut file, mut idx, mut resolved) =
                if !info.icon_path.is_empty() && is_tool_output(&info.icon_path) {
                    let (f, i) = original_for(lnk_path, &info);
                    (f, i, true)
                } else if !info.icon_path.is_empty() {
                    (info.icon_path.clone(), info.icon_index, false)
                } else {
                    (info.target.clone(), 0, false)
                };
            let mut img = extract_icon(&file, idx)?;
            if !resolved && is_squircle(&img) {
                let (f, i) = original_for(lnk_path, &info);
                if f != file || i != idx {
                    file = f;
                    idx = i;
                    resolved = true;
                    img = extract_icon(&file, idx)?;
                }
            }
            Ok((img, resolved))
        }
        other => anyhow::bail!("未知来源类型: {other}"),
    }
}

/// Compose a 512 preview image for one item.
pub fn compose_preview(
    src: &SourceSpec,
    lnk_path: &str,
    base: &str,
    mock: bool,
    glyph_scale: f32,
    icon_radius: f32,
    base_size: f32,
    base_exponent: f32,
) -> anyhow::Result<(image::RgbaImage, String)> {
    // Mock mode (Linux UI demo): "current" source has no real .lnk to
    // parse, so fall back to the mock icon for that shortcut name.
    if mock && src.kind == "current" {
        let png = mock_icon_png(&lnk_stem(lnk_path));
        let img = image::load_from_memory(&png)?.to_rgba8();
        return compose_image(
            &img,
            base,
            glyph_scale,
            icon_radius,
            base_size,
            base_exponent,
        );
    }
    let (img, resolved) = load_source_image(src, lnk_path)?;
    if src.kind == "itunes" {
        let out = composite::remask(&img);
        Ok((out, "remask".to_string()))
    } else if src.kind == "file_design" {
        let style = crate::presets::StylePreset {
            name: "本地编辑".into(),
            base: base.into(),
            glyph_scale,
            icon_radius,
            base_size,
            base_exponent,
        };
        Ok((crate::studio::render(&img, &style)?, "自定义编辑".into()))
    } else if src.kind == "file" {
        // Local file icons are already finished icons: never add a
        // base plate. (The old glyph branch deconstructed a finished
        // icon into glyph + new base — e.g. a complete YesPlayMusic
        // icon came back as triangle-on-white.) Just mask to the
        // squircle.
        let out = composite::remask(&img);
        Ok((out, "remask（本地文件：不加底板）".to_string()))
    } else {
        let (out, branch) = compose_image(
            &img,
            base,
            glyph_scale,
            icon_radius,
            base_size,
            base_exponent,
        )?;
        let branch = if resolved {
            format!("{branch}（兜底已用原始图标）")
        } else {
            branch
        };
        Ok((out, branch))
    }
}

fn compose_image(
    img: &image::RgbaImage,
    base: &str,
    glyph_scale: f32,
    icon_radius: f32,
    base_size: f32,
    base_exponent: f32,
) -> anyhow::Result<(image::RgbaImage, String)> {
    let (tpl, picked) = crate::gradient::resolve_base(base, img);
    let opts = Options {
        base: tpl,
        glyph_scale,
        shadow: true,
        icon_radius,
        base_size,
        base_exponent,
    };
    let out = composite::composite(img, &opts);
    let branch = match out.branch {
        composite::Branch::Glyph => {
            if base == "pick" && picked {
                "glyph·取色"
            } else if base == "pick" {
                "glyph（取色回退白底：字形单色）"
            } else if base.starts_with("custom:") {
                "glyph·自定义"
            } else {
                "glyph"
            }
        }
        composite::Branch::MaskOnly => "mask_only",
    };
    Ok((out.image, branch.to_string()))
}

/// One applied-icon cache entry: the generated ICO a shortcut was last
/// applied with. The ICO file itself lives in `icons/` (apply always
/// writes a fresh file, never overwrites), so the cache is just this
/// mapping: shortcut path -> applied ICO.
/// Default glyph scale for cache entries written before the zoom
/// slider existed (and for any entry missing the field).
fn default_glyph_scale() -> f32 {
    composite::DEFAULT_GLYPH_SCALE
}

/// Defaults for cache entries written before the three LinkEcho-basis
/// sliders (icon radius / base size / base radius) existed.
fn default_icon_radius() -> f32 {
    composite::DEFAULT_ICON_RADIUS
}
fn default_base_size() -> f32 {
    composite::DEFAULT_BASE_SIZE
}
fn default_base_exponent() -> f32 {
    composite::DEFAULT_BASE_EXPONENT
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppliedEntry {
    pub icon_path: String,
    pub applied_at: u64,
    #[serde(default)]
    pub base: String,
    /// Glyph scale the icon was applied with (crop-to-zoom).
    #[serde(default = "default_glyph_scale")]
    pub glyph_scale: f32,
    /// Icon corner radius the icon was applied with.
    #[serde(default = "default_icon_radius")]
    pub icon_radius: f32,
    /// Base-plate size the icon was applied with.
    #[serde(default = "default_base_size")]
    pub base_size: f32,
    /// Base-plate superellipse exponent the icon was applied with.
    #[serde(default = "default_base_exponent")]
    pub base_exponent: f32,
}

/// Applied-icon cache index: `%LOCALAPPDATA%\IconForge\applied.json`.
pub fn applied_cache_path() -> PathBuf {
    data_dir().join("applied.json")
}

fn applied_cache_load_from(path: &Path) -> std::collections::HashMap<String, AppliedEntry> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn applied_cache_save_to(
    path: &Path,
    map: &std::collections::HashMap<String, AppliedEntry>,
) -> anyhow::Result<()> {
    crate::webdav::atomic_write(path, &serde_json::to_vec_pretty(map)?)
}

pub fn applied_cache_load_checked(
) -> anyhow::Result<std::collections::HashMap<String, AppliedEntry>> {
    use anyhow::Context;
    match std::fs::read(applied_cache_path()) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).context("恢复记录已损坏，原文件保留；请先从备份恢复记录")
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
        Err(e) => Err(e).context("无法读取恢复记录，未覆盖原文件"),
    }
}

/// Whole applied-icon cache (empty when the index is missing/corrupt).
pub fn applied_cache_load() -> std::collections::HashMap<String, AppliedEntry> {
    applied_cache_load_from(&applied_cache_path())
}

/// Record a successful apply: shortcut -> generated ICO (+ base).
pub fn applied_cache_put(
    lnk_path: &str,
    icon_path: &str,
    base: &str,
    glyph_scale: f32,
    icon_radius: f32,
    base_size: f32,
    base_exponent: f32,
) -> anyhow::Result<()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("恢复记录写入锁异常"))?;
    let mut map = applied_cache_load_checked()?;
    map.insert(
        lnk_path.to_string(),
        AppliedEntry {
            icon_path: icon_path.to_string(),
            applied_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            base: base.to_string(),
            glyph_scale,
            icon_radius,
            base_size,
            base_exponent,
        },
    );
    applied_cache_save_to(&applied_cache_path(), &map)
}

/// Write one snapshot manifest for the given entries (unique id, .lnk
/// copies alongside). Returns the snapshot id.
fn write_snapshot(entries: Vec<SnapshotEntry>) -> anyhow::Result<String> {
    // Unique id: two applies within the same second used to share
    // one `snap-{secs}` id, so the second manifest OVERWROTE the
    // first and its true original was lost (undo then restored to
    // an intermediate icon instead of the original).
    let base = format!(
        "snap-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    );
    let mut id = base.clone();
    let mut n = 2;
    while snapshots_dir().join(&id).exists() {
        id = format!("{base}-{n}");
        n += 1;
    }
    let manifest = SnapshotManifest {
        created_at: id.clone(),
        entries: entries.clone(),
    };
    snapshot::write_verified_backup(&snapshots_dir(), &manifest)?;
    Ok(id)
}

/// Small artwork URL for candidate thumbnails: same mzstatic pattern as
/// `artwork_url_1024`, but 100px (a 40px thumbnail doesn't need 512px).
pub fn thumb_url(url_512: &str) -> String {
    url_512.replace("512x512", "100x100")
}

/// Round the corners of a thumbnail (radius = 1/4 of the smaller side)
/// so it reads as a rounded-square icon instead of a hard square.
pub fn rounded_thumbnail(mut img: image::RgbaImage) -> image::RgbaImage {
    let (w, h) = img.dimensions();
    let r = (w.min(h) as f32 / 4.0).max(1.0);
    let corners = [
        (r, r),
        (w as f32 - r, r),
        (r, h as f32 - r),
        (w as f32 - r, h as f32 - r),
    ];
    for (x, y, px) in img.enumerate_pixels_mut() {
        let fx = x as f32 + 0.5;
        let fy = y as f32 + 0.5;
        for (cx, cy) in corners {
            let in_corner_x = if cx < w as f32 / 2.0 {
                fx < cx
            } else {
                fx > cx
            };
            let in_corner_y = if cy < h as f32 / 2.0 {
                fy < cy
            } else {
                fy > cy
            };
            if in_corner_x && in_corner_y {
                let dx = fx - cx;
                let dy = fy - cy;
                if dx * dx + dy * dy > r * r {
                    px[3] = 0;
                }
            }
        }
    }
    img
}

/// One-click re-apply from the applied-icon cache: point the shortcut
/// back at the ICO it was last applied with, without re-searching,
/// re-picking or re-synthesizing. Used after an app update resets
/// the shortcut's icon to the original. Snapshots the current state
/// first (so 撤回 can restore it), then re-points + refreshes.
/// No-op when the shortcut already points at the cached icon.
pub fn reapply(lnk_path: &str) -> anyhow::Result<ApplyResult> {
    let cache = applied_cache_load_checked()?;
    let Some(entry) = cache.get(lnk_path) else {
        anyhow::bail!("该快捷方式没有已缓存的应用记录，请先正常应用一次");
    };
    if !Path::new(&entry.icon_path).exists() {
        anyhow::bail!("缓存图标文件已丢失：{}，请重新应用一次", entry.icon_path);
    }
    let info = lnk::parse(Path::new(lnk_path))?;
    let (cur_path, cur_idx, target) = (info.icon_path, info.icon_index, info.target);
    if cur_path == entry.icon_path {
        return Ok(ApplyResult {
            lnk_path: lnk_path.to_string(),
            ok: true,
            error: None,
            icon_path: Some(entry.icon_path.clone()),
            needs_admin: false,
        });
    }
    write_snapshot(vec![SnapshotEntry {
        lnk_path: lnk_path.to_string(),
        target_path: target,
        original_icon_path: cur_path,
        original_icon_index: cur_idx,
        new_icon_path: entry.icon_path.clone(),
    }])?;
    if let Err(e) = lnk::set_icon(Path::new(lnk_path), &entry.icon_path, 0) {
        return Ok(ApplyResult {
            lnk_path: lnk_path.to_string(),
            ok: false,
            error: Some(format!("写入失败: {e:#}")),
            icon_path: None,
            needs_admin: is_access_denied(&e),
        });
    }
    lnk::refresh_item(lnk_path);
    lnk::refresh_icons();
    // The existing durable cache entry already points at this ICO; do not rewrite it.
    Ok(ApplyResult {
        lnk_path: lnk_path.to_string(),
        ok: true,
        error: None,
        icon_path: Some(entry.icon_path.clone()),
        needs_admin: false,
    })
}

/// Apply a batch of items: compose -> write ICO -> snapshot original ->
/// SetIconLocation (skipped when `mock`) -> refresh once.
/// Returns (per-item results, snapshot id if one was created).
pub fn apply_items(
    items: &[ApplyItem],
    dry_run: bool,
    mock: bool,
) -> (Vec<ApplyResult>, Option<String>) {
    let mut results = Vec::new();
    if !dry_run && !mock {
        if let Err(error) = applied_cache_load_checked() {
            return (
                items
                    .iter()
                    .map(|item| ApplyResult {
                        lnk_path: item.lnk_path.clone(),
                        ok: false,
                        error: Some(format!("恢复记录不可用，未修改快捷方式：{error:#}")),
                        icon_path: None,
                        needs_admin: is_access_denied(&error),
                    })
                    .collect(),
                None,
            );
        }
    }
    let mut prepared = Vec::new();
    let mut entries = Vec::new();
    for item in items {
        let outcome = (|| -> anyhow::Result<String> {
            // Parse failures must not silently erase the original location.
            let info = if !dry_run && !mock {
                Some(lnk::parse(Path::new(&item.lnk_path))?)
            } else {
                None
            };
            let (img, _) = compose_preview(
                &item.source,
                &item.lnk_path,
                &item.base,
                mock,
                item.glyph_scale,
                item.icon_radius,
                item.base_size,
                item.base_exponent,
            )?;
            let dir = icons_dir();
            std::fs::create_dir_all(&dir)?;
            let stem = safe_stem(&lnk_stem(&item.lnk_path));
            let mut path = dir.join(format!("{stem}.ico"));
            let mut n = 1;
            while path.exists() {
                n += 1;
                path = dir.join(format!("{stem}_{n}.ico"));
            }
            ico::save_ico(&img, &path)?;
            let ico = path.to_string_lossy().into_owned();
            if let Some(info) = info {
                entries.push(SnapshotEntry {
                    lnk_path: item.lnk_path.clone(),
                    target_path: info.target,
                    original_icon_path: info.icon_path,
                    original_icon_index: info.icon_index,
                    new_icon_path: ico.clone(),
                });
            }
            Ok(ico)
        })();
        match outcome {
            Ok(ico) => prepared.push((item, ico)),
            Err(e) => results.push(ApplyResult {
                lnk_path: item.lnk_path.clone(),
                ok: false,
                error: Some(format!("{e:#}")),
                icon_path: None,
                needs_admin: is_access_denied(&e),
            }),
        }
    }
    // Finish every original backup before performing the first shortcut write.
    let snapshot_id = if entries.is_empty() {
        None
    } else {
        match write_snapshot(entries) {
            Ok(id) => Some(id),
            Err(e) => {
                for (item, ico) in prepared {
                    let _ = std::fs::remove_file(ico);
                    results.push(ApplyResult {
                        lnk_path: item.lnk_path.clone(),
                        ok: false,
                        error: Some(format!("备份失败，未修改快捷方式: {e:#}")),
                        icon_path: None,
                        needs_admin: is_access_denied(&e),
                    });
                }
                return (results, None);
            }
        }
    };
    let mut changed = false;
    for (item, ico) in prepared {
        if !dry_run && !mock {
            let original = match lnk::parse(Path::new(&item.lnk_path)) {
                Ok(info) => info,
                Err(error) => {
                    results.push(ApplyResult {
                        lnk_path: item.lnk_path.clone(),
                        ok: false,
                        error: Some(format!("无法读取当前图标，未修改：{error:#}")),
                        icon_path: None,
                        needs_admin: is_access_denied(&error),
                    });
                    continue;
                }
            };
            if let Err(e) = lnk::set_icon(Path::new(&item.lnk_path), &ico, 0) {
                results.push(ApplyResult {
                    lnk_path: item.lnk_path.clone(),
                    ok: false,
                    error: Some(format!("写入失败（原始备份已保留）: {e:#}")),
                    icon_path: None,
                    needs_admin: is_access_denied(&e),
                });
                continue;
            }
            changed = true;
            lnk::refresh_item(&item.lnk_path);
            if let Err(error) = applied_cache_put(
                &item.lnk_path,
                &ico,
                &item.base,
                item.glyph_scale,
                item.icon_radius,
                item.base_size,
                item.base_exponent,
            ) {
                let rollback = lnk::set_icon(
                    Path::new(&item.lnk_path),
                    &original.icon_path,
                    original.icon_index,
                );
                lnk::refresh_item(&item.lnk_path);
                let detail = match rollback {
                    Ok(()) => "已撤销本次图标修改，原始快照保留".to_string(),
                    Err(e) => format!("图标回退也失败：{e:#}；请用本次快照恢复"),
                };
                results.push(ApplyResult {
                    lnk_path: item.lnk_path.clone(),
                    ok: false,
                    error: Some(format!("恢复记录保存失败：{error:#}；{detail}")),
                    icon_path: None,
                    needs_admin: is_access_denied(&error),
                });
                continue;
            }
        }
        results.push(ApplyResult {
            lnk_path: item.lnk_path.clone(),
            ok: true,
            error: None,
            icon_path: Some(ico),
            needs_admin: false,
        });
    }
    if changed {
        lnk::refresh_icons();
    }
    (results, snapshot_id)
}

pub fn list_snapshots() -> Vec<SnapshotRow> {
    let mut list = Vec::new();
    if let Ok(rd) = std::fs::read_dir(snapshots_dir()) {
        for entry in rd.flatten() {
            let manifest = entry.path().join("manifest.json");
            if let Ok(m) = snapshot::read_snapshot(&manifest) {
                list.push(SnapshotRow {
                    id: entry.file_name().to_string_lossy().to_string(),
                    created_at: m.created_at,
                    count: m.entries.len(),
                });
            }
        }
    }
    list.sort_by(|a, b| b.id.cmp(&a.id));
    list
}

/// Restore one snapshot entry: point the shortcut back at its recorded
/// ORIGINAL IconLocation (shared by all restore paths). On success,
/// `icon_path` is the icon restored TO (the GUI updates the row from it).
/// Empty original = the shortcut had no explicit IconLocation, so its
/// icon came from the target: restore to the target (LinkEcho's base
/// restore). Old snapshots recorded neither original nor target, so
/// read the target from the .lnk as it stands now (the target never
/// changes across applies).
fn restore_entry(e: &SnapshotEntry) -> ApplyResult {
    let (path, idx) = if !e.original_icon_path.is_empty() {
        (e.original_icon_path.clone(), e.original_icon_index)
    } else if !e.target_path.is_empty() {
        (e.target_path.clone(), e.original_icon_index)
    } else {
        match lnk::parse(Path::new(&e.lnk_path)) {
            Ok(info) => (info.target.clone(), e.original_icon_index),
            Err(_) => (String::new(), 0),
        }
    };
    let r = if path.is_empty() {
        Err(anyhow::anyhow!("原始图标位置未知"))
    } else {
        lnk::set_icon(Path::new(&e.lnk_path), &path, idx)
    };
    match r {
        Ok(()) => {
            lnk::refresh_item(&e.lnk_path);
            ApplyResult {
                lnk_path: e.lnk_path.clone(),
                ok: true,
                error: None,
                icon_path: Some(path),
                needs_admin: false,
            }
        }
        Err(err) => ApplyResult {
            lnk_path: e.lnk_path.clone(),
            ok: false,
            error: Some(format!("{err:#}")),
            needs_admin: is_access_denied(&err),
            icon_path: None,
        },
    }
}

/// All snapshot entries, oldest snapshot first (snapshot ids are
/// timestamps, so ascending id order == chronological).
fn all_snapshot_entries() -> Vec<SnapshotEntry> {
    let mut snaps = list_snapshots();
    snaps.sort_by(|a, b| a.id.cmp(&b.id));
    let mut out = Vec::new();
    for s in snaps {
        if let Ok(m) = snapshot::read_snapshot(&snapshots_dir().join(&s.id).join("manifest.json")) {
            out.extend(m.entries);
        }
    }
    out
}

pub fn restore_snapshot(id: &str) -> anyhow::Result<Vec<ApplyResult>> {
    let manifest = snapshot::read_snapshot(&snapshots_dir().join(id).join("manifest.json"))?;
    let results: Vec<_> = manifest.entries.iter().map(restore_entry).collect();
    lnk::refresh_icons();
    Ok(results)
}

/// Restore one shortcut to its original icon (the OLDEST snapshot's
/// record for it), i.e. fully undo every apply on that item.
/// Returns None when no snapshot covers it.
pub fn restore_one(lnk_path: &str) -> anyhow::Result<Option<ApplyResult>> {
    let entry = all_snapshot_entries()
        .into_iter()
        .find(|e| e.lnk_path == lnk_path);
    let Some(e) = entry else {
        return Ok(None);
    };
    let r = restore_entry(&e);
    lnk::refresh_icons();
    Ok(Some(r))
}

/// Restore the given snapshots (batch undo). For a shortcut covered by
/// several of the selected snapshots, the OLDEST record wins, so the
/// item goes back to its pre-apply original, not an intermediate state.
pub fn restore_snapshots(ids: &[&str]) -> anyhow::Result<Vec<ApplyResult>> {
    restore_snapshots_with_progress(ids, |_, _, _| {})
}

pub fn restore_snapshots_with_progress(
    ids: &[&str],
    mut progress: impl FnMut(usize, usize, &ApplyResult),
) -> anyhow::Result<Vec<ApplyResult>> {
    let mut ids_sorted: Vec<&str> = ids.to_vec();
    ids_sorted.sort();
    let mut by_lnk: Vec<SnapshotEntry> = Vec::new();
    for id in ids_sorted {
        let m = snapshot::read_snapshot(&snapshots_dir().join(id).join("manifest.json"))?;
        {
            for e in m.entries {
                if !by_lnk.iter().any(|x| x.lnk_path == e.lnk_path) {
                    by_lnk.push(e);
                }
            }
        }
    }
    let total = by_lnk.len();
    let mut results = Vec::with_capacity(total);
    for entry in &by_lnk {
        let result = restore_entry(entry);
        progress(results.len() + 1, total, &result);
        results.push(result);
    }
    lnk::refresh_icons();
    Ok(results)
}

/// Restore every snapshot: every modified shortcut back to its
/// original icon (oldest record per shortcut).
pub fn restore_all() -> anyhow::Result<Vec<ApplyResult>> {
    let ids: Vec<String> = list_snapshots().into_iter().map(|s| s.id).collect();
    let refs: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
    restore_snapshots(&refs)
}

/// LinkEcho's base restore, exposed as an escape hatch: point the
/// shortcut straight back at its target. Use it when the snapshot's
/// recorded original is itself a processed icon (e.g. the snapshot
/// was clobbered by the old same-second id collision), or for icons
/// another tool changed. Note it discards any standalone-ICO original
/// the snapshot would have preserved -- prefer 撤回 when available.
pub fn restore_to_target(lnk_path: &str) -> anyhow::Result<ApplyResult> {
    let info = lnk::parse(Path::new(lnk_path))?;
    if info.target.is_empty() {
        anyhow::bail!("目标路径未知");
    }
    lnk::set_icon(Path::new(lnk_path), &info.target, 0)?;
    lnk::refresh_item(lnk_path);
    lnk::refresh_icons();
    Ok(ApplyResult {
        lnk_path: lnk_path.to_string(),
        ok: true,
        error: None,
        icon_path: Some(info.target),
        needs_admin: false,
    })
}

// ---------- mock data (Linux UI testing) ----------

pub fn mock_shortcuts() -> Vec<ShortcutInfo> {
    vec![
        ShortcutInfo {
            path: "C:\\Users\\demo\\Desktop\\FlClash.lnk".into(),
            name: "FlClash".into(),
            target: "C:\\Program Files\\FlClash\\FlClash.exe".into(),
            icon_path: "C:\\Program Files\\FlClash\\FlClash.exe".into(),
            icon_index: 0,
            kind: "normal".into(),
        },
        ShortcutInfo {
            path: "C:\\Users\\demo\\Desktop\\微信.lnk".into(),
            name: "微信".into(),
            target: "C:\\Program Files\\Tencent\\WeChat\\WeChat.exe".into(),
            icon_path: "C:\\Program Files\\Tencent\\WeChat\\WeChat.exe".into(),
            icon_index: 0,
            kind: "normal".into(),
        },
        ShortcutInfo {
            path: "C:\\Users\\demo\\Desktop\\Steam.lnk".into(),
            name: "Steam".into(),
            target: "C:\\Program Files (x86)\\Steam\\steam.exe".into(),
            icon_path: "C:\\Program Files (x86)\\Steam\\steam.exe".into(),
            icon_index: 0,
            kind: "normal".into(),
        },
    ]
}

pub fn mock_icon_png(name: &str) -> Vec<u8> {
    let color: [u8; 3] = match name {
        "微信" => [7, 193, 96],
        "Steam" => [27, 40, 56],
        _ => [108, 92, 231],
    };
    let mut img = image::RgbaImage::new(256, 256);
    for px in img.pixels_mut() {
        *px = image::Rgba([color[0], color[1], color[2], 255]);
    }
    let masked = composite::remask(&img);
    let mut buf = Vec::new();
    let mut cur = std::io::Cursor::new(&mut buf);
    let _ = masked.write_to(&mut cur, image::ImageFormat::Png);
    buf
}

pub fn placeholder_png() -> Vec<u8> {
    let mut img = image::RgbaImage::new(256, 256);
    for px in img.pixels_mut() {
        *px = image::Rgba([180, 180, 180, 255]);
    }
    let masked = composite::remask(&img);
    let mut buf = Vec::new();
    let mut cur = std::io::Cursor::new(&mut buf);
    let _ = masked.write_to(&mut cur, image::ImageFormat::Png);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};

    #[test]
    fn applied_cache_round_trips() {
        let dir = std::env::temp_dir().join("iconforge-cache-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("applied.json");
        let mut map = std::collections::HashMap::new();
        map.insert(
            r"C:\Users\umie\Desktop\Spotify.lnk".to_string(),
            AppliedEntry {
                icon_path: r"C:\Users\umie\AppData\Local\IconForge\icons\spotify.ico".to_string(),
                applied_at: 1_760_870_000,
                base: "white".to_string(),
                glyph_scale: 0.76,
                icon_radius: 0.0,
                base_size: 1.0,
                base_exponent: 5.2,
            },
        );
        applied_cache_save_to(&path, &map).unwrap();
        let back = applied_cache_load_from(&path);
        assert_eq!(back, map);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn thumb_url_downsizes_mzstatic() {
        assert_eq!(
            thumb_url("https://is1-ssl.mzstatic.com/image/thumb/x/512x512bb.jpg"),
            "https://is1-ssl.mzstatic.com/image/thumb/x/100x100bb.jpg"
        );
        // Non-mzstatic URLs pass through untouched.
        assert_eq!(
            thumb_url("https://example.com/icon.png"),
            "https://example.com/icon.png"
        );
    }

    #[test]
    fn rounded_thumbnail_clears_corners_only() {
        let mut img = RgbaImage::new(40, 40);
        for px in img.pixels_mut() {
            *px = Rgba([255, 0, 0, 255]);
        }
        let out = rounded_thumbnail(img);
        assert_eq!(out.get_pixel(0, 0)[3], 0);
        assert_eq!(out.get_pixel(39, 0)[3], 0);
        assert_eq!(out.get_pixel(0, 39)[3], 0);
        assert_eq!(out.get_pixel(39, 39)[3], 0);
        assert_eq!(out.get_pixel(20, 20)[3], 255);
        assert_eq!(out.get_pixel(0, 20)[3], 255);
        assert_eq!(out.get_pixel(20, 0)[3], 255);
    }

    #[test]
    fn reapply_without_cache_entry_errors() {
        // No cache entry for a fake path: bails before any disk writes.
        let err = reapply(r"C:\nope\missing.lnk").unwrap_err();
        assert!(format!("{err:#}").contains("没有已缓存的应用记录"));
    }

    #[test]
    fn changed_rule_follows_linkecho() {
        // Icon == target -> unchanged.
        assert!(!is_changed(
            r"C:\App\app.exe",
            r"C:\App\app.exe",
            "normal",
            None
        ));
        // No explicit icon -> unchanged.
        assert!(!is_changed("", r"C:\App\app.exe", "normal", None));
        // Icon in the target's dir -> unchanged (LinkEcho's rule).
        assert!(!is_changed(
            r"C:\App\icon.ico",
            r"C:\App\app.exe",
            "normal",
            None
        ));
        // System icon (%windir%) -> unchanged.
        assert!(!is_changed(
            r"%windir%\System32\imageres.dll",
            r"C:\App\app.exe",
            "normal",
            None
        ));
        // Icon elsewhere (tool output) -> changed.
        assert!(is_changed(
            r"C:\Users\umie\AppData\Local\IconForge\icons\app.ico",
            r"C:\App\app.exe",
            "normal",
            None
        ));
        // Snapshot-aware: current == recorded original -> unchanged,
        // even when the original lives outside the target dir.
        assert!(!is_changed(
            r"D:\Icons\app.ico",
            r"C:\App\app.exe",
            "normal",
            Some(r"D:\Icons\app.ico")
        ));
        // Snapshot-aware: current != recorded original -> changed.
        assert!(is_changed(
            r"C:\Users\umie\AppData\Local\IconForge\icons\app.ico",
            r"C:\App\app.exe",
            "normal",
            Some(r"D:\Icons\app.ico")
        ));
    }

    #[test]
    fn squircle_detection() {
        // Opaque squircle-ish block with transparent corners -> squircle.
        let mut img = RgbaImage::new(100, 100);
        for px in img.pixels_mut() {
            *px = Rgba([40, 60, 90, 255]);
        }
        for &(x, y) in &[(0, 0), (99, 0), (0, 99), (99, 99)] {
            img.put_pixel(x, y, Rgba([0, 0, 0, 0]));
        }
        // Punch ~5% transparent corners/edges to land in the 0.88-0.98 band.
        for y in 0..7 {
            for x in 0..7 {
                img.put_pixel(x, y, Rgba([0, 0, 0, 0]));
                img.put_pixel(99 - x, y, Rgba([0, 0, 0, 0]));
                img.put_pixel(x, 99 - y, Rgba([0, 0, 0, 0]));
                img.put_pixel(99 - x, 99 - y, Rgba([0, 0, 0, 0]));
            }
        }
        assert!(is_squircle(&img));
        // Freeform glyph (low fill) -> not a squircle.
        let mut glyph = RgbaImage::new(100, 100);
        for y in 30..70 {
            for x in 30..70 {
                glyph.put_pixel(x, y, Rgba([40, 60, 90, 255]));
            }
        }
        assert!(!is_squircle(&glyph));
    }
}
