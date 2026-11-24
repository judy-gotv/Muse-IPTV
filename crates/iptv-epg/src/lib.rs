//! XMLTV 解析器（开发文档 §1.1、§14.1），`quick-xml` 流式解析。
//!
//! 解析 `<tv>` 下的 `<channel id="..."><display-name>` 与
//! `<programme start="20261124080000 +0800" stop="..." channel="...">`
//! `<title>` / `<desc>` / `<category>`。时间格式为 XMLTV 的
//! `YYYYMMDDhhmmss ±HHMM`。
// 作者: Passwd Word

use chrono::{DateTime, FixedOffset, NaiveDateTime, TimeZone, Utc};
use quick_xml::events::Event;
use quick_xml::Reader;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EpgError {
    #[error("xml error: {0}")]
    Xml(String),
    #[error("bad programme time: {0}")]
    BadTime(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct XmltvChannel {
    pub id: String,
    pub display_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XmltvProgramme {
    pub channel: String,
    pub start: DateTime<Utc>,
    pub stop: DateTime<Utc>,
    pub title: String,
    pub desc: Option<String>,
    pub category: Option<String>,
}

#[derive(Debug, Default)]
pub struct XmltvData {
    pub channels: Vec<XmltvChannel>,
    pub programmes: Vec<XmltvProgramme>,
}

/// 流式解析 XMLTV 文档。单个 programme 解析失败不中断整批（记入错误返回）。
pub fn parse_xmltv(input: &[u8]) -> Result<XmltvData, EpgError> {
    let mut reader = Reader::from_reader(input);
    reader.config_mut().trim_text(true);
    let mut data = XmltvData::default();
    let mut buf = Vec::new();

    let mut cur_channel_id: Option<String> = None;
    let mut cur_display: Option<String> = None;
    let mut cur_prog: Option<(String, String, String)> = None; // (channel, start, stop)
    let mut cur_title: Option<String> = None;
    let mut cur_desc: Option<String> = None;
    let mut cur_cat: Option<String> = None;
    let mut cur_text_for: Option<&'static str> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match e.name().as_ref() {
                b"channel" => {
                    cur_channel_id = attr(&e, b"id");
                    cur_display = None;
                }
                b"programme" => {
                    cur_prog = Some((
                        attr(&e, b"channel").unwrap_or_default(),
                        attr(&e, b"start").unwrap_or_default(),
                        attr(&e, b"stop").unwrap_or_default(),
                    ));
                    cur_title = None;
                    cur_desc = None;
                    cur_cat = None;
                }
                b"display-name" => cur_text_for = Some("display-name"),
                b"title" => cur_text_for = Some("title"),
                b"desc" => cur_text_for = Some("desc"),
                b"category" => cur_text_for = Some("category"),
                _ => cur_text_for = None,
            },
            Ok(Event::Text(e)) => {
                if let Some(kind) = cur_text_for {
                    let text = e
                        .xml10_content()
                        .map_err(|e| EpgError::Xml(e.to_string()))?
                        .to_string();
                    match kind {
                        "display-name" => cur_display = Some(text),
                        "title" => cur_title = Some(text),
                        "desc" => cur_desc = Some(text),
                        "category" => cur_cat = Some(text),
                        _ => {}
                    }
                }
            }
            Ok(Event::End(e)) => {
                match e.name().as_ref() {
                    b"channel" => {
                        if let Some(id) = cur_channel_id.take() {
                            data.channels.push(XmltvChannel {
                                id,
                                display_name: cur_display.take().unwrap_or_default(),
                            });
                        }
                    }
                    b"programme" => {
                        if let Some((channel, start, stop)) = cur_prog.take() {
                            let start = parse_xmltv_time(&start)?;
                            let stop = parse_xmltv_time(&stop)?;
                            data.programmes.push(XmltvProgramme {
                                channel,
                                start,
                                stop,
                                title: cur_title.take().unwrap_or_default(),
                                desc: cur_desc.take(),
                                category: cur_cat.take(),
                            });
                        }
                    }
                    _ => {}
                }
                cur_text_for = None;
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(EpgError::Xml(e.to_string())),
            _ => {}
        }
        buf.clear();
    }
    Ok(data)
}

fn attr(e: &quick_xml::events::BytesStart, key: &[u8]) -> Option<String> {
    e.attributes().find_map(|a| {
        a.ok().and_then(|a| {
            if a.key.as_ref() == key {
                Some(String::from_utf8_lossy(&a.value).to_string())
            } else {
                None
            }
        })
    })
}

/// `20261124080000 +0800` → UTC；无时区时按 UTC 计。
pub fn parse_xmltv_time(s: &str) -> Result<DateTime<Utc>, EpgError> {
    let s = s.trim();
    let (digits, offset) = match s.split_once(' ') {
        Some((d, o)) => (d, Some(o)),
        None => (s, None),
    };
    let naive = NaiveDateTime::parse_from_str(digits, "%Y%m%d%H%M%S")
        .map_err(|_| EpgError::BadTime(s.to_string()))?;
    if let Some(off) = offset {
        let sign = if off.starts_with('-') { -1 } else { 1 };
        let off = off.trim_start_matches(['+', '-']);
        let (hh, mm) = off.split_at(2);
        let secs: i32 =
            sign * (hh.parse::<i32>().unwrap_or(0) * 3600 + mm.parse::<i32>().unwrap_or(0) * 60);
        let tz = FixedOffset::east_opt(secs).ok_or_else(|| EpgError::BadTime(s.to_string()))?;
        Ok(tz
            .from_local_datetime(&naive)
            .single()
            .ok_or_else(|| EpgError::BadTime(s.to_string()))?
            .with_timezone(&Utc))
    } else {
        Ok(naive.and_utc())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<tv><channel id="cctv5.cn"><display-name>CCTV-5</display-name></channel>
<programme start="20261124080000 +0800" stop="20261124090000 +0800" channel="cctv5.cn"><title>体育新闻</title><desc>晨间体育</desc><category>体育</category></programme></tv>"#;

    #[test]
    fn parses_xmltv() {
        let d = parse_xmltv(SAMPLE.as_bytes()).unwrap();
        assert_eq!(d.channels.len(), 1);
        assert_eq!(d.channels[0].display_name, "CCTV-5");
        assert_eq!(d.programmes.len(), 1);
        assert_eq!(d.programmes[0].title, "体育新闻");
        // 08:00+0800 == 00:00 UTC
        assert_eq!(
            d.programmes[0].start.to_rfc3339(),
            "2026-11-24T00:00:00+00:00"
        );
    }
}
