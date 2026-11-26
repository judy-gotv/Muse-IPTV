//! 语言包、locale 协商、格式化（开发文档 §12）。
//!
//! - Fluent（`.ftl`）资源编译期嵌入：`include_str!` 载入，运行时不读磁盘。
//! - 判定顺序（§12.2）：用户设置（DB）→ Cookie → `Accept-Language` →
//!   系统默认 → `zh-Hans-CN`；回退链 `zh-Hans-CN → zh-Hans → zh → en`。
//! - 默认语言：`zh-Hans-CN`（简体中文）。用户 2026-11-26 决定，覆盖
//!   文档 §12 的默认 `zh-Hant-TW`，见 README/ACCEPTANCE 偏差说明。
//! - 日期/数字格式化：`chrono` 按 locale 模式格式化（ICU 等价实现，
//!   见 README §12 偏差说明；复数走 Fluent 的 select / plural 规则）。
// 作者: Passwd Word

use fluent_bundle::concurrent::FluentBundle;
use fluent_bundle::{FluentArgs, FluentResource};
use std::collections::HashMap;
use std::sync::OnceLock;
use unic_langid::LanguageIdentifier;

pub const DEFAULT_LOCALE: &str = "zh-Hans-CN";
pub const SUPPORTED_LOCALES: &[&str] = &["zh-Hant-TW", "zh-Hans-CN", "en", "ja", "ko"];

/// 语言代码 → 界面显示名称（专有名词，直接写死，不走 i18n key；
/// 用户 2026-11-26 决定：语言选项显示名称，不显示代码）。
pub fn locale_display_name(code: &str) -> &'static str {
    match code {
        "zh-Hant-TW" => "繁体中文",
        "zh-Hans-CN" => "简体中文",
        "en" => "English",
        "ja" => "日本語",
        "ko" => "한국어",
        _ => "English",
    }
}

/// 编译期嵌入的语言包（§12.4：容器镜像只含二进制）。
/// `include_str!` 要求字面量路径，故按 locale 字面量分支。
fn bundle_for(locale: &str) -> FluentBundle<FluentResource> {
    let langid: LanguageIdentifier = locale
        .parse()
        .unwrap_or_else(|_| DEFAULT_LOCALE.parse().unwrap());
    let mut bundle = FluentBundle::new_concurrent(vec![langid]);
    let files: [&str; 4] = match locale {
        "zh-Hant-TW" => [
            include_str!("../../../locales/zh-Hant-TW/common.ftl"),
            include_str!("../../../locales/zh-Hant-TW/portal.ftl"),
            include_str!("../../../locales/zh-Hant-TW/admin.ftl"),
            include_str!("../../../locales/zh-Hant-TW/errors.ftl"),
        ],
        "zh-Hans-CN" => [
            include_str!("../../../locales/zh-Hans-CN/common.ftl"),
            include_str!("../../../locales/zh-Hans-CN/portal.ftl"),
            include_str!("../../../locales/zh-Hans-CN/admin.ftl"),
            include_str!("../../../locales/zh-Hans-CN/errors.ftl"),
        ],
        "ja" => [
            include_str!("../../../locales/ja/common.ftl"),
            include_str!("../../../locales/ja/portal.ftl"),
            include_str!("../../../locales/ja/admin.ftl"),
            include_str!("../../../locales/ja/errors.ftl"),
        ],
        "ko" => [
            include_str!("../../../locales/ko/common.ftl"),
            include_str!("../../../locales/ko/portal.ftl"),
            include_str!("../../../locales/ko/admin.ftl"),
            include_str!("../../../locales/ko/errors.ftl"),
        ],
        "en-XA" => [
            include_str!("../../../locales/en-XA/common.ftl"),
            include_str!("../../../locales/en-XA/portal.ftl"),
            include_str!("../../../locales/en-XA/admin.ftl"),
            include_str!("../../../locales/en-XA/errors.ftl"),
        ],
        _ => [
            include_str!("../../../locales/en/common.ftl"),
            include_str!("../../../locales/en/portal.ftl"),
            include_str!("../../../locales/en/admin.ftl"),
            include_str!("../../../locales/en/errors.ftl"),
        ],
    };
    for src in files {
        let res = FluentResource::try_new(src.to_string()).expect("ftl parses");
        bundle.add_resource(res).expect("no duplicate ftl keys");
    }
    bundle
}

static BUNDLES: OnceLock<HashMap<String, FluentBundle<FluentResource>>> = OnceLock::new();

fn bundles() -> &'static HashMap<String, FluentBundle<FluentResource>> {
    BUNDLES.get_or_init(|| {
        let mut map = HashMap::new();
        for loc in SUPPORTED_LOCALES.iter().chain(std::iter::once(&"en-XA")) {
            map.insert(loc.to_string(), bundle_for(loc));
        }
        map
    })
}

pub struct I18n;

impl I18n {
    pub fn new() -> Self {
        // 预热嵌入包，启动即发现 ftl 语法问题。
        let _ = bundles();
        Self
    }

    pub fn supported_locales() -> Vec<&'static str> {
        SUPPORTED_LOCALES.to_vec()
    }

    /// §12.2 回退链。
    pub fn fallback_chain(locale: &str) -> Vec<String> {
        let mut chain = vec![locale.to_string()];
        if let Some((lang, _)) = locale.split_once('-') {
            if locale != lang {
                chain.push(lang.to_string());
            }
        }
        match locale {
            "zh-Hant-TW" => chain.extend(["zh-Hant".into(), "zh".into()]),
            "zh-Hans-CN" => chain.extend(["zh-Hans".into(), "zh".into()]),
            _ => {}
        }
        if locale != "en" {
            chain.push("en".into());
        }
        chain
    }

    /// §12.2 判定顺序：用户设置 → Cookie → Accept-Language → 默认。
    /// `zh` 不带脚本时按地区猜测（TW/HK/MO → Hant，其余 → Hans）。
    pub fn negotiate(user: Option<&str>, cookie: Option<&str>, accept: Option<&str>) -> String {
        Self::negotiate_with_default(user, cookie, accept, DEFAULT_LOCALE)
    }

    /// 同 `negotiate`，但最后的“默认”由调用方传入（M1：API 中间件传系统
    /// 设置 `default_locale`；读不到时仍回退到 `DEFAULT_LOCALE`）。
    pub fn negotiate_with_default(
        user: Option<&str>,
        cookie: Option<&str>,
        accept: Option<&str>,
        default: &str,
    ) -> String {
        for cand in [user, cookie] {
            if let Some(c) = cand.and_then(normalize_locale) {
                return c;
            }
        }
        if let Some(header) = accept {
            for part in header.split(',') {
                let tag = part.split(';').next().unwrap_or("").trim();
                if let Some(c) = normalize_locale(tag) {
                    return c;
                }
                // `zh-TW` / `zh-HK` 无脚本时按地区猜测
                if tag.eq_ignore_ascii_case("zh-tw")
                    || tag.eq_ignore_ascii_case("zh-hk")
                    || tag.eq_ignore_ascii_case("zh-mo")
                {
                    return "zh-Hant-TW".to_string();
                }
                if tag.eq_ignore_ascii_case("zh-cn") || tag.eq_ignore_ascii_case("zh-sg") {
                    return "zh-Hans-CN".to_string();
                }
                if tag.eq_ignore_ascii_case("zh") {
                    return default.to_string();
                }
            }
        }
        default.to_string()
    }

    /// 查文案：按回退链逐个 locale 找，找不到返回 key 本身（并记为漏翻信号）。
    /// 逻辑 key 用点分层级（§12.3，如 `portal.quota.used`），Fluent 文件里的
    /// 标识符不允许有点，故查表时把 `.` 转成 `-`（如 `portal-quota-used`）。
    pub fn t(&self, locale: &str, key: &str, args: Option<&FluentArgs>) -> String {
        let ftl_key = key.replace('.', "-");
        for loc in Self::fallback_chain(locale) {
            if let Some(bundle) = bundles().get(&loc) {
                if let Some(msg) = bundle.get_message(&ftl_key) {
                    if let Some(pattern) = msg.value() {
                        let value = bundle.format_pattern(pattern, args, &mut vec![]);
                        return value.to_string();
                    }
                }
            }
        }
        key.to_string()
    }

    pub fn t_simple(&self, locale: &str, key: &str) -> String {
        self.t(locale, key, None)
    }

    /// 错误码本地化（§12.5）：`code` 不变，`message` 按 locale 渲染。
    pub fn error_message(&self, locale: &str, code: i32) -> String {
        self.t_simple(locale, &format!("errors.e{code}"))
    }

    /// 日期格式化：按 locale 的惯用顺序（ISO 为底，`zh` 用 `/` 分隔中文习惯）。
    pub fn format_datetime(&self, locale: &str, dt: chrono::DateTime<chrono::Utc>) -> String {
        match locale {
            "en" => dt.format("%Y-%m-%d %H:%M UTC").to_string(),
            "ja" => dt.format("%Y年%m月%d日 %H:%M").to_string(),
            "ko" => dt.format("%Y년 %m월 %d일 %H:%M").to_string(),
            _ => dt.format("%Y-%m-%d %H:%M").to_string(),
        }
    }

    /// 数字千分位（§12.6）。
    pub fn format_number(&self, _locale: &str, v: u64) -> String {
        let s = v.to_string();
        let mut out = String::new();
        for (i, c) in s.chars().rev().enumerate() {
            if i > 0 && i % 3 == 0 {
                out.push(',');
            }
            out.push(c);
        }
        out.chars().rev().collect()
    }
}

impl Default for I18n {
    fn default() -> Self {
        Self::new()
    }
}

/// 规范化 BCP 47 标签到支持的 locale；不支持返回 None。
/// 接受 `zh-Hant-TW`、`zh-TW`、`en-US`、`en` 等形式。
pub fn normalize_locale(tag: &str) -> Option<String> {
    let tag = tag.trim();
    if tag.is_empty() {
        return None;
    }
    let lower = tag.to_ascii_lowercase();
    for loc in SUPPORTED_LOCALES {
        if lower == loc.to_ascii_lowercase() {
            return Some(loc.to_string());
        }
    }
    // 带地区变体：en-US -> en, ja-JP -> ja, ko-KR -> ko
    let lang = lower.split('-').next().unwrap_or("");
    match lang {
        "en" => Some("en".to_string()),
        "ja" => Some("ja".to_string()),
        "ko" => Some("ko".to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_locales_have_all_keys() {
        // M11：全量断言——以 zh-Hant-TW 的 4 个 FTL 文件为 key 全集，要求
        // 每个 key 在全部 5 种语言 + en-XA 中都存在（此前只抽查 5 个 key，
        // 49 个缺 key 全部漏网）。解析规则与 xtask i18n-check 一致：
        // 行首 `key = value` 为新 key，缩进续行归属上一个 key。
        let files = [
            include_str!("../../../locales/zh-Hant-TW/common.ftl"),
            include_str!("../../../locales/zh-Hant-TW/portal.ftl"),
            include_str!("../../../locales/zh-Hant-TW/admin.ftl"),
            include_str!("../../../locales/zh-Hant-TW/errors.ftl"),
        ];
        let mut keys: Vec<String> = Vec::new();
        for src in files {
            for line in src.lines() {
                if line.starts_with(|c: char| c.is_whitespace())
                    || line.trim().is_empty()
                    || line.trim_start().starts_with('#')
                {
                    continue;
                }
                if let Some((k, _)) = line.split_once('=') {
                    let k = k.trim();
                    if !k.is_empty()
                        && k.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
                        && k.chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
                    {
                        // FTL key 用横线，代码 key 用点：`admin-dashboard` →
                        // `admin.dashboard`（与 `I18n::t` 的点转横线规则互逆）。
                        keys.push(k.replacen('-', ".", 1));
                    }
                }
            }
        }
        // M11 补齐时共 104 个 key；只增不减——后续补 key 只会让总数变大。
        assert!(
            keys.len() >= 104,
            "FTL key 总数异常：{}（预期 ≥ 104）",
            keys.len()
        );
        let i18n = I18n::new();
        for loc in SUPPORTED_LOCALES.iter().chain(std::iter::once(&"en-XA")) {
            for key in &keys {
                let v = i18n.t_simple(loc, key);
                assert_ne!(v, key.as_str(), "missing {key} in {loc}");
            }
        }
    }

    #[test]
    fn locale_display_names() {
        assert_eq!(locale_display_name("zh-Hant-TW"), "繁体中文");
        assert_eq!(locale_display_name("zh-Hans-CN"), "简体中文");
        assert_eq!(locale_display_name("en"), "English");
        assert_eq!(locale_display_name("ja"), "日本語");
        assert_eq!(locale_display_name("ko"), "한국어");
    }

    #[test]
    fn negotiate_order() {
        assert_eq!(I18n::negotiate(Some("ja"), Some("en"), Some("en-US")), "ja");
        assert_eq!(I18n::negotiate(None, Some("ko"), Some("en-US")), "ko");
        assert_eq!(I18n::negotiate(None, None, Some("en-US,en;q=0.9")), "en");
        assert_eq!(I18n::negotiate(None, None, Some("zh-TW")), "zh-Hant-TW");
        assert_eq!(I18n::negotiate(None, None, None), "zh-Hans-CN");
        assert_eq!(I18n::negotiate(None, None, Some("fr-FR")), "zh-Hans-CN");
    }

    #[test]
    fn fallback_and_args() {
        let i18n = I18n::new();
        let mut args = FluentArgs::new();
        args.set("used", 1);
        args.set("max", 2);
        let v = i18n.t("zh-Hant-TW", "portal.quota-used", Some(&args));
        assert!(v.contains("1") && v.contains("2"), "{v}");
        assert!(i18n.error_message("en", 42901).contains("Concurrent"));
        // 漏翻 key 原样返回（监控“未翻译 key 命中数”的信号）
        assert_eq!(i18n.t_simple("en", "no.such.key"), "no.such.key");
    }
}
