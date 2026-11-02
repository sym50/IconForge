//! iTunes Search API source: CN + US, iOS + Mac, merged, deduped by bundleId.
//!
//! No key, no login. Artwork: take `artworkUrl512`, swap `512x512` for
//! `1024x1024` (verified to return a real 1024 image), then re-mask to our
//! superellipse so the whole desktop is consistent.

use std::collections::HashMap;
use std::time::Duration;

use image::RgbaImage;
use serde_json::Value;

use crate::matching::score;

#[derive(Debug, Clone)]
pub struct AppCandidate {
    pub track_name: String,
    pub artist_name: String,
    pub bundle_id: String,
    pub version: String,
    pub artwork_url_512: String,
    /// Countries where this candidate was found: "CN", "US".
    pub countries: Vec<String>,
    /// Kind: "software" (iOS) or "mac-software" (Mac).
    pub kind: String,
}

impl AppCandidate {
    pub fn artwork_url_1024(&self) -> String {
        self.artwork_url_512.replace("512x512", "1024x1024")
    }
}

const SEARCH_URL: &str = "https://itunes.apple.com/search";

/// HTTP client honouring the user's proxy env vars (ALL_PROXY / HTTPS_PROXY /
/// HTTP_PROXY) and NO_PROXY. The proxy URL (which may carry credentials)
/// is never logged.
///
/// TLS uses the system stack (native-tls), not rustls: see Cargo.toml.
fn client() -> anyhow::Result<reqwest::blocking::Client> {
    let mut builder = reqwest::blocking::Client::builder().timeout(Duration::from_secs(20));
    for var in [
        "ALL_PROXY",
        "all_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
    ] {
        if let Ok(v) = std::env::var(var) {
            if let Ok(proxy) = reqwest::Proxy::all(&v) {
                builder = builder.proxy(proxy);
                break;
            }
        }
    }
    Ok(builder.build()?)
}

/// Retry a fallible HTTP step up to 3 times with backoff. The egress
/// path shows transient timeouts (observed on mzstatic artwork downloads),
/// so one retry is not enough to call it a failure.
fn retry<T>(mut f: impl FnMut() -> anyhow::Result<T>) -> anyhow::Result<T> {
    let mut last_err = None;
    for attempt in 0..3 {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) => {
                last_err = Some(e);
                if attempt < 2 {
                    std::thread::sleep(Duration::from_millis(500 * (attempt as u64 + 1)));
                }
            }
        }
    }
    Err(last_err.unwrap())
}

fn search_one(
    term: &str,
    country: &str,
    entity: &str,
    limit: u32,
) -> anyhow::Result<Vec<AppCandidate>> {
    let json: Value = retry(|| {
        let resp = client()?
            .get(SEARCH_URL)
            .query(&[
                ("term", term),
                ("country", country),
                ("entity", entity),
                ("limit", &limit.to_string()),
            ])
            .send()?
            .error_for_status()?;
        Ok(resp.json()?)
    })?;
    let mut out = Vec::new();
    if let Some(results) = json.get("results").and_then(|r| r.as_array()) {
        for r in results {
            let get = |k: &str| r.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
            let artwork = get("artworkUrl512");
            if artwork.is_empty() {
                continue;
            }
            out.push(AppCandidate {
                track_name: get("trackName"),
                artist_name: get("artistName"),
                bundle_id: get("bundleId"),
                version: get("version"),
                artwork_url_512: artwork,
                countries: vec![country.to_string()],
                kind: entity.to_string(),
            });
        }
    }
    Ok(out)
}

/// Search CN+US x iOS+Mac, merge, dedupe by bundleId (keeping the union
/// of countries), ranked by match score against `term`.
///
/// Two terms are queried when they differ beyond case/whitespace:
/// the raw term (iTunes handles full store titles like
/// "123云盘——上传下载不限速第六年") and the normalized term
/// ("123云盘", covers junk suffixes like "微信 (2)"). Results merge.
///
/// Score-0 candidates (Apple's unrelated filler: 抖音/支付宝/… in every
/// query) are dropped, so the UI list stays clean.
pub fn search_all(term: &str) -> anyhow::Result<Vec<(u32, AppCandidate)>> {
    let mut terms = vec![term.to_string()];
    let normalized = crate::matching::normalize(term);
    let raw_folded: String = term
        .to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    if !normalized.is_empty() && normalized != raw_folded {
        terms.push(normalized);
    }
    let mut by_bundle: HashMap<String, AppCandidate> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for query in &terms {
        for country in ["CN", "US"] {
            for (entity, limit) in [("software", 20), ("macSoftware", 15)] {
                match search_one(query, country, entity, limit) {
                    Ok(cands) => {
                        for c in cands {
                            if c.bundle_id.is_empty() {
                                continue;
                            }
                            if let Some(existing) = by_bundle.get_mut(&c.bundle_id) {
                                if !existing.countries.contains(&country.to_string()) {
                                    existing.countries.push(country.to_string());
                                }
                                if existing.version.is_empty() && !c.version.is_empty() {
                                    existing.version = c.version.clone();
                                }
                            } else {
                                order.push(c.bundle_id.clone());
                                by_bundle.insert(c.bundle_id.clone(), c);
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("iTunes {country}/{entity} failed: {e:#}");
                    }
                }
            }
        }
    }
    let mut scored: Vec<(u32, AppCandidate)> = order
        .into_iter()
        .filter_map(|id| by_bundle.remove(&id))
        .map(|c| (score(term, &c.track_name), c))
        .collect();
    // Drop Apple's unrelated filler (score 0).
    scored.retain(|(s, _)| *s > 0);
    // Stable sort: best score first, ties keep discovery order.
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(scored)
}

/// Download artwork at 1024px and decode to RGBA.
pub fn download_artwork(cand: &AppCandidate) -> anyhow::Result<RgbaImage> {
    download_image(&cand.artwork_url_1024())
}

/// Download any image URL and decode to RGBA.
pub fn download_image(url: &str) -> anyhow::Result<RgbaImage> {
    let bytes = retry(|| {
        let resp = client()?.get(url).send()?.error_for_status()?;
        Ok(resp.bytes()?)
    })?;
    let img = image::load_from_memory(&bytes)?;
    Ok(img.to_rgba8())
}

/// Cache key: bundleId@version. Version change => re-download.
pub fn cache_name(cand: &AppCandidate) -> String {
    let safe_bundle = cand
        .bundle_id
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '.' || c == '-' { c } else { '_' })
        .collect::<String>();
    format!("{safe_bundle}@{}.png", cand.version)
}

/// Parse a raw iTunes `search` response (used by tests / offline tooling).
pub fn parse_search_response(json: &Value, country: &str, entity: &str) -> Vec<AppCandidate> {
    let mut out = Vec::new();
    if let Some(results) = json.get("results").and_then(|r| r.as_array()) {
        for r in results {
            let get = |k: &str| r.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
            let artwork = get("artworkUrl512");
            if artwork.is_empty() {
                continue;
            }
            out.push(AppCandidate {
                track_name: get("trackName"),
                artist_name: get("artistName"),
                bundle_id: get("bundleId"),
                version: get("version"),
                artwork_url_512: artwork,
                countries: vec![country.to_string()],
                kind: entity.to_string(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artwork_1024_swap() {
        let c = AppCandidate {
            track_name: "WeChat".into(),
            artist_name: "Tencent".into(),
            bundle_id: "com.tencent.xin".into(),
            version: "8.0.79".into(),
            artwork_url_512: "https://example.com/image/512x512bb.jpg".into(),
            countries: vec!["CN".into()],
            kind: "software".into(),
        };
        assert_eq!(
            c.artwork_url_1024(),
            "https://example.com/image/1024x1024bb.jpg"
        );
    }

    #[test]
    fn parse_response() {
        let json: Value = serde_json::json!({
            "resultCount": 1,
            "results": [{
                "trackName": "WeChat",
                "artistName": "Tencent",
                "bundleId": "com.tencent.xin",
                "version": "8.0.79",
                "artworkUrl512": "https://example.com/a/512x512bb.jpg"
            }]
        });
        let out = parse_search_response(&json, "CN", "software");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].track_name, "WeChat");
        assert_eq!(out[0].countries, vec!["CN"]);
    }

    #[test]
    fn cache_name_sanitizes() {
        let c = AppCandidate {
            track_name: String::new(),
            artist_name: String::new(),
            bundle_id: "com.tencent/xin".into(),
            version: "8.0.79".into(),
            artwork_url_512: String::new(),
            countries: vec![],
            kind: String::new(),
        };
        assert_eq!(cache_name(&c), "com.tencent_xin@8.0.79.png");
    }

    #[test]
    fn normalize_import_used() {
        // Sanity: matching::normalize is the same one used for ranking.
        assert_eq!(crate::matching::normalize("微信"), "微信");
    }
}
