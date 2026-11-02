//! Open folders via Windows Shell associations, independent of PATH.
use anyhow::Context;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

#[derive(Clone, Copy)]
pub enum Folder {
    Apps,
    Icons,
}

pub fn open_export_location(file: &std::path::Path) -> anyhow::Result<()> {
    let directory = file.parent().context("导出文件缺少父目录")?;
    anyhow::ensure!(directory.is_dir(), "导出目录已不存在");
    open(directory.as_os_str())
}

pub fn open_folder(folder: Folder) -> anyhow::Result<()> {
    match folder {
        Folder::Apps => open(OsStr::new("shell:AppsFolder")),
        Folder::Icons => {
            let directory = crate::pipeline::icons_dir();
            std::fs::create_dir_all(&directory).context("无法创建图标目录")?;
            open(directory.as_os_str())
        }
    }
}

// Called on a dedicated worker: resolving a Shell namespace can block.
fn open(target: &OsStr) -> anyhow::Result<()> {
    use windows::core::PCWSTR;
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::{
        ShellExecuteExW, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW,
    };
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let file: Vec<u16> = target.encode_wide().chain(Some(0)).collect();
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED)
            .ok()
            .context("无法初始化 Windows Shell")?;
        let mut info = SHELLEXECUTEINFOW::default();
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI;
        info.lpFile = PCWSTR(file.as_ptr());
        info.nShow = SW_SHOWNORMAL.0;
        let result = ShellExecuteExW(&mut info);
        CoUninitialize();
        result.context("Windows 无法打开此文件夹")
    }
}
