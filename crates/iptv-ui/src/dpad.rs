//! 电视端 D-pad / 遥控器导航（开发文档 §11.6 / §16，m23）。
//!
//! 分两层：
//! - 纯函数（全平台可测）：[`DpadKey::from_key`]（键名 → 导航意图）、
//!   [`next_focus`]（按空间方向选下一焦点，不是 DOM 顺序 Tab）、
//!   [`back_action`]（返回键：有历史则后退，无历史则回首页）、
//!   [`looks_like_tv`]（电视/盒子 UA 判定，供门户/后台在 Cookie 未设置
//!   `iptv_device` 时兜底）、[`is_text_editable`]（文本编辑态判定，方向键
//!   与 Backspace 在输入框内不劫持，留给光标/删字用）。
//! - 浏览器接线（仅 wasm32）：[`handle_keydown`]，由门户/后台的 `<body
//!   on:keydown>` 调用，只在 `data-device="tv"` 时生效；普通 web 端
//!   不启用，不改现有键盘行为。
//! - 提示条组件 [`TvRemoteHint`]：电视端在页面底部显示遥控用法，`is_tv`
//!   由门户/后台按 Cookie 判定后传入。
// 作者: Passwd Word

use crate::t_or;
use leptos::prelude::*;

/// 元素几何（`getBoundingClientRect` 的子集），坐标为视口 CSS 像素。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn center(&self) -> (f64, f64) {
        (self.x + self.width / 2.0, self.y + self.height / 2.0)
    }

    pub fn is_empty(&self) -> bool {
        self.width <= 0.0 || self.height <= 0.0
    }

    /// 与另一矩形是否同一元素（中心与尺寸都在半像素内；可聚焦元素列表
    /// 里会包含当前焦点元素本身，选下一焦点时必须跳过它）。
    fn same_as(&self, other: &Self) -> bool {
        let (cx, cy) = self.center();
        let (ox, oy) = other.center();
        (cx - ox).abs() <= 0.5
            && (cy - oy).abs() <= 0.5
            && (self.width - other.width).abs() <= 0.5
            && (self.height - other.height).abs() <= 0.5
    }
}

/// 空间方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

/// 遥控键意图。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DpadKey {
    /// 方向键：按空间方向移动焦点。
    Move(Direction),
    /// 确认键：激活当前焦点（原生可激活元素由浏览器默认处理）。
    Activate,
    /// 返回键：Escape / Backspace。
    Back,
}

impl DpadKey {
    /// `KeyboardEvent.key()` → 导航意图；不关心的键返回 `None`。
    /// Space 的 `key` 是单个空格 `" "`，老式浏览器为 `"Spacebar"`。
    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "ArrowUp" => Some(Self::Move(Direction::Up)),
            "ArrowDown" => Some(Self::Move(Direction::Down)),
            "ArrowLeft" => Some(Self::Move(Direction::Left)),
            "ArrowRight" => Some(Self::Move(Direction::Right)),
            "Enter" => Some(Self::Activate),
            " " | "Spacebar" => Some(Self::Activate),
            "Escape" | "Backspace" => Some(Self::Back),
            _ => None,
        }
    }
}

/// 按空间方向选下一焦点：给定当前焦点几何与候选几何，返回候选下标。
///
/// 算法：只考虑中心落在该方向半平面内的候选（沿方向距离 `primary > 0`），
/// 排除夹角过大的候选（垂直偏移 `> 2 × primary + 当前元素在该轴的尺寸`），
/// 得分 `primary + 2.5 × secondary` 最小者胜——同排对齐的近处元素优先于
/// 斜方向更近的元素；无候选时返回 `None`（焦点不动）。
/// 当前元素本身（几何相同）与零尺寸候选直接跳过。
pub fn next_focus(current: Rect, candidates: &[Rect], dir: Direction) -> Option<usize> {
    if current.is_empty() {
        return None;
    }
    let (cx, cy) = current.center();
    // 夹角门限里加当前元素在垂直轴上的尺寸：与当前元素有重叠带的候选
    // （如等高卡片行）即使中心略偏也不应被排除。
    let secondary_limit = match dir {
        Direction::Left | Direction::Right => current.height,
        Direction::Up | Direction::Down => current.width,
    };
    let mut best: Option<(f64, usize)> = None;
    for (i, cand) in candidates.iter().enumerate() {
        if cand.is_empty() || cand.same_as(&current) {
            continue;
        }
        let (x, y) = cand.center();
        let (primary, secondary) = match dir {
            Direction::Right => (x - cx, (y - cy).abs()),
            Direction::Left => (cx - x, (y - cy).abs()),
            Direction::Down => (y - cy, (x - cx).abs()),
            Direction::Up => (cy - y, (x - cx).abs()),
        };
        if primary <= 0.0 || secondary > primary * 2.0 + secondary_limit {
            continue;
        }
        let score = primary + 2.5 * secondary;
        if best.is_none_or(|(bs, _)| score < bs) {
            best = Some((score, i));
        }
    }
    best.map(|(_, i)| i)
}

/// 返回键行为。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackAction {
    /// 有浏览历史：`history.back()`。
    GoBack,
    /// 无历史可退：跳首页/上一级（门户 `/`、后台 `/admin`）。
    GoHome,
}

/// `history.length()`（含当前页）大于 1 才有可退的历史。
pub fn back_action(history_len: u32) -> BackAction {
    if history_len > 1 {
        BackAction::GoBack
    } else {
        BackAction::GoHome
    }
}

/// 电视/盒子 UA 特征（小写包含匹配）。只作 `iptv_device` Cookie 未设置
/// 时的兜底，Cookie 显式值优先；桌面/手机 UA 不命中。
const TV_UA_MARKERS: &[&str] = &[
    "smarttv",
    "smart-tv",
    "tizen",
    "web0s",
    "android tv",
    "googletv",
    "appletv",
    "aftk",
    "aftm",
    "aftr",  // Fire TV 机型前缀（AFTKA/AFTMM/...）
    "crkey", // Chromecast
    "viera",
    "netcast",
    "roku",
    "fire tv",
    "hbbtv",
];

pub fn looks_like_tv(user_agent: &str) -> bool {
    let ua = user_agent.to_ascii_lowercase();
    TV_UA_MARKERS.iter().any(|m| ua.contains(m))
}

/// 当前焦点是否处于文本编辑态：方向键与 Backspace 此时不劫持，留给
/// 光标移动/删字；`INPUT` 只有文本类 `type` 才算（checkbox/button 等
/// 不算）。`tag_name` 为大写（如 `INPUT`），`input_type` 为 `type`
/// 属性值（缺省为空串，按 text 处理）。
pub fn is_text_editable(tag_name: &str, input_type: &str, content_editable: bool) -> bool {
    if content_editable {
        return true;
    }
    match tag_name {
        "TEXTAREA" | "SELECT" => true,
        "INPUT" => matches!(
            input_type.to_ascii_lowercase().as_str(),
            "" | "text"
                | "search"
                | "url"
                | "tel"
                | "password"
                | "email"
                | "number"
                | "date"
                | "time"
                | "datetime-local"
                | "month"
                | "week"
        ),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// 浏览器接线（仅 wasm32；SSR 构建不编译这里，`cargo check` 需另跑 wasm32）
// ---------------------------------------------------------------------------

/// 可聚焦元素选择器：链接、按钮、输入、下来框与自定义 `tabindex` 元素。
#[cfg(target_arch = "wasm32")]
const FOCUSABLE_SELECTOR: &str = "a[href], button, input, select, textarea, [tabindex]";

/// 当前页是否为电视端（`<html data-device="tv">`，由服务端按 Cookie/UA 写）。
#[cfg(target_arch = "wasm32")]
fn is_tv_device() -> bool {
    web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.document_element())
        .and_then(|el| el.get_attribute("data-device"))
        .is_some_and(|v| v == "tv")
}

/// 收集可聚焦元素（文档顺序）及其几何：跳过禁用、跳过 `tabindex="-1"`、
/// 跳过零尺寸（`display:none` / `type="hidden"` 等）。
#[cfg(target_arch = "wasm32")]
fn focusable_elements(document: &web_sys::Document) -> Vec<(web_sys::HtmlElement, Rect)> {
    use wasm_bindgen::JsCast;
    let Ok(list) = document.query_selector_all(FOCUSABLE_SELECTOR) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for i in 0..list.length() {
        let Some(node) = list.item(i) else {
            continue;
        };
        let Ok(el) = node.dyn_into::<web_sys::HtmlElement>() else {
            continue;
        };
        if el.has_attribute("disabled") {
            continue;
        }
        if el
            .get_attribute("tabindex")
            .is_some_and(|t| t.trim() == "-1")
        {
            continue;
        }
        let r = el.get_bounding_client_rect();
        let rect = Rect::new(r.x(), r.y(), r.width(), r.height());
        if rect.is_empty() {
            continue;
        }
        out.push((el, rect));
    }
    out
}

/// 当前焦点几何；无焦点或焦点在 `<body>` 上时返回 `None`。
#[cfg(target_arch = "wasm32")]
fn current_focus_rect(document: &web_sys::Document) -> Option<Rect> {
    let active = document.active_element()?;
    if active.tag_name() == "BODY" {
        return None;
    }
    let r = active.get_bounding_client_rect();
    let rect = Rect::new(r.x(), r.y(), r.width(), r.height());
    if rect.is_empty() {
        None
    } else {
        Some(rect)
    }
}

/// 当前焦点是否处于文本编辑态。
#[cfg(target_arch = "wasm32")]
fn active_is_text_editable(document: &web_sys::Document) -> bool {
    let Some(active) = document.active_element() else {
        return false;
    };
    let input_type = active.get_attribute("type").unwrap_or_default();
    let content_editable = active
        .get_attribute("contenteditable")
        .is_some_and(|v| v != "false");
    is_text_editable(&active.tag_name(), &input_type, content_editable)
}

#[cfg(target_arch = "wasm32")]
fn focus_first(document: &web_sys::Document) {
    if let Some((el, _)) = focusable_elements(document).into_iter().next() {
        let _ = el.focus();
    }
}

#[cfg(target_arch = "wasm32")]
fn move_focus(document: &web_sys::Document, dir: Direction) {
    let items = focusable_elements(document);
    if items.is_empty() {
        return;
    }
    let Some(cur) = current_focus_rect(document) else {
        // 无焦点：先落到首个可聚焦元素，而不是不动。
        focus_first(document);
        return;
    };
    let rects: Vec<Rect> = items.iter().map(|(_, r)| *r).collect();
    if let Some(i) = next_focus(cur, &rects, dir) {
        let _ = items[i].0.focus();
    }
}

/// `<body on:keydown>` 的处理入口（只在电视端生效）。
///
/// - 方向键：在可聚焦元素间按空间方向移动焦点；输入框内不劫持。
/// - 确认键：原生可激活元素（链接/按钮/输入/下拉）由浏览器默认处理；
///   自定义 `tabindex` 元素没有默认激活行为，合成一次 `click()`；
///   无焦点时先落到首个可聚焦元素。
/// - 返回键（Escape/Backspace）：输入框内不劫持；否则有历史则
///   `history.back()`，无历史则跳 `home_href`（门户 `/`、后台 `/admin`）。
#[cfg(target_arch = "wasm32")]
pub fn handle_keydown(ev: &web_sys::KeyboardEvent, home_href: &str) {
    use wasm_bindgen::JsCast;
    if !is_tv_device() {
        return;
    }
    let Some(key) = DpadKey::from_key(&ev.key()) else {
        return;
    };
    let Some(window) = web_sys::window() else {
        return;
    };
    let Some(document) = window.document() else {
        return;
    };
    match key {
        DpadKey::Move(dir) => {
            if active_is_text_editable(&document) {
                return;
            }
            ev.prevent_default();
            move_focus(&document, dir);
        }
        DpadKey::Activate => {
            let Some(active) = document.active_element() else {
                focus_first(&document);
                return;
            };
            if active.tag_name() == "BODY" {
                focus_first(&document);
                return;
            }
            if matches!(
                active.tag_name().as_str(),
                "A" | "BUTTON" | "INPUT" | "SELECT" | "TEXTAREA"
            ) {
                return;
            }
            ev.prevent_default();
            if let Ok(el) = active.dyn_into::<web_sys::HtmlElement>() {
                el.click();
            }
        }
        DpadKey::Back => {
            if active_is_text_editable(&document) {
                return;
            }
            ev.prevent_default();
            let history_len = window
                .history()
                .ok()
                .and_then(|h| h.length().ok())
                .unwrap_or(0);
            match back_action(history_len) {
                BackAction::GoBack => {
                    if let Ok(history) = window.history() {
                        let _ = history.back();
                    }
                }
                BackAction::GoHome => {
                    let _ = window.location().set_href(home_href);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 提示条组件
// ---------------------------------------------------------------------------

/// 电视端遥控提示条：只在电视端渲染（`is_tv` 由门户/后台按 Cookie 判定
/// 后传入）；普通 web 端不渲染任何内容。
#[component]
pub fn TvRemoteHint(is_tv: bool) -> impl IntoView {
    let hint = t_or(
        "common.tv-remote-hint",
        "TV remote: arrow keys to move, OK to confirm, Back to go back.",
    );
    view! {
        {is_tv.then(|| view! {
            <div
                role="note"
                style="position:fixed;left:50%;transform:translateX(-50%);bottom:12px;z-index:60;max-width:90vw;padding:8px 16px;border-radius:999px;background:rgba(0,0,0,0.65);color:#fff;text-align:center;pointer-events:none"
            >{hint}</div>
        })}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2×2 卡片网格（卡片 100×50，横向间距 20，纵向间距 20）。
    fn grid() -> Vec<Rect> {
        vec![
            Rect::new(0.0, 0.0, 100.0, 50.0),    // 0 左上
            Rect::new(120.0, 0.0, 100.0, 50.0),  // 1 右上
            Rect::new(0.0, 70.0, 100.0, 50.0),   // 2 左下
            Rect::new(120.0, 70.0, 100.0, 50.0), // 3 右下
        ]
    }

    #[test]
    fn grid_moves_follow_geometry_not_dom_order() {
        let g = grid();
        // 右/下/左/上各一格；左上按左键无候选，焦点不动。
        assert_eq!(next_focus(g[0], &g, Direction::Right), Some(1));
        assert_eq!(next_focus(g[0], &g, Direction::Down), Some(2));
        assert_eq!(next_focus(g[0], &g, Direction::Left), None);
        assert_eq!(next_focus(g[0], &g, Direction::Up), None);
        assert_eq!(next_focus(g[3], &g, Direction::Up), Some(1));
        assert_eq!(next_focus(g[3], &g, Direction::Left), Some(2));
        assert_eq!(next_focus(g[2], &g, Direction::Right), Some(3));
        assert_eq!(next_focus(g[1], &g, Direction::Down), Some(3));
    }

    #[test]
    fn aligned_candidate_beats_closer_diagonal_one() {
        let cur = Rect::new(0.0, 0.0, 100.0, 50.0);
        // A 斜向更近（沿方向距离 200），B 同排对齐但更远（260）：
        // 得分 A=200+2.5×60=350，B=260，应选 B。
        let cands = vec![
            Rect::new(200.0, 60.0, 100.0, 50.0),
            Rect::new(260.0, 0.0, 100.0, 50.0),
        ];
        assert_eq!(next_focus(cur, &cands, Direction::Right), Some(1));
    }

    #[test]
    fn current_element_itself_and_empty_are_skipped() {
        let cur = Rect::new(0.0, 0.0, 100.0, 50.0);
        let cands = vec![
            cur,                             // 当前焦点本身在列表里
            Rect::new(120.0, 0.0, 0.0, 0.0), // 零尺寸
            Rect::new(120.0, 0.0, 100.0, 50.0),
        ];
        assert_eq!(next_focus(cur, &cands, Direction::Right), Some(2));
        assert_eq!(next_focus(cur, &[], Direction::Right), None);
        assert_eq!(
            next_focus(Rect::new(0.0, 0.0, 0.0, 0.0), &cands, Direction::Right),
            None
        );
    }

    #[test]
    fn from_key_maps_remote_keys_only() {
        assert_eq!(
            DpadKey::from_key("ArrowUp"),
            Some(DpadKey::Move(Direction::Up))
        );
        assert_eq!(
            DpadKey::from_key("ArrowDown"),
            Some(DpadKey::Move(Direction::Down))
        );
        assert_eq!(
            DpadKey::from_key("ArrowLeft"),
            Some(DpadKey::Move(Direction::Left))
        );
        assert_eq!(
            DpadKey::from_key("ArrowRight"),
            Some(DpadKey::Move(Direction::Right))
        );
        assert_eq!(DpadKey::from_key("Enter"), Some(DpadKey::Activate));
        assert_eq!(DpadKey::from_key(" "), Some(DpadKey::Activate));
        assert_eq!(DpadKey::from_key("Spacebar"), Some(DpadKey::Activate));
        assert_eq!(DpadKey::from_key("Escape"), Some(DpadKey::Back));
        assert_eq!(DpadKey::from_key("Backspace"), Some(DpadKey::Back));
        assert_eq!(DpadKey::from_key("Tab"), None);
        assert_eq!(DpadKey::from_key("a"), None);
        assert_eq!(DpadKey::from_key(""), None);
    }

    #[test]
    fn back_action_needs_more_than_one_history_entry() {
        assert_eq!(back_action(0), BackAction::GoHome);
        assert_eq!(back_action(1), BackAction::GoHome);
        assert_eq!(back_action(2), BackAction::GoBack);
        assert_eq!(back_action(9), BackAction::GoBack);
    }

    #[test]
    fn text_editable_matrix() {
        assert!(is_text_editable("TEXTAREA", "", false));
        assert!(is_text_editable("SELECT", "", false));
        assert!(is_text_editable("INPUT", "text", false));
        assert!(is_text_editable("INPUT", "", false)); // 缺省 type 即 text
        assert!(is_text_editable("INPUT", "password", false));
        assert!(!is_text_editable("INPUT", "checkbox", false));
        assert!(!is_text_editable("INPUT", "button", false));
        assert!(!is_text_editable("INPUT", "submit", false));
        assert!(!is_text_editable("BUTTON", "", false));
        assert!(!is_text_editable("A", "", false));
        assert!(!is_text_editable("DIV", "", false));
        assert!(is_text_editable("DIV", "", true)); // contenteditable
    }

    #[test]
    fn looks_like_tv_markers() {
        assert!(looks_like_tv(
            "Mozilla/5.0 (SMART-TV; Linux; Tizen 6.0) AppleWebKit/537.36 TV Safari/537.36"
        ));
        assert!(looks_like_tv("Mozilla/5.0 (Linux; Android 11; Android TV)"));
        assert!(looks_like_tv(
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/131.0 Safari/537.36 CrKey/1.0"
        ));
        assert!(!looks_like_tv(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0 Safari/537.36"
        ));
        assert!(!looks_like_tv(
            "Mozilla/5.0 (iPhone; CPU iPhone OS 18_1 like Mac OS X) AppleWebKit/605.1.15 Mobile/15E148"
        ));
        assert!(!looks_like_tv(""));
    }
}
