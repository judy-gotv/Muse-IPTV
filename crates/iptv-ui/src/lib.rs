//! 共享业务组件（开发文档 §1.1 / §4.6 / §11.5）。
//!
//! - 全部文案走 [`iptv_i18n::I18n`] 的 key；语言包里没有的 key 用
//!   [`t_or`] 回退到英文原文字面量（语言包目前定义了 100 余个 key，
//!   见 `locales/*/common.ftl|portal.ftl|admin.ftl|errors.ftl`；回退不是硬编码
//!   第三语言文案，而是缺 key 时的英文兜底，见 README 的 i18n 说明；
//!   缺 key 时 `t_or` / `t_args_or` 打警告，`cargo xtask i18n-check`
//!   扫描代码引用，缺 key 即失败）。
//! - `LocaleCtx`：每请求由门户/后台的 `App` 组件从请求头协商后注入。
// 作者: Passwd Word

use chrono::{DateTime, Utc};
use iptv_design::{Badge, BadgeTone, EmptyState, ProgressRing, Row, StatusDot, StatusKind};
use iptv_dto::{Channel, DeviceView, EpgProgram, MeQuota, PlaybackStartResponse, TokenFeatures};
use iptv_i18n::I18n;
use leptos::prelude::*;
use std::time::Duration;

pub mod dpad;

/// 每请求 locale（由 App 从 Cookie / Accept-Language 协商后注入，§12.2）。
#[derive(Debug, Clone)]
pub struct LocaleCtx(pub String);

/// 当前请求的 locale；无上下文时回退默认语言。
pub fn locale() -> String {
    use_context::<LocaleCtx>()
        .map(|c| c.0)
        .unwrap_or_else(|| iptv_i18n::DEFAULT_LOCALE.to_string())
}

/// 查文案；key 缺失（`I18n::t` 原样返回 key）时用英文兜底，并打警告
/// （M11：此前静默回退，缺 key 无任何信号）。
/// 注意：`log` crate 不在 workspace 依赖里、且本 crate 是同构 Leptos
/// 组件（浏览器端无 `log` 接收器），故用 `leptos::logging::warn!`——浏览器走
/// `console.warn`，服务端走 `eprintln!`，无需新增依赖。
pub fn t_or(key: &str, fallback: &str) -> String {
    let i18n = I18n::new();
    let loc = locale();
    let v = i18n.t_simple(&loc, key);
    if v == key {
        leptos::logging::warn!("i18n 缺 key：`{key}`（locale `{loc}`），回退英文兜底");
        fallback.to_string()
    } else {
        v
    }
}

/// 带参数的文案；缺 key 时用 `format!` 的英文兜底，并打警告（同 `t_or`）。
pub fn t_args_or(key: &str, args: &[(&str, String)], fallback: String) -> String {
    let i18n = I18n::new();
    let loc = locale();
    let mut fa = fluent_bundle::FluentArgs::new();
    for (k, v) in args {
        fa.set(*k, v.clone());
    }
    let v = i18n.t(&loc, key, Some(&fa));
    if v == key {
        leptos::logging::warn!("i18n 缺 key：`{key}`（locale `{loc}`），回退英文兜底");
        fallback
    } else {
        v
    }
}

/// 状态列本地化：入参是 Rust Debug 文本（如 `Suspended`）或小写状态串
/// （如会话的 `active`）；未知值原样返回，不吞信息。
pub fn status_label(debug: &str) -> String {
    match debug {
        "Active" | "active" => t_or("common.status-active", "Active"),
        "Disabled" | "disabled" => t_or("common.status-disabled", "Disabled"),
        "Suspended" | "suspended" => t_or("common.status-suspended", "Suspended"),
        "Expired" | "expired" => t_or("common.status-expired", "Expired"),
        other => other.to_string(),
    }
}

/// 角色列本地化：入参是 Rust Debug 文本（如 `SuperAdmin`）。
pub fn role_label(debug: &str) -> String {
    match debug {
        "SuperAdmin" => t_or("common.role-super-admin", "Super admin"),
        "Operator" => t_or("common.role-operator", "Operator"),
        "Support" => t_or("common.role-support", "Support"),
        "Readonly" => t_or("common.role-readonly", "Readonly"),
        other => other.to_string(),
    }
}

/// 日期按 locale 格式化（§12.6，m15：时区默认 `Asia/Taipei`；调用方有
/// 用户时区时改用 `fmt_dt_in_tz`）。
pub fn fmt_dt(dt: DateTime<Utc>) -> String {
    I18n::new().format_datetime_in_tz(&locale(), dt, iptv_i18n::DEFAULT_TIMEZONE)
}

/// 日期按 locale 与指定时区格式化（`tz_name` 为 IANA 名，如
/// `America/New_York`；非法时回退默认时区）。
pub fn fmt_dt_in_tz(dt: DateTime<Utc>, tz_name: &str) -> String {
    I18n::new().format_datetime_in_tz(&locale(), dt, tz_name)
}

// ---------------------------------------------------------------------------
// ChannelCard：Logo / 分类 / 当前节目 / 播放按钮（§4.1 / §4.4）
// ---------------------------------------------------------------------------

/// 频道卡片。播放按钮为真实链接 `/watch/{id}`（进入播放页后调播放接口）。
#[component]
pub fn ChannelCard(
    channel: Channel,
    #[prop(optional)] current_program: Option<EpgProgram>,
) -> impl IntoView {
    let watch_href = format!("/watch/{}", channel.id);
    let name = channel
        .display_name
        .clone()
        .unwrap_or_else(|| channel.name.clone());
    let group = channel.group_title.clone().unwrap_or_default();
    let now_title = current_program.map(|p| p.title);
    let logo = channel.logo_url.clone();
    let initial = name
        .chars()
        .next()
        .map(|c| c.to_string())
        .unwrap_or_else(|| "?".to_string());
    let watch_label = t_or("portal.watch-now", "Watch now");
    view! {
        <article class="channel-card">
            {match logo {
                Some(url) => view! { <img class="channel-card-logo" src=url alt=name.clone() loading="lazy"/> }.into_any(),
                None => view! { <div class="channel-card-logo-fallback" aria-hidden="true">{initial}</div> }.into_any(),
            }}
            <div class="channel-card-body">
                <div class="channel-card-name">{name}</div>
                <div class="channel-card-meta">
                    {group}
                    {channel.language.clone().map(|l| format!(" · {l}")).unwrap_or_default()}
                </div>
                {now_title.map(|t| view! { <div class="channel-card-now">{t}</div> })}
                <a class="btn btn-filled" href=watch_href>{watch_label}</a>
            </div>
        </article>
    }
}

// ---------------------------------------------------------------------------
// QuotaRing：并发额度环（§4.1 / §11.5 ProgressRing）
// ---------------------------------------------------------------------------

/// 并发额度环：`active / max`，并显示剩余路数（§4.5 `quota-used` / `quota-remaining`）。
#[component]
pub fn QuotaRing(quota: MeQuota) -> impl IntoView {
    let max = quota.max_concurrent_sessions.max(1);
    let used = quota.active_sessions.clamp(0, max);
    let frac = used as f64 / max as f64;
    let remaining = (max - used).max(0);
    let used_text = t_args_or(
        "portal.quota-used",
        &[("used", used.to_string()), ("max", max.to_string())],
        format!("{used} of {max} streams in use"),
    );
    let remaining_text = t_args_or(
        "portal.quota-remaining",
        &[("count", remaining.to_string())],
        format!("{remaining} streams remaining"),
    );
    let exhausted = used >= max;
    view! {
        <div style="display:flex;gap:16px;align-items:center;flex-wrap:wrap">
            <ProgressRing fraction=frac label=used_text.clone()/>
            <div>
                <div>{used_text}</div>
                <div style="color:var(--c-label-secondary)">{remaining_text}</div>
                <div style="margin-top:8px">
                    {format!("{} / {}", quota.bound_devices, quota.max_bound_devices)}
                    <span style="color:var(--c-label-secondary)">" bound devices"</span>
                </div>
                {exhausted.then(|| view! {
                    <div style="margin-top:8px">
                        <Badge text=t_or("portal.quota-exhausted-title", "Stream limit reached") tone=BadgeTone::Warning/>
                    </div>
                })}
            </div>
        </div>
    }
}

// ---------------------------------------------------------------------------
// DeviceRow：在线设备行（§4.1：IP 已脱敏展示 + 下线按钮）
// ---------------------------------------------------------------------------

/// 设备行。下线走 POST 表单 `/me/devices/{id}/kick`（门户 Axum 处理器调
/// 后端 `DELETE /api/v1/me/devices/{id}`，服务端只下线本 token 的会话）。
#[component]
pub fn DeviceRow(device: DeviceView) -> impl IntoView {
    let kick_action = format!("/me/devices/{}/kick", device.session_id);
    let since = fmt_dt(device.last_heartbeat_at);
    let online_since = t_args_or(
        "portal.device-online-since",
        &[("since", since.clone())],
        format!("Online since {since}"),
    );
    let kick_label = t_or("common.kick", "Kick");
    view! {
        <li>
            <div class="row device-row">
                <div class="row-main">
                    <div class="row-title">{device.device_id}</div>
                    <div class="row-subtitle">{format!("{} · {}", device.ip_masked, online_since)}</div>
                    {device.user_agent.clone().map(|ua| view! { <div class="row-subtitle">{ua}</div> })}
                </div>
                {if device.is_current {
                    view! { <StatusDot label=t_or("common.current-device", "This device") kind=StatusKind::Online/> }.into_any()
                } else {
                    view! {
                        <form method="post" action=kick_action>
                            <button class="btn btn-destructive" type="submit">{kick_label}</button>
                        </form>
                    }.into_any()
                }}
            </div>
        </li>
    }
}

// ---------------------------------------------------------------------------
// EpgSidebar：当前 / 下一节目（§14.1）
// ---------------------------------------------------------------------------

/// EPG 侧栏：第一条为当前节目，其余按开始时间列出。
#[component]
pub fn EpgSidebar(
    programs: Vec<EpgProgram>,
    #[prop(optional)] now: Option<DateTime<Utc>>,
) -> impl IntoView {
    let now = now.unwrap_or_else(Utc::now);
    let (current, upcoming): (Vec<_>, Vec<_>) = programs
        .iter()
        .partition(|p| p.start_at <= now && now < p.stop_at);
    let current = current.into_iter().next();
    let upcoming: Vec<_> = upcoming.into_iter().take(6).collect();
    view! {
        <aside class="epg-sidebar" aria-label="epg">
            <h3 style="margin-top:0">{t_or("admin.epg", "Program guide")}</h3>
            {match current {
                Some(p) => view! {
                    <div>
                        <div class="epg-now">{p.title.clone()}</div>
                        <div class="epg-next">{format!("{} – {}", fmt_dt(p.start_at), fmt_dt(p.stop_at))}</div>
                        {p.description.clone().map(|d| view! { <p style="font-size:var(--text-footnote)">{d}</p> })}
                    </div>
                }.into_any(),
                None => view! { <EmptyState title=t_or("portal.no-epg", "No program now") icon="◷">{view! { <></> }}</EmptyState> }.into_any(),
            }}
            {if upcoming.is_empty() {
                view! { "" }.into_any()
            } else {
                view! {
                    <ul class="list" style="margin-top:12px">
                        {upcoming.into_iter().map(|p| view! {
                            <Row title=p.title.clone() subtitle=format!("{} – {}", fmt_dt(p.start_at), fmt_dt(p.stop_at)) value=p.category.clone().unwrap_or_default()/>
                        }).collect_view()}
                    </ul>
                }.into_any()
            }}
        </aside>
    }
}

// ---------------------------------------------------------------------------
// LanguageSwitcher：语言切换（§12.4：写 Cookie + PATCH /api/v1/me/settings）
// ---------------------------------------------------------------------------

/// 语言切换。纯 HTML 表单 POST 到门户 `/locale`：服务端写
/// `iptv_locale` Cookie（持久化）并调后端 `PATCH /api/v1/me/settings`
/// 同步用户语言设置（§12.4），随后 302 回原页。
#[component]
pub fn LanguageSwitcher(
    #[prop(optional, into)] action: Option<String>,
    #[prop(optional, into)] redirect_to: Option<String>,
) -> impl IntoView {
    let loc = locale();
    let action = action.unwrap_or_else(|| "/locale".to_string());
    let label = t_or("common.language", "Language");
    let aria = label.clone();
    let locales = I18n::supported_locales();
    view! {
        <form method="post" action=action class="inline-form" aria-label=aria>
            <label for="locale-select" style="font-size:var(--text-footnote);color:var(--c-label-secondary)">{label}</label>
            <select id="locale-select" name="locale" style="min-height:44px;border-radius:var(--radius-m);border:1px solid var(--c-separator);background:var(--c-surface)">
                {locales.into_iter().map(|l| {
                    let selected = l == loc;
                    view! { <option value=l selected=selected>{iptv_i18n::locale_display_name(l)}</option> }
                }).collect_view()}
            </select>
            {redirect_to.map(|r| view! { <input type="hidden" name="redirect_to" value=r/> })}
            <button class="btn btn-tinted" type="submit">{t_or("common.save", "Save")}</button>
        </form>
    }
}

// ---------------------------------------------------------------------------
// Player：HLS 播放器（§4.6 / §8）
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// hls.js 接线（m16，§8：非 Safari 走 MSE）
//
// 浏览器端（wasm32）才执行：`window.Hls` 是门户 `GET /vendor/hls.min.js`
// 提供的 UMD 全局（hls.js 1.5.13，见 `crates/iptv-portal/vendor/README.md`）。
// 全部经 `js-sys::Reflect` 调方法，不手写 JS 字符串拼接；服务端渲染时
// 这些函数直接返回默认值——非 wasm 目标上调 `web-sys` 导入函数会 panic，
// 故每个入口先过 `cfg!(target_arch = "wasm32")`。
// ---------------------------------------------------------------------------

use std::cell::RefCell;

// 当前播放会话的 hls.js 实例（`new Hls()` 的返回值）；只在浏览器端有值。
// 挂 thread_local 而不是信号里：`JsValue` 不是 `Send`，进不了 Leptos 信号。
thread_local! {
    static HLS_INSTANCE: RefCell<Option<wasm_bindgen::JsValue>> = const { RefCell::new(None) };
}

/// `window.Hls` 构造函数；脚本未加载（或加载失败）时 `None`。
fn hls_constructor() -> Option<js_sys::Function> {
    if !cfg!(target_arch = "wasm32") {
        return None;
    }
    use wasm_bindgen::JsCast;
    let window = web_sys::window()?;
    let ctor = js_sys::Reflect::get(&window, &wasm_bindgen::JsValue::from_str("Hls")).ok()?;
    if ctor.is_undefined() || ctor.is_null() {
        return None;
    }
    ctor.dyn_into::<js_sys::Function>().ok()
}

/// `Hls.isSupported()`（MSE 与关键 API 是否可用）。
fn hls_is_supported(ctor: &js_sys::Function) -> bool {
    use wasm_bindgen::JsCast;
    let func = js_sys::Reflect::get(ctor, &wasm_bindgen::JsValue::from_str("isSupported"))
        .ok()
        .and_then(|v| v.dyn_into::<js_sys::Function>().ok());
    func.and_then(|f| f.call0(&wasm_bindgen::JsValue::NULL).ok())
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// 在 hls.js 实例上调一个方法（`loadSource` / `attachMedia` / `startLoad` /
/// `destroy` / `on` 等），忽略返回值；实例不存在或方法缺失时静默 no-op。
fn hls_call(method: &str, args: &[wasm_bindgen::JsValue]) {
    if !cfg!(target_arch = "wasm32") {
        return;
    }
    HLS_INSTANCE.with(|cell| {
        let borrowed = cell.borrow();
        let Some(instance) = borrowed.as_ref() else {
            return;
        };
        let Ok(func) = js_sys::Reflect::get(instance, &wasm_bindgen::JsValue::from_str(method))
        else {
            return;
        };
        use wasm_bindgen::JsCast;
        let Ok(func) = func.dyn_into::<js_sys::Function>() else {
            return;
        };
        let argv = js_sys::Array::new();
        for arg in args {
            argv.push(arg);
        }
        let _ = func.apply(instance, &argv);
    });
}

/// 在实例上挂一个 hls.js 事件监听（`hls.on(event, cb)`）。
/// 回调签名与 hls.js 一致：`(event, data)`。
/// `Closure` 随播放器生命周期 `forget`——组件卸载时 `hls_destroy()` 会让
/// hls.js 摘掉全部监听，泄漏止于一次播放页挂载的两个小闭包。
fn hls_on<F>(event: &str, callback: F)
where
    F: FnMut(wasm_bindgen::JsValue, wasm_bindgen::JsValue) + 'static,
{
    if !cfg!(target_arch = "wasm32") {
        return;
    }
    let closure = wasm_bindgen::closure::Closure::wrap(
        Box::new(callback) as Box<dyn FnMut(wasm_bindgen::JsValue, wasm_bindgen::JsValue)>
    );
    hls_call(
        "on",
        &[
            wasm_bindgen::JsValue::from_str(event),
            closure.as_js_value().clone(),
        ],
    );
    closure.forget();
}

/// 读 `hls.levels`：每档 `(height, bitrate)`；未就绪时空 vec。
fn hls_levels() -> Vec<(u32, u64)> {
    if !cfg!(target_arch = "wasm32") {
        return Vec::new();
    }
    let mut out = Vec::new();
    HLS_INSTANCE.with(|cell| {
        let borrowed = cell.borrow();
        let Some(instance) = borrowed.as_ref() else {
            return;
        };
        let Ok(v) = js_sys::Reflect::get(instance, &wasm_bindgen::JsValue::from_str("levels"))
        else {
            return;
        };
        if !js_sys::Array::is_array(&v) {
            return;
        }
        let arr = js_sys::Array::from(&v);
        for i in 0..arr.length() {
            let item = arr.get(i);
            let height = js_sys::Reflect::get(&item, &wasm_bindgen::JsValue::from_str("height"))
                .ok()
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0) as u32;
            let bitrate = js_sys::Reflect::get(&item, &wasm_bindgen::JsValue::from_str("bitrate"))
                .ok()
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0) as u64;
            if height > 0 {
                out.push((height, bitrate));
            }
        }
    });
    out
}

/// 选档：`level = -1` 为自动（ABR），否则为 `hls.levels` 的下标。
fn hls_set_level(level: i32) {
    if !cfg!(target_arch = "wasm32") {
        return;
    }
    HLS_INSTANCE.with(|cell| {
        if let Some(instance) = cell.borrow().as_ref() {
            let _ = js_sys::Reflect::set(
                instance,
                &wasm_bindgen::JsValue::from_str("nextLevel"),
                &wasm_bindgen::JsValue::from_f64(level as f64),
            );
        }
    });
}

/// 退避重载（MSE 模式）：新 URL 重新 `loadSource` + `startLoad`。
/// 原生模式下无实例，no-op（`src` 信号刷新即重载）。
fn hls_reload(url: &str) {
    if !cfg!(target_arch = "wasm32") {
        return;
    }
    hls_call("loadSource", &[wasm_bindgen::JsValue::from_str(url)]);
    hls_call("startLoad", &[]);
}

/// 销毁实例（组件卸载时）；无实例时 no-op。
fn hls_destroy() {
    if !cfg!(target_arch = "wasm32") {
        return;
    }
    hls_call("destroy", &[]);
    HLS_INSTANCE.with(|cell| *cell.borrow_mut() = None);
}

/// 浏览器端把 hls.js 接到 `<video>` 上（`video` 由 `NodeRef::on_load` 给）。
/// - Safari（`canPlayType("application/vnd.apple.mpegurl")` 非空）→ 原生直挂，
///   什么都不做，`src` 信号继续负责播放与重连。
/// - 其余浏览器：等 `window.Hls` 就绪（`<script src="/vendor/hls.min.js">`
///   在本组件视图里同步加载，一般已就绪；未就绪则 100ms 轮询，最多 5s），
///   `Hls.isSupported()` 时建实例并接上；不支持或超时则回落原生直挂
///   （`src` 信号仍在，Chrome 下走原有 `on:error` 退避）。
///
/// 挂载后：`MANIFEST_PARSED` 读 `hls.levels` 填清晰度档位（`on_levels`，
/// 由调用方同时清退避计数/横幅）；致命 `ERROR` 走同一指数退避
/// （`on_fatal_error`）。
fn hls_attach(
    video: web_sys::HtmlVideoElement,
    video_src: impl Fn() -> String + Copy + 'static,
    on_levels: impl Fn(Vec<(u32, u64)>) + Copy + 'static,
    on_fatal_error: impl Fn() + Copy + 'static,
    set_active: impl Fn(bool) + Copy + 'static,
) {
    if !cfg!(target_arch = "wasm32") {
        return;
    }
    // Safari 原生 HLS：canPlayType 返回 "probably"/"maybe"。
    let native = !video
        .can_play_type("application/vnd.apple.mpegurl")
        .is_empty();
    if native {
        return;
    }
    hls_try_attach(video, video_src, on_levels, on_fatal_error, set_active, 50);
}

/// 等 `window.Hls` 就绪后建实例（`tries_left` 轮 100ms 轮询）。
fn hls_try_attach(
    video: web_sys::HtmlVideoElement,
    video_src: impl Fn() -> String + Copy + 'static,
    on_levels: impl Fn(Vec<(u32, u64)>) + Copy + 'static,
    on_fatal_error: impl Fn() + Copy + 'static,
    set_active: impl Fn(bool) + Copy + 'static,
    tries_left: u32,
) {
    if !cfg!(target_arch = "wasm32") {
        return;
    }
    if let Some(ctor) = hls_constructor() {
        // Hls 存在但 MSE 不支持 → 回落原生直挂。
        if hls_is_supported(&ctor) {
            hls_attach_now(
                video,
                ctor,
                video_src,
                on_levels,
                on_fatal_error,
                set_active,
            );
        }
        return;
    }
    if tries_left == 0 {
        // 脚本 5s 未就绪 → 回落原生直挂（on:error 退避兜底）。
        return;
    }
    set_timeout(
        move || {
            hls_try_attach(
                video,
                video_src,
                on_levels,
                on_fatal_error,
                set_active,
                tries_left - 1,
            );
        },
        Duration::from_millis(100),
    );
}

/// `new Hls()` + 事件接线 + `loadSource` + `attachMedia`。
fn hls_attach_now(
    video: web_sys::HtmlVideoElement,
    ctor: js_sys::Function,
    video_src: impl Fn() -> String + Copy + 'static,
    on_levels: impl Fn(Vec<(u32, u64)>) + Copy + 'static,
    on_fatal_error: impl Fn() + Copy + 'static,
    set_active: impl Fn(bool) + Copy + 'static,
) {
    let Ok(instance) = js_sys::Reflect::construct(&ctor, &js_sys::Array::new()) else {
        return;
    };
    HLS_INSTANCE.with(|cell| *cell.borrow_mut() = Some(instance));
    // 先置 MSE 模式：把原生 src 属性摘掉，避免浏览器对 .m3u8 再发起一次
    // 原生加载（Chrome 下会直接报 error，误触 on:error 退避）。
    set_active(true);
    // MANIFEST_PARSED：读档位。
    hls_on("hlsManifestParsed", move |_, _| {
        on_levels(hls_levels());
    });
    // ERROR：只有 fatal 才走退避；非致命由 hls.js 内部恢复。
    hls_on("hlsError", move |_, data| {
        let fatal = js_sys::Reflect::get(&data, &wasm_bindgen::JsValue::from_str("fatal"))
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if fatal {
            on_fatal_error();
        }
    });
    hls_call(
        "loadSource",
        &[wasm_bindgen::JsValue::from_str(&video_src())],
    );
    hls_call("attachMedia", &[wasm_bindgen::JsValue::from(video)]);
}

/// 播放器。
///
/// - `<video>` 播放 HLS：Safari（`canPlayType("application/vnd.apple.mpegurl")`
///   非空）用原生 HLS 直挂 `src`；其余浏览器走 hls.js 1.5.13（MSE，
///   `GET /vendor/hls.min.js`，见 `crates/iptv-portal/vendor/README.md`），
///   `new Hls()` + `loadSource` + `attachMedia`，`src` 属性随即摘掉；
///   hls.js 未加载或 `Hls.isSupported()` 为假时回落原生直挂。
///   播放器链路（`play_url` 为后端签发的短时效 HMAC 地址，§6.4/§8）。
/// - 多码率：hls.js 自动选档（ABR）；`MANIFEST_PARSED` 后读 `hls.levels`
///   出清晰度档位行，可点“自动”或指定档（`hls.nextLevel`）。
/// - 缓冲/错误：`on:error` 触发指数退避重连（1s → 2s → 4s → 8s → 16s → 32s，
///   最多 5 次），`set_timeout` 到时后刷新 `src`（加 `retry` 参数避缓存）；
///   MSE 模式下改调 hls.js `loadSource` 重拉，`on:error` 由 hls.js 的
///   致命 ERROR 事件接管（同款退避计数）。
/// - 清晰度切换：调 server fn `start_playback`（见门户 crate）重新
///   `POST /api/v1/playback/start` 取新签名的 `play_url`，旧会话由
///   `POST /api/v1/playback/stop` 释放。
/// - 5 次失败后：调 server fn `stop_playback` 释放会话并显示错误，
///   `42901` 时引导去 `/me/devices` 下线其他设备（§4.6）。
#[component]
pub fn Player(
    channel: Channel,
    initial: PlaybackStartResponse,
    #[prop(optional)] qualities: Option<Vec<String>>,
    #[prop(optional)] features: Option<TokenFeatures>,
    /// 5 次重连失败后回调（父级接 `stop_playback` server fn，真实释放播放会话）。
    #[prop(optional)]
    on_failed: Option<Callback<String>>,
) -> impl IntoView {
    let qualities = qualities.unwrap_or_else(|| {
        let mut q = vec!["1080p".to_string(), "720p".to_string(), "480p".to_string()];
        if let Some(f) = &features {
            q.retain(|x| quality_rank(x) <= quality_rank(&f.max_quality));
        }
        if q.is_empty() {
            q.push("480p".to_string());
        }
        q
    });
    let play_url = RwSignal::new(initial.play_url.clone());
    let session_uid = RwSignal::new(initial.session_uid.clone());
    let quality = RwSignal::new(
        qualities
            .first()
            .cloned()
            .unwrap_or_else(|| "1080p".to_string()),
    );
    let attempts = RwSignal::new(0u32);
    let reload_nonce = RwSignal::new(0u32);
    let status = RwSignal::new(String::new());
    let failed = RwSignal::new(false);
    // m16：MSE 模式（hls.js 已接管）/ 清单档位 / 当前档（-1 = 自动 ABR）。
    let hls_active = RwSignal::new(false);
    let hls_levels = RwSignal::new(Vec::<(u32, u64)>::new());
    let hls_level = RwSignal::new(-1i32);
    let video_ref = NodeRef::<leptos::html::Video>::new();
    let channel_id = channel.id;
    let channel_name = channel
        .display_name
        .clone()
        .unwrap_or_else(|| channel.name.clone());
    // src 只在 reload_nonce 变化时变；退避延迟到时才由 set_timeout  bump 它。
    let video_src = move || {
        let base = play_url.get();
        let n = reload_nonce.get();
        if n == 0 {
            base
        } else {
            format!("{base}&retry={n}")
        }
    };

    // 指数退避重连：错误后按 2^attempts 秒延迟刷新。
    // 原生模式刷新 `src`（加 `retry` 参数避缓存）；MSE 模式调 hls.js
    // `loadSource` 重拉同一 URL（`src` 属性此时已摘掉，刷新它无效）。
    // 由 `on:error` 与 hls.js 致命 ERROR 共用。
    let schedule_retry = move || {
        let n = attempts.get() + 1;
        attempts.set(n);
        if n > 5 {
            failed.set(true);
            status.set(t_or("errors.e50301", "Channel source unavailable"));
            // 真实释放播放会话：父级（WatchPage）把 `stop_playback` server fn
            // 接到 on_failed；无回调时会话仍由服务端 90s TTL 兜底过期（§6.3）。
            if let Some(cb) = on_failed {
                cb.run(session_uid.get());
            }
            return;
        }
        let delay = 1u64 << n.min(5);
        status.set(t_args_or(
            "portal.player-retrying",
            &[("n", n.to_string()), ("secs", delay.to_string())],
            format!("Reconnecting… attempt {n}, retry in {delay}s"),
        ));
        set_timeout(
            move || {
                // 退避延迟到时才刷新（避缓存重载），实现真正的指数退避。
                reload_nonce.update(|v| *v += 1);
                if hls_active.get() {
                    hls_reload(&video_src());
                }
            },
            Duration::from_secs(delay),
        );
    };
    // 原生 <video> 的 error：MSE 模式下由 hls.js ERROR 事件接管，这里直接
    // 忽略，避免 MSE 附着后原生 error 事件误触退避。hls.js 尚在接管前
    // （轮询 window.Hls 期间）原生 error 仍走退避，其 set_timeout 回调
    // 会在 MSE 已激活时改走 hls_reload，不会误刷 src。
    let on_error = move |_| {
        if hls_active.get() {
            return;
        }
        schedule_retry();
    };
    let on_retry = move |_| {
        attempts.set(0);
        failed.set(false);
        status.set(String::new());
        reload_nonce.update(|v| *v += 1);
        if hls_active.get() {
            hls_reload(&video_src());
        }
    };

    // m16：浏览器端接 hls.js（`<video>` 挂载后触发；SSR 时不执行）。
    // 组件卸载时销毁 hls.js 实例。
    on_cleanup(hls_destroy);
    {
        let set_active = move |active: bool| hls_active.set(active);
        let on_levels = move |levels: Vec<(u32, u64)>| {
            hls_levels.set(levels);
            attempts.set(0);
            status.set(String::new());
        };
        let on_fatal = move || schedule_retry();
        video_ref.on_load(move |video| {
            hls_attach(video, video_src, on_levels, on_fatal, set_active);
        });
    }

    view! {
        <div class="player-wrap">
            <div class="player">
                // m16：hls.js（UMD）同步加载——在 <video> 解析前执行，浏览器端
                // 挂载时 window.Hls 一般已就绪；未就绪则由 hls_try_attach 轮询。
                // Safari 不用它（原生 HLS），但多加载一次无副作用。
                <script src="/vendor/hls.min.js"></script>
                <video
                    node_ref=video_ref
                    controls
                    autoplay
                    playsinline
                    preload="auto"
                    src=move || if hls_active.get() { None } else { Some(video_src()) }
                    aria-label=channel_name.clone()
                    on:error=on_error
                ></video>
            </div>
            <div class="player-status" role="status">
                {move || status.get()}
                {move || failed.get().then(|| view! {
                    <span>
                        " "
                        <a href="/me/devices">{t_or("portal.quota-exhausted", "Concurrent stream limit reached. Stop another device and try again.")}</a>
                    </span>
                })}
            </div>
            <div style="display:flex;gap:8px;align-items:center;flex-wrap:wrap;margin-top:8px">
                <span style="font-size:var(--text-footnote);color:var(--c-label-secondary)">{t_or("common.quality", "Quality")}</span>
                {qualities.into_iter().map(|q| {
                    let q2 = q.clone();
                    let active = move || quality.get() == q2;
                    let label = q.clone();
                    view! {
                        <form method="post" action=format!("/watch/{channel_id}/quality") style="display:inline">
                            <input type="hidden" name="quality" value=q.clone()/>
                            <input type="hidden" name="session_uid" value=move || session_uid.get()/>
                            <button
                                type="submit"
                                class=move || if active() { "btn btn-filled" } else { "btn btn-tinted" }
                            >{label}</button>
                        </form>
                    }
                }).collect_view()}
                <button class="btn btn-plain" type="button" on:click=on_retry>
                    {t_or("portal.player-retry", "Retry")}
                </button>
            </div>
            // m16：hls.js 清单档位行——MANIFEST_PARSED 读到 hls.levels 后才出现；
            // “自动”为 ABR（nextLevel = -1），其余为清单档位下标。原生模式无此行。
            {move || (!hls_levels.get().is_empty()).then(|| {
                let levels = hls_levels.get();
                view! {
                    <div style="display:flex;gap:8px;align-items:center;flex-wrap:wrap;margin-top:8px">
                        <span style="font-size:var(--text-footnote);color:var(--c-label-secondary)">{t_or("common.quality", "Quality")}</span>
                        <button
                            type="button"
                            class=move || if hls_level.get() == -1 { "btn btn-filled" } else { "btn btn-tinted" }
                            on:click=move |_| { hls_set_level(-1); hls_level.set(-1); }
                        >{t_or("portal.player-hls-auto", "Auto")}</button>
                        {levels.into_iter().enumerate().map(|(i, (height, _bitrate))| {
                            let label = format!("{height}p");
                            view! {
                                <button
                                    type="button"
                                    class=move || if hls_level.get() == i as i32 { "btn btn-filled" } else { "btn btn-tinted" }
                                    on:click=move |_| { hls_set_level(i as i32); hls_level.set(i as i32); }
                                >{label}</button>
                            }
                        }).collect_view()}
                    </div>
                }
            })}
            <p style="font-size:var(--text-footnote);color:var(--c-label-secondary)">
                {format!("session: {}", session_uid.get())}
            </p>
        </div>
    }
}

/// 清晰度排序（用于按 Token 的 max_quality 过滤，§3.2）。
fn quality_rank(q: &str) -> u32 {
    match q {
        "2160p" | "4K" => 5,
        "1440p" => 4,
        "1080p" => 3,
        "720p" => 2,
        "480p" => 1,
        _ => 0,
    }
}
