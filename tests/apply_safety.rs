#![cfg(windows)]
use iconforge::{lnk, pipeline, snapshot};
use std::path::Path;

#[test]
fn apply_preserves_original_and_refuses_failed_backup() {
    // A child process isolates LOCALAPPDATA from the user's application data
    // and from the other concurrently running unit tests.
    if std::env::var_os("ICONFORGE_ISOLATED_TEST").is_none() {
        let root = std::env::temp_dir().join(format!("iconforge-apply-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "apply_preserves_original_and_refuses_failed_backup",
                "--nocapture",
            ])
            .env("LOCALAPPDATA", &root)
            .env("ICONFORGE_ISOLATED_TEST", "1")
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(root);
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        return;
    }
    use windows::core::{Interface, PCWSTR};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, IPersistFile, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }
    let root = pipeline::data_dir();
    std::fs::create_dir_all(&root).unwrap();
    let shortcut = root.join("sample.lnk");
    let target = std::env::var("WINDIR").unwrap() + "\\System32\\notepad.exe";
    let old_icon = std::env::var("WINDIR").unwrap() + "\\System32\\shell32.dll";
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap();
        {
            let link: IShellLinkW =
                CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).unwrap();
            link.SetPath(PCWSTR(wide(&target).as_ptr())).unwrap();
            link.SetIconLocation(PCWSTR(wide(&old_icon).as_ptr()), 3)
                .unwrap();
            let persist: IPersistFile = link.cast().unwrap();
            persist
                .Save(PCWSTR(wide(&shortcut.to_string_lossy()).as_ptr()), true)
                .unwrap();
        }
        CoUninitialize();
    }
    let original_bytes = std::fs::read(&shortcut).unwrap();
    let input = root.join("source.png");
    image::RgbaImage::from_pixel(64, 64, image::Rgba([50, 100, 200, 255]))
        .save(&input)
        .unwrap();
    let item = pipeline::ApplyItem {
        lnk_path: shortcut.to_string_lossy().into_owned(),
        source: pipeline::SourceSpec {
            kind: "file".into(),
            artwork_url: String::new(),
            bundle_id: String::new(),
            path: input.to_string_lossy().into_owned(),
        },
        base: "white".into(),
        glyph_scale: 0.76,
        icon_radius: 0.0,
        base_size: 1.0,
        base_exponent: 5.2,
    };
    // Block the snapshots directory to simulate a backup I/O failure.
    std::fs::write(pipeline::snapshots_dir(), b"blocked").unwrap();
    let (results, id) = pipeline::apply_items(&[item.clone()], false, false);
    assert!(!results[0].ok);
    assert!(id.is_none());
    assert_eq!(std::fs::read(&shortcut).unwrap(), original_bytes);
    std::fs::remove_file(pipeline::snapshots_dir()).unwrap();
    let (results, id) = pipeline::apply_items(&[item.clone()], false, false);
    assert!(results[0].ok, "{:?}", results);
    let dir = pipeline::snapshots_dir().join(id.unwrap());
    assert_eq!(
        std::fs::read(dir.join("original-0.lnk")).unwrap(),
        original_bytes
    );
    let manifest = snapshot::read_snapshot(&dir.join("manifest.json")).unwrap();
    assert_eq!(manifest.entries[0].original_icon_index, 3);
    assert_eq!(
        manifest.entries[0].original_icon_path.to_lowercase(),
        old_icon.to_lowercase()
    );
    assert_eq!(
        lnk::parse(&shortcut).unwrap().icon_path,
        results[0].icon_path.clone().unwrap()
    );
    let restored = pipeline::restore_one(&item.lnk_path).unwrap().unwrap();
    assert!(restored.ok);
    let info = lnk::parse(Path::new(&item.lnk_path)).unwrap();
    assert_eq!(info.icon_path.to_lowercase(), old_icon.to_lowercase());
    assert_eq!(info.icon_index, 3);
    // Reapply must also stop if it cannot back up the current shortcut.
    let cached = pipeline::applied_cache_load();
    assert!(cached.contains_key(&item.lnk_path));
    std::fs::rename(pipeline::snapshots_dir(), root.join("saved-snapshots")).unwrap();
    std::fs::write(pipeline::snapshots_dir(), b"blocked").unwrap();
    assert!(pipeline::reapply(&item.lnk_path).is_err());
    let info = lnk::parse(&shortcut).unwrap();
    assert_eq!(info.icon_index, 3);
    assert_eq!(info.icon_path.to_lowercase(), old_icon.to_lowercase());
    // Unblock backups, then deny replacement of a valid cache file. A late
    // cache commit failure must roll the shortcut back and preserve the index.
    std::fs::remove_file(pipeline::snapshots_dir()).unwrap();
    std::fs::rename(root.join("saved-snapshots"), pipeline::snapshots_dir()).unwrap();
    let cache_path = pipeline::applied_cache_path();
    let cache_before = std::fs::read(&cache_path).unwrap();
    use std::os::windows::fs::OpenOptionsExt;
    let cache_lock = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&cache_path)
        .unwrap();
    let (result, backup) = pipeline::apply_items(&[item.clone()], false, false);
    assert!(backup.is_some());
    assert!(!result[0].ok, "{:?}", result);
    assert!(result[0]
        .error
        .as_ref()
        .unwrap()
        .contains("恢复记录保存失败"));
    assert_eq!(std::fs::read(&cache_path).unwrap(), cache_before);
    let rolled_back = lnk::parse(&shortcut).unwrap();
    assert_eq!(
        rolled_back.icon_path.to_lowercase(),
        old_icon.to_lowercase()
    );
    assert_eq!(rolled_back.icon_index, 3);
    drop(cache_lock);
    // Corrupt data is preserved instead of silently being reset to an empty map.
    std::fs::write(&cache_path, b"corrupt recovery index").unwrap();
    let untouched = std::fs::read(&shortcut).unwrap();
    let (result, backup) = pipeline::apply_items(&[item.clone()], false, false);
    assert!(!result[0].ok);
    assert!(backup.is_none());
    assert_eq!(std::fs::read(&shortcut).unwrap(), untouched);
    assert_eq!(
        std::fs::read(&cache_path).unwrap(),
        b"corrupt recovery index"
    );
    assert!(pipeline::reapply(&item.lnk_path).is_err());
    std::fs::write(&cache_path, &cache_before).unwrap();
    // The same worker used by the GUI skips excluded paths and reports progress.
    let excluded = std::collections::HashSet::from([lnk::path_key(&item.lnk_path)]);
    let mut events = Vec::new();
    let report = iconforge::recovery::run(iconforge::recovery::Action::Cache(excluded), |p| {
        events.push((p.done, p.total))
    })
    .unwrap();
    assert_eq!(report.skipped, 1);
    assert!(report.results.is_empty());
    assert_eq!(events.last(), Some(&(1, 1)));
    assert_eq!(std::fs::read(&shortcut).unwrap(), untouched);
    let report = iconforge::recovery::run(
        iconforge::recovery::Action::Reapply(item.lnk_path.clone()),
        |_| {},
    )
    .unwrap();
    assert!(report.results[0].ok);
    events.clear();
    let report = iconforge::recovery::run(iconforge::recovery::Action::All, |p| {
        events.push((p.done, p.total))
    })
    .unwrap();
    assert!(report.results.iter().all(|r| r.ok));
    assert_eq!(
        events.last(),
        Some(&(report.results.len(), report.results.len()))
    );
    let restored = lnk::parse(&shortcut).unwrap();
    assert_eq!(restored.icon_index, 3);
    // New preferences survive a separate load while old config remains compatible.
    let mut config = iconforge::server::AppConfig::default();
    config.excluded_shortcuts = vec![item.lnk_path.clone()];
    config.save().unwrap();
    assert_eq!(
        iconforge::server::AppConfig::load_checked()
            .unwrap()
            .excluded_shortcuts,
        vec![item.lnk_path]
    );
}
