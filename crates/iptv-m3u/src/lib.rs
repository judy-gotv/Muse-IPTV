//! M3U / EXTINF 解析器（开发文档 §7）。
//!
//! 容错规则（§7.2）：BOM、CRLF、空行、重复频道、非法 URL、缺属性、超大文件
//! 均不得整批失败；逐条报错并汇总（`ParseOutcome.errors`）。
//! 支持的属性：`tvg-id` / `tvg-name` / `tvg-logo` / `group-title` /
//! `catchup` / `catchup-source` / `x-tvg-url` / `url-tvg`（订阅级 EPG 地址）。
// 作者: Passwd Word

use iptv_dto::M3uLineError;
use serde::{Deserialize, Serialize};

/// 解析后规范化的单频道（供 `iptv-store::upsert_channels` 消费）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizedChannel {
    pub tvg_id: Option<String>,
    pub name: String,
    pub logo: Option<String>,
    pub group: Option<String>,
    pub url: String,
    pub catchup: Option<String>,
    pub catchup_source: Option<String>,
}

/// 解析结果：成功频道 + 逐条错误 + 订阅级 EPG 地址。
#[derive(Debug, Clone, Default)]
pub struct ParseOutcome {
    pub channels: Vec<NormalizedChannel>,
    pub errors: Vec<M3uLineError>,
    pub x_tvg_url: Option<String>,
}

/// 解析 M3U 文本。`#EXTINF:-1 <attrs>,<标题>` 后下一行非 `#` 即播放地址。
pub fn parse_m3u(input: &str) -> ParseOutcome {
    let mut out = ParseOutcome::default();
    // §7.2：BOM 必须剥离，否则首行 `#EXTM3U` 认不出来（同类教训：BOM 致 0 频道）。
    let input = input.strip_prefix('\u{feff}').unwrap_or(input);
    let mut pending: Option<(usize, String, String)> = None; // (行号, 属性串, 标题)
    let mut seen_urls = std::collections::HashSet::new();

    for (idx, raw_line) in input.lines().enumerate() {
        let line_no = idx + 1;
        let line = raw_line.trim_end_matches(['\r', '\n']).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with("#EXTM3U") {
            if let Some(url) = parse_attr(line, "x-tvg-url").or_else(|| parse_attr(line, "url-tvg"))
            {
                out.x_tvg_url = Some(url);
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            let (attrs, title) = match rest.split_once(',') {
                Some((a, t)) => (a.to_string(), t.trim().to_string()),
                None => {
                    out.errors.push(M3uLineError {
                        line: line_no,
                        message: "EXTINF 缺少逗号后的频道标题".into(),
                    });
                    continue;
                }
            };
            pending = Some((line_no, attrs, title));
            continue;
        }
        if line.starts_with('#') {
            continue; // 其他扩展行忽略
        }
        // 播放地址行
        let Some((ext_line, attrs, title)) = pending.take() else {
            out.errors.push(M3uLineError {
                line: line_no,
                message: "播放地址前缺少 #EXTINF 行".into(),
            });
            continue;
        };
        let _ = ext_line;
        if !is_playable_url(line) {
            out.errors.push(M3uLineError {
                line: line_no,
                message: format!("非法播放地址: {line}"),
            });
            continue;
        }
        if !seen_urls.insert(line.to_string()) {
            out.errors.push(M3uLineError {
                line: line_no,
                message: format!("重复频道地址，已跳过: {line}"),
            });
            continue;
        }
        let tvg_id = parse_attr(&attrs, "tvg-id");
        let name = parse_attr(&attrs, "tvg-name")
            .filter(|s| !s.is_empty())
            .unwrap_or(title);
        out.channels.push(NormalizedChannel {
            tvg_id,
            name,
            logo: parse_attr(&attrs, "tvg-logo"),
            group: parse_attr(&attrs, "group-title"),
            url: line.to_string(),
            catchup: parse_attr(&attrs, "catchup"),
            catchup_source: parse_attr(&attrs, "catchup-source"),
        });
    }
    if let Some((line_no, _, _)) = pending {
        out.errors.push(M3uLineError {
            line: line_no,
            message: "最后一个 #EXTINF 没有对应的播放地址".into(),
        });
    }
    out
}

fn is_playable_url(u: &str) -> bool {
    (u.starts_with("http://")
        || u.starts_with("https://")
        || u.starts_with("rtmp://")
        || u.starts_with("rtsp://"))
        && !u.contains(' ')
        && u.len() <= 2048
}

/// 解析 `key="value"` 或 `key=value` 形式的属性（容错：无引号、单引号）。
fn parse_attr(attrs: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=");
    let start = attrs.find(&needle)? + needle.len();
    let rest = &attrs[start..];
    let value = if let Some(stripped) = rest.strip_prefix('"') {
        stripped.split('"').next().unwrap_or("").to_string()
    } else if let Some(stripped) = rest.strip_prefix('\'') {
        stripped.split('\'').next().unwrap_or("").to_string()
    } else {
        rest.split_whitespace().next().unwrap_or("").to_string()
    };
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\u{feff}#EXTM3U x-tvg-url=\"https://example.com/epg.xml\"\r\n#EXTINF:-1 tvg-id=\"cctv5.cn\" tvg-name=\"CCTV-5\" tvg-logo=\"https://cdn/cctv5.png\" group-title=\"体育\",CCTV-5 体育\r\nhttps://example.com/live/cctv5.m3u8\r\n#EXTINF:-1 tvg-id=\"cctv1.cn\",CCTV-1\r\nhttps://example.com/live/cctv1.m3u8\r\n#EXTINF:-1 tvg-id=\"bad\",Bad\r\nnot a url\r\n#EXTINF:-1 tvg-id=\"dup\",Dup\r\nhttps://example.com/live/cctv5.m3u8\r\n";

    #[test]
    fn parses_sample_with_bom_and_errors() {
        let out = parse_m3u(SAMPLE);
        assert_eq!(
            out.x_tvg_url.as_deref(),
            Some("https://example.com/epg.xml")
        );
        assert_eq!(out.channels.len(), 2);
        assert_eq!(out.channels[0].name, "CCTV-5");
        assert_eq!(out.channels[0].group.as_deref(), Some("体育"));
        assert_eq!(
            out.channels[0].logo.as_deref(),
            Some("https://cdn/cctv5.png")
        );
        assert_eq!(out.channels[1].name, "CCTV-1"); // 无 tvg-name 时回退标题
        assert_eq!(out.errors.len(), 2); // 非法 URL + 重复地址
    }

    #[test]
    fn empty_and_header_only() {
        assert!(parse_m3u("").channels.is_empty());
        assert!(parse_m3u("#EXTM3U\n").channels.is_empty());
    }
}
