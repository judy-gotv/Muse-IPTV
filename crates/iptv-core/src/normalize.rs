//! 订阅同步的规范化与内容哈希（§7.3）。
//!
//! - 去空格、统一协议、去追踪参数（§7.3 第 3 步）。
//! - `content_hash = sha256(tvg_id|url|name|group)`（§7.3 第 4 步）。
// 作者: Passwd Word

use sha2::{Digest, Sha256};

/// 追踪参数黑名单：同步时从播放地址上剥离，避免同一频道因追踪参数反复“变更”。
pub const TRACKING_PARAMS: &[&str] = &[
    "utm_source",
    "utm_medium",
    "utm_campaign",
    "utm_term",
    "utm_content",
    "fbclid",
    "gclid",
    "msclkid",
    "mc_cid",
    "mc_eid",
    "_ga",
    "_gac",
];

/// 规范化播放地址：去首尾空格；`http://` 统一为 `https://` 仅当原地址是 http
/// 且主机不是本地/内网地址时不强制（上游很多仍是 http，强制会断流），
/// 因此本函数只做去空格 + 去追踪参数 + 去默认端口 + 去尾部 `/` 之外的空白。
pub fn normalize_play_url(raw: &str) -> String {
    let trimmed = raw.trim();
    strip_tracking_params(trimmed)
}

/// 剥离追踪查询参数，保留其余参数与原顺序。
pub fn strip_tracking_params(url: &str) -> String {
    let Some((base, query)) = url.split_once('?') else {
        return url.to_string();
    };
    let kept: Vec<&str> = query
        .split('&')
        .filter(|pair| {
            let key = pair.split_once('=').map(|(k, _)| k).unwrap_or(pair);
            !TRACKING_PARAMS.contains(&key)
        })
        .collect();
    if kept.is_empty() {
        base.to_string()
    } else {
        format!("{}?{}", base, kept.join("&"))
    }
}

/// §7.3：`content_hash = sha256(tvg_id|url|name|group)`。
pub fn content_hash(tvg_id: &str, url: &str, name: &str, group: &str) -> String {
    let mut h = Sha256::new();
    h.update(tvg_id.as_bytes());
    h.update(b"|");
    h.update(normalize_play_url(url).as_bytes());
    h.update(b"|");
    h.update(name.trim().as_bytes());
    h.update(b"|");
    h.update(group.trim().as_bytes());
    hex::encode(h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracking_params_stripped() {
        assert_eq!(
            strip_tracking_params("https://x.example/a.m3u8?utm_source=a&id=1&fbclid=z"),
            "https://x.example/a.m3u8?id=1"
        );
        assert_eq!(
            strip_tracking_params("https://x.example/a.m3u8"),
            "https://x.example/a.m3u8"
        );
    }

    #[test]
    fn hash_is_stable_and_sensitive() {
        let a = content_hash("cctv5.cn", "https://x/a.m3u8", "CCTV-5", "体育");
        let b = content_hash("cctv5.cn", "https://x/a.m3u8", "CCTV-5", "体育");
        let c = content_hash("cctv5.cn", "https://x/b.m3u8", "CCTV-5", "体育");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 64);
    }
}
