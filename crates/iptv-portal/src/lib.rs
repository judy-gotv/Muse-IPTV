//! 用户端门户（开发文档 §4）：Leptos SSR 应用。
//!
//! - 路由（§4.4）：`/`、`/t/{ticket}`、`/me`、`/me/devices`、`/channels`、
//!   `/watch/{channel_id}`、`/history`、`/settings`、`/help`。
//! - 数据获取：SSR 阶段由 `#[server]` 函数用 reqwest 调后端 API
//!   （`IPTV_API_BASE`，默认 `http://127.0.0.1:8080`），Cookie 自动携带；
//!   WASM 侧不接触 Token（§4.6）。
//! - 变更（登录/退出/语言/下线设备/改设置/切清晰度）走纯 Axum POST
//!   处理器（见 `main.rs`），无 hydrate 也能工作；SSR 渲染不依赖 JS。
//! - hydrate：由 cargo-leptos 生成 `pkg/`（见 README）；没有 pkg 文件时
//!   页面仍可 SSR 渲染与表单提交，交互在 hydrate 后增强。
// 作者: Passwd Word

use iptv_design::{
    Badge, BadgeTone, EmptyState, InsetGroupedList, NavItem, Row, SearchField, SegmentedControl,
    TabBar, TextField, Toggle, Toolbar,
};
use iptv_dto::{
    Channel, DeviceView, EpgProgram, MePlan, MeQuota, MeResponse, Paged, PlaybackEvent,
    PlaybackStartResponse,
};
// 只在服务端用（api_get/api_post 解 Envelope）。
#[cfg(feature = "ssr")]
use iptv_dto::Envelope;
use iptv_ui::dpad::TvRemoteHint;
use iptv_ui::{
    fmt_dt, t_args_or, t_or, ChannelCard, DeviceRow, EpgSidebar, LanguageSwitcher, LocaleCtx,
    Player, QuotaRing,
};
use leptos::prelude::*;
#[cfg(feature = "ssr")]
use leptos_axum::redirect;
use leptos_meta::{provide_meta_context, MetaTags, Title};
use leptos_router::components::{Route, Router, Routes};
use leptos_router::hooks::{use_params_map, use_query_map};
use leptos_router::path;
#[cfg(feature = "ssr")]
use serde::de::DeserializeOwned;
#[cfg(feature = "ssr")]
use serde::Serialize;

// ---------------------------------------------------------------------------
// 后端 API 客户端（server fn 与 Axum 处理器共用，§4.5）
// ---------------------------------------------------------------------------

/// 后端地址：`IPTV_API_BASE`，默认 `http://127.0.0.1:8080`。
/// 只在服务端存在（main.rs 与 server fn 用；WASM 侧不接触后端地址，§4.6）。
#[cfg(feature = "ssr")]
pub fn api_base() -> String {
    std::env::var("IPTV_API_BASE").unwrap_or_else(|_| "http://127.0.0.1:8080".to_string())
}

/// 从 `Cookie:` 头取值。
pub fn cookie_value(header: &str, name: &str) -> Option<String> {
    header.split(';').find_map(|kv| {
        let mut it = kv.splitn(2, '=');
        match (it.next(), it.next()) {
            (Some(k), Some(v)) if k.trim() == name => Some(v.trim().to_string()),
            _ => None,
        }
    })
}

/// 请求上下文里的 Cookie 头（server fn 内使用）。
#[cfg(feature = "ssr")]
fn parts_cookie(name: &str) -> Option<String> {
    let parts = use_context::<axum::http::request::Parts>()?;
    let header = parts
        .headers
        .get(axum::http::header::COOKIE)?
        .to_str()
        .ok()?;
    cookie_value(header, name)
}

/// WASM 侧读 Cookie：`document.cookie`。hydrate 后与 SSR 渲染的 locale /
/// 主题必须一致，否则页面会闪回默认值。
#[cfg(not(feature = "ssr"))]
fn parts_cookie(name: &str) -> Option<String> {
    // `cookie` 在 HtmlDocument 上（Document 没有这个方法）。
    use wasm_bindgen::JsCast;
    let document = web_sys::window()?.document()?;
    let html = document.dyn_into::<web_sys::HtmlDocument>().ok()?;
    let cookie = html.cookie().ok()?;
    cookie_value(&cookie, name)
}

/// 请求上下文里的 Accept-Language。
#[cfg(feature = "ssr")]
fn parts_accept_language() -> Option<String> {
    let parts = use_context::<axum::http::request::Parts>()?;
    parts
        .headers
        .get(axum::http::header::ACCEPT_LANGUAGE)?
        .to_str()
        .ok()
        .map(|s| s.to_string())
}

/// WASM 侧读浏览器语言。
#[cfg(not(feature = "ssr"))]
fn parts_accept_language() -> Option<String> {
    web_sys::window()?.navigator().language()
}

/// 请求上下文里的 User-Agent（m23：Cookie 未设置 `iptv_device` 时按 UA
/// 兜底判定电视端；见 `cookie_device`）。
#[cfg(feature = "ssr")]
fn parts_user_agent() -> Option<String> {
    let parts = use_context::<axum::http::request::Parts>()?;
    parts
        .headers
        .get(axum::http::header::USER_AGENT)?
        .to_str()
        .ok()
        .map(|s| s.to_string())
}

/// WASM 侧读浏览器 UA。
#[cfg(not(feature = "ssr"))]
fn parts_user_agent() -> Option<String> {
    web_sys::window()?.navigator().user_agent().ok()
}

/// 请求上下文里的原始客户端 IP（网关写好的 X-Forwarded-For 首项，或
/// X-Real-IP），调 API 时原样转发，API 才能按真实 IP 做限流分桶（M2）；
/// 直连门户时浏览器一般不带这两个头，返回 None，API 回落到对端 IP。
#[cfg(feature = "ssr")]
fn parts_forwarded_for() -> Option<String> {
    let parts = use_context::<axum::http::request::Parts>()?;
    parts
        .headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            parts
                .headers
                .get("x-real-ip")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
}

#[cfg(feature = "ssr")]
fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        // 绝不能跟随重定向：API 的票据兑换成功返回 302（§4.3，带 Location 与
        // Set-Cookie），跟随了会 GET 到 API 上不存在的 /me、丢掉 Set-Cookie，
        // 再把 404 当 JSON 解，兑换永远失败（冒烟实测）。
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// GET 后端接口，返回 `Envelope<T>` 的 data；401 返回 `Ok(None)`（未登录）。
/// 返回 `(data, set_cookie)`：登录类接口需把后端 Set-Cookie 转发给浏览器。
#[cfg(feature = "ssr")]
async fn api_get<T: DeserializeOwned>(
    path: &str,
    sid: Option<&str>,
) -> Result<(Option<T>, Option<String>), String> {
    let url = format!("{}{path}", api_base());
    let mut req = http_client().get(&url);
    if let Some(s) = sid {
        req = req.header("Cookie", format!("iptv_portal_sid={s}"));
    }
    if let Some(xff) = parts_forwarded_for() {
        req = req.header("X-Forwarded-For", xff);
    }
    let res = req.send().await.map_err(|e| e.to_string())?;
    let set_cookie = res
        .headers()
        .get(reqwest::header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    if res.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Ok((None, set_cookie));
    }
    let env: Envelope<T> = res.json().await.map_err(|e| e.to_string())?;
    if env.code != 0 {
        return Err(env.message);
    }
    Ok((env.data, set_cookie))
}

/// POST 后端接口（JSON 体），返回 data 与后端 Set-Cookie。
#[cfg(feature = "ssr")]
async fn api_post<B: Serialize, T: DeserializeOwned>(
    path: &str,
    body: &B,
    sid: Option<&str>,
) -> Result<(Option<T>, Option<String>), String> {
    let url = format!("{}{path}", api_base());
    let mut req = http_client().post(&url).json(body);
    // m14：server fn 的内部 POST 也带双提交——token 取自浏览器发来的
    // `iptv_csrf` Cookie（SSR 中间件签发），以请求头 + Cookie 头同时发给
    // API；尚无会话（票据兑换）时没有 token，API 走 Origin 兜底。
    let mut cookie = String::new();
    if let Some(s) = sid {
        cookie.push_str(&format!("iptv_portal_sid={s}"));
    }
    if let Some(c) = parts_cookie("iptv_csrf") {
        if !cookie.is_empty() {
            cookie.push_str("; ");
        }
        cookie.push_str(&format!("iptv_csrf={c}"));
        req = req.header("x-csrf-token", c);
    }
    if !cookie.is_empty() {
        req = req.header("Cookie", cookie);
    }
    if let Some(xff) = parts_forwarded_for() {
        req = req.header("X-Forwarded-For", xff);
    }
    let res = req.send().await.map_err(|e| e.to_string())?;
    let set_cookie = res
        .headers()
        .get(reqwest::header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    if res.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Ok((None, set_cookie));
    }
    let env: Envelope<T> = res.json().await.map_err(|e| e.to_string())?;
    if env.code != 0 {
        return Err(env.message);
    }
    Ok((env.data, set_cookie))
}

/// 把后端 Set-Cookie 转发给浏览器（server fn 内使用）。
#[cfg(feature = "ssr")]
fn forward_set_cookie(set_cookie: Option<String>) {
    if let Some(sc) = set_cookie {
        if let Some(res) = use_context::<leptos_axum::ResponseOptions>() {
            if let Ok(v) = axum::http::HeaderValue::from_str(&sc) {
                res.insert_header(axum::http::header::SET_COOKIE, v);
            }
        }
    }
}

/// 门户会话 Cookie；无则为 None（页面据此显示登录提示）。
#[cfg(feature = "ssr")]
fn portal_sid() -> Option<String> {
    parts_cookie("iptv_portal_sid")
}

/// 设备指纹：复用 `iptv_device_id` Cookie，没有则生成一个并写回浏览器。
#[cfg(feature = "ssr")]
fn device_fingerprint() -> String {
    if let Some(d) = parts_cookie("iptv_device_id") {
        return d;
    }
    let d = format!("web-{}", uuid::Uuid::new_v4());
    if let Some(res) = use_context::<leptos_axum::ResponseOptions>() {
        if let Ok(v) = axum::http::HeaderValue::from_str(&format!(
            "iptv_device_id={d}; Path=/; HttpOnly; SameSite=Lax; Max-Age=7776000"
        )) {
            res.insert_header(axum::http::header::SET_COOKIE, v);
        }
    }
    d
}

// ---------------------------------------------------------------------------
// Server functions（SSR 数据获取，§4.5 / §4.6）
// ---------------------------------------------------------------------------

#[server]
pub async fn api_me() -> Result<Option<MeResponse>, ServerFnError> {
    api_get::<MeResponse>("/api/v1/me", portal_sid().as_deref())
        .await
        .map(|(d, _)| d)
        .map_err(ServerFnError::new)
}

#[server]
pub async fn api_usage() -> Result<Option<MeQuota>, ServerFnError> {
    api_get::<MeQuota>("/api/v1/me/usage", portal_sid().as_deref())
        .await
        .map(|(d, _)| d)
        .map_err(ServerFnError::new)
}

#[server]
pub async fn api_devices() -> Result<Option<Vec<DeviceView>>, ServerFnError> {
    api_get::<Vec<DeviceView>>("/api/v1/me/devices", portal_sid().as_deref())
        .await
        .map(|(d, _)| d)
        .map_err(ServerFnError::new)
}

#[server]
pub async fn api_channels_with_now(
    q: Option<String>,
    group: Option<String>,
) -> Result<Option<(Paged<Channel>, Vec<EpgProgram>)>, ServerFnError> {
    let sid = portal_sid();
    let mut url = "/api/v1/me/channels?page=1&page_size=50".to_string();
    if let Some(q) = &q {
        url.push_str(&format!("&q={}", urlencoding(q)));
    }
    if let Some(g) = &group {
        if !g.is_empty() {
            url.push_str(&format!("&group={}", urlencoding(g)));
        }
    }
    let (page, _) = api_get::<Paged<Channel>>(&url, sid.as_deref())
        .await
        .map_err(ServerFnError::new)?;
    let Some(page) = page else { return Ok(None) };
    let ids: Vec<String> = page.items.iter().map(|c| c.id.to_string()).collect();
    let mut now_programs = Vec::new();
    if !ids.is_empty() {
        let (programs, _) = api_get::<Vec<EpgProgram>>(
            &format!("/api/v1/epg/now?channel_ids={}", ids.join(",")),
            sid.as_deref(),
        )
        .await
        .map_err(ServerFnError::new)?;
        now_programs = programs.unwrap_or_default();
    }
    Ok(Some((page, now_programs)))
}

#[server]
pub async fn api_history() -> Result<Option<Paged<PlaybackEvent>>, ServerFnError> {
    api_get::<Paged<PlaybackEvent>>(
        "/api/v1/me/history?page=1&page_size=50",
        portal_sid().as_deref(),
    )
    .await
    .map(|(d, _)| d)
    .map_err(ServerFnError::new)
}

#[server]
pub async fn api_plan() -> Result<Option<MePlan>, ServerFnError> {
    api_get::<Option<MePlan>>("/api/v1/me/plan", portal_sid().as_deref())
        .await
        .map(|(d, _)| d.flatten())
        .map_err(ServerFnError::new)
}

/// 播放页数据：频道（从授权列表里找）+ 开始播放（扣并发，§6.4）+ EPG。
#[server]
pub async fn api_watch_data(
    channel_id: i64,
    quality: Option<String>,
) -> Result<Option<(Channel, PlaybackStartResponse, Vec<EpgProgram>)>, ServerFnError> {
    let sid = portal_sid();
    let (page, _) =
        api_get::<Paged<Channel>>("/api/v1/me/channels?page=1&page_size=200", sid.as_deref())
            .await
            .map_err(ServerFnError::new)?;
    let Some(page) = page else { return Ok(None) };
    let Some(channel) = page.items.into_iter().find(|c| c.id == channel_id) else {
        return Err(ServerFnError::new("channel not found".to_string()));
    };
    let device_id = device_fingerprint();
    let body = serde_json::json!({
        "channel_id": channel_id,
        "quality": quality,
        "device_id": device_id,
    });
    let (start, _) =
        api_post::<_, PlaybackStartResponse>("/api/v1/playback/start", &body, sid.as_deref())
            .await
            .map_err(ServerFnError::new)?;
    let Some(start) = start else {
        return Err(ServerFnError::new("not logged in".to_string()));
    };
    let (programs, _) = api_get::<Vec<EpgProgram>>(
        &format!("/api/v1/epg/channels/{channel_id}/programs"),
        sid.as_deref(),
    )
    .await
    .map_err(ServerFnError::new)?;
    Ok(Some((channel, start, programs.unwrap_or_default())))
}

/// 释放播放会话（Player 5 次重连失败后调用，§4.6）。
#[server]
pub async fn api_stop_playback(session_uid: String) -> Result<(), ServerFnError> {
    let body = serde_json::json!({ "session_uid": session_uid });
    api_post::<_, serde_json::Value>("/api/v1/playback/stop", &body, portal_sid().as_deref())
        .await
        .map_err(ServerFnError::new)?;
    Ok(())
}

/// 票据兑换（§4.3）：成功后转发会话 Cookie 并 302 到干净 URL `/me`。
#[server]
pub async fn api_ticket_exchange(ticket: String) -> Result<bool, ServerFnError> {
    let body = serde_json::json!({
        "ticket": ticket,
        "device_fingerprint": device_fingerprint(),
    });
    let (data, set_cookie) =
        api_post::<_, serde_json::Value>("/api/v1/auth/ticket-exchange", &body, None)
            .await
            .map_err(ServerFnError::new)?;
    if data.is_some() {
        forward_set_cookie(set_cookie);
        redirect("/me");
        Ok(true)
    } else {
        Err(ServerFnError::new("ticket exchange failed".to_string()))
    }
}

/// URL 查询参数编码（按 UTF-8 字节百分号编码）。
#[cfg(feature = "ssr")]
fn urlencoding(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        let c = *b as char;
        if c.is_ascii_alphanumeric() || "-_.~".contains(c) {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Shell / App（§11.6 响应式 + 电视端；§12.2 locale 协商）
// ---------------------------------------------------------------------------

/// 每请求协商 locale：Cookie `iptv_locale` → Accept-Language → 默认。
/// 同时从 `iptv_theme` / `iptv_device` Cookie 读主题与设备形态。
fn negotiated_locale() -> String {
    let cookie = parts_cookie("iptv_locale");
    let accept = parts_accept_language();
    iptv_i18n::I18n::negotiate(None, cookie.as_deref(), accept.as_deref())
}

fn cookie_theme() -> String {
    parts_cookie("iptv_theme").unwrap_or_else(|| "system".to_string())
}

fn cookie_device() -> String {
    // m23：Cookie 显式值优先；此前全仓没有任何代码写 `iptv_device`
    // Cookie，电视/盒子靠 UA 兜底，否则电视端（含 D-pad 导航）永远进不去。
    if let Some(d) = parts_cookie("iptv_device") {
        return d;
    }
    if parts_user_agent().is_some_and(|ua| iptv_ui::dpad::looks_like_tv(&ua)) {
        return "tv".to_string();
    }
    "web".to_string()
}

/// HTML 外壳（普通函数，由 main 的 `leptos_routes` 调用）。
/// LeptosOptions 由 cargo-leptos / 环境变量提供。
pub fn shell(props: leptos::config::LeptosOptions) -> impl IntoView {
    let theme = cookie_theme();
    let device = cookie_device();
    let data_theme = match theme.as_str() {
        "dark" => "dark",
        "light" => "light",
        _ => "system",
    };
    view! {
        <!DOCTYPE html>
        <html lang=negotiated_locale() data-theme=data_theme data-device=device>
            <head>
                <meta charset="utf-8"/>
                <meta name="viewport" content="width=device-width, initial-scale=1"/>
                <meta name="referrer" content="no-referrer"/>
                <AutoReload options=props.clone()/>
                <HydrationScripts options=props.clone()/>
                <MetaTags/>
                <link rel="stylesheet" href="/design.css"/>
                <Title text="IPTV 播放平台"/>
            </head>
            <body
                // m23 电视端 D-pad：只在 data-device="tv" 时由
                // `handle_keydown` 接管方向/确认/返回键；普通 web 端不启用。
                // 监听挂在 <body> 上：焦点在页面内任何元素或 <body> 本身时
                // 按键都会冒泡到这里；SSR 渲染不输出任何属性，hydrate 后生效。
                on:keydown=move |ev| {
                    #[cfg(target_arch = "wasm32")]
                    iptv_ui::dpad::handle_keydown(&ev, "/");
                    #[cfg(not(target_arch = "wasm32"))]
                    {
                        let _ = ev;
                    }
                }
            >
                <App/>
            </body>
        </html>
    }
}

#[component]
pub fn App() -> impl IntoView {
    provide_meta_context();
    // 每请求协商 locale 并注入上下文（§12.2 / §12.4）。
    let loc = negotiated_locale();
    provide_context(LocaleCtx(loc));
    // m23：电视端在页面底部显示遥控用法提示。
    let is_tv = cookie_device() == "tv";
    view! {
        <Router>
            <TvRemoteHint is_tv=is_tv/>
            <Routes fallback=|| view! { <NotFoundPage/> }>
                <Route path=path!("/") view=LandingPage/>
                <Route path=path!("/t/:ticket") view=TicketPage/>
                <Route path=path!("/me") view=MePage/>
                <Route path=path!("/me/devices") view=DevicesPage/>
                <Route path=path!("/channels") view=ChannelsPage/>
                <Route path=path!("/watch/:channel_id") view=WatchPage/>
                <Route path=path!("/history") view=HistoryPage/>
                <Route path=path!("/settings") view=SettingsPage/>
                <Route path=path!("/help") view=HelpPage/>
            </Routes>
        </Router>
    }
}

/// 门户布局：Toolbar + TabBar（移动端底部导航，§11.5）。
#[component]
fn PortalLayout(
    #[prop(into)] title: String,
    #[prop(optional, into)] back_href: Option<String>,
    active: &'static str,
    children: Children,
) -> impl IntoView {
    let tabs = vec![
        NavItem::new(
            t_or("common.nav-me", "My Profile"),
            "/me",
            "⌂",
            active == "me",
        ),
        NavItem::new(
            t_or("common.nav-devices", "Devices"),
            "/me/devices",
            "▣",
            active == "devices",
        ),
        NavItem::new(
            t_or("common.nav-channels", "My Channels"),
            "/channels",
            "▤",
            active == "channels",
        ),
        NavItem::new(
            t_or("common.nav-history", "History"),
            "/history",
            "◷",
            active == "history",
        ),
        NavItem::new(
            t_or("common.nav-settings", "Settings"),
            "/settings",
            "⚙",
            active == "settings",
        ),
    ];
    view! {
        <Toolbar title=title back_href=back_href.unwrap_or_default()>
            <LanguageSwitcher redirect_to="/me"/>
            <form method="post" action="/logout" style="display:inline">
                <button class="btn btn-plain" type="submit">{t_or("common.logout", "Sign out")}</button>
            </form>
        </Toolbar>
        <div class="page">{children()}</div>
        <TabBar items=tabs/>
    }
}

/// 未登录提示（§4.6：会话过期引导重新登录）。
#[component]
fn LoginPrompt() -> impl IntoView {
    view! {
        <EmptyState title=t_or("errors.e40101", "Session expired, please sign in again") icon="⚿">
            <a class="btn btn-filled" href="/">{t_or("common.login", "Sign in")}</a>
        </EmptyState>
    }
}

#[component]
fn NotFoundPage() -> impl IntoView {
    view! {
        <div class="page">
            <EmptyState title=t_or("errors.e40401", "Not found") icon="?">
                <a class="btn btn-filled" href="/">{t_or("common.nav-me", "My Profile")}</a>
            </EmptyState>
        </div>
    }
}

/// 加载骨架（§4.3：SSR 先渲染登录中骨架，无白屏）。
#[component]
fn LoadingSkeleton() -> impl IntoView {
    view! { <p role="status">{t_or("common.loading", "Loading…")}</p> }
}

// ---------------------------------------------------------------------------
// §4.4 门户页面
// ---------------------------------------------------------------------------

/// `/` 落地页：Token 输入框 + 扫码提示（§4.2 方式 B；方式 A 用票据链接）。
#[component]
fn LandingPage() -> impl IntoView {
    let query = use_query_map();
    let error = move || query.get().get("error").unwrap_or_default();
    view! {
        <Toolbar title=t_or("common.app-name", "IPTV Platform")>
            <LanguageSwitcher redirect_to="/"/>
        </Toolbar>
        <div class="page">
            <div class="card glass" style="max-width:560px;margin:0 auto">
                <h1 style="font-size:var(--text-title1);margin-top:0">{t_or("common.app-name", "IPTV Platform")}</h1>
                <p style="color:var(--c-label-secondary)">{t_or("portal.landing-hint", "Paste your Token to sign in, or open the ticket link / scan the QR code from your admin.")}</p>
                {move || {
                    let e = error();
                    if e.is_empty() { view! { "" }.into_any() } else {
                        view! { <div class="notice notice-error" role="alert">{e}</div> }.into_any()
                    }
                }}
                <form method="post" action="/login">
                    <TextField label=t_or("portal.token-label", "Token") name="token" input_type="password" required=true autocomplete="off"/>
                    <div style="margin-bottom:12px">
                        <Toggle label=t_or("portal.remember-device", "Remember this device (90 days)") name="remember_device"/>
                    </div>
                    <button class="btn btn-filled" type="submit" style="width:100%">{t_or("common.login", "Sign in")}</button>
                </form>
                <p style="font-size:var(--text-footnote);color:var(--c-label-secondary)">
                    {t_or("portal.ticket-hint", "Got a login link like /t/…? Just open it — the ticket is single-use and valid for 60 seconds.")}
                </p>
                <p><a href="/help">{t_or("common.nav-help", "Help")}</a></p>
            </div>
        </div>
    }
}

/// `/t/{ticket}` 票据兑换中间页（§4.3：先渲染骨架，兑换成功 302 到 `/me`）。
#[component]
fn TicketPage() -> impl IntoView {
    let params = use_params_map();
    let ticket = move || params.get().get("ticket").unwrap_or_default();
    let exchange = Resource::new_blocking(ticket, |t| async move { api_ticket_exchange(t).await });
    view! {
        <Toolbar title=t_or("common.app-name", "IPTV Platform")>
            <LanguageSwitcher redirect_to="/"/>
        </Toolbar>
        <div class="page">
            <Suspense fallback=LoadingSkeleton>
                {move || exchange.get().map(|res| match res {
                    Ok(true) => view! { <p>{t_or("common.loading", "Loading…")}</p> }.into_any(),
                    Ok(false) => view! {
                        <EmptyState title=t_or("errors.e40101", "Session expired, please sign in again") icon="⚿">
                            <a class="btn btn-filled" href="/">{t_or("common.login", "Sign in")}</a>
                        </EmptyState>
                    }.into_any(),
                    Err(e) => view! {
                        <div class="notice notice-error" role="alert">{e.to_string()}</div>
                        <a class="btn btn-filled" href="/">{t_or("common.login", "Sign in")}</a>
                    }.into_any(),
                })}
            </Suspense>
        </div>
    }
}

/// `/me` 我的资料（§4.1 / §4.5 示例字段逐一展示）。
#[component]
fn MePage() -> impl IntoView {
    let me = Resource::new_blocking(|| (), |_| async move { api_me().await });
    view! {
        <PortalLayout title=t_or("common.nav-me", "My Profile") active="me">
            <Suspense fallback=LoadingSkeleton>
                {move || me.get().map(|res| match res {
                    Ok(Some(m)) => {
                        let expires = m.token.expires_at.map(fmt_dt).unwrap_or_else(|| "—".to_string());
                        let plan_line = match &m.plan {
                            Some(p) => format!("{} · {}", p.name, p.expires_at.map(fmt_dt).unwrap_or_default()),
                            None => t_or("portal.no-plan", "No plan"),
                        };
                        let groups: Vec<(String, String)> = m.allowed_groups.iter().map(|g| (g.clone(), g.clone())).collect();
                        view! {
                            <div class="card glass">
                                <h2 style="margin-top:0">{m.user.display_name.clone()}</h2>
                                <p style="color:var(--c-label-secondary)">{plan_line}</p>
                                <p>
                                    <Badge text=m.token.name.clone() tone=BadgeTone::Accent/>
                                    " "
                                    {match m.token.status {
                                        iptv_dto::TokenStatus::Active => view! { <Badge text="active" tone=BadgeTone::Success/> }.into_any(),
                                        _ => view! { <Badge text=t_or("portal.token-expired", "Token expired") tone=BadgeTone::Destructive/> }.into_any(),
                                    }}
                                </p>
                                <p>{t_args_or_portal_expires(m.token.days_remaining)}</p>
                                <p style="color:var(--c-label-secondary)">{format!("{} · {}", m.token.device_type.clone().unwrap_or_default(), expires)}</p>
                            </div>
                            <div class="card">
                                <h3 style="margin-top:0">{t_or("portal.quota-title", "Concurrent quota")}</h3>
                                <QuotaRing quota=m.quota.clone()/>
                                <p><a href="/me/devices">{t_or("common.nav-devices", "Devices")}" ›"</a></p>
                            </div>
                            <div class="card">
                                <h3 style="margin-top:0">{t_or("portal.allowed-groups", "Allowed groups")}</h3>
                                {if groups.is_empty() {
                                    view! { <EmptyState title=t_or("portal.no-channels", "No channels available") icon="▤">{view! { <></> }}</EmptyState> }.into_any()
                                } else {
                                    view! { <SegmentedControl options=groups selected=m.allowed_groups.first().cloned().unwrap_or_default() link_base="/channels?group="/> }.into_any()
                                }}
                                <p style="margin-top:12px">
                                    "catchup: " {if m.features.catchup { "✓" } else { "✗" }}
                                    " · recording: " {if m.features.recording { "✓" } else { "✗" }}
                                    " · max quality: " {m.features.max_quality.clone()}
                                </p>
                            </div>
                        }.into_any()
                    }
                    Ok(None) => view! { <LoginPrompt/> }.into_any(),
                    Err(e) => view! { <div class="notice notice-error" role="alert">{e.to_string()}</div> }.into_any(),
                })}
            </Suspense>
        </PortalLayout>
    }
}

/// 会话到期文案：走 Fluent 复数选择（`portal.session-expires-in`），缺 key
/// 时由 `t_args_or` 的英文兜底承接，不在代码里另写英文分支。
fn t_args_or_portal_expires(days: i64) -> String {
    t_args_or(
        "portal.session-expires-in",
        &[("days", days.to_string())],
        if days == 0 {
            "Expires today".to_string()
        } else {
            format!("Expires in {days} days")
        },
    )
}

/// `/me/devices` 在线设备与并发占用（§4.1）。
#[component]
fn DevicesPage() -> impl IntoView {
    let devices = Resource::new_blocking(|| (), |_| async move { api_devices().await });
    let usage = Resource::new_blocking(|| (), |_| async move { api_usage().await });
    view! {
        <PortalLayout title=t_or("common.nav-devices", "Devices") active="devices">
            <Suspense fallback=LoadingSkeleton>
                {move || usage.get().map(|res| match res {
                    Ok(Some(q)) => view! { <div class="card"><QuotaRing quota=q/></div> }.into_any(),
                    Ok(None) => view! { <LoginPrompt/> }.into_any(),
                    Err(e) => view! { <div class="notice notice-error" role="alert">{e.to_string()}</div> }.into_any(),
                })}
            </Suspense>
            <Suspense fallback=LoadingSkeleton>
                {move || devices.get().map(|res| match res {
                    Ok(Some(list)) => {
                        if list.is_empty() {
                            view! { <EmptyState title=t_or("portal.no-devices", "No online devices") icon="▣">{view! { <></> }}</EmptyState> }.into_any()
                        } else {
                            view! {
                                <InsetGroupedList title=t_or("common.nav-devices", "Devices")>
                                    {list.into_iter().map(|d| view! { <DeviceRow device=d/> }).collect_view()}
                                </InsetGroupedList>
                            }.into_any()
                        }
                    }
                    Ok(None) => view! { <LoginPrompt/> }.into_any(),
                    Err(e) => view! { <div class="notice notice-error" role="alert">{e.to_string()}</div> }.into_any(),
                })}
            </Suspense>
        </PortalLayout>
    }
}

/// `/channels` 我的频道（§4.4：授权分组、频道、当前节目、播放按钮）。
#[component]
fn ChannelsPage() -> impl IntoView {
    let query = use_query_map();
    let q = move || query.get().get("q");
    let group = move || query.get().get("group");
    let data = Resource::new_blocking(
        move || (q(), group()),
        |(q, g)| async move { api_channels_with_now(q, g).await },
    );
    let me = Resource::new_blocking(|| (), |_| async move { api_me().await });
    view! {
        <PortalLayout title=t_or("common.nav-channels", "My Channels") active="channels">
            <form method="get" action="/channels" style="display:flex;gap:8px;margin-bottom:12px">
                <SearchField value=q().unwrap_or_default() placeholder=t_or("common.search", "Search")/>
                <button class="btn btn-tinted" type="submit">{t_or("common.search", "Search")}</button>
            </form>
            <Suspense fallback=LoadingSkeleton>
                {move || me.get().map(|res| match res {
                    Ok(Some(m)) => {
                        let mut opts: Vec<(String, String)> = vec![("".to_string(), t_or("common.all", "All"))];
                        for g in &m.allowed_groups { opts.push((g.clone(), g.clone())); }
                        let sel = group().unwrap_or_default();
                        view! { <div style="margin-bottom:12px"><SegmentedControl options=opts selected=sel link_base="/channels?group="/></div> }.into_any()
                    }
                    _ => view! { "" }.into_any(),
                })}
            </Suspense>
            <Suspense fallback=LoadingSkeleton>
                {move || data.get().map(|res| match res {
                    Ok(Some((page, now))) => {
                        if page.items.is_empty() {
                            view! { <EmptyState title=t_or("portal.no-channels", "No channels available") icon="▤">{view! { <></> }}</EmptyState> }.into_any()
                        } else {
                            let shown = page.items.len();
                            let total = page.total;
                            view! {
                                <div class="channel-grid">
                                    {page.items.into_iter().map(|c| {
                                        // Leptos 0.8 约定：optional prop 传内层值，省略即 None。
                                        match now.iter().find(|p| p.channel_id == c.id).cloned() {
                                            Some(cur) => view! { <ChannelCard channel=c current_program=cur/> }.into_any(),
                                            None => view! { <ChannelCard channel=c/> }.into_any(),
                                        }
                                    }).collect_view()}
                                </div>
                                <p style="color:var(--c-label-secondary);font-size:var(--text-footnote)">
                                    {format!("{shown} / {total}")}
                                </p>
                            }.into_any()
                        }
                    }
                    Ok(None) => view! { <LoginPrompt/> }.into_any(),
                    Err(e) => view! { <div class="notice notice-error" role="alert">{e.to_string()}</div> }.into_any(),
                })}
            </Suspense>
        </PortalLayout>
    }
}

/// `/watch/{channel_id}` 播放页（§4.4 / §4.6：播放器、清晰度、EPG 侧栏）。
#[component]
fn WatchPage() -> impl IntoView {
    let params = use_params_map();
    let query = use_query_map();
    let channel_id = move || {
        params
            .get()
            .get("channel_id")
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0)
    };
    let quality = move || query.get().get("quality");
    let data = Resource::new_blocking(
        move || (channel_id(), quality()),
        |(id, q)| async move { api_watch_data(id, q).await },
    );
    // 播放失败回调：真实调 POST /api/v1/playback/stop 释放会话（§4.6）。
    let stop_action = Action::new(|uid: &String| {
        let uid = uid.clone();
        async move { api_stop_playback(uid).await }
    });
    let on_failed = Callback::new(move |uid: String| {
        stop_action.dispatch(uid);
    });
    view! {
        <PortalLayout title=t_or("portal.watch", "Watch") back_href="/channels" active="channels">
            <Suspense fallback=LoadingSkeleton>
                {move || data.get().map(|res| match res {
                    Ok(Some((channel, start, programs))) => {
                        let name = channel.display_name.clone().unwrap_or_else(|| channel.name.clone());
                        view! {
                            <h2 style="font-size:var(--text-title1);margin-top:0">{name}</h2>
                            <div style="display:grid;grid-template-columns:1fr;gap:16px">
                                <Player channel=channel initial=start on_failed=on_failed/>
                                <EpgSidebar programs=programs/>
                            </div>
                        }.into_any()
                    }
                    Ok(None) => view! { <LoginPrompt/> }.into_any(),
                    Err(e) => {
                        // 42901 并发超限：引导去 /me/devices 下线其他设备（§4.6）。
                        let msg = e.to_string();
                        let is_limit = msg.contains("42901") || msg.contains("Concurrent") || msg.contains("並發");
                        view! {
                            <div class="notice notice-error" role="alert">{msg.clone()}</div>
                            {is_limit.then(|| view! {
                                <p><a class="btn btn-filled" href="/me/devices">{t_or("common.nav-devices", "Devices")}</a></p>
                            })}
                            <p><a href="/channels">{t_or("common.nav-channels", "My Channels")}</a></p>
                        }.into_any()
                    }
                })}
            </Suspense>
        </PortalLayout>
    }
}

/// `/history` 播放记录（§4.4）。
#[component]
fn HistoryPage() -> impl IntoView {
    let history = Resource::new_blocking(|| (), |_| async move { api_history().await });
    view! {
        <PortalLayout title=t_or("common.nav-history", "History") active="history">
            <Suspense fallback=LoadingSkeleton>
                {move || history.get().map(|res| match res {
                    Ok(Some(page)) => {
                        if page.items.is_empty() {
                            view! { <EmptyState title=t_or("portal.no-history", "No playback history yet") icon="◷">{view! { <></> }}</EmptyState> }.into_any()
                        } else {
                            let shown = page.items.len();
                            let total = page.total;
                            view! {
                                <InsetGroupedList title=t_or("common.nav-history", "History")>
                                    {page.items.into_iter().map(|e| view! {
                                        <Row
                                            title=format!("channel #{} · {}", e.channel_id, e.event_type)
                                            subtitle=fmt_dt(e.created_at)
                                            value=e.session_uid.clone()
                                        />
                                    }).collect_view()}
                                </InsetGroupedList>
                                <p style="color:var(--c-label-secondary);font-size:var(--text-footnote)">
                                    {format!("{shown} / {total}")}
                                </p>
                            }.into_any()
                        }
                    }
                    Ok(None) => view! { <LoginPrompt/> }.into_any(),
                    Err(e) => view! { <div class="notice notice-error" role="alert">{e.to_string()}</div> }.into_any(),
                })}
            </Suspense>
        </PortalLayout>
    }
}

/// `/settings` 个人设置（§4.4：显示名、语言、外观、退出设备）。
#[component]
fn SettingsPage() -> impl IntoView {
    let me = Resource::new_blocking(|| (), |_| async move { api_me().await });
    let query = use_query_map();
    let saved = move || query.get().get("saved").is_some();
    view! {
        <PortalLayout title=t_or("common.nav-settings", "Settings") active="settings">
            {move || saved().then(|| view! {
                <div class="notice notice-success" role="status">{t_or("common.saved", "Saved")}</div>
            })}
            <Suspense fallback=LoadingSkeleton>
                {move || me.get().map(|res| match res {
                    Ok(Some(m)) => view! {
                        <div class="card">
                            <form method="post" action="/me/settings">
                                <TextField label=t_or("portal.display-name", "Display name") name="display_name" value=m.user.display_name.clone()/>
                                <div class="field">
                                    <label for="theme-select">{t_or("portal.theme", "Appearance")}</label>
                                    <select id="theme-select" name="theme" class="textfield">
                                        <option value="system" selected=m.user.theme == "system">system</option>
                                        <option value="light" selected=m.user.theme == "light">light</option>
                                        <option value="dark" selected=m.user.theme == "dark">dark</option>
                                    </select>
                                </div>
                                <div class="field">
                                    <label for="locale-select">{t_or("common.language", "Language")}</label>
                                    <select id="locale-select" name="locale" class="textfield">
                                        {iptv_i18n::I18n::supported_locales().into_iter().map(|l| {
                                            let sel = l == m.user.locale;
                                            view! { <option value=l selected=sel>{iptv_i18n::locale_display_name(l)}</option> }
                                        }).collect_view()}
                                    </select>
                                </div>
                                <button class="btn btn-filled" type="submit">{t_or("common.save", "Save")}</button>
                            </form>
                        </div>
                        <div class="card">
                            <h3 style="margin-top:0">{t_or("common.language", "Language")}</h3>
                            <LanguageSwitcher redirect_to="/settings"/>
                        </div>
                        <div class="card">
                            <h3 style="margin-top:0">{t_or("portal.devices", "Devices")}</h3>
                            <p><a href="/me/devices">{t_or("common.nav-devices", "Devices")}" ›"</a></p>
                            <form method="post" action="/logout-all">
                                <button class="btn btn-destructive" type="submit">{t_or("portal.logout-all", "Sign out of all devices")}</button>
                            </form>
                        </div>
                    }.into_any(),
                    Ok(None) => view! { <LoginPrompt/> }.into_any(),
                    Err(e) => view! { <div class="notice notice-error" role="alert">{e.to_string()}</div> }.into_any(),
                })}
            </Suspense>
        </PortalLayout>
    }
}

/// `/help` 帮助与客服（§4.4：FAQ、联系方式）。
#[component]
fn HelpPage() -> impl IntoView {
    view! {
        <PortalLayout title=t_or("common.nav-help", "Help") active="settings">
            <div class="card">
                <h2 style="margin-top:0">{t_or("portal.help-title", "Help & support")}</h2>
                <InsetGroupedList title=t_or("portal.faq", "FAQ")>
                    <Row title=t_or("portal.faq-login", "How do I sign in?") subtitle=t_or("portal.faq-login-a", "Open the ticket link from your admin, or paste your Token on the home page.")/>
                    <Row title=t_or("portal.faq-limit", "Why does playback say the stream limit is reached?") subtitle=t_or("portal.faq-limit-a", "Your Token allows only a limited number of concurrent streams. Go to Devices and sign out another device, then try again.")/>
                    <Row title=t_or("portal.faq-ticket", "My login link does not work") subtitle=t_or("portal.faq-ticket-a", "Tickets are single-use and valid for 60 seconds. Ask your admin for a new login link.")/>
                    <Row title=t_or("portal.faq-quality", "Can I change quality?") subtitle=t_or("portal.faq-quality-a", "Yes — use the quality buttons on the watch page. The maximum quality is set by your plan.")/>
                </InsetGroupedList>
                <p style="color:var(--c-label-secondary);font-size:var(--text-footnote)">
                    {t_or("portal.help-contact", "For account issues, contact your admin or operator.")}
                </p>
            </div>
        </PortalLayout>
    }
}

// ---------------------------------------------------------------------------
// WASM hydrate 入口（B4）
// ---------------------------------------------------------------------------

/// hydrate 入口：cargo-leptos 生成的 `/pkg/iptv-portal.js` 在加载后调用它，
/// 把 SSR 出来的 HTML 挂上交互（`#[server]` 函数的客户端存根、资源重取）。
/// 只在 hydrate 构建里编译；服务端构建不包含它。
#[cfg(feature = "hydrate")]
#[wasm_bindgen::prelude::wasm_bindgen]
pub fn hydrate() {
    console_error_panic_hook::set_once();
    leptos::mount::hydrate_body(App);
}
