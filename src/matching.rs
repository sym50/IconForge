//! Name normalization + candidate ranking for the iTunes source.
//!
//! Handles the suffix junk real App Store names carry:
//! "123云盘——上传下载不限速第六年" -> "123云盘"
//! "哔哩哔哩-弹幕番剧直播高清视频" -> "哔哩哔哩"
//! "PotPlayer - HD Video Player" -> "potplayer"
//! Also decodes the `#U5fae#U4fe1` (微信) filename escapes found in
//! user-collected icon packs.

/// Decode `#UXXXX` (4 hex digits) escapes into the real character.
pub fn decode_unicode_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '#' && chars.peek() == Some(&'U') {
            // Peek past the 'U', then take 4 hex digits.
            let mut probe = chars.clone();
            probe.next(); // 'U'
            let hex: String = probe.take(4).collect();
            if hex.len() == 4 && hex.chars().all(|h| h.is_ascii_hexdigit()) {
                if let Ok(n) = u32::from_str_radix(&hex, 16) {
                    if let Some(ch) = char::from_u32(n) {
                        chars.next(); // 'U'
                        for _ in 0..4 {
                            chars.next();
                        }
                        out.push(ch);
                        continue;
                    }
                }
            }
            out.push('#');
        } else {
            out.push(c);
        }
    }
    out
}

/// Normalize an app name for matching: decode escapes, cut at the first
/// separator (suffix junk), lowercase, drop whitespace.
pub fn normalize(name: &str) -> String {
    let decoded = decode_unicode_escapes(name);
    let seps = [
        '-', '/', '—', '：', ':', '·', '|', '（', '(', '「', '[', '【', '·',
    ];
    let first = decoded
        .split(|c| seps.contains(&c))
        .next()
        .unwrap_or(&decoded);
    first
        .trim()
        .to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect()
}

/// Match score: exact 100, prefix 80, contains 50, else 0.
pub fn score(query: &str, candidate: &str) -> u32 {
    let q = normalize(query);
    let c = normalize(candidate);
    if q.is_empty() || c.is_empty() {
        return 0;
    }
    if q == c {
        100
    } else if c.starts_with(&q) || q.starts_with(&c) {
        80
    } else if c.contains(&q) || q.contains(&c) {
        50
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffix_stripping() {
        assert_eq!(normalize("123云盘——上传下载不限速第六年"), "123云盘");
        assert_eq!(normalize("哔哩哔哩-弹幕番剧直播高清视频"), "哔哩哔哩");
        assert_eq!(normalize("PotPlayer - HD Video Player"), "potplayer");
        assert_eq!(normalize("微信"), "微信");
    }

    #[test]
    fn unicode_escape_decoding() {
        assert_eq!(decode_unicode_escapes("#U5fae#U4fe1"), "微信");
        assert_eq!(normalize("#U5fae#U4fe1"), "微信");
    }

    #[test]
    fn scoring() {
        assert_eq!(score("微信", "微信"), 100);
        assert_eq!(score("微信", "微信读书"), 80);
        assert_eq!(score("微信", "腾讯微信"), 50);
        assert_eq!(score("微信", "支付宝"), 0);
    }
}
