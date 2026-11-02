//! `.lnk` shortcut handling (Windows-only COM) + stub on other platforms.
//!
//! - Enumerate `*.lnk` in a directory, parse each via `IShellLinkW` +
//!   `IPersistFile` (target, icon location).
//! - `set_icon`: `SetIconLocation` + `Save` (restores exactly, unlike
//!   LinkEcho's "point back at the target" restore).
//! - `extract_icon_png`: `ExtractIconExW` + GDI `GetDIBits` -> PNG bytes,
//!   for the "before" preview. Falls back to `None` on any failure.
//! - UWP shortcuts (target empty / `shell:AppsFolder\...`) are listed with
//!   `kind = "uwp"` and marked unrestorable.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShortcutInfo {
    pub path: String,
    pub name: String,
    pub target: String,
    pub icon_path: String,
    pub icon_index: i32,
    /// "normal" | "uwp" | "broken"
    pub kind: String,
}

impl ShortcutInfo {
    fn broken(path: &Path) -> Self {
        Self {
            path: path.to_string_lossy().to_string(),
            name: path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default(),
            target: String::new(),
            icon_path: String::new(),
            icon_index: 0,
            kind: "broken".to_string(),
        }
    }
}

/// List `.lnk` files in `dir` (non-recursive), parsed.
pub fn enumerate(dir: &Path) -> Vec<ShortcutInfo> {
    let mut out = Vec::new();
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return out,
    };
    for entry in rd.flatten() {
        let p = entry.path();
        let is_lnk = p
            .extension()
            .map(|e| e.eq_ignore_ascii_case("lnk"))
            .unwrap_or(false);
        if !is_lnk {
            continue;
        }
        match parse(&p) {
            Ok(info) => out.push(info),
            Err(_) => out.push(ShortcutInfo::broken(&p)),
        }
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out
}

/// Merge distinct shortcut paths; same-name personal/public links stay distinct.
pub fn enumerate_many(directories: &[PathBuf]) -> Vec<ShortcutInfo> {
    let mut seen = std::collections::HashSet::new();
    let mut links = Vec::new();
    for directory in directories {
        for link in enumerate(directory) {
            let key =
                std::fs::canonicalize(&link.path).unwrap_or_else(|_| PathBuf::from(&link.path));
            if seen.insert(key.to_string_lossy().replace('/', "\\").to_lowercase()) {
                links.push(link);
            }
        }
    }
    links.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.path.cmp(&b.path))
    });
    links
}

pub fn path_key(path: &str) -> String {
    path.replace('/', "\\").to_lowercase()
}

/// Start-menu scanning deliberately skips junctions and symbolic directories.
pub fn enumerate_recursive(root: &Path) -> Vec<ShortcutInfo> {
    let mut pending = vec![root.to_path_buf()];
    let mut links = Vec::new();
    let mut seen = std::collections::HashSet::new();
    while let Some(directory) = pending.pop() {
        let key = std::fs::canonicalize(&directory).unwrap_or_else(|_| directory.clone());
        if !seen.insert(key) {
            continue;
        }
        links.extend(enumerate(&directory));
        if let Ok(entries) = std::fs::read_dir(&directory) {
            for entry in entries.flatten() {
                let Ok(metadata) = std::fs::symlink_metadata(entry.path()) else {
                    continue;
                };
                if metadata.is_dir() && !is_directory_link(&metadata) {
                    pending.push(entry.path());
                }
            }
        }
    }
    links.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.path.cmp(&b.path))
    });
    links
}
fn is_directory_link(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

#[cfg(windows)]
fn known_folder(id: &windows::core::GUID) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows::Win32::System::Com::{
        CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::{SHGetKnownFolderPath, KF_FLAG_DEFAULT};
    unsafe {
        let initialized = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
        let result = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None)
            .ok()
            .map(|value| {
                let path = PathBuf::from(std::ffi::OsString::from_wide(value.as_wide()));
                CoTaskMemFree(Some(value.0.cast()));
                path
            });
        if initialized {
            CoUninitialize();
        }
        result
    }
}

pub fn desktop_shortcuts() -> Vec<ShortcutInfo> {
    let directories = known_folders()
        .into_iter()
        .filter(|(name, _)| name.starts_with("桌面"))
        .map(|(_, path)| path)
        .collect::<Vec<_>>();
    enumerate_many(&directories)
}

#[cfg(test)]
mod merged_desktop_tests {
    use super::*;
    #[test]
    fn recursive_scan_finds_nested_links_without_changing_top_level_scans() {
        let root = std::env::temp_dir().join(format!(
            "iconforge-recursive-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("Vendor/Tools")).unwrap();
        std::fs::write(root.join("Top.lnk"), b"fixture").unwrap();
        std::fs::write(root.join("Vendor/Tools/Nested.lnk"), b"fixture").unwrap();
        std::fs::write(root.join("Vendor/Tools/ignored.txt"), b"fixture").unwrap();
        assert_eq!(enumerate(&root).len(), 1);
        let links = enumerate_recursive(&root);
        assert_eq!(links.len(), 2);
        assert!(links.iter().any(|l| l.name == "Nested"));
        assert_eq!(
            path_key("C:/Users/Example.LNK"),
            path_key("c:\\users\\example.lnk")
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn merge_retains_same_name_links_and_deduplicates_repeated_directories() {
        let root = std::env::temp_dir().join(format!(
            "iconforge-desktop-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let personal = root.join("personal");
        let public = root.join("public");
        std::fs::create_dir_all(&personal).unwrap();
        std::fs::create_dir_all(&public).unwrap();
        std::fs::write(personal.join("同名.lnk"), b"fixture").unwrap();
        std::fs::write(public.join("同名.lnk"), b"fixture").unwrap();
        std::fs::write(public.join("仅公共.lnk"), b"fixture").unwrap();
        let result = enumerate_many(&[
            personal.clone(),
            public.clone(),
            personal,
            root.join("missing"),
        ]);
        assert_eq!(result.len(), 3);
        assert_eq!(result.iter().filter(|s| s.name == "同名").count(), 2);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(windows)]
fn to_wide(s: &str) -> Vec<u16> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Fixed MAX_PATH buffer for PrivateExtractIconsW (takes `&[u16; 260]`).
/// Over-long paths truncate (and then fail over to ExtractIconExW).
#[cfg(windows)]
fn to_wide_260(s: &str) -> [u16; 260] {
    let mut buf = [0u16; 260];
    let w = to_wide(s);
    let n = w.len().min(259);
    buf[..n].copy_from_slice(&w[..n]);
    buf
}

#[cfg(windows)]
fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

#[cfg(windows)]
pub fn parse(path: &Path) -> anyhow::Result<ShortcutInfo> {
    use windows::core::{Interface, PCWSTR};
    use windows::Win32::Storage::FileSystem::WIN32_FIND_DATAW;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, IPersistFile, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED, STGM_READ,
    };
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink, SLGP_UNCPRIORITY};

    unsafe {
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        if hr.0 < 0 {
            anyhow::bail!("CoInitializeEx failed: {hr:?}");
        }
        let result = (|| -> anyhow::Result<ShortcutInfo> {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            let persist: IPersistFile = link.cast()?;
            let wpath = to_wide(&path.to_string_lossy());
            persist.Load(PCWSTR(wpath.as_ptr()), STGM_READ)?;

            let mut target_buf = vec![0u16; 1024];
            let mut find_data = std::mem::zeroed::<WIN32_FIND_DATAW>();
            let _ = link.GetPath(&mut target_buf, &mut find_data, SLGP_UNCPRIORITY.0 as u32);
            let target = from_wide(&target_buf);

            let mut icon_buf = vec![0u16; 1024];
            let mut icon_index = 0i32;
            let _ = link.GetIconLocation(&mut icon_buf, &mut icon_index);
            let icon_path = from_wide(&icon_buf);

            let kind = if target.is_empty() || target.to_lowercase().starts_with("shell:") {
                "uwp"
            } else {
                "normal"
            };
            Ok(ShortcutInfo {
                path: path.to_string_lossy().to_string(),
                name: path
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default(),
                target,
                icon_path,
                icon_index,
                kind: kind.to_string(),
            })
        })();
        CoUninitialize();
        result
    }
}

#[cfg(not(windows))]
pub fn parse(_path: &Path) -> anyhow::Result<ShortcutInfo> {
    anyhow::bail!("shortcut parsing is Windows-only")
}

/// Write a new icon location into a shortcut and save it.
#[cfg(windows)]
pub fn set_icon(lnk_path: &Path, icon_path: &str, icon_index: i32) -> anyhow::Result<()> {
    use windows::core::{Interface, PCWSTR};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, IPersistFile, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED, STGM_READWRITE,
    };
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};

    unsafe {
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        if hr.0 < 0 {
            anyhow::bail!("CoInitializeEx failed: {hr:?}");
        }
        let result = (|| -> anyhow::Result<()> {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            let persist: IPersistFile = link.cast()?;
            let wlnk = to_wide(&lnk_path.to_string_lossy());
            // READWRITE: Save must persist the change; STGM_READ is not
            // enough for a reliable write-back.
            persist.Load(PCWSTR(wlnk.as_ptr()), STGM_READWRITE)?;
            let wicon = to_wide(icon_path);
            link.SetIconLocation(PCWSTR(wicon.as_ptr()), icon_index)?;
            persist.Save(PCWSTR::null(), true)?;
            Ok(())
        })();
        CoUninitialize();
        result
    }
}

#[cfg(not(windows))]
pub fn set_icon(_lnk_path: &Path, _icon_path: &str, _icon_index: i32) -> anyhow::Result<()> {
    anyhow::bail!("setting shortcut icons is Windows-only")
}

/// Ask Explorer to refresh icon display (lightweight; no explorer restart).
#[cfg(windows)]
pub fn refresh_icons() {
    use windows::Win32::UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST};
    unsafe {
        SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None);
    }
}

/// Tell Explorer one specific file changed (SHCNE_UPDATEITEM with its
/// PIDL). The global ASSOCCHANGED alone sometimes leaves a desktop icon
/// stale; the per-item notification makes the change show up.
#[cfg(windows)]
pub fn refresh_item(path: &str) {
    use windows::core::PCWSTR;
    use windows::Win32::System::Com::{CoTaskMemFree, IBindCtx};
    use windows::Win32::UI::Shell::{
        Common::ITEMIDLIST, SHChangeNotify, SHParseDisplayName, SHCNE_UPDATEITEM, SHCNF_IDLIST,
    };
    unsafe {
        let wide = to_wide(path);
        let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
        let parsed =
            SHParseDisplayName(PCWSTR(wide.as_ptr()), None::<&IBindCtx>, &mut pidl, 0, None)
                .is_ok();
        if parsed && !pidl.is_null() {
            SHChangeNotify(
                SHCNE_UPDATEITEM,
                SHCNF_IDLIST,
                Some(pidl as *const std::ffi::c_void),
                None,
            );
            CoTaskMemFree(Some(pidl as *const std::ffi::c_void));
        }
    }
}
#[cfg(not(windows))]
pub fn refresh_item(_path: &str) {}

/// True when `path` lives under a system-protected location (Public
/// Desktop / Public Start Menu / ProgramData) whose .lnk files a
/// non-elevated process cannot rewrite (access denied on Save).
pub fn is_protected_folder(path: &Path) -> bool {
    let norm = path.to_string_lossy().to_lowercase().replace('/', "\\");
    ["PUBLIC", "ProgramData"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .map(|v| v.to_lowercase().replace('/', "\\"))
        .any(|root| norm == root || norm.starts_with(&format!("{root}\\")))
}

/// True when this process runs with an elevated (admin) token.
#[cfg(windows)]
pub fn is_elevated() -> bool {
    use windows::Win32::UI::Shell::IsUserAnAdmin;
    unsafe { IsUserAnAdmin().as_bool() }
}
#[cfg(not(windows))]
pub fn is_elevated() -> bool {
    false
}

#[cfg(not(windows))]
pub fn refresh_icons() {}

/// Extract an icon from a file (EXE/DLL/ICO) at `index` as PNG bytes.
/// Returns `None` on any failure (caller falls back to a placeholder).
#[cfg(windows)]
pub fn extract_icon_png(file: &str, index: i32, size: u32) -> Option<Vec<u8>> {
    use windows::core::PCWSTR;
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
        SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HGDIOBJ,
    };
    use windows::Win32::UI::Shell::ExtractIconExW;
    use windows::Win32::UI::WindowsAndMessaging::{
        DestroyIcon, DrawIconEx, PrivateExtractIconsW, DI_NORMAL, HICON,
    };

    unsafe {
        let wfile = to_wide(file);
        let s = size as i32;
        // Native-size extraction first: PrivateExtractIcons with the
        // requested size returns the embedded 256x256 icon from modern
        // EXEs. ExtractIconExW only ever returns the 32x32 system icon,
        // which upscales soft -- keep it as the fallback.
        let mut hicons = [HICON(std::ptr::null_mut()); 1];
        let n_priv =
            PrivateExtractIconsW(&to_wide_260(file), index, s, s, Some(&mut hicons), None, 0);
        let hicon = if n_priv >= 1 && !hicons[0].is_invalid() {
            hicons[0]
        } else {
            let mut large = HICON(std::ptr::null_mut());
            let mut small = HICON(std::ptr::null_mut());
            let n = ExtractIconExW(
                PCWSTR(wfile.as_ptr()),
                index,
                Some(&mut large),
                Some(&mut small),
                1,
            );
            if n == 0 {
                return None;
            }
            let h = if !large.is_invalid() { large } else { small };
            if h.is_invalid() {
                return None;
            }
            h
        };
        let out = (|| -> Option<Vec<u8>> {
            // DrawIconEx scales the icon to the requested size. The old
            // code read the icon's own bitmap with GetDIBits at `size`:
            // GetDIBits does NOT scale, so a 32x32 system icon came back
            // as a tiny glyph in a mostly-transparent 256x256 canvas --
            // which is why list icons rendered as unreadable dots.
            let mut bmi = std::mem::zeroed::<BITMAPINFO>();
            bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = s;
            bmi.bmiHeader.biHeight = -s; // top-down
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = BI_RGB.0;
            let hdc_screen = GetDC(None);
            let hdc = CreateCompatibleDC(Some(hdc_screen));
            if hdc.is_invalid() {
                ReleaseDC(None, hdc_screen);
                return None;
            }
            let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
            let hbm = match CreateDIBSection(Some(hdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
                Ok(b) => b,
                Err(_) => {
                    let _ = DeleteDC(hdc);
                    ReleaseDC(None, hdc_screen);
                    return None;
                }
            };
            if bits.is_null() {
                let _ = DeleteObject(HGDIOBJ(hbm.0));
                let _ = DeleteDC(hdc);
                ReleaseDC(None, hdc_screen);
                return None;
            }
            let old = SelectObject(hdc, HGDIOBJ(hbm.0));
            let drawn = DrawIconEx(hdc, 0, 0, hicon, s, s, 0, None, DI_NORMAL).is_ok();
            SelectObject(hdc, old);
            let mut pixels = if drawn {
                let n = (size * size * 4) as usize;
                let mut buf = vec![0u8; n];
                std::ptr::copy_nonoverlapping(bits as *const u8, buf.as_mut_ptr(), n);
                Some(buf)
            } else {
                None
            };
            let _ = DeleteObject(HGDIOBJ(hbm.0));
            let _ = DeleteDC(hdc);
            ReleaseDC(None, hdc_screen);
            let mut pixels = pixels.take()?;
            // BGRA -> RGBA.
            for px in pixels.chunks_exact_mut(4) {
                px.swap(0, 2);
            }
            let img = image::RgbaImage::from_raw(size, size, pixels)?;
            let mut png = Vec::new();
            let mut cursor = std::io::Cursor::new(&mut png);
            img.write_to(&mut cursor, image::ImageFormat::Png).ok()?;
            Some(png)
        })();
        let _ = DestroyIcon(hicon);
        out
    }
}

#[cfg(not(windows))]
pub fn extract_icon_png(_file: &str, _index: i32, _size: u32) -> Option<Vec<u8>> {
    None
}

/// Well-known shortcut folders on Windows. Empty on other platforms.
pub fn known_folders() -> Vec<(String, PathBuf)> {
    #[cfg(windows)]
    {
        use windows::Win32::UI::Shell::{
            FOLDERID_CommonPrograms, FOLDERID_Desktop, FOLDERID_Programs, FOLDERID_PublicDesktop,
        };
        let mut out = Vec::new();
        let home = std::env::var("USERPROFILE").unwrap_or_default();
        let public = std::env::var("PUBLIC").unwrap_or_default();
        let appdata = std::env::var("APPDATA").unwrap_or_default();
        let program_data = std::env::var("ProgramData").unwrap_or_default();
        if !home.is_empty() {
            out.push((
                "桌面（个人）".to_string(),
                known_folder(&FOLDERID_Desktop)
                    .unwrap_or_else(|| PathBuf::from(&home).join("Desktop")),
            ));
        }
        if !public.is_empty() {
            out.push((
                "桌面（公共）".to_string(),
                known_folder(&FOLDERID_PublicDesktop)
                    .unwrap_or_else(|| PathBuf::from(&public).join("Desktop")),
            ));
        }
        if !appdata.is_empty() {
            out.push((
                "开始菜单（个人）".to_string(),
                known_folder(&FOLDERID_Programs).unwrap_or_else(|| {
                    PathBuf::from(&appdata).join("Microsoft\\Windows\\Start Menu\\Programs")
                }),
            ));
        }
        if !program_data.is_empty() {
            out.push((
                "开始菜单（公共）".to_string(),
                known_folder(&FOLDERID_CommonPrograms).unwrap_or_else(|| {
                    PathBuf::from(&program_data).join("Microsoft\\Windows\\Start Menu\\Programs")
                }),
            ));
        }
        out
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}
