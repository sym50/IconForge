//! Local Web UI server (single exe, no WebView).
//!
//! `iconforge serve` starts this on 127.0.0.1 and (via the native GUI's
//! fallback, or by hand) opens the browser. The UI is one embedded HTML
//! file served at `/`; everything else is a JSON API under `/api/*`.
//! The heavy lifting lives in `pipeline.rs`, shared with the native GUI.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use base64::Engine;
use serde::{Deserialize, Serialize};
use tiny_http::{Header, Method, Response, Server};

use crate::itunes::{self, AppCandidate};
use crate::lnk;
use crate::pipeline::{self, ApplyItem, SourceSpec};

const INDEX_HTML: &str = include_str!("../static/index.html");

pub fn data_dir() -> PathBuf {
    pipeline::data_dir()
}

fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub dark_mode: bool,
    /// Disable the transparency grid in all native previews.
    #[serde(default)]
    pub hide_checkerboard: bool,
    /// UI zoom factor (1.0 = 100%). 0.0 / missing means "use default".
    #[serde(default)]
    pub ui_scale: f32,
    /// Compact layout has migrated the old desktop zoom preference.
    #[serde(default)]
    pub compact_layout: bool,
    #[serde(default)]
    pub style_presets: Vec<crate::presets::StylePreset>,
    #[serde(default)]
    pub excluded_shortcuts: Vec<String>,
}

impl AppConfig {
    pub fn load_checked() -> anyhow::Result<Self> {
        match std::fs::read(config_path()) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }
    pub fn load() -> Self {
        let p = config_path();
        let raw = std::fs::read_to_string(p).unwrap_or_default();
        // Old config.json files may still carry the removed AI fields
        // (ai_endpoint/ai_model/ai_key): serde ignores unknown fields.
        let mut config: Self = serde_json::from_str(&raw).unwrap_or_default();
        let mut seen = std::collections::HashSet::new();
        config
            .style_presets
            .retain(|p| p.validate().is_ok() && seen.insert(p.name.trim().to_lowercase()));
        config
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let dir = data_dir();
        std::fs::create_dir_all(&dir)?;
        crate::webdav::atomic_write(&config_path(), &serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }
}

// ---------- small helpers ----------

fn percent_decode(s: &str) -> String {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("00");
                if let Ok(v) = u8::from_str_radix(hex, 16) {
                    out.push(v);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query_params(url: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Some(q) = url.split('?').nth(1) else {
        return map;
    };
    for pair in q.split('&') {
        let mut it = pair.splitn(2, '=');
        if let (Some(k), Some(v)) = (it.next(), it.next()) {
            map.insert(percent_decode(k), percent_decode(v));
        }
    }
    map
}

fn json_response(value: serde_json::Value) -> Response<std::io::Cursor<Vec<u8>>> {
    let body = serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string());
    Response::from_data(body.into_bytes())
        .with_header(Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap())
}

fn error_response(msg: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    json_response(serde_json::json!({ "ok": false, "error": msg }))
}

fn png_response(png: Vec<u8>) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_data(png)
        .with_header(Header::from_bytes(&b"Content-Type"[..], &b"image/png"[..]).unwrap())
}

fn png_b64(img: &image::RgbaImage) -> String {
    let mut buf = Vec::new();
    let mut cur = std::io::Cursor::new(&mut buf);
    let _ = img.write_to(&mut cur, image::ImageFormat::Png);
    base64::engine::general_purpose::STANDARD.encode(buf)
}

fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---------- request handling ----------

fn handle(req: &mut tiny_http::Request) -> Response<std::io::Cursor<Vec<u8>>> {
    let url = req.url().to_string();
    let path = url.split('?').next().unwrap_or("").to_string();
    let method = req.method().clone();
    let params = query_params(&url);

    match (method, path.as_str()) {
        (Method::Get, "/") => Response::from_data(INDEX_HTML.as_bytes().to_vec()).with_header(
            Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap(),
        ),

        (Method::Get, "/api/folders") => {
            let folders: Vec<serde_json::Value> = lnk::known_folders()
                .into_iter()
                .map(|(name, p)| serde_json::json!({ "name": name, "path": p.to_string_lossy() }))
                .collect();
            json_response(serde_json::json!({ "ok": true, "folders": folders }))
        }

        (Method::Get, "/api/shortcuts") => {
            let mock = params.get("mock").map(|v| v == "1").unwrap_or(false);
            let shortcuts = if mock {
                pipeline::mock_shortcuts()
            } else {
                let folder = params.get("folder").cloned().unwrap_or_default();
                lnk::enumerate(Path::new(&folder))
            };
            let list: Vec<serde_json::Value> = shortcuts
                .into_iter()
                .map(|s| {
                    let icon_url = format!(
                        "/api/icon?path={}&mock={}",
                        url_encode(&s.path),
                        if mock { "1" } else { "0" }
                    );
                    serde_json::json!({
                        "path": s.path, "name": s.name, "target": s.target,
                        "icon_path": s.icon_path, "icon_index": s.icon_index,
                        "kind": s.kind, "icon_url": icon_url,
                    })
                })
                .collect();
            json_response(serde_json::json!({ "ok": true, "shortcuts": list }))
        }

        (Method::Get, "/api/icon") => {
            let mock = params.get("mock").map(|v| v == "1").unwrap_or(false);
            let lnk_path = params.get("path").cloned().unwrap_or_default();
            if mock {
                return png_response(pipeline::mock_icon_png(&pipeline::lnk_stem(&lnk_path)));
            }
            let png = lnk::parse(Path::new(&lnk_path)).ok().and_then(|info| {
                let (file, idx) = if !info.icon_path.is_empty() {
                    (info.icon_path.clone(), info.icon_index)
                } else {
                    (info.target.clone(), 0)
                };
                lnk::extract_icon_png(&file, idx, 256)
            });
            png_response(png.unwrap_or_else(pipeline::placeholder_png))
        }

        (Method::Get, "/api/search") => {
            let query = params.get("query").cloned().unwrap_or_default();
            match itunes::search_all(&query) {
                Ok(cands) => {
                    let list: Vec<serde_json::Value> = cands
                        .into_iter()
                        .map(|(score, c): (u32, AppCandidate)| {
                            serde_json::json!({
                                "score": score, "track_name": c.track_name,
                                "artist_name": c.artist_name, "bundle_id": c.bundle_id,
                                "version": c.version, "artwork_url": c.artwork_url_512,
                                "countries": c.countries, "kind": c.kind,
                            })
                        })
                        .collect();
                    json_response(serde_json::json!({ "ok": true, "candidates": list }))
                }
                Err(e) => error_response(&format!("搜索失败: {e:#}")),
            }
        }

        (Method::Post, "/api/preview") => {
            let body = read_body(req);
            let v: serde_json::Value = match serde_json::from_slice(&body) {
                Ok(v) => v,
                Err(e) => return error_response(&format!("请求解析失败: {e}")),
            };
            let src: SourceSpec =
                match serde_json::from_value(v.get("source").cloned().unwrap_or_default()) {
                    Ok(s) => s,
                    Err(e) => return error_response(&format!("来源解析失败: {e}")),
                };
            let lnk_path = v.get("lnk_path").and_then(|x| x.as_str()).unwrap_or("");
            let base = v.get("base").and_then(|x| x.as_str()).unwrap_or("white");
            let mock = v.get("mock").and_then(|x| x.as_bool()).unwrap_or(false);
            match pipeline::compose_preview(
                &src,
                lnk_path,
                base,
                mock,
                crate::composite::DEFAULT_GLYPH_SCALE,
                crate::composite::DEFAULT_ICON_RADIUS,
                crate::composite::DEFAULT_BASE_SIZE,
                crate::composite::DEFAULT_BASE_EXPONENT,
            ) {
                Ok((img, branch)) => json_response(serde_json::json!({
                    "ok": true, "png_b64": png_b64(&img), "branch": branch,
                })),
                Err(e) => error_response(&format!("预览失败: {e:#}")),
            }
        }

        (Method::Post, "/api/apply") => {
            let body = read_body(req);
            let v: serde_json::Value = match serde_json::from_slice(&body) {
                Ok(v) => v,
                Err(e) => return error_response(&format!("请求解析失败: {e}")),
            };
            let items_v = v
                .get("items")
                .and_then(|x| x.as_array())
                .cloned()
                .unwrap_or_default();
            let dry_run = v.get("dry_run").and_then(|x| x.as_bool()).unwrap_or(false);
            let mock = v.get("mock").and_then(|x| x.as_bool()).unwrap_or(false);
            let mut items = Vec::new();
            for item in &items_v {
                let lnk_path = item
                    .get("lnk_path")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let base = item
                    .get("base")
                    .and_then(|x| x.as_str())
                    .unwrap_or("white")
                    .to_string();
                let src: Option<SourceSpec> =
                    serde_json::from_value(item.get("source").cloned().unwrap_or_default()).ok();
                if let Some(src) = src {
                    items.push(ApplyItem {
                        lnk_path,
                        source: src,
                        base,
                        glyph_scale: crate::composite::DEFAULT_GLYPH_SCALE,
                        icon_radius: crate::composite::DEFAULT_ICON_RADIUS,
                        base_size: crate::composite::DEFAULT_BASE_SIZE,
                        base_exponent: crate::composite::DEFAULT_BASE_EXPONENT,
                    });
                }
            }
            let (results, snapshot_id) = pipeline::apply_items(&items, dry_run, mock);
            let results_json: Vec<serde_json::Value> = results
                .into_iter()
                .map(|r| {
                    serde_json::json!({
                        "lnk_path": r.lnk_path, "ok": r.ok,
                        "error": r.error, "icon_path": r.icon_path,
                    })
                })
                .collect();
            json_response(
                serde_json::json!({ "ok": true, "results": results_json, "snapshot_id": snapshot_id }),
            )
        }

        (Method::Get, "/api/snapshots") => {
            let list: Vec<serde_json::Value> = pipeline::list_snapshots()
                .into_iter()
                .map(|s| {
                    serde_json::json!({ "id": s.id, "created_at": s.created_at, "count": s.count })
                })
                .collect();
            json_response(serde_json::json!({ "ok": true, "snapshots": list }))
        }

        (Method::Post, "/api/restore") => {
            let body = read_body(req);
            let v: serde_json::Value = match serde_json::from_slice(&body) {
                Ok(v) => v,
                Err(e) => return error_response(&format!("请求解析失败: {e}")),
            };
            let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("");
            match pipeline::restore_snapshot(id) {
                Ok(results) => {
                    let results_json: Vec<serde_json::Value> = results
                        .into_iter()
                        .map(|r| {
                            serde_json::json!({ "lnk_path": r.lnk_path, "ok": r.ok, "error": r.error })
                        })
                        .collect();
                    json_response(serde_json::json!({ "ok": true, "results": results_json }))
                }
                Err(e) => error_response(&format!("快照读取失败: {e:#}")),
            }
        }

        (Method::Post, "/api/upload") => {
            let body = read_body(req);
            let v: serde_json::Value = match serde_json::from_slice(&body) {
                Ok(v) => v,
                Err(e) => return error_response(&format!("请求解析失败: {e}")),
            };
            let name = v
                .get("name")
                .and_then(|x| x.as_str())
                .unwrap_or("upload.png");
            let b64 = v.get("b64").and_then(|x| x.as_str()).unwrap_or("");
            let bytes = match base64::engine::general_purpose::STANDARD.decode(b64) {
                Ok(b) => b,
                Err(e) => return error_response(&format!("base64 解码失败: {e}")),
            };
            let dir = data_dir().join("uploads");
            if let Err(e) = std::fs::create_dir_all(&dir) {
                return error_response(&format!("创建上传目录失败: {e}"));
            }
            let mut out = dir.join(pipeline::safe_stem(name));
            let mut n = 1;
            while out.exists() {
                n += 1;
                out = dir.join(format!("{}_{}", pipeline::safe_stem(name), n));
            }
            match std::fs::write(&out, bytes) {
                Ok(()) => {
                    json_response(serde_json::json!({ "ok": true, "path": out.to_string_lossy() }))
                }
                Err(e) => error_response(&format!("写入失败: {e}")),
            }
        }

        _ => Response::from_data(b"not found".to_vec()).with_status_code(404),
    }
}

fn read_body(req: &mut tiny_http::Request) -> Vec<u8> {
    let mut body = Vec::new();
    let _ = req.as_reader().read_to_end(&mut body);
    body
}

/// Start the server on 127.0.0.1:`port` (0 = auto-pick). Blocks forever.
/// Returns the URL the UI is served at.
pub fn run(port: u16) -> anyhow::Result<String> {
    let server = Server::http(format!("127.0.0.1:{port}"))
        .map_err(|e| anyhow::anyhow!("启动本地服务失败: {e}"))?;
    let addr = server.server_addr();
    let url = format!("http://{addr}");
    println!("IconForge running at {url}");
    for mut req in server.incoming_requests() {
        let resp = handle(&mut req);
        let _ = req.respond(resp);
    }
    Ok(url)
}

pub fn open_browser(url: &str) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
}
