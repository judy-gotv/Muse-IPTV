//! 语言包、locale 协商、格式化（开发文档 §12）。
//!
//! - Fluent（`.ftl`）资源编译期嵌入：`include_str!` 载入，运行时不读磁盘。
//! - 判定顺序（§12.2）：用户设置（DB）→ Cookie → `Accept-Language` →
//!   系统默认 → `zh-Hans-CN`；回退链 `zh-Hans-CN → zh-Hans → zh → en`。
//! - 默认语言：`zh-Hans-CN`（简体中文）。用户 2026-11-26 决定，覆盖
//!   文档 §12 的默认 `zh-Hant-TW`，见 README/ACCEPTANCE 偏差说明。
//! - 日期/数字格式化：`chrono` + `chrono-tz` 按 locale 模式与 IANA 时区
//!   格式化（ICU 等价实现，见 README §12 偏差说明；复数走 Fluent 的
//!   select / plural 规则）；排序按 locale 定序（中文拼音、日文假名、
//!   英文大小写不敏感，`pinyin` crate 供中文拼音键）。
//! - 文案覆盖：`i18n_override` 表（§12.8 运行时修词，不必发版），
//!   由 API 进程启动时读库装入进程内覆盖表（`I18n::set_overrides`）；
//!   `I18n::t` 先查覆盖（指定 key 与 locale），再查 FTL。
// 作者: Passwd Word

use chrono_tz::Tz;
use fluent_bundle::concurrent::FluentBundle;
use fluent_bundle::{FluentArgs, FluentResource};
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};
use unic_langid::LanguageIdentifier;

pub const DEFAULT_LOCALE: &str = "zh-Hans-CN";
/// §12：时区按用户设置，默认 `Asia/Taipei`。
pub const DEFAULT_TIMEZONE: &str = "Asia/Taipei";
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

/// m22：`i18n_override` 表的进程内镜像（key 为 `(locale, dotted-key)`，
/// 如 `("zh-Hans-CN", "admin.settings")`）。API 进程启动时读库装入，
/// 后台改覆盖后刷新；后台/门户进程由读覆盖的 server fn 装入。覆盖只
/// 作用于指定 key 与 locale（§12.8），`t()` 先查覆盖再查 FTL。
static OVERRIDES: OnceLock<RwLock<HashMap<(String, String), String>>> = OnceLock::new();

fn overrides() -> &'static RwLock<HashMap<(String, String), String>> {
    OVERRIDES.get_or_init(|| RwLock::new(HashMap::new()))
}

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

    /// m22：整表装入覆盖（API 启动 / 后台改覆盖后刷新）。`key` 用点分
    /// 形式，与 `t()` 的入参一致。
    pub fn set_overrides(map: HashMap<(String, String), String>) {
        if let Ok(mut guard) = overrides().write() {
            *guard = map;
        }
    }

    /// m22：单条覆盖（后台改覆盖后调用；空 `value` 视为删除该覆盖）。
    pub fn set_override(locale: &str, key: &str, value: &str) {
        if let Ok(mut guard) = overrides().write() {
            if value.is_empty() {
                guard.remove(&(locale.to_string(), key.to_string()));
            } else {
                guard.insert((locale.to_string(), key.to_string()), value.to_string());
            }
        }
    }

    /// m22：删除单条覆盖。
    pub fn remove_override(locale: &str, key: &str) {
        if let Ok(mut guard) = overrides().write() {
            guard.remove(&(locale.to_string(), key.to_string()));
        }
    }

    /// m22：清空全部覆盖（仅测试用）。
    #[cfg(test)]
    pub fn clear_overrides() {
        if let Ok(mut guard) = overrides().write() {
            guard.clear();
        }
    }

    /// 查覆盖：按回退链逐个 locale 找（`zh-Hans-CN` 的覆盖不作用于
    /// `en`，反之亦然；回退链内先命中先用）。
    fn lookup_override(locale: &str, key: &str) -> Option<String> {
        let guard = overrides().read().ok()?;
        for loc in Self::fallback_chain(locale) {
            if let Some(v) = guard.get(&(loc, key.to_string())) {
                return Some(v.clone());
            }
        }
        None
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

    /// 查文案：先查 `i18n_override` 覆盖（m22），再按回退链逐个 locale
    /// 找 FTL，找不到返回 key 本身（并记为漏翻信号）。
    /// 逻辑 key 用点分层级（§12.3，如 `portal.quota.used`），Fluent 文件里的
    /// 标识符不允许有点，故查表时把 `.` 转成 `-`（如 `portal-quota-used`）。
    /// 覆盖值里的 `{ $var }` 占位符按传入的 args 做简单替换（覆盖只换文案）。
    pub fn t(&self, locale: &str, key: &str, args: Option<&FluentArgs>) -> String {
        if let Some(v) = Self::lookup_override(locale, key) {
            return interpolate_override(&v, args);
        }
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

    /// N1：按 FTL key + JSON 参数渲染文案（`ApiError::with_key` 的序列化
    /// 点用；参数按 JSON 类型进 Fluent：整数/浮点进数字，字符串进文本，
    /// 其余转字符串）。key 缺失时返回 key 本身，调用方回退英文兜底。
    pub fn t_json_args(
        &self,
        locale: &str,
        key: &str,
        args: &serde_json::Map<String, serde_json::Value>,
    ) -> String {
        let mut fa = FluentArgs::new();
        for (k, v) in args {
            match v {
                serde_json::Value::Number(n) => {
                    if let Some(i) = n.as_i64() {
                        fa.set(k.as_str(), i);
                    } else if let Some(f) = n.as_f64() {
                        fa.set(k.as_str(), f);
                    } else {
                        fa.set(k.as_str(), n.to_string());
                    }
                }
                serde_json::Value::String(s) => {
                    fa.set(k.as_str(), s.as_str());
                }
                serde_json::Value::Bool(b) => {
                    fa.set(k.as_str(), b.to_string());
                }
                other => {
                    fa.set(k.as_str(), other.to_string());
                }
            }
        }
        self.t(locale, key, Some(&fa))
    }

    /// m15：解析时区名：IANA 名（`Asia/Taipei`）、`UTC`/`GMT`；非法时
    /// 回退默认时区 `Asia/Taipei`（§12）。固定偏移（`+08:00`）由
    /// `format_datetime_in_tz` 另行处理（`Tz` 无法表示固定偏移）。
    pub fn parse_timezone(name: &str) -> Tz {
        let name = name.trim();
        if name.eq_ignore_ascii_case("utc") || name.eq_ignore_ascii_case("gmt") {
            return chrono_tz::UTC;
        }
        if let Ok(tz) = name.parse::<Tz>() {
            return tz;
        }
        DEFAULT_TIMEZONE.parse().unwrap_or(chrono_tz::UTC)
    }

    /// 日期格式化（m15）：先把 UTC 时间换算到 `tz`，再按 locale 的惯用
    /// 顺序排版，末尾带时区缩写（§12.6：绝对时间带时区）。
    /// - en：`Nov 27, 2026, 02:30 PM CST`；en-XA：伪本地化加长版。
    /// - ja：`2026年11月27日 14:30 JST`；ko：`2026년 11월 27일 14:30 KST`。
    /// - zh-Hans-CN / zh-Hant-TW：`2026年11月27日 14:30 CST`。
    pub fn format_datetime(
        &self,
        locale: &str,
        dt: chrono::DateTime<chrono::Utc>,
        tz: Tz,
    ) -> String {
        Self::fmt_local(locale, dt.with_timezone(&tz))
    }

    /// `format_datetime` 的字符串时区版：`tz_name` 为 IANA 名
    /// （`Asia/Taipei` / `UTC` / `America/New_York`，§12.9 三档）或固定
    /// 偏移（`+08:00`）；非法时回退 `DEFAULT_TIMEZONE`。
    pub fn format_datetime_in_tz(
        &self,
        locale: &str,
        dt: chrono::DateTime<chrono::Utc>,
        tz_name: &str,
    ) -> String {
        let name = tz_name.trim();
        if name.eq_ignore_ascii_case("utc") || name.eq_ignore_ascii_case("gmt") {
            return Self::fmt_local(locale, dt.with_timezone(&chrono_tz::UTC));
        }
        if let Ok(tz) = name.parse::<Tz>() {
            return self.format_datetime(locale, dt, tz);
        }
        if let Some(off) = parse_fixed_offset(name) {
            if let Some(fixed) = chrono::FixedOffset::east_opt(off) {
                return Self::fmt_local(locale, dt.with_timezone(&fixed));
            }
        }
        self.format_datetime(locale, dt, Self::parse_timezone(tz_name))
    }

    /// 已换算到目标时区的本地时间按 locale 排版（`%Z` 带时区缩写）。
    fn fmt_local<Tz2>(locale: &str, local: chrono::DateTime<Tz2>) -> String
    where
        Tz2: chrono::TimeZone,
        Tz2::Offset: std::fmt::Display,
    {
        let zone = local.format("%Z").to_string();
        let body = match locale {
            "en" => local.format("%b %d, %Y, %I:%M %p").to_string(),
            "ja" => local.format("%Y年%m月%d日 %H:%M").to_string(),
            "ko" => local.format("%Y년 %m월 %d일 %H:%M").to_string(),
            "en-XA" => pseudo_localize(&local.format("%b %d, %Y, %I:%M %p").to_string()),
            _ => local.format("%Y年%m月%d日 %H:%M").to_string(),
        };
        format!("{body} {zone}")
    }

    /// 数字千分位/小数点按 locale（§12.6，m15）。
    /// 六语的 CLDR 约定：分组符一律 `,`、小数点一律 `.`（en/ja/ko/
    /// zh-Hans-CN/zh-Hant-TW/en-XA 均如此）；分组符与小数点由
    /// `number_separators` 集中定义，后续加 locale 时只改该表。
    pub fn format_number(&self, locale: &str, v: u64) -> String {
        let (group, _) = Self::number_separators(locale);
        group_integer(&v.to_string(), group)
    }

    /// 带小数的数字：整数部分按 locale 分组，小数部分按 `precision` 位
    /// 截断/补零，小数点按 locale（`1,234,567.89`）。
    pub fn format_decimal(&self, locale: &str, v: f64, precision: usize) -> String {
        let (group, decimal) = Self::number_separators(locale);
        let sign = if v < 0.0 { "-" } else { "" };
        let text = format!("{:.precision$}", v.abs(), precision = precision);
        let (int_part, frac_part) = match text.split_once('.') {
            Some((i, f)) => (i.to_string(), Some(f.to_string())),
            None => (text, None),
        };
        let grouped = group_integer(&int_part, group);
        match frac_part {
            Some(f) => format!("{sign}{grouped}{decimal}{f}"),
            None => format!("{sign}{grouped}"),
        }
    }

    /// 各 locale 的（分组符，小数点）。CLDR：六语均为 `,` / `.`。
    pub fn number_separators(locale: &str) -> (char, char) {
        match locale {
            "en" | "en-XA" | "ja" | "ko" | "zh-Hans-CN" | "zh-Hant-TW" => (',', '.'),
            _ => (',', '.'),
        }
    }

    /// m22 排序（§12.9）：按 locale 定序比较后原地排序。
    /// - en：大小写不敏感（`Apple` 与 `apple` 相邻），再按原串 tiebreak；
    /// - ja：片假名先折成平假名再比（`ア` 与 `あ` 同序）；
    /// - zh：按拼音排（`pinyin` crate；无拼音的字符按原字符排最后）；
    /// - 其余 locale：按 Unicode 码点（与此前行为一致）。
    pub fn sort_strings(&self, locale: &str, items: &mut [String]) {
        items.sort_by(|a, b| {
            let ka = Self::sort_key(locale, a);
            let kb = Self::sort_key(locale, b);
            ka.cmp(&kb).then_with(|| a.cmp(b))
        });
    }

    /// 单条排序键（与 `sort_strings` 同规则，供调用方复用）。
    pub fn sort_key(locale: &str, s: &str) -> String {
        match locale {
            "ja" => s.chars().map(fold_kana).collect(),
            "ko" | "en" | "en-XA" => s.to_lowercase(),
            "zh-Hans-CN" | "zh-Hant-TW" => pinyin_sort_key(s),
            _ => s.to_string(),
        }
    }
}

/// 覆盖值里的 `{ $var }` 占位符替换（m22；无 args 时原样返回）。
fn interpolate_override(value: &str, args: Option<&FluentArgs>) -> String {
    let Some(args) = args else {
        return value.to_string();
    };
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find('{') {
        let (head, tail) = rest.split_at(start);
        out.push_str(head);
        let Some(end) = tail.find('}') else {
            out.push_str(tail);
            return out;
        };
        let inner = tail[1..end].trim();
        if let Some(name) = inner.strip_prefix('$') {
            let name = name.trim();
            if let Some(rendered) = args.get(name).and_then(|v| match v {
                fluent_bundle::FluentValue::String(s) => Some(s.to_string()),
                fluent_bundle::FluentValue::Number(n) => Some(n.as_string().to_string()),
                _ => None,
            }) {
                out.push_str(&rendered);
            } else {
                out.push_str(&tail[..=end]);
            }
        } else {
            out.push_str(&tail[..=end]);
        }
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    out
}

/// 固定偏移解析：`+08:00` / `-0530` / `+08` → 秒数；非法返回 None。
fn parse_fixed_offset(name: &str) -> Option<i32> {
    let (sign, digits) = match name.strip_prefix('+') {
        Some(d) => (1i32, d),
        None => (-1i32, name.strip_prefix('-')?),
    };
    let compact: String = digits.chars().filter(|c| *c != ':').collect();
    if compact.len() != 2 && compact.len() != 4 {
        return None;
    }
    let hours: i32 = compact[..2].parse().ok()?;
    let mins: i32 = if compact.len() == 4 {
        compact[2..].parse().ok()?
    } else {
        0
    };
    if !(0..24).contains(&hours) || !(0..60).contains(&mins) {
        return None;
    }
    Some(sign * (hours * 3600 + mins * 60))
}

/// 整数部分按 `group` 每三位分组（`1234567` → `1,234,567`）。
fn group_integer(digits: &str, group: char) -> String {
    let mut out = String::new();
    for (i, c) in digits.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(group);
        }
        out.push(c);
    }
    out.chars().rev().collect()
}

/// en-XA 伪本地化（§12.8 步骤 4：字符替换 + 加长）：ASCII 字母换重音
/// 形、末尾加 ` [~~~]`，供溢出测试断言“比英文长”。
fn pseudo_localize(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 6);
    for c in s.chars() {
        out.push(match c {
            'a' => 'à',
            'e' => 'ḗ',
            'i' => 'î',
            'o' => 'ø',
            'u' => 'ŭ',
            'A' => 'À',
            'E' => 'Ḗ',
            'I' => 'Î',
            'O' => 'Ø',
            'U' => 'Ŭ',
            'n' => 'ñ',
            'N' => 'Ñ',
            's' => 'ş',
            'S' => 'Ş',
            't' => 'ŧ',
            'T' => 'Ŧ',
            'c' => 'ƈ',
            'C' => 'Ƈ',
            'd' => 'ď',
            'D' => 'Ď',
            'l' => 'ŀ',
            'L' => 'Ŀ',
            'r' => 'ř',
            'R' => 'Ř',
            'y' => 'ŷ',
            'Y' => 'Ŷ',
            'p' => 'ƥ',
            'P' => 'Ƥ',
            'm' => 'ɱ',
            'M' => 'Ṁ',
            other => other,
        });
    }
    out.push_str(" [~~~]");
    out
}

/// 片假名折成平假名（`ア` → `あ`；浊点/半浊点保持原字符序）。
fn fold_kana(c: char) -> char {
    if ('\u{30A1}'..='\u{30F6}').contains(&c) {
        char::from_u32(c as u32 - 0x60).unwrap_or(c)
    } else {
        c
    }
}

/// 中文拼音排序键：每字取无调拼音（如 `苹` → `ping`），非汉字原样保留。
fn pinyin_sort_key(s: &str) -> String {
    use pinyin::ToPinyin;
    let mut out = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        match c.to_pinyin() {
            Some(p) => out.push_str(p.plain()),
            None => out.push(c),
        }
        out.push('\u{1}');
    }
    out
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
    fn format_datetime_timezones() {
        // m15 / §12.9 三档时区：同一 UTC 时刻在三地落在不同日期/小时。
        // 2026-11-27T00:30:00Z：台北 11-27 08:30、UTC 11-27 00:30、
        // 纽约 11-26 19:30（EST，UTC-5）。
        use chrono::TimeZone;
        let i18n = I18n::new();
        let dt = chrono::Utc
            .with_ymd_and_hms(2026, 11, 27, 0, 30, 0)
            .unwrap();
        let taipei = i18n.format_datetime_in_tz("zh-Hans-CN", dt, "Asia/Taipei");
        assert!(taipei.starts_with("2026年11月27日 08:30"), "{taipei}");
        let utc = i18n.format_datetime_in_tz("zh-Hans-CN", dt, "UTC");
        assert!(utc.starts_with("2026年11月27日 00:30"), "{utc}");
        let ny = i18n.format_datetime_in_tz("zh-Hans-CN", dt, "America/New_York");
        assert!(ny.starts_with("2026年11月26日 19:30"), "{ny}");
        // 夏令时：2026-07-01T12:00:00Z 在纽约是 EDT（UTC-4）08:00，
        // 冬季同一时刻是 EST（UTC-5）07:00——固定偏移会算错这两档。
        let summer = chrono::Utc.with_ymd_and_hms(2026, 7, 1, 12, 0, 0).unwrap();
        let ny_summer = i18n.format_datetime_in_tz("en", summer, "America/New_York");
        assert!(ny_summer.contains("08:00 AM"), "{ny_summer}");
        let winter = chrono::Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
        let ny_winter = i18n.format_datetime_in_tz("en", winter, "America/New_York");
        assert!(ny_winter.contains("07:00 AM"), "{ny_winter}");
        // 非法时区名回退默认 Asia/Taipei；固定偏移 +08:00 与台北同值。
        let bad = i18n.format_datetime_in_tz("zh-Hans-CN", dt, "Mars/Olympus");
        assert_eq!(bad, taipei, "{bad}");
        let fixed = i18n.format_datetime_in_tz("zh-Hans-CN", dt, "+08:00");
        assert!(fixed.starts_with("2026年11月27日 08:30"), "{fixed}");
        // 各 locale 的排版都带时区缩写（§12.6：绝对时间带时区）。
        assert!(i18n
            .format_datetime_in_tz("en", dt, "Asia/Taipei")
            .ends_with("CST"));
        assert!(i18n
            .format_datetime_in_tz("ja", dt, "Asia/Tokyo")
            .starts_with("2026年11月27日 09:30"));
        assert!(i18n
            .format_datetime_in_tz("ko", dt, "Asia/Seoul")
            .starts_with("2026년 11월 27일 09:30"));
    }

    #[test]
    fn format_number_all_locales() {
        // m15：六语千分位/小数点（CLDR：六语均为分组 `,`、小数 `.`）。
        let i18n = I18n::new();
        for loc in SUPPORTED_LOCALES.iter().chain(std::iter::once(&"en-XA")) {
            assert_eq!(i18n.format_number(loc, 1_234_567), "1,234,567", "{loc}");
            assert_eq!(i18n.format_number(loc, 999), "999", "{loc}");
            assert_eq!(
                i18n.format_decimal(loc, 1_234_567.89, 2),
                "1,234,567.89",
                "{loc}"
            );
            assert_eq!(i18n.format_decimal(loc, -42.5, 1), "-42.5", "{loc}");
            assert_eq!(I18n::number_separators(loc), (',', '.'), "{loc}");
        }
    }

    #[test]
    fn sort_strings_by_locale() {
        // m22 / §12.9 排序测试。
        let i18n = I18n::new();
        // 英文：大小写不敏感（Apple 与 apple 相邻，不按 ASCII 码点排）。
        let mut en = vec![
            "banana".to_string(),
            "Apple".to_string(),
            "cherry".to_string(),
        ];
        i18n.sort_strings("en", &mut en);
        assert_eq!(en, ["Apple", "banana", "cherry"]);
        // 日文：假名序，片假名折成平假名后同序。
        let mut ja = vec!["う".to_string(), "あ".to_string(), "エ".to_string()];
        i18n.sort_strings("ja", &mut ja);
        assert_eq!(ja, ["あ", "う", "エ"]);
        // 中文：拼音序（橙子 chéngzi < 苹果 píngguǒ < 香蕉 xiāngjiāo；
        // 按码点排会是完全不同的顺序）。
        let mut zh = vec!["香蕉".to_string(), "苹果".to_string(), "橙子".to_string()];
        i18n.sort_strings("zh-Hans-CN", &mut zh);
        assert_eq!(zh, ["橙子", "苹果", "香蕉"]);
    }

    #[test]
    fn en_xa_pseudo_localization_is_longer() {
        // m22 / §12.8 步骤 4：en-XA 伪本地化必须比英文长（溢出测试才有效），
        // 且每条文案都与英文不同。抽查各文件有代表性的 key。
        let i18n = I18n::new();
        let keys = [
            "admin.dashboard",
            "admin.settings",
            "admin.create-token",
            "common.save",
            "common.cancel",
            "common.loading",
            "portal.quota-used",
            "errors.e40900",
        ];
        for key in keys {
            let en = i18n.t_simple("en", key);
            let xa = i18n.t_simple("en-XA", key);
            assert_ne!(en, key, "en 缺 key {key}");
            assert_ne!(xa, en, "en-XA 与 en 相同：{key} = {xa}");
            assert!(
                xa.chars().count() > en.chars().count(),
                "en-XA 未加长：{key} en={en:?} xa={xa:?}"
            );
        }
        // 伪本地化日期也比英文长。
        use chrono::TimeZone;
        let dt = chrono::Utc
            .with_ymd_and_hms(2026, 11, 27, 0, 30, 0)
            .unwrap();
        let en = i18n.format_datetime_in_tz("en", dt, "UTC");
        let xa = i18n.format_datetime_in_tz("en-XA", dt, "UTC");
        assert!(xa.chars().count() > en.chars().count(), "en={en} xa={xa}");
    }

    #[test]
    fn i18n_override_wins_over_ftl() {
        // m22：覆盖优先于 FTL，只作用于指定 key 与 locale；删除后恢复 FTL。
        let i18n = I18n::new();
        I18n::clear_overrides();
        let before = i18n.t_simple("zh-Hans-CN", "admin.settings");
        assert_ne!(before, "系统设置（覆盖）");
        I18n::set_override("zh-Hans-CN", "admin.settings", "系统设置（覆盖）");
        assert_eq!(
            i18n.t_simple("zh-Hans-CN", "admin.settings"),
            "系统设置（覆盖）"
        );
        // 同 key 的英文不受中文覆盖影响。
        assert_ne!(i18n.t_simple("en", "admin.settings"), "系统设置（覆盖）");
        // 覆盖值里的 { $var } 按 args 替换。
        let mut args = FluentArgs::new();
        args.set("used", 1);
        args.set("max", 2);
        I18n::set_override(
            "en",
            "portal.quota-used",
            "Used { $used } of { $max } (override)",
        );
        assert_eq!(
            i18n.t("en", "portal.quota-used", Some(&args)),
            "Used 1 of 2 (override)"
        );
        // FTL 里没有的 key 也能被覆盖补上；删覆盖后回落 key 本身。
        I18n::set_override("en", "no.such.key", "covered");
        assert_eq!(i18n.t_simple("en", "no.such.key"), "covered");
        I18n::remove_override("en", "no.such.key");
        assert_eq!(i18n.t_simple("en", "no.such.key"), "no.such.key");
        I18n::remove_override("zh-Hans-CN", "admin.settings");
        assert_eq!(i18n.t_simple("zh-Hans-CN", "admin.settings"), before);
        I18n::remove_override("en", "portal.quota-used");
        I18n::clear_overrides();
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
