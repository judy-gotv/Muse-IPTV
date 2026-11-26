//! 设计令牌 + 基础组件（开发文档 §11：Apple HIG 级别还原的 Rust 组件系统）。
//!
//! - 令牌值逐字来自 §11.3 / §11.4；CSS 由 [`design_tokens_css`] 输出，
//!   服务端以 `/design.css` 下发（编译期 `include_str!`，运行时不读磁盘）。
//! - 组件清单（§11.5）全部真实渲染：Sidebar、TabBar、Toolbar、List /
//!   InsetGroupedList、Row、Button、SegmentedControl、Toggle、TextField /
//!   SearchField、Sheet、Alert、Badge / StatusDot、ProgressRing、EmptyState。
//! - SF 字体 / SF Symbols 不打包（§11.2 授权红线）：字体用系统字体栈，
//!   图标用文字符号（`NavItem.icon` 为单个文字符号，如 `⌂`），不冒充系统符号。
// 作者: Passwd Word

use leptos::prelude::*;

// ---------------------------------------------------------------------------
// §11.3 设计令牌（Rust 常量，与 design.css 同源，改动须两边同步）
// ---------------------------------------------------------------------------

/// 浅色语义色。
pub const COLOR_BG_LIGHT: &str = "#f5f5f7";
pub const COLOR_SURFACE_LIGHT: &str = "#ffffff";
pub const COLOR_LABEL_LIGHT: &str = "#1d1d1f";
pub const COLOR_ACCENT_LIGHT: &str = "#007aff";
pub const COLOR_DESTRUCTIVE: &str = "#ff3b30";
pub const COLOR_SUCCESS: &str = "#34c759";
pub const COLOR_WARNING: &str = "#ff9f0a";
/// 深色语义色。
pub const COLOR_BG_DARK: &str = "#000000";
pub const COLOR_SURFACE_DARK: &str = "#1c1c1e";
pub const COLOR_SURFACE_ELEVATED_DARK: &str = "#2c2c2e";
pub const COLOR_LABEL_DARK: &str = "#ffffff";
pub const COLOR_ACCENT_DARK: &str = "#0a84ff";

/// 字体栈（§11.2：Apple 设备用系统字体，其余 Inter / Noto 回退，不打包 SF）。
pub const FONT_UI: &str =
    "-apple-system, BlinkMacSystemFont, \"Inter\", \"Noto Sans SC\", \"Noto Sans TC\", sans-serif";

/// 字号（rem，SF 比例 §11.3）。
pub const TEXT_LARGE_TITLE_REM: f32 = 2.125;
pub const TEXT_TITLE1_REM: f32 = 1.75;
pub const TEXT_TITLE2_REM: f32 = 1.375;
pub const TEXT_HEADLINE_REM: f32 = 1.0625;
pub const TEXT_BODY_REM: f32 = 1.0625;
pub const TEXT_FOOTNOTE_REM: f32 = 0.8125;
pub const TEXT_CAPTION_REM: f32 = 0.75;

/// 间距（8pt 栅格 §11.3，单位 px）。
pub const SPACE: [u32; 7] = [4, 8, 12, 16, 20, 24, 32];
/// 圆角（§11.3）。
pub const RADIUS_S_PX: u32 = 8;
pub const RADIUS_M_PX: u32 = 12;
pub const RADIUS_L_PX: u32 = 18;
/// 动效（§11.3）。
pub const EASE_STANDARD: &str = "cubic-bezier(.25,.1,.25,1)";
pub const DUR_FAST_MS: u32 = 150;
pub const DUR_BASE_MS: u32 = 250;
pub const DUR_SLOW_MS: u32 = 350;

/// §11.7：最小触控目标（pt）。
pub const MIN_TOUCH_TARGET_PT: u32 = 44;

/// 完整设计系统 CSS（§11.3 + §11.4）。
pub fn design_tokens_css() -> &'static str {
    include_str!("design.css")
}

// ---------------------------------------------------------------------------
// 共享类型
// ---------------------------------------------------------------------------

/// 导航项（Sidebar / TabBar 共用）。`icon` 为文字符号，不使用 SF Symbols（§11.2）。
#[derive(Debug, Clone, PartialEq)]
pub struct NavItem {
    pub label: String,
    pub href: String,
    pub icon: String,
    pub active: bool,
}

impl NavItem {
    pub fn new(
        label: impl Into<String>,
        href: impl Into<String>,
        icon: impl Into<String>,
        active: bool,
    ) -> Self {
        Self {
            label: label.into(),
            href: href.into(),
            icon: icon.into(),
            active,
        }
    }
}

/// Button 变体（§11.5：Filled / Tinted / Plain / Destructive）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ButtonVariant {
    #[default]
    Filled,
    Tinted,
    Plain,
    Destructive,
}

impl ButtonVariant {
    pub fn css_class(self) -> &'static str {
        match self {
            ButtonVariant::Filled => "btn btn-filled",
            ButtonVariant::Tinted => "btn btn-tinted",
            ButtonVariant::Plain => "btn btn-plain",
            ButtonVariant::Destructive => "btn btn-destructive",
        }
    }
}

/// Badge 语气。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BadgeTone {
    #[default]
    Accent,
    Success,
    Warning,
    Destructive,
    Muted,
}

impl BadgeTone {
    pub fn css_class(self) -> &'static str {
        match self {
            BadgeTone::Accent => "badge badge-accent",
            BadgeTone::Success => "badge badge-success",
            BadgeTone::Warning => "badge badge-warning",
            BadgeTone::Destructive => "badge badge-destructive",
            BadgeTone::Muted => "badge badge-muted",
        }
    }
}

/// 状态点状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StatusKind {
    Online,
    Offline,
    Warning,
    #[default]
    Unknown,
}

impl StatusKind {
    pub fn css_class(self) -> &'static str {
        match self {
            StatusKind::Online => "status-dot online",
            StatusKind::Offline => "status-dot offline",
            StatusKind::Warning => "status-dot warning",
            StatusKind::Unknown => "status-dot",
        }
    }
}

/// Alert 语气。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AlertTone {
    #[default]
    Info,
    Warning,
    Destructive,
}

// ---------------------------------------------------------------------------
// §11.5 基础组件
// ---------------------------------------------------------------------------

/// Sidebar / NavigationSplitView：后台主导航，可折叠（§11.5）。
#[component]
pub fn Sidebar(
    #[prop(into)] title: String,
    items: Vec<NavItem>,
    #[prop(default = false)] collapsed: bool,
    #[prop(optional)] on_toggle: Option<Callback<()>>,
) -> impl IntoView {
    let cls = if collapsed {
        "sidebar collapsed"
    } else {
        "sidebar"
    };
    view! {
        <nav class=cls aria-label="sidebar">
            <div class="sidebar-title">{title}</div>
            {items.into_iter().map(|item| {
                let cls = if item.active { "nav-link active" } else { "nav-link" };
                view! {
                    <a class=cls href=item.href aria-current=if item.active { Some("page") } else { None }>
                        <span class="nav-icon" aria-hidden="true">{item.icon}</span>
                        <span class="nav-label">{item.label}</span>
                    </a>
                }
            }).collect_view()}
            {on_toggle.map(|cb| view! {
                <button class="btn btn-plain" type="button" on:click=move |_| cb.run(()) aria-label="toggle sidebar">
                    {if collapsed { "»" } else { "«" }}
                </button>
            })}
        </nav>
    }
}

/// TabBar：门户移动端底部导航（§11.5）。
#[component]
pub fn TabBar(items: Vec<NavItem>) -> impl IntoView {
    view! {
        <>
            <div class="tabbar-spacer" aria-hidden="true"></div>
            <nav class="tabbar glass" aria-label="tab bar">
                {items.into_iter().map(|item| {
                    let cls = if item.active { "nav-link active" } else { "nav-link" };
                    view! {
                        <a class=cls href=item.href aria-current=if item.active { Some("page") } else { None }>
                            <span class="nav-icon" aria-hidden="true">{item.icon}</span>
                            <span class="nav-label">{item.label}</span>
                        </a>
                    }
                }).collect_view()}
            </nav>
        </>
    }
}

/// Toolbar / Navigation Bar：标题 + 右侧操作 + 返回（§11.5）。
#[component]
pub fn Toolbar(
    #[prop(into)] title: String,
    #[prop(optional, into)] back_href: String,
    children: Children,
) -> impl IntoView {
    view! {
        <header class="toolbar glass">
            {(!back_href.is_empty()).then(|| view! {
                <a class="toolbar-back" href=back_href aria-label="back">"‹"</a>
            })}
            <div class="toolbar-title">{title}</div>
            <div class="toolbar-actions">{children()}</div>
        </header>
    }
}

/// List：列表容器（§11.5）。
#[component]
pub fn List(#[prop(optional, into)] title: Option<String>, children: Children) -> impl IntoView {
    view! {
        <section>
            {title.map(|t| view! { <div class="list-title">{t}</div> })}
            <ul class="list">{children()}</ul>
        </section>
    }
}

/// InsetGroupedList：内嵌分组列表（§11.5）。
#[component]
pub fn InsetGroupedList(
    #[prop(optional, into)] title: Option<String>,
    children: Children,
) -> impl IntoView {
    view! {
        <section>
            {title.map(|t| view! { <div class="list-title">{t}</div> })}
            <div class="inset-list"><ul class="list">{children()}</ul></div>
        </section>
    }
}

/// Row：双行文本 + 尾部值 + 箭头（§11.5）。
/// 注意（Leptos 0.8）：`#[prop(optional)]` 的 `String` prop 调用时直接传
/// 值、省略即默认空串；不要包 `Some(...)`（那是旧版写法）。
#[component]
pub fn Row(
    #[prop(into)] title: String,
    #[prop(optional, into)] subtitle: String,
    #[prop(optional, into)] value: String,
    #[prop(optional, into)] href: String,
    #[prop(default = false)] disclosure: bool,
) -> impl IntoView {
    let inner = view! {
        <div class="row-main">
            <div class="row-title">{title}</div>
            {(!subtitle.is_empty()).then(|| view! { <div class="row-subtitle">{subtitle}</div> })}
        </div>
        {(!value.is_empty()).then(|| view! { <div class="row-value">{value}</div> })}
        {disclosure.then(|| view! { <span class="row-chevron" aria-hidden="true">"›"</span> })}
    };
    if href.is_empty() {
        view! { <li class="row">{inner}</li> }.into_any()
    } else {
        view! {
            <li><a class="row" href=href style="text-decoration:none;color:inherit;display:flex">{inner}</a></li>
        }
        .into_any()
    }
}

/// Button：Filled / Tinted / Plain / Destructive（§11.5）。
#[component]
pub fn Button(
    #[prop(default = ButtonVariant::Filled)] variant: ButtonVariant,
    #[prop(default = "button")] button_type: &'static str,
    #[prop(default = false)] disabled: bool,
    #[prop(optional)] on_click: Option<Callback<()>>,
    children: Children,
) -> impl IntoView {
    view! {
        <button
            class=variant.css_class()
            type=button_type
            disabled=disabled
            on:click=move |_| { if let Some(cb) = on_click { cb.run(()); } }
        >
            {children()}
        </button>
    }
}

/// SegmentedControl：频道分类切换（§11.5）。
/// `link_base` 给出时渲染为链接（`{link_base}{value}`），否则渲染为按钮 + 回调。
#[component]
pub fn SegmentedControl(
    options: Vec<(String, String)>,
    #[prop(into)] selected: String,
    #[prop(optional)] on_select: Option<Callback<String>>,
    #[prop(optional, into)] link_base: Option<String>,
) -> impl IntoView {
    view! {
        <div class="segmented" role="tablist">
            {options.into_iter().map(|(value, label)| {
                let active = value == selected;
                let cls = if active { "active" } else { "" };
                match (&link_base, &on_select) {
                    (Some(base), _) => {
                        let href = format!("{base}{value}");
                        view! { <a class=cls href=href role="tab" aria-selected=active>{label}</a> }.into_any()
                    }
                    (None, Some(cb)) => {
                        let cb = *cb;
                        let v = value.clone();
                        view! {
                            <button type="button" class=cls role="tab" aria-selected=active
                                on:click=move |_| cb.run(v.clone())>{label}</button>
                        }.into_any()
                    }
                    (None, None) => view! { <button type="button" class=cls role="tab" aria-selected=active>{label}</button> }.into_any(),
                }
            }).collect_view()}
        </div>
    }
}

/// Toggle / Switch：设置项（§11.5）。原生 checkbox + CSS，表单提交即生效。
#[component]
pub fn Toggle(
    #[prop(into)] label: String,
    #[prop(into)] name: String,
    #[prop(default = false)] checked: bool,
) -> impl IntoView {
    view! {
        <label class="toggle">
            <input type="checkbox" name=name checked=checked value="true"/>
            <span class="toggle-track" aria-hidden="true"></span>
            <span>{label}</span>
        </label>
    }
}

/// TextField：登录、表单输入（§11.5）。
#[component]
pub fn TextField(
    #[prop(into)] label: String,
    #[prop(into)] name: String,
    #[prop(default = String::new(), into)] value: String,
    #[prop(optional, into)] placeholder: Option<String>,
    #[prop(default = "text")] input_type: &'static str,
    #[prop(default = false)] required: bool,
    #[prop(optional, into)] autocomplete: Option<String>,
) -> impl IntoView {
    view! {
        <div class="field">
            <label for=name.clone()>{label}</label>
            <input
                class="textfield"
                id=name.clone()
                name=name
                type=input_type
                value=value
                placeholder=placeholder.unwrap_or_default()
                required=required
                autocomplete=autocomplete.unwrap_or_default()
            />
        </div>
    }
}

/// SearchField：频道搜索（§11.5）。GET 表单，提交即筛选。
#[component]
pub fn SearchField(
    #[prop(default = String::new())] value: String,
    #[prop(optional, into)] placeholder: Option<String>,
    #[prop(default = "q")] name: &'static str,
) -> impl IntoView {
    view! {
        <div class="searchfield" role="search">
            <input
                type="search"
                name=name
                value=value
                placeholder=placeholder.unwrap_or_else(|| "Search".to_string())
                aria-label="search"
            />
        </div>
    }
}

/// Sheet / Popover：编辑 Token、确认弹窗（§11.5）。
#[component]
pub fn Sheet(
    #[prop(into)] title: String,
    open: bool,
    #[prop(optional)] on_close: Option<Callback<()>>,
    children: Children,
) -> impl IntoView {
    // Show 的 children 闭包必须是 Fn + Sync，而 children() 只能调用一次，
    // 故用组件顶层的 Rust if 分支：关闭时渲染空片段。
    if open {
        let label = title.clone();
        view! {
            <div class="sheet-overlay" role="dialog" aria-modal="true" aria-label=label>
                <div class="sheet">
                    <div style="display:flex;justify-content:space-between;align-items:center">
                        <h2 class="sheet-title">{title}</h2>
                        {on_close.map(|cb| view! {
                            <button class="btn btn-plain" type="button" aria-label="close"
                                on:click=move |_| cb.run(())>"✕"</button>
                        })}
                    </div>
                    {children()}
                </div>
            </div>
        }
        .into_any()
    } else {
        view! { "" }.into_any()
    }
}

/// Alert：并发超限、危险操作（§11.5）。
#[component]
pub fn Alert(
    #[prop(into)] title: String,
    #[prop(into)] message: String,
    #[prop(default = AlertTone::Info)] tone: AlertTone,
    children: Children,
) -> impl IntoView {
    let cls = match tone {
        AlertTone::Info => "alert",
        AlertTone::Warning => "alert",
        AlertTone::Destructive => "alert",
    };
    let style = match tone {
        AlertTone::Destructive => "border:2px solid var(--c-destructive)",
        AlertTone::Warning => "border:2px solid var(--c-warning)",
        AlertTone::Info => "",
    };
    view! {
        <div class="alert-overlay">
            <div class=cls role="alert" style=style>
                <h2 class="alert-title">{title}</h2>
                <p class="alert-message">{message}</p>
                <div class="alert-actions">{children()}</div>
            </div>
        </div>
    }
}

/// Badge：在线、失效、同步状态（§11.5）。
#[component]
pub fn Badge(
    #[prop(into)] text: String,
    #[prop(default = BadgeTone::Accent)] tone: BadgeTone,
) -> impl IntoView {
    view! { <span class=tone.css_class()>{text}</span> }
}

/// StatusDot：状态圆点 + 文字（§11.5）。
#[component]
pub fn StatusDot(
    #[prop(into)] label: String,
    #[prop(default = StatusKind::Unknown)] kind: StatusKind,
) -> impl IntoView {
    view! { <span class=kind.css_class()>{label}</span> }
}

/// ProgressRing：额度占用环（§11.5）。SVG 圆环，`fraction` 0.0–1.0。
#[component]
pub fn ProgressRing(
    fraction: f64,
    #[prop(into)] label: String,
    #[prop(default = 120)] size: u32,
) -> impl IntoView {
    let frac = fraction.clamp(0.0, 1.0);
    let r = 54.0f64;
    let c = 2.0 * std::f64::consts::PI * r;
    let offset = c * (1.0 - frac);
    let pct = (frac * 100.0).round() as i64;
    let size_str = size.to_string();
    let center = (size / 2).to_string();
    let dash = format!("{c:.1}");
    let off = format!("{offset:.1}");
    view! {
        <div class="progress-ring">
            <svg width=size_str.clone() height=size_str aria-hidden="true" viewBox="0 0 120 120">
                <circle cx=center.clone() cy=center.clone() r="54" fill="none" stroke="var(--c-separator)" stroke-width="10"/>
                <circle cx=center.clone() cy=center r="54" fill="none" stroke="var(--c-accent)" stroke-width="10"
                    stroke-linecap="round" stroke-dasharray=dash stroke-dashoffset=off/>
                <text x="60" y="66" text-anchor="middle" font-size="20" font-weight="700" fill="var(--c-label)">
                    {format!("{pct}%")}
                </text>
            </svg>
            <div class="progress-ring-label">{label}</div>
        </div>
    }
}

/// EmptyState：空列表、无权限（§11.5）。
#[component]
pub fn EmptyState(
    #[prop(into)] title: String,
    #[prop(optional, into)] message: Option<String>,
    #[prop(default = "○")] icon: &'static str,
    children: Children,
) -> impl IntoView {
    view! {
        <div class="empty-state">
            <div class="empty-state-icon" aria-hidden="true">{icon}</div>
            <h3 class="empty-state-title">{title}</h3>
            {message.map(|m| view! { <p class="empty-state-message">{m}</p> })}
            {children()}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_match_doc_section_11_3() {
        assert_eq!(COLOR_ACCENT_LIGHT, "#007aff");
        assert_eq!(COLOR_ACCENT_DARK, "#0a84ff");
        assert_eq!(COLOR_BG_LIGHT, "#f5f5f7");
        assert_eq!(COLOR_BG_DARK, "#000000");
        assert_eq!(COLOR_DESTRUCTIVE, "#ff3b30");
        assert_eq!(COLOR_SUCCESS, "#34c759");
        assert_eq!(COLOR_WARNING, "#ff9f0a");
        assert_eq!(SPACE, [4, 8, 12, 16, 20, 24, 32]);
        assert_eq!((RADIUS_S_PX, RADIUS_M_PX, RADIUS_L_PX), (8, 12, 18));
        assert_eq!((DUR_FAST_MS, DUR_BASE_MS, DUR_SLOW_MS), (150, 250, 350));
        assert_eq!(MIN_TOUCH_TARGET_PT, 44);
    }

    #[test]
    fn css_contains_all_tokens_and_fallbacks() {
        let css = design_tokens_css();
        for token in [
            "--c-bg: #f5f5f7",
            "--c-surface: #ffffff",
            "--c-label: #1d1d1f",
            "--c-accent: #007aff",
            "--c-accent: #0a84ff",
            "--c-bg: #000000",
            ".glass",
            "backdrop-filter: blur(24px) saturate(180%)",
            "@supports not (backdrop-filter: blur(1px))",
            "prefers-reduced-transparency",
            "prefers-contrast: more",
            "prefers-reduced-motion",
            "[data-text-size=\"xxxl\"]",
            "--radius-l: 18px",
        ] {
            assert!(css.contains(token), "missing css token: {token}");
        }
    }
}
