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
    /// 解析过的总行数（含空行与注释行）。
    pub total_lines: usize,
}

/// 逐行喂入的状态机：`parse_m3u`（整串）与 `parse_m3u_reader`（流式）
/// 共用，保证两种入口行为一致。
pub struct M3uParser {
    pending: Option<(usize, String, String)>, // (行号, 属性串, 标题)
    seen_urls: std::collections::HashSet<String>,
    out: ParseOutcome,
    /// 超过错误上限后丢弃的错误条数（防超大文件把 errors 撑爆内存）。
    dropped_errors: usize,
}

/// 逐条错误最多保留条数，超出后只计数，结束时补一条汇总。
pub const MAX_ERRORS: usize = 1000;

impl Default for M3uParser {
    fn default() -> Self {
        Self::new()
    }
}

impl M3uParser {
    pub fn new() -> Self {
        Self {
            pending: None,
            seen_urls: std::collections::HashSet::new(),
            out: ParseOutcome::default(),
            dropped_errors: 0,
        }
    }

    fn push_error(&mut self, line: usize, message: String) {
        if self.out.errors.len() < MAX_ERRORS {
            self.out.errors.push(M3uLineError { line, message });
        } else {
            self.dropped_errors += 1;
        }
    }

    /// 喂一行（行号从 1 起；调用方负责剥 BOM——只在首行出现）。
    pub fn feed_line(&mut self, line_no: usize, raw_line: &str) {
        self.out.total_lines += 1;
        let line = raw_line.trim_end_matches(['\r', '\n']).trim();
        if line.is_empty() {
            return;
        }
        if line.starts_with("#EXTM3U") {
            if let Some(url) = parse_attr(line, "x-tvg-url").or_else(|| parse_attr(line, "url-tvg"))
            {
                self.out.x_tvg_url = Some(url);
            }
            return;
        }
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            let (attrs, title) = match rest.split_once(',') {
                Some((a, t)) => (a.to_string(), t.trim().to_string()),
                None => {
                    self.push_error(line_no, "EXTINF 缺少逗号后的频道标题".into());
                    return;
                }
            };
            self.pending = Some((line_no, attrs, title));
            return;
        }
        if line.starts_with('#') {
            return; // 其他扩展行忽略
        }
        // 播放地址行
        let Some((ext_line, attrs, title)) = self.pending.take() else {
            self.push_error(line_no, "播放地址前缺少 #EXTINF 行".into());
            return;
        };
        let _ = ext_line;
        if !is_playable_url(line) {
            self.push_error(line_no, format!("非法播放地址: {line}"));
            return;
        }
        if !self.seen_urls.insert(line.to_string()) {
            self.push_error(line_no, format!("重复频道地址，已跳过: {line}"));
            return;
        }
        let tvg_id = parse_attr(&attrs, "tvg-id");
        let name = parse_attr(&attrs, "tvg-name")
            .filter(|s| !s.is_empty())
            .unwrap_or(title);
        self.out.channels.push(NormalizedChannel {
            tvg_id,
            name,
            logo: parse_attr(&attrs, "tvg-logo"),
            group: parse_attr(&attrs, "group-title"),
            url: line.to_string(),
            catchup: parse_attr(&attrs, "catchup"),
            catchup_source: parse_attr(&attrs, "catchup-source"),
        });
    }

    /// 收尾：悬空的 #EXTINF 报错；超限丢弃的错误补一条汇总。
    pub fn finish(mut self) -> ParseOutcome {
        if let Some((line_no, _, _)) = self.pending.take() {
            self.push_error(line_no, "最后一个 #EXTINF 没有对应的播放地址".into());
        }
        if self.dropped_errors > 0 {
            self.out.errors.push(M3uLineError {
                line: 0,
                message: format!(
                    "另有 {} 条错误已省略，仅保留前 {MAX_ERRORS} 条",
                    self.dropped_errors
                ),
            });
        }
        self.out
    }
}

/// 解析 M3U 文本。`#EXTINF:-1 <attrs>,<标题>` 后下一行非 `#` 即播放地址。
pub fn parse_m3u(input: &str) -> ParseOutcome {
    // §7.2：BOM 必须剥离，否则首行 `#EXTM3U` 认不出来（同类教训：BOM 致 0 频道）。
    let input = input.strip_prefix('\u{feff}').unwrap_or(input);
    let mut p = M3uParser::new();
    for (idx, raw_line) in input.lines().enumerate() {
        p.feed_line(idx + 1, raw_line);
    }
    p.finish()
}

/// 流式解析 M3U：逐行读，不把整包攒进内存（上传不限大小后的安全兜底）。
/// `reader` 须是 `BufRead`（如文件 `BufReader`）；BOM 只剥首行。
pub fn parse_m3u_reader<R: std::io::BufRead>(reader: R) -> std::io::Result<ParseOutcome> {
    parse_m3u_reader_progress(reader, |_, _, _| {})
}

/// M16：带进度回调的流式解析。回调每读 `PROGRESS_EVERY_LINES` 行触发一次，
/// 参数为（已读总行数，已解析频道数，已累计错误数）；结束时再触发一次终态。
/// 供异步上传任务在解析中途上报进度，行为与 `parse_m3u_reader` 完全一致。
pub const PROGRESS_EVERY_LINES: usize = 2000;

pub fn parse_m3u_reader_progress<R: std::io::BufRead, F: FnMut(usize, usize, usize)>(
    mut reader: R,
    mut on_progress: F,
) -> std::io::Result<ParseOutcome> {
    let mut p = M3uParser::new();
    let mut line_no = 0usize;
    let mut buf = String::new();
    loop {
        buf.clear();
        let n = reader.read_line(&mut buf)?;
        if n == 0 {
            break;
        }
        line_no += 1;
        let line = if line_no == 1 {
            buf.strip_prefix('\u{feff}').unwrap_or(&buf)
        } else {
            &buf
        };
        p.feed_line(line_no, line);
        if line_no % PROGRESS_EVERY_LINES == 0 {
            on_progress(p.out.total_lines, p.out.channels.len(), p.out.errors.len());
        }
    }
    let out = p.finish();
    on_progress(out.total_lines, out.channels.len(), out.errors.len());
    Ok(out)
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

    #[test]
    fn progress_callback_reports_monotonic_counts() {
        // M16：回调行数/频道数单调不减，终态与无回调解析一致。
        let mut seen: Vec<(usize, usize, usize)> = Vec::new();
        let out = parse_m3u_reader_progress(std::io::Cursor::new(SAMPLE), |l, c, e| {
            seen.push((l, c, e));
        })
        .unwrap();
        let via_reader = parse_m3u_reader(std::io::Cursor::new(SAMPLE)).unwrap();
        assert_eq!(out.channels.len(), via_reader.channels.len());
        assert_eq!(out.total_lines, via_reader.total_lines);
        assert_eq!(out.errors.len(), via_reader.errors.len());
        assert!(!seen.is_empty());
        let last = *seen.last().unwrap();
        assert_eq!(
            last,
            (out.total_lines, out.channels.len(), out.errors.len())
        );
        for w in seen.windows(2) {
            assert!(w[1].0 >= w[0].0 && w[1].1 >= w[0].1 && w[1].2 >= w[0].2);
        }
    }

    #[test]
    fn reader_matches_whole_string() {
        let out = parse_m3u(SAMPLE);
        let via_reader = parse_m3u_reader(std::io::Cursor::new(SAMPLE.as_bytes())).unwrap();
        assert_eq!(via_reader.channels, out.channels);
        assert_eq!(via_reader.errors.len(), out.errors.len());
        assert_eq!(via_reader.x_tvg_url, out.x_tvg_url);
        assert_eq!(via_reader.total_lines, SAMPLE.lines().count());
        assert_eq!(out.total_lines, SAMPLE.lines().count());
    }

    #[test]
    fn errors_capped_with_summary() {
        // 2005 条坏行：只保留前 1000 条 + 1 条汇总，不无限增长。
        let mut text = String::from("#EXTM3U\n");
        for i in 0..2005 {
            text.push_str(&format!("#EXTINF:-1,Bad{i}\nnot a url\n"));
        }
        let out = parse_m3u(&text);
        assert_eq!(out.errors.len(), MAX_ERRORS + 1);
        assert!(out.errors[MAX_ERRORS].message.contains("条错误已省略"));
    }
}
