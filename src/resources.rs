//! Read-only native resource icon browsing. No executable modification or execution.
use anyhow::{ensure, Context};
use std::path::{Path, PathBuf};
pub struct Page {
    pub file: PathBuf,
    pub total: u32,
    pub start: u32,
    pub icons: Vec<(i32, Vec<u8>)>,
}
pub const PAGE_SIZE: u32 = 24;
pub fn page(file: &Path, start: u32) -> anyhow::Result<Page> {
    ensure!(
        matches!(
            file.extension()
                .and_then(|s| s.to_str())
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Some("exe" | "dll" | "icl" | "ico")
        ),
        "请选择 EXE、DLL、ICL 或 ICO 文件"
    );
    ensure!(file.is_file(), "资源文件不存在");
    #[cfg(windows)]
    let total = unsafe {
        use std::os::windows::ffi::OsStrExt;
        let path: Vec<u16> = file.as_os_str().encode_wide().chain(Some(0)).collect();
        windows::Win32::UI::Shell::ExtractIconExW(
            windows::core::PCWSTR(path.as_ptr()),
            -1,
            None,
            None,
            0,
        )
    };
    #[cfg(not(windows))]
    let total = 0;
    ensure!(total > 0 && total != u32::MAX, "未找到可读取的图标资源");
    ensure!(start < total && total <= 65536, "资源索引超出范围");
    let file_string = file.to_string_lossy();
    let icons = (start..(start + PAGE_SIZE).min(total))
        .filter_map(|i| {
            crate::lnk::extract_icon_png(&file_string, i as i32, 64).map(|png| (i as i32, png))
        })
        .collect();
    Ok(Page {
        file: file.to_owned(),
        total,
        start,
        icons,
    })
}
pub fn select(file: &Path, index: i32, uploads: &Path) -> anyhow::Result<PathBuf> {
    ensure!(index >= 0, "资源索引无效");
    let png = crate::lnk::extract_icon_png(&file.to_string_lossy(), index, 256)
        .context("无法读取所选资源图标")?;
    let name = format!(
        "resource-{}-{}.png",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    );
    let stem = file
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, '-' | '_'))
        .take(40)
        .collect::<String>();
    let path = uploads
        .join(name.trim_end_matches(".png"))
        .join(format!("{stem}-图标{index}.png"));
    crate::webdav::atomic_write(&path, &png)?;
    Ok(path)
}
#[cfg(all(test, windows))]
mod tests {
    use super::*;
    #[test]
    fn native_resource_selection_leaves_source_unchanged() {
        let source =
            PathBuf::from(std::env::var_os("WINDIR").unwrap()).join("System32/shell32.dll");
        let before = std::fs::metadata(&source).unwrap();
        let page = page(&source, 0).unwrap();
        assert!(page.total > 24);
        assert!(!page.icons.is_empty());
        let root = std::env::temp_dir().join(format!("iconforge-resource-{}", std::process::id()));
        let output = select(&source, page.icons[0].0, &root).unwrap();
        assert_eq!(image::open(output).unwrap().width(), 256);
        assert_eq!(
            std::fs::metadata(&source).unwrap().modified().unwrap(),
            before.modified().unwrap()
        );
        assert!(super::page(&source, page.total).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
