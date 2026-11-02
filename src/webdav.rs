//! Manual WebDAV backups. Credentials are local-only; archives contain
//! application data and never execute shortcut files or apply desktop changes.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context};
use base64::{engine::general_purpose::STANDARD, Engine};
use reqwest::{
    blocking::{Client, RequestBuilder, Response},
    Method, Url,
};
use serde::{Deserialize, Serialize};

use crate::{pipeline, server::AppConfig, snapshot::SnapshotManifest};

const MAX_RAW: usize = 64 * 1024 * 1024;
const MAX_TRANSFER: usize = 96 * 1024 * 1024;
const MAX_FILES: usize = 10_000;
const GROUPS: [&str; 3] = ["icons", "uploads", "snapshots"];

#[derive(Clone, Default)]
pub struct Settings {
    pub url: String,
    pub username: String,
    pub password: String,
}

#[derive(Default, Serialize, Deserialize)]
struct SavedSettings {
    #[serde(default)]
    url: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    protected_password: String,
}

fn stamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .to_string()
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let parent = path.parent().context("文件缺少父目录")?;
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".iconforge-{}.tmp", stamp()));
    let result = (|| -> anyhow::Result<()> {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        #[cfg(windows)]
        unsafe {
            use windows::core::PCWSTR;
            use windows::Win32::Storage::FileSystem::{
                MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
            };
            let from: Vec<u16> = tmp
                .as_os_str()
                .to_string_lossy()
                .encode_utf16()
                .chain(Some(0))
                .collect();
            let to: Vec<u16> = path
                .as_os_str()
                .to_string_lossy()
                .encode_utf16()
                .chain(Some(0))
                .collect();
            MoveFileExW(
                PCWSTR(from.as_ptr()),
                PCWSTR(to.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )?;
        }
        #[cfg(not(windows))]
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    result
}

#[cfg(windows)]
fn crypt(input: &[u8], decrypt: bool) -> anyhow::Result<Vec<u8>> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    let blob = CRYPT_INTEGER_BLOB {
        cbData: input.len().try_into()?,
        pbData: input.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        if decrypt {
            CryptUnprotectData(
                &blob,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )?;
        } else {
            CryptProtectData(
                &blob,
                PCWSTR::null(),
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )?;
        }
        let bytes = if output.cbData == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec()
        };
        let _ = LocalFree(Some(HLOCAL(output.pbData.cast())));
        Ok(bytes)
    }
}

#[cfg(not(windows))]
fn crypt(_: &[u8], _: bool) -> anyhow::Result<Vec<u8>> {
    bail!("保存密码需要 Windows DPAPI")
}

impl Settings {
    pub fn load() -> anyhow::Result<Self> {
        let path = pipeline::data_dir().join("webdav.json");
        if !path.exists() {
            return Ok(Self::default());
        }
        let saved: SavedSettings =
            serde_json::from_slice(&std::fs::read(path)?).context("WebDAV 配置损坏")?;
        let password = if saved.protected_password.is_empty() {
            String::new()
        } else {
            String::from_utf8(
                crypt(&STANDARD.decode(&saved.protected_password)?, true)
                    .context("无法解密 WebDAV 密码，请重新填写配置")?,
            )?
        };
        Ok(Self {
            url: saved.url,
            username: saved.username,
            password,
        })
    }

    pub fn save(&self) -> anyhow::Result<()> {
        self.endpoint()?;
        let saved = SavedSettings {
            url: self.url.trim().into(),
            username: self.username.clone(),
            protected_password: if self.password.is_empty() {
                String::new()
            } else {
                STANDARD.encode(crypt(self.password.as_bytes(), false)?)
            },
        };
        atomic_write(
            &pipeline::data_dir().join("webdav.json"),
            &serde_json::to_vec_pretty(&saved)?,
        )
    }

    fn endpoint(&self) -> anyhow::Result<Url> {
        let mut url =
            Url::parse(self.url.trim()).map_err(|_| anyhow::anyhow!("请输入完整 WebDAV 地址"))?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            bail!("WebDAV 地址必须以 http:// 或 https:// 开头");
        }
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            bail!("地址中不要包含账号、密码、查询参数或 #；请在下方填写账号密码");
        }
        if !url.path().ends_with('/') {
            let path = format!("{}/", url.path());
            url.set_path(&path);
        }
        Ok(url.join("IconForge/")?)
    }
}

struct Dav {
    client: Client,
    root: Url,
    settings: Settings,
}

fn network_error(e: reqwest::Error) -> anyhow::Error {
    // Do not expose endpoint URLs, credentials, query strings, or response bodies.
    if e.is_timeout() {
        anyhow::anyhow!("WebDAV 请求超时，请检查网络后重试")
    } else if e.is_connect() {
        anyhow::anyhow!("无法连接 WebDAV，请检查地址、代理和证书")
    } else {
        anyhow::anyhow!("WebDAV 网络请求失败，请检查服务器状态")
    }
}

impl Dav {
    fn new(settings: &Settings) -> anyhow::Result<Self> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(120))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(network_error)?;
        Ok(Self {
            client,
            root: settings.endpoint()?,
            settings: settings.clone(),
        })
    }
    fn request(&self, method: Method, name: &str) -> anyhow::Result<RequestBuilder> {
        let url = if name.is_empty() {
            self.root.clone()
        } else {
            self.root.join(name)?
        };
        let request = self.client.request(method, url);
        Ok(
            if self.settings.username.is_empty() && self.settings.password.is_empty() {
                request
            } else {
                request.basic_auth(&self.settings.username, Some(&self.settings.password))
            },
        )
    }
    fn check(response: Response, what: &str) -> anyhow::Result<Response> {
        let code = response.status().as_u16();
        if response.status().is_success() {
            return Ok(response);
        }
        match code {
            401 | 403 => bail!("{what}失败：账号、应用专用密码或目录权限不正确（HTTP {code}）"),
            404 => bail!("{what}失败：目录或备份不存在（HTTP 404）"),
            409 => bail!("{what}失败：请先在服务器上创建所填地址对应的父目录（HTTP 409）"),
            301 | 302 | 307 | 308 => {
                bail!("{what}失败：服务器要求跳转，请填写最终 WebDAV 地址（HTTP {code}）")
            }
            _ => bail!("{what}失败（HTTP {code}）"),
        }
    }
    fn ensure_collection(&self) -> anyhow::Result<()> {
        let response = self
            .request(Method::from_bytes(b"MKCOL")?, "")?
            .send()
            .map_err(network_error)?;
        if response.status().as_u16() != 405 {
            Self::check(response, "创建备份目录")?;
        }
        let response = self.request(Method::from_bytes(b"PROPFIND")?, "")?
            .header("Depth", "0").header("Content-Type", "application/xml; charset=utf-8")
            .body("<?xml version=\"1.0\"?><d:propfind xmlns:d=\"DAV:\"><d:prop><d:resourcetype/></d:prop></d:propfind>")
            .send().map_err(network_error)?;
        if response.status().as_u16() != 207 {
            Self::check(response, "读取 WebDAV 目录")?;
            bail!("服务器未返回 WebDAV 目录响应，请确认填写的是 WebDAV 地址");
        }
        Ok(())
    }
    fn get(&self, name: &str) -> anyhow::Result<Vec<u8>> {
        let response = Self::check(
            self.request(Method::GET, name)?
                .send()
                .map_err(network_error)?,
            "下载备份",
        )?;
        if response.content_length().unwrap_or(0) > MAX_TRANSFER as u64 {
            bail!("远程备份超过 96 MiB 上限");
        }
        let mut bytes = Vec::new();
        response
            .take(MAX_TRANSFER as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_TRANSFER {
            bail!("远程备份超过 96 MiB 上限");
        }
        Ok(bytes)
    }
    fn put(&self, name: &str, bytes: Vec<u8>) -> anyhow::Result<()> {
        Self::check(
            self.request(Method::PUT, name)?
                .header("Content-Type", "application/json")
                .body(bytes)
                .send()
                .map_err(network_error)?,
            "上传备份",
        )?;
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct Archive {
    format: String,
    version: u32,
    created_at: u64,
    source_root: String,
    files: BTreeMap<String, String>,
}
#[derive(Serialize, Deserialize)]
struct Latest {
    version: u32,
    file: String,
}

pub struct Downloaded {
    archive: Archive,
    pub file_count: usize,
    pub total_bytes: usize,
    pub source_root: String,
}

pub fn test_connection(settings: &Settings) -> anyhow::Result<String> {
    Dav::new(settings)?.ensure_collection()?;
    Ok("连接成功：WebDAV 备份目录可访问".into())
}

fn reject_link(path: &Path) -> anyhow::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        bail!("不支持符号链接：{}", path.display());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            bail!("不支持重解析点：{}", path.display());
        }
    }
    Ok(())
}

fn allowed_path(name: &str) -> bool {
    let parts: Vec<_> = name.split('/').collect();
    if parts.iter().any(|p| {
        p.is_empty()
            || p.len() > 240
            || *p == "."
            || *p == ".."
            || p.ends_with([' ', '.'])
            || p.chars()
                .any(|c| c.is_control() || "\\:<>\"|?*".contains(c))
            || matches!(
                p.split('.')
                    .next()
                    .unwrap_or("")
                    .to_ascii_uppercase()
                    .as_str(),
                "CON"
                    | "PRN"
                    | "AUX"
                    | "NUL"
                    | "COM1"
                    | "COM2"
                    | "COM3"
                    | "COM4"
                    | "COM5"
                    | "COM6"
                    | "COM7"
                    | "COM8"
                    | "COM9"
                    | "LPT1"
                    | "LPT2"
                    | "LPT3"
                    | "LPT4"
                    | "LPT5"
                    | "LPT6"
                    | "LPT7"
                    | "LPT8"
                    | "LPT9"
            )
    }) {
        return false;
    }
    if parts.len() > 18 {
        return false;
    }
    if parts.len() == 1 {
        return matches!(name, "config.json" | "applied.json");
    }
    GROUPS.contains(&parts[0])
}

fn read_bounded(path: &Path) -> anyhow::Result<Vec<u8>> {
    reject_link(path)?;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_RAW as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_RAW {
        bail!("备份文件过大：{}", path.display());
    }
    Ok(bytes)
}

fn collect(
    root: &Path,
    dir: &Path,
    files: &mut BTreeMap<String, String>,
    total: &mut usize,
) -> anyhow::Result<()> {
    reject_link(dir)?;
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        reject_link(&path)?;
        let name = path
            .strip_prefix(root)?
            .to_string_lossy()
            .replace('\\', "/");
        if !allowed_path(&name) {
            bail!("备份包含不支持的路径：{name}");
        }
        if path.is_dir() {
            collect(root, &path, files, total)?;
        } else {
            let bytes = read_bounded(&path)?;
            *total += bytes.len();
            if *total > MAX_RAW || files.len() >= MAX_FILES {
                bail!("备份超过 64 MiB 或 10000 个文件上限");
            }
            files.insert(name, STANDARD.encode(bytes));
        }
    }
    Ok(())
}

fn pack(root: &Path) -> anyhow::Result<Archive> {
    let mut files = BTreeMap::new();
    let mut total = 0;
    if root.exists() {
        reject_link(root)?;
    }
    for group in GROUPS {
        let dir = root.join(group);
        if dir.exists() {
            collect(root, &dir, &mut files, &mut total)?;
        }
    }
    // Serialize known preferences only: obsolete config fields may contain old API keys.
    let config = if root.join("config.json").exists() {
        serde_json::from_slice::<AppConfig>(&read_bounded(&root.join("config.json"))?)?
    } else {
        AppConfig::default()
    };
    files.insert(
        "config.json".into(),
        STANDARD.encode(serde_json::to_vec(&config)?),
    );
    if root.join("applied.json").exists() {
        files.insert(
            "applied.json".into(),
            STANDARD.encode(read_bounded(&root.join("applied.json"))?),
        );
    }
    let archive = Archive {
        format: "iconforge-webdav".into(),
        version: 1,
        created_at: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        source_root: root.to_string_lossy().into_owned(),
        files,
    };
    validate(&archive)?;
    Ok(archive)
}

fn validate(archive: &Archive) -> anyhow::Result<BTreeMap<String, Vec<u8>>> {
    if archive.format != "iconforge-webdav" || archive.version != 1 {
        bail!("备份格式或版本不受支持");
    }
    if archive.source_root.len() < 3 || archive.files.is_empty() || archive.files.len() > MAX_FILES
    {
        bail!("备份目录或文件数量无效");
    }
    let mut files = BTreeMap::new();
    let mut names = HashSet::new();
    let mut total = 0;
    for (name, encoded) in &archive.files {
        if !allowed_path(name) || !names.insert(name.to_lowercase()) {
            bail!("备份路径不合法或存在大小写冲突：{name}");
        }
        let bytes = STANDARD.decode(encoded).context("备份内容编码无效")?;
        total += bytes.len();
        if total > MAX_RAW {
            bail!("备份内容超过 64 MiB 上限");
        }
        if name == "config.json" {
            serde_json::from_slice::<AppConfig>(&bytes).context("备份配置无效")?;
        }
        if name == "applied.json" {
            serde_json::from_slice::<HashMap<String, pipeline::AppliedEntry>>(&bytes)
                .context("备份应用缓存无效")?;
        }
        if name.starts_with("snapshots/") && name.ends_with("/manifest.json") {
            let m: SnapshotManifest = serde_json::from_slice(&bytes).context("备份快照无效")?;
            if name != &format!("snapshots/{}/manifest.json", m.created_at) {
                bail!("快照目录与标识不一致");
            }
        }
        files.insert(name.clone(), bytes);
    }
    // A file cannot simultaneously be an ancestor directory of another file.
    for name in files.keys() {
        let mut prefix = String::new();
        for part in name.split('/').take(name.split('/').count() - 1) {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            if names.contains(&prefix.to_lowercase()) {
                bail!("备份文件和目录冲突");
            }
        }
    }
    Ok(files)
}

pub fn upload(settings: &Settings) -> anyhow::Result<String> {
    upload_from(settings, &pipeline::data_dir())
}
fn upload_from(settings: &Settings, root: &Path) -> anyhow::Result<String> {
    let archive = pack(root)?;
    let bytes = serde_json::to_vec(&archive)?;
    if bytes.len() > MAX_TRANSFER {
        bail!("备份包超过 96 MiB 上限");
    }
    let dav = Dav::new(settings)?;
    dav.ensure_collection()?;
    let name = format!("backup-{}.json", stamp());
    dav.put(&name, bytes)?;
    // Publish the pointer only after the full independent backup is stored.
    dav.put(
        "latest.json",
        serde_json::to_vec(&Latest {
            version: 1,
            file: name,
        })?,
    )?;
    Ok(format!(
        "上传完成：{} 个文件，历史备份已保留",
        archive.files.len()
    ))
}

pub fn download(settings: &Settings) -> anyhow::Result<Downloaded> {
    let dav = Dav::new(settings)?;
    let latest: Latest =
        serde_json::from_slice(&dav.get("latest.json")?).context("远程备份索引无效")?;
    if latest.version != 1
        || !latest.file.starts_with("backup-")
        || !latest.file.ends_with(".json")
        || latest.file.len() > 80
        || !latest
            .file
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
    {
        bail!("远程备份文件名无效");
    }
    let archive: Archive =
        serde_json::from_slice(&dav.get(&latest.file)?).context("远程备份数据无效")?;
    let files = validate(&archive)?;
    Ok(Downloaded {
        file_count: files.len(),
        total_bytes: files.values().map(Vec::len).sum(),
        source_root: archive.source_root.clone(),
        archive,
    })
}

fn rebase(value: &str, old: &str, new: &str) -> String {
    if value == old {
        return new.into();
    }
    if let Some(tail) = value.strip_prefix(old) {
        if tail.starts_with(['/', '\\']) {
            return format!("{new}{tail}");
        }
    }
    value.into()
}
fn rebase_json(value: serde_json::Value, old: &str, new: &str) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => serde_json::Value::String(rebase(&s, old, new)),
        serde_json::Value::Array(a) => {
            serde_json::Value::Array(a.into_iter().map(|v| rebase_json(v, old, new)).collect())
        }
        serde_json::Value::Object(o) => serde_json::Value::Object(
            o.into_iter()
                .map(|(k, v)| (rebase(&k, old, new), rebase_json(v, old, new)))
                .collect(),
        ),
        other => other,
    }
}
fn safe_destination(root: &Path, name: &str) -> anyhow::Result<PathBuf> {
    let mut path = root.to_path_buf();
    // Also reject a linked ancestor of the application data directory.
    for ancestor in root.ancestors() {
        if ancestor.exists() {
            reject_link(ancestor)?;
        }
    }
    for part in name.split('/') {
        path.push(part);
        if std::fs::symlink_metadata(&path).is_ok() {
            reject_link(&path)?;
        }
    }
    Ok(path)
}

pub fn restore(downloaded: Downloaded) -> anyhow::Result<String> {
    restore_into(downloaded, &pipeline::data_dir())
}
fn restore_into(downloaded: Downloaded, root: &Path) -> anyhow::Result<String> {
    let mut files = validate(&downloaded.archive)?;
    let old = &downloaded.archive.source_root;
    let new = root.to_string_lossy();
    for (name, bytes) in &mut files {
        if name == "config.json" {
            *bytes = serde_json::to_vec_pretty(&serde_json::from_slice::<AppConfig>(bytes)?)?;
        } else if name == "applied.json"
            || (name.starts_with("snapshots/") && name.ends_with("/manifest.json"))
        {
            let value = serde_json::from_slice(bytes)?;
            *bytes = serde_json::to_vec_pretty(&rebase_json(value, old, &new))?;
        }
        let path = safe_destination(root, name)?;
        if path.is_dir() {
            bail!("恢复目标是文件夹：{name}");
        }
    }
    let backup = root.join("webdav-restore-backups").join(stamp());
    safe_destination(
        root,
        &format!(
            "webdav-restore-backups/{}",
            backup.file_name().unwrap().to_string_lossy()
        ),
    )?;
    std::fs::create_dir_all(&backup)?;
    // Snapshot every overwritten file before any write; missing originals are recorded as None.
    let mut previous = BTreeMap::new();
    let mut previous_bytes = 0;
    for name in files.keys() {
        let path = safe_destination(root, name)?;
        if path.exists() {
            let data = read_bounded(&path)?;
            previous_bytes += data.len();
            if previous_bytes > MAX_RAW {
                bail!("待覆盖的本地数据超过 64 MiB，已停止恢复");
            }
            atomic_write(&backup.join(name), &data)?;
            previous.insert(name.clone(), Some(data));
        } else {
            previous.insert(name.clone(), None);
        }
    }
    let mut committed: Vec<String> = Vec::new();
    for (name, bytes) in &files {
        let outcome = safe_destination(root, name).and_then(|path| atomic_write(&path, bytes));
        if let Err(e) = outcome {
            let mut rollback_failed = false;
            for name in committed.iter().rev() {
                if let Ok(path) = safe_destination(root, name) {
                    let result = match &previous[name] {
                        Some(data) => atomic_write(&path, data),
                        None => std::fs::remove_file(path).map_err(anyhow::Error::from),
                    };
                    if result.is_err() {
                        rollback_failed = true;
                    }
                } else {
                    rollback_failed = true;
                }
            }
            bail!(
                "恢复失败{}；原文件备份在 {}：{e:#}",
                if rollback_failed {
                    "，部分文件需手动回退"
                } else {
                    "，已回退文件修改"
                },
                backup.display()
            );
        }
        committed.push(name.clone());
    }
    Ok(format!(
        "已恢复 {} 个数据文件；恢复前副本：{}。桌面图标未自动修改。",
        files.len(),
        backup.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Temp(PathBuf);
    impl Temp {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!("iconforge-dav-{label}-{}", stamp()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn seed(root: &Path) {
        std::fs::create_dir_all(root.join("icons")).unwrap();
        std::fs::write(root.join("icons/sample.ico"), b"icon bytes").unwrap();
        std::fs::write(
            root.join("config.json"),
            br#"{"dark_mode":true,"ui_scale":1.1,"ai_key":"do-not-upload"}"#,
        )
        .unwrap();
        std::fs::write(root.join("webdav.json"), b"local credentials only").unwrap();
        let mut entries = HashMap::new();
        entries.insert(
            "C:\\Desktop\\App.lnk".to_string(),
            pipeline::AppliedEntry {
                icon_path: root.join("icons/sample.ico").to_string_lossy().into_owned(),
                applied_at: 1,
                base: "white".into(),
                glyph_scale: 0.76,
                icon_radius: 0.0,
                base_size: 1.0,
                base_exponent: 5.2,
            },
        );
        std::fs::write(
            root.join("applied.json"),
            serde_json::to_vec(&entries).unwrap(),
        )
        .unwrap();
        let manifest = SnapshotManifest {
            created_at: "snap-1".into(),
            entries: vec![crate::snapshot::SnapshotEntry {
                lnk_path: "C:\\Desktop\\App.lnk".into(),
                target_path: "C:\\App.exe".into(),
                original_icon_path: "C:\\old.dll".into(),
                original_icon_index: 3,
                new_icon_path: root.join("icons/sample.ico").to_string_lossy().into_owned(),
            }],
        };
        crate::snapshot::write_snapshot(&root.join("snapshots"), &manifest).unwrap();
        std::fs::write(
            root.join("snapshots/snap-1/original-0.lnk"),
            b"shortcut backup",
        )
        .unwrap();
    }
    fn downloaded(archive: Archive) -> Downloaded {
        let files = validate(&archive).unwrap();
        Downloaded {
            file_count: files.len(),
            total_bytes: files.values().map(Vec::len).sum(),
            source_root: archive.source_root.clone(),
            archive,
        }
    }
    #[test]
    fn archive_restores_paths_and_preserves_local_credentials_and_originals() {
        let source = Temp::new("source");
        seed(&source.0);
        let archive = pack(&source.0).unwrap();
        assert!(!archive.files.contains_key("webdav.json"));
        assert!(
            !String::from_utf8(STANDARD.decode(&archive.files["config.json"]).unwrap())
                .unwrap()
                .contains("do-not-upload")
        );
        let target = Temp::new("target");
        std::fs::create_dir_all(target.0.join("icons")).unwrap();
        std::fs::write(target.0.join("icons/sample.ico"), b"previous icon").unwrap();
        std::fs::write(target.0.join("webdav.json"), b"keep these credentials").unwrap();
        restore_into(downloaded(archive), &target.0).unwrap();
        assert_eq!(
            std::fs::read(target.0.join("icons/sample.ico")).unwrap(),
            b"icon bytes"
        );
        assert_eq!(
            std::fs::read(target.0.join("webdav.json")).unwrap(),
            b"keep these credentials"
        );
        let applied: HashMap<String, pipeline::AppliedEntry> =
            serde_json::from_slice(&std::fs::read(target.0.join("applied.json")).unwrap()).unwrap();
        assert_eq!(
            applied["C:\\Desktop\\App.lnk"].icon_path,
            target.0.join("icons/sample.ico").to_string_lossy()
        );
        let manifest =
            crate::snapshot::read_snapshot(&target.0.join("snapshots/snap-1/manifest.json"))
                .unwrap();
        assert_eq!(manifest.entries[0].original_icon_index, 3);
        assert_eq!(
            manifest.entries[0].new_icon_path,
            target.0.join("icons/sample.ico").to_string_lossy()
        );
        let backups: Vec<_> = std::fs::read_dir(target.0.join("webdav-restore-backups"))
            .unwrap()
            .collect();
        assert_eq!(backups.len(), 1);
        let backup = backups[0].as_ref().unwrap().path();
        assert_eq!(
            std::fs::read(backup.join("icons/sample.ico")).unwrap(),
            b"previous icon"
        );
    }
    #[test]
    fn untrusted_archive_paths_and_case_collisions_are_rejected() {
        for name in [
            "../config.json",
            "icons/../../outside",
            "icons/C:/x",
            "icons\\x",
            "icons/NUL.txt",
            "icons/x.",
            "webdav.json",
            "/icons/x",
        ] {
            assert!(!allowed_path(name), "{name}");
        }
        let root = Temp::new("validation");
        seed(&root.0);
        let mut archive = pack(&root.0).unwrap();
        archive
            .files
            .insert("icons/SAMPLE.ico".into(), STANDARD.encode(b"other"));
        assert!(validate(&archive).is_err());
        archive.files.remove("icons/SAMPLE.ico");
        archive
            .files
            .insert("icons/sample.ico/child".into(), STANDARD.encode(b"other"));
        assert!(validate(&archive).is_err());
    }
    #[test]
    fn named_style_presets_survive_cloud_archive_restore() {
        let source = Temp::new("preset-source");
        seed(&source.0);
        let mut config = AppConfig::default();
        let mut preset = crate::presets::StylePreset::default();
        preset.name = "蓝底大图标".into();
        preset.base = "custom:#2299FF".into();
        preset.glyph_scale = 1.1;
        config.style_presets.push(preset.clone());
        config.excluded_shortcuts.push("C:\\Users\\demo\\Desktop\\Keep.lnk".into());
        std::fs::write(
            source.0.join("config.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        let target = Temp::new("preset-target");
        restore_into(downloaded(pack(&source.0).unwrap()), &target.0).unwrap();
        let restored: AppConfig =
            serde_json::from_slice(&std::fs::read(target.0.join("config.json")).unwrap()).unwrap();
        assert_eq!(restored.style_presets, vec![preset]);
        assert_eq!(restored.excluded_shortcuts,vec!["C:\\Users\\demo\\Desktop\\Keep.lnk"]);
    }
    #[test]
    fn invalid_restore_destination_does_not_change_existing_data() {
        let source = Temp::new("source-conflict");
        seed(&source.0);
        let target = Temp::new("target-conflict");
        std::fs::create_dir_all(target.0.join("icons/sample.ico")).unwrap();
        std::fs::write(target.0.join("config.json"), b"unchanged").unwrap();
        assert!(restore_into(downloaded(pack(&source.0).unwrap()), &target.0).is_err());
        assert_eq!(
            std::fs::read(target.0.join("config.json")).unwrap(),
            b"unchanged"
        );
    }
    #[test]
    fn endpoint_rejects_embedded_credentials_and_preserves_encoded_paths() {
        let mut settings = Settings {
            url: "https://example.test/dav/%E5%9B%BE%E6%A0%87".into(),
            ..Default::default()
        };
        assert_eq!(
            settings.endpoint().unwrap().as_str(),
            "https://example.test/dav/%E5%9B%BE%E6%A0%87/IconForge/"
        );
        settings.url = "https://name:secret@example.test/dav".into();
        let error = settings.endpoint().unwrap_err().to_string();
        assert!(!error.contains("secret"));
        settings.url = "file:///C:/data".into();
        assert!(settings.endpoint().is_err());
    }
    #[cfg(windows)]
    #[test]
    fn dpapi_password_roundtrip_is_not_plaintext() {
        let secret = b"test-only-password";
        let encrypted = crypt(secret, false).unwrap();
        assert_ne!(encrypted, secret);
        assert_eq!(crypt(&encrypted, true).unwrap(), secret);
        let saved = SavedSettings {
            url: "https://example.test".into(),
            username: "test".into(),
            protected_password: STANDARD.encode(encrypted),
        };
        assert!(!serde_json::to_string(&saved)
            .unwrap()
            .contains("test-only-password"));
    }
    #[test]
    fn webdav_authenticated_upload_download_restore_roundtrip() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let address = format!("http://{}/dav/", server.server_addr());
        let handle = std::thread::spawn(move || {
            let mut objects: HashMap<String, Vec<u8>> = HashMap::new();
            for _ in 0..8 {
                let mut request = server
                    .recv_timeout(Duration::from_secs(15))
                    .unwrap()
                    .expect("expected DAV request");
                let auth = request
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv("Authorization"))
                    .unwrap()
                    .value
                    .as_str();
                assert_eq!(auth, "Basic dGVzdDp0ZXN0LXBhc3M=");
                let method = request.method().as_str().to_string();
                let path = request.url().to_string();
                assert!(path.starts_with("/dav/IconForge/"));
                let (code, bytes) = match method.as_str() {
                    "MKCOL" => (405, Vec::new()),
                    "PROPFIND" => {
                        assert!(request
                            .headers()
                            .iter()
                            .any(|h| h.field.equiv("Depth") && h.value.as_str() == "0"));
                        (207, b"<d:multistatus xmlns:d=\"DAV:\"/>".to_vec())
                    }
                    "PUT" => {
                        let mut body = Vec::new();
                        request.as_reader().read_to_end(&mut body).unwrap();
                        if path.ends_with("latest.json") {
                            let latest: Latest = serde_json::from_slice(&body).unwrap();
                            assert!(
                                objects.contains_key(&format!("/dav/IconForge/{}", latest.file)),
                                "pointer must follow the complete backup"
                            );
                        }
                        assert!(!String::from_utf8_lossy(&body).contains("local credentials only"));
                        objects.insert(path, body);
                        (201, Vec::new())
                    }
                    "GET" => (200, objects.get(&path).unwrap().clone()),
                    other => panic!("unexpected method {other}"),
                };
                request
                    .respond(tiny_http::Response::from_data(bytes).with_status_code(code))
                    .unwrap();
            }
        });
        let settings = Settings {
            url: address,
            username: "test".into(),
            password: "test-pass".into(),
        };
        test_connection(&settings).unwrap();
        let source = Temp::new("network-source");
        seed(&source.0);
        upload_from(&settings, &source.0).unwrap();
        let backup = download(&settings).unwrap();
        assert_eq!(backup.file_count, 5);
        let target = Temp::new("network-target");
        restore_into(backup, &target.0).unwrap();
        assert_eq!(
            std::fs::read(target.0.join("icons/sample.ico")).unwrap(),
            b"icon bytes"
        );
        handle.join().unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn restore_rolls_back_when_a_later_write_fails() {
        let source = Temp::new("rollback-source");
        seed(&source.0);
        let target = Temp::new("rollback-target");
        std::fs::create_dir_all(target.0.join("icons")).unwrap();
        let icon = target.0.join("icons/sample.ico");
        std::fs::write(&icon, b"readonly original").unwrap();
        std::fs::write(target.0.join("config.json"), b"previous config").unwrap();
        let mut permissions = std::fs::metadata(&icon).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&icon, permissions.clone()).unwrap();
        let result = restore_into(downloaded(pack(&source.0).unwrap()), &target.0);
        permissions.set_readonly(false);
        std::fs::set_permissions(&icon, permissions).unwrap();
        assert!(result.is_err());
        assert_eq!(
            std::fs::read(target.0.join("config.json")).unwrap(),
            b"previous config"
        );
        assert!(!target.0.join("applied.json").exists());
        assert_eq!(std::fs::read(icon).unwrap(), b"readonly original");
    }
    #[test]
    fn failed_backup_upload_does_not_publish_a_latest_pointer() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}/dav/", server.server_addr());
        let handle = std::thread::spawn(move || {
            for code in [405, 207, 403] {
                let request = server
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap()
                    .unwrap();
                assert!(!request.url().ends_with("latest.json"));
                request.respond(tiny_http::Response::empty(code)).unwrap();
            }
        });
        let source = Temp::new("failed-upload");
        seed(&source.0);
        let result = upload_from(
            &Settings {
                url,
                ..Default::default()
            },
            &source.0,
        );
        assert!(result.is_err());
        handle.join().unwrap();
    }
    #[test]
    fn authentication_errors_do_not_expose_passwords() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}/dav/", server.server_addr());
        let handle = std::thread::spawn(move || {
            let request = server
                .recv_timeout(Duration::from_secs(10))
                .unwrap()
                .unwrap();
            request.respond(tiny_http::Response::empty(401)).unwrap();
        });
        let error = test_connection(&Settings {
            url,
            username: "test".into(),
            password: "private-test-secret".into(),
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("401"));
        assert!(!error.contains("private-test-secret"));
        handle.join().unwrap();
    }
}
