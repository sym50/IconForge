//! Background-friendly recovery operations with per-shortcut progress.
use crate::{
    lnk,
    pipeline::{self, ApplyResult},
};
use std::collections::HashSet;
#[derive(Clone)]
pub enum Action {
    Cache(HashSet<String>),
    Reapply(String),
    Target(String),
    One(String),
    Snapshots(Vec<String>),
    All,
}
pub struct Report {
    pub results: Vec<ApplyResult>,
    pub skipped: usize,
}
pub struct Progress {
    pub done: usize,
    pub total: usize,
    pub name: String,
}

pub fn run(action: Action, mut progress: impl FnMut(Progress)) -> anyhow::Result<Report> {
    let mut skipped = 0;
    let single = matches!(
        &action,
        Action::One(_) | Action::Target(_) | Action::Reapply(_)
    );
    let results = match action {
        Action::Cache(excluded) => {
            let map = pipeline::applied_cache_load_checked()?;
            let mut paths = map.keys().cloned().collect::<Vec<_>>();
            paths.sort();
            let total = paths.len();
            let mut results = Vec::new();
            for (i, path) in paths.into_iter().enumerate() {
                let already = std::path::Path::new(&map[&path].icon_path).is_file()
                    && lnk::parse(std::path::Path::new(&path))
                        .is_ok_and(|info| info.icon_path == map[&path].icon_path);
                if excluded.contains(&lnk::path_key(&path)) || already {
                    skipped += 1;
                } else {
                    results.push(match pipeline::reapply(&path) {
                        Ok(result) => result,
                        Err(e) => failure(&path, e),
                    });
                }
                progress(Progress {
                    done: i + 1,
                    total,
                    name: path,
                });
            }
            results
        }
        Action::Snapshots(ids) => {
            let refs = ids.iter().map(String::as_str).collect::<Vec<_>>();
            pipeline::restore_snapshots_with_progress(&refs, |done, total, result| {
                progress(Progress {
                    done,
                    total,
                    name: result.lnk_path.clone(),
                })
            })?
        }
        Action::All => {
            return run(
                Action::Snapshots(
                    pipeline::list_snapshots()
                        .into_iter()
                        .map(|s| s.id)
                        .collect(),
                ),
                progress,
            )
        }
        Action::One(path) => vec![pipeline::restore_one(&path)?
            .ok_or_else(|| anyhow::anyhow!("没有该图标的快照，无法撤回"))?],
        Action::Target(path) => vec![pipeline::restore_to_target(&path)?],
        Action::Reapply(path) => vec![pipeline::reapply(&path)?],
    };
    if single {
        if let Some(result) = results.first() {
            progress(Progress {
                done: 1,
                total: 1,
                name: result.lnk_path.clone(),
            });
        }
    }
    Ok(Report { results, skipped })
}
fn failure(path: &str, e: anyhow::Error) -> ApplyResult {
    ApplyResult {
        lnk_path: path.into(),
        ok: false,
        needs_admin: pipeline::is_access_denied(&e),
        error: Some(format!("{e:#}")),
        icon_path: None,
    }
}
