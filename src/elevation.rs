//! Explicit UAC restart. The resume payload contains UI state only, never jobs.
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResumeState {
    pub directory: Option<PathBuf>,
    pub selected: Option<String>,
    pub filter: String,
    pub page: usize,
    pub editing: bool,
    pub style: crate::presets::StylePreset,
    pub file: Option<String>,
    pub pending_image: Option<PathBuf>,
    pub search_query: String,
    #[serde(default)]
    pub file_design: bool,
}

impl ResumeState {
    pub fn encode(&self) -> anyhow::Result<String> {
        self.style.validate()?;
        let token = URL_SAFE_NO_PAD.encode(serde_json::to_vec(self)?);
        anyhow::ensure!(
            token.len() <= 24000,
            "当前编辑内容过长，请先返回列表再启用管理员"
        );
        Ok(token)
    }
    pub fn decode(token: &str) -> anyhow::Result<Self> {
        anyhow::ensure!(token.len() <= 24000, "重启参数过长");
        let state: Self = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(token)?)?;
        state.style.validate()?;
        anyhow::ensure!(matches!(state.page, 0 | 2 | 3 | 4 | 5), "重启页面无效");
        Ok(state)
    }
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Started,
    Cancelled,
}

fn cancelled(code: i32) -> bool {
    code as u32 == 0x800704c7
}

#[cfg(windows)]
pub fn restart(state: &ResumeState) -> anyhow::Result<Outcome> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::{
        ShellExecuteExW, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW,
    };
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let exe = std::env::current_exe()?;
    let file: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
    let directory: Vec<u16> = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("无法读取程序目录"))?
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // URL-safe Base64 contains no quotes, spaces, slashes or shell operators.
    let params: Vec<u16> = format!("gui --resume-ui {}\0", state.encode()?)
        .encode_utf16()
        .collect();
    let verb: Vec<u16> = "runas\0".encode_utf16().collect();
    unsafe {
        let initialized = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
        let mut info = SHELLEXECUTEINFOW::default();
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI;
        info.lpVerb = PCWSTR(verb.as_ptr());
        info.lpFile = PCWSTR(file.as_ptr());
        info.lpDirectory = PCWSTR(directory.as_ptr());
        info.lpParameters = PCWSTR(params.as_ptr());
        info.nShow = SW_SHOWNORMAL.0;
        let result = ShellExecuteExW(&mut info);
        if initialized {
            CoUninitialize();
        }
        match result {
            Ok(()) => Ok(Outcome::Started),
            Err(e) if cancelled(e.code().0) => Ok(Outcome::Cancelled),
            Err(e) => Err(anyhow::anyhow!("无法启动管理员窗口：{e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample() -> ResumeState {
        ResumeState {
            directory: Some(PathBuf::from("C:\\Users\\测试 用户\\Desktop\\")),
            selected: Some("D:\\A & B\\应用.lnk".into()),
            filter: "中文 空格 & \"引号\"".into(),
            page: 0,
            editing: true,
            style: crate::presets::StylePreset::default(),
            file: Some("D:\\图片\\图标.png".into()),
            pending_image: None,
            search_query: "应用".into(),
            file_design: false,
        }
    }
    #[test]
    fn resume_roundtrip_preserves_unicode_and_has_no_command_line_metacharacters() {
        let original = sample();
        let token = original.encode().unwrap();
        assert!(token
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-'));
        let restored = ResumeState::decode(&token).unwrap();
        assert_eq!(restored.directory, original.directory);
        assert_eq!(restored.selected, original.selected);
        assert_eq!(restored.filter, original.filter);
        assert_eq!(restored.file, original.file);
        assert_eq!(restored.style, original.style);
    }
    #[test]
    fn resume_rejects_invalid_parameters_and_never_carries_an_apply_operation() {
        assert!(ResumeState::decode("broken!").is_err());
        assert!(ResumeState::decode(&"a".repeat(24001)).is_err());
        let mut state = sample();
        state.page = 6;
        assert!(ResumeState::decode(&state.encode().unwrap()).is_err());
        state.page = 0;
        state.style.glyph_scale = 5.;
        assert!(state.encode().is_err());
        assert!(cancelled(0x800704c7u32 as i32));
        assert!(!cancelled(0x80070005u32 as i32));
    }
}
