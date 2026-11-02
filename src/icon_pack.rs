//! Read-only icon-pack planning. Only the GUI's explicit confirmation applies it.
use crate::lnk::ShortcutInfo;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
#[derive(Clone, Debug)]
pub struct Candidate {
    pub path: PathBuf,
    pub name: String,
    pub score: u32,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Quality {
    Exact,
    Approximate,
    Ambiguous,
    Unmatched,
    Manual,
}
#[derive(Clone, Debug)]
pub struct PackRow {
    pub shortcut: ShortcutInfo,
    pub candidates: Vec<Candidate>,
    pub choice: Option<usize>,
    pub enabled: bool,
    pub quality: Quality,
}
impl PackRow {
    pub fn candidate(&self) -> Option<&Candidate> {
        self.choice.and_then(|i| self.candidates.get(i))
    }
    pub fn choose_file(&mut self, path: PathBuf) {
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let index = self
            .candidates
            .iter()
            .position(|c| c.path == path)
            .unwrap_or_else(|| {
                self.candidates.push(Candidate {
                    path,
                    name,
                    score: 0,
                });
                self.candidates.len() - 1
            });
        self.choice = Some(index);
        self.enabled = true;
        self.quality = Quality::Manual;
    }
}
#[derive(Debug)]
pub struct PackPlan {
    pub rows: Vec<PackRow>,
    pub file_count: usize,
    pub ignored_count: usize,
    pub directory: PathBuf,
}
impl PackPlan {
    pub fn selected_count(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| r.enabled && r.candidate().is_some())
            .count()
    }
    pub fn matched_count(&self) -> usize {
        self.rows.iter().filter(|r| r.candidate().is_some()).count()
    }
}
pub fn supported_image(path: &Path) -> bool {
    path.extension()
        .and_then(|x| x.to_str())
        .map(|s| {
            matches!(
                s.to_ascii_lowercase().as_str(),
                "ico" | "png" | "jpg" | "jpeg" | "webp" | "bmp"
            )
        })
        .unwrap_or(false)
}
pub fn plan(
    shortcuts: Vec<ShortcutInfo>,
    excluded: HashSet<String>,
    directory: PathBuf,
) -> anyhow::Result<PackPlan> {
    let mut icons = Vec::new();
    let mut ignored = 0;
    for entry in std::fs::read_dir(&directory)? {
        let path = entry?.path();
        if !path.is_file() || !supported_image(&path) {
            continue;
        }
        // Validate on the worker: broken files must not masquerade as exact matches.
        if image::open(&path).is_err() {
            ignored += 1;
            continue;
        }
        icons.push(path);
    }
    let rows = match_paths(shortcuts, excluded, &icons);
    Ok(PackPlan {
        rows,
        file_count: icons.len(),
        ignored_count: ignored,
        directory,
    })
}
fn match_paths(
    shortcuts: Vec<ShortcutInfo>,
    excluded: HashSet<String>,
    icons: &[PathBuf],
) -> Vec<PackRow> {
    shortcuts
        .into_iter()
        .filter(|s| !excluded.iter().any(|p|crate::lnk::path_key(p)==crate::lnk::path_key(&s.path)))
        .map(|shortcut| {
            let mut candidates: Vec<_> = icons
                .iter()
                .filter_map(|path| {
                    let stem = path.file_stem()?.to_string_lossy();
                    let score = crate::matching::score(&shortcut.name, &stem);
                    (score >= 50).then(|| Candidate {
                        path: path.clone(),
                        name: path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned(),
                        score,
                    })
                })
                .collect();
            candidates.sort_by(|a, b| {
                b.score
                    .cmp(&a.score)
                    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                    .then_with(|| a.path.cmp(&b.path))
            });
            let quality = if let Some(first) = candidates.first() {
                if candidates
                    .get(1)
                    .map(|c| c.score == first.score)
                    .unwrap_or(false)
                {
                    Quality::Ambiguous
                } else if first.score == 100 {
                    Quality::Exact
                } else {
                    Quality::Approximate
                }
            } else {
                Quality::Unmatched
            };
            let choice = if candidates.is_empty() { None } else { Some(0) };
            PackRow {
                shortcut,
                candidates,
                choice,
                enabled: quality == Quality::Exact,
                quality,
            }
        })
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    fn shortcut(name: &str) -> ShortcutInfo {
        ShortcutInfo {
            path: format!("C:/desktop/{name}.lnk"),
            name: name.into(),
            target: String::new(),
            icon_path: String::new(),
            icon_index: 0,
            kind: "normal".into(),
        }
    }
    #[test]
    fn only_unique_exact_matches_are_selected_by_default() {
        let paths = vec![
            PathBuf::from("/icons/Telegram.png"),
            PathBuf::from("/icons/Photoshop Express.png"),
        ];
        let rows = match_paths(
            vec![
                shortcut("Telegram"),
                shortcut("Photoshop"),
                shortcut("Missing"),
            ],
            HashSet::new(),
            &paths,
        );
        assert!(rows[0].enabled);
        assert!(!rows[1].enabled);
        assert_eq!(rows[1].quality, Quality::Approximate);
        assert!(!rows[2].enabled);
        assert!(rows[2].candidate().is_none());
    }
    #[test]
    fn ties_are_unchecked_and_order_does_not_depend_on_directory_enumeration() {
        let paths = vec![
            PathBuf::from("/b/Telegram.png"),
            PathBuf::from("/a/Telegram.ico"),
        ];
        let first = match_paths(vec![shortcut("Telegram")], HashSet::new(), &paths);
        let mut reverse = paths.clone();
        reverse.reverse();
        let second = match_paths(vec![shortcut("Telegram")], HashSet::new(), &reverse);
        assert_eq!(first[0].quality, Quality::Ambiguous);
        assert!(!first[0].enabled);
        assert_eq!(
            first[0].candidate().unwrap().path,
            second[0].candidate().unwrap().path
        );
    }
    #[test]
    fn respects_exclusions_and_unicode_encoded_filenames() {
        let a = shortcut("微信");
        let b = shortcut("Telegram");
        let excluded = HashSet::from([b.path.clone()]);
        let rows = match_paths(
            vec![a, b],
            excluded,
            &[
                PathBuf::from("#U5FAE#U4FE1.png"),
                PathBuf::from("Telegram.png"),
            ],
        );
        assert_eq!(rows.len(), 1);
        assert!(rows[0].enabled);
        assert_eq!(rows[0].quality, Quality::Exact);
    }
    #[test]
    fn planning_skips_broken_images_and_never_writes_to_the_source_pack() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("iconforge-pack-{unique}"));
        std::fs::create_dir_all(&root).unwrap();
        image::RgbaImage::from_pixel(16, 16, image::Rgba([30, 100, 200, 255]))
            .save(root.join("Telegram.png"))
            .unwrap();
        std::fs::write(root.join("Broken.png"), b"bad image").unwrap();
        let plan = plan(
            vec![shortcut("Telegram"), shortcut("Broken")],
            HashSet::new(),
            root.clone(),
        )
        .unwrap();
        assert_eq!(plan.file_count, 1);
        assert_eq!(plan.ignored_count, 1);
        assert_eq!(plan.selected_count(), 1);
        assert!(!plan.rows[1].enabled);
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn manual_choice_is_explicit_and_does_not_change_other_rows() {
        let mut rows = match_paths(
            vec![shortcut("Missing"), shortcut("Other")],
            HashSet::new(),
            &[],
        );
        rows[0].choose_file(PathBuf::from("picked.png"));
        assert!(rows[0].enabled);
        assert_eq!(rows[0].quality, Quality::Manual);
        assert!(!rows[1].enabled);
    }
}
