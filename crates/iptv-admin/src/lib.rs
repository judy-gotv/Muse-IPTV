// M19：行内编辑表单把 view! 嵌套推过默认 128 层（编译器建议值），
// 代码生成阶段会报 queries overflow the depth limit。
#![recursion_limit = "256"]
//! 管理后台（开发文档 §5）：Leptos SSR 应用。
//!
//! - 路由（§5.1）：`/admin/login`、`/admin`、`/admin/users`、`/admin/tokens`、
//!   `/admin/channels`、`/admin/sources`、`/admin/groups`、`/admin/epg`、
//!   `/admin/sessions`、`/admin/plans`、`/admin/admins`、`/admin/audit`、
//!   `/admin/settings`。
//! - 左侧深色导航栏（Sidebar，可折叠）；表格 + 搜索 + 分页；写操作调后端
//!   API 并展示结果（§5.2）。
//! - 权限按角色做展示差异（§5.3，角色来自登录后写的 `iptv_admin_role`
//!   Cookie，仅作展示；服务端已校验，前端隐藏不等于授权）。
//! - 变更走纯 Axum POST 处理器（见 `main.rs`），无 hydrate 也能工作。
// 作者: Passwd Word

use iptv_design::{
    Badge, BadgeTone, EmptyState, InsetGroupedList, NavItem, Row, SearchField, Sidebar, StatusDot,
    StatusKind, TextField, Toggle, Toolbar,
};
use iptv_dto::{
    AdminRole, AdminUser, AppUser, AuditLog, Channel, ChannelGroup, EpgProgram, EpgSource, Paged,
    Plan, PlaybackSession, PlaylistSource, StatsOverview, SyncDiff, SyncLog, SystemSetting,
    TokenStatus, UserStatus, UserToken,
};
// 只在服务端用（admin_get 解 Envelope）。
#[cfg(feature = "ssr")]
use iptv_dto::Envelope;
use iptv_ui::{fmt_dt, role_label, status_label, t_args_or, t_or, LanguageSwitcher, LocaleCtx};
use leptos::prelude::*;
use leptos_meta::{provide_meta_context, MetaTags, Title};
use leptos_router::components::{Route, Router, Routes};
use leptos_router::hooks::use_query_map;
use leptos_router::path;
#[cfg(feature = "ssr")]
use serde::de::DeserializeOwned;

// ---------------------------------------------------------------------------
// 后端 API 客户端（server fn 与 Axum 处理器共用，§5.2）
// ---------------------------------------------------------------------------

/// 后端地址：`IPTV_API_BASE`，默认 `http://127.0.0.1:8080`。
/// 只在服务端存在（main.rs 与 server fn 用；WASM 侧不接触后端地址）。
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
/// 主题 / 角色必须一致，否则页面会闪回默认值。
#[cfg(not(feature = "ssr"))]
fn parts_cookie(name: &str) -> Option<String> {
    // `cookie` 在 HtmlDocument 上（Document 没有这个方法）。
    use wasm_bindgen::JsCast;
    let document = web_sys::window()?.document()?;
    let html = document.dyn_into::<web_sys::HtmlDocument>().ok()?;
    let cookie = html.cookie().ok()?;
    cookie_value(&cookie, name)
}

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

#[cfg(feature = "ssr")]
fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// GET 后端接口；401 返回 `Ok(None)`（未登录）。
#[cfg(feature = "ssr")]
async fn admin_get<T: DeserializeOwned>(path: &str) -> Result<Option<T>, String> {
    let url = format!("{}{path}", api_base());
    let mut req = http_client().get(&url);
    if let Some(s) = parts_cookie("iptv_admin_sid") {
        req = req.header("Cookie", format!("iptv_admin_sid={s}"));
    }
    let res = req.send().await.map_err(|e| e.to_string())?;
    if res.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Ok(None);
    }
    let env: Envelope<T> = res.json().await.map_err(|e| e.to_string())?;
    if env.code != 0 {
        return Err(env.message);
    }
    Ok(env.data)
}

/// 当前登录管理员的角色（展示用，来自登录时写的 Cookie；服务端另行校验）。
#[derive(Debug, Clone)]
pub struct AdminRoleCtx(pub Option<AdminRole>);

pub fn admin_role() -> Option<AdminRole> {
    use_context::<AdminRoleCtx>().and_then(|c| c.0.clone())
}

fn negotiated_locale() -> String {
    let cookie = parts_cookie("iptv_locale");
    let accept = parts_accept_language();
    iptv_i18n::I18n::negotiate(None, cookie.as_deref(), accept.as_deref())
}

fn cookie_theme() -> String {
    parts_cookie("iptv_theme").unwrap_or_else(|| "system".to_string())
}

// ---------------------------------------------------------------------------
// Server functions（SSR 数据获取，§5.2）
// ---------------------------------------------------------------------------

#[server]
pub async fn admin_overview() -> Result<Option<StatsOverview>, ServerFnError> {
    admin_get::<StatsOverview>("/api/v1/admin/stats/overview")
        .await
        .map_err(ServerFnError::new)
}

#[server]
pub async fn admin_users(
    page: u32,
    q: Option<String>,
) -> Result<Option<Paged<AppUser>>, ServerFnError> {
    let mut url = format!("/api/v1/admin/users?page={page}&page_size=20");
    if let Some(q) = q {
        if !q.is_empty() {
            url.push_str(&format!("&q={}", pct_encode(&q)));
        }
    }
    admin_get::<Paged<AppUser>>(&url)
        .await
        .map_err(ServerFnError::new)
}

#[server]
pub async fn admin_tokens(
    page: u32,
    q: Option<String>,
) -> Result<Option<Paged<UserToken>>, ServerFnError> {
    let mut url = format!("/api/v1/admin/tokens?page={page}&page_size=20");
    if let Some(q) = q {
        if !q.is_empty() {
            url.push_str(&format!("&q={}", pct_encode(&q)));
        }
    }
    admin_get::<Paged<UserToken>>(&url)
        .await
        .map_err(ServerFnError::new)
}

#[server]
pub async fn admin_channels(
    page: u32,
    q: Option<String>,
) -> Result<Option<Paged<Channel>>, ServerFnError> {
    let mut url = format!("/api/v1/admin/channels?page={page}&page_size=20");
    if let Some(q) = q {
        if !q.is_empty() {
            url.push_str(&format!("&q={}", pct_encode(&q)));
        }
    }
    admin_get::<Paged<Channel>>(&url)
        .await
        .map_err(ServerFnError::new)
}

#[server]
pub async fn admin_sources() -> Result<Option<Vec<PlaylistSource>>, ServerFnError> {
    admin_get::<Vec<PlaylistSource>>("/api/v1/admin/sources")
        .await
        .map_err(ServerFnError::new)
}

#[server]
pub async fn admin_source_diff(source_id: i64) -> Result<Option<SyncDiff>, ServerFnError> {
    admin_get::<SyncDiff>(&format!("/api/v1/admin/sources/{source_id}/diff"))
        .await
        .map_err(ServerFnError::new)
}

#[server]
pub async fn admin_source_logs(source_id: i64) -> Result<Option<Paged<SyncLog>>, ServerFnError> {
    admin_get::<Paged<SyncLog>>(&format!(
        "/api/v1/admin/sources/{source_id}/logs?page=1&page_size=20"
    ))
    .await
    .map_err(ServerFnError::new)
}

#[server]
pub async fn admin_groups() -> Result<Option<Vec<ChannelGroup>>, ServerFnError> {
    admin_get::<Vec<ChannelGroup>>("/api/v1/admin/groups")
        .await
        .map_err(ServerFnError::new)
}

#[server]
pub async fn admin_epg_sources() -> Result<Option<Vec<EpgSource>>, ServerFnError> {
    admin_get::<Vec<EpgSource>>("/api/v1/admin/epg/sources")
        .await
        .map_err(ServerFnError::new)
}

#[server]
pub async fn admin_epg_programs(channel_id: i64) -> Result<Option<Vec<EpgProgram>>, ServerFnError> {
    admin_get::<Vec<EpgProgram>>(&format!("/api/v1/epg/channels/{channel_id}/programs"))
        .await
        .map_err(ServerFnError::new)
}

#[server]
pub async fn admin_sessions(page: u32) -> Result<Option<Paged<PlaybackSession>>, ServerFnError> {
    admin_get::<Paged<PlaybackSession>>(&format!("/api/v1/admin/sessions?page={page}&page_size=20"))
        .await
        .map_err(ServerFnError::new)
}

#[server]
pub async fn admin_plans() -> Result<Option<Vec<Plan>>, ServerFnError> {
    admin_get::<Vec<Plan>>("/api/v1/admin/plans")
        .await
        .map_err(ServerFnError::new)
}

#[server]
pub async fn admin_admins() -> Result<Option<Vec<AdminUser>>, ServerFnError> {
    admin_get::<Vec<AdminUser>>("/api/v1/admin/admins")
        .await
        .map_err(ServerFnError::new)
}

#[server]
pub async fn admin_audit(
    actor: Option<String>,
    action: Option<String>,
) -> Result<Option<Paged<AuditLog>>, ServerFnError> {
    let mut url = "/api/v1/admin/audit?page=1&page_size=50".to_string();
    if let Some(a) = actor {
        if !a.is_empty() {
            url.push_str(&format!("&actor={}", pct_encode(&a)));
        }
    }
    if let Some(a) = action {
        if !a.is_empty() {
            url.push_str(&format!("&action={}", pct_encode(&a)));
        }
    }
    admin_get::<Paged<AuditLog>>(&url)
        .await
        .map_err(ServerFnError::new)
}

#[server]
pub async fn admin_settings() -> Result<Option<Vec<SystemSetting>>, ServerFnError> {
    admin_get::<Vec<SystemSetting>>("/api/v1/admin/settings")
        .await
        .map_err(ServerFnError::new)
}

#[cfg(feature = "ssr")]
fn pct_encode(s: &str) -> String {
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
// Shell / App / 布局（§5.1 / §11.5 Sidebar 可折叠）
// ---------------------------------------------------------------------------

/// HTML 外壳（普通函数，由 main 的 `leptos_routes` 调用）。
pub fn shell(props: leptos::config::LeptosOptions) -> impl IntoView {
    let theme = cookie_theme();
    let data_theme = match theme.as_str() {
        "dark" => "dark",
        "light" => "light",
        _ => "system",
    };
    view! {
        <!DOCTYPE html>
        <html lang=negotiated_locale() data-theme=data_theme>
            <head>
                <meta charset="utf-8"/>
                <meta name="viewport" content="width=device-width, initial-scale=1"/>
                <meta name="referrer" content="no-referrer"/>
                <AutoReload options=props.clone()/>
                <HydrationScripts options=props.clone()/>
                <MetaTags/>
                <link rel="stylesheet" href="/design.css"/>
                <Title text="IPTV 管理后台"/>
            </head>
            <body>
                <App/>
            </body>
        </html>
    }
}

#[component]
pub fn App() -> impl IntoView {
    provide_meta_context();
    provide_context(LocaleCtx(negotiated_locale()));
    // 角色仅作展示用（服务端校验）；Cookie 值形如 super_admin。
    let role = parts_cookie("iptv_admin_role").and_then(|r| match r.as_str() {
        "super_admin" => Some(AdminRole::SuperAdmin),
        "operator" => Some(AdminRole::Operator),
        "support" => Some(AdminRole::Support),
        "readonly" => Some(AdminRole::Readonly),
        _ => None,
    });
    provide_context(AdminRoleCtx(role));
    view! {
        <Router>
            <Routes fallback=|| view! { <NotFoundPage/> }>
                <Route path=path!("/admin/login") view=LoginPage/>
                <Route path=path!("/admin") view=DashboardPage/>
                <Route path=path!("/admin/users") view=UsersPage/>
                <Route path=path!("/admin/tokens") view=TokensPage/>
                <Route path=path!("/admin/channels") view=ChannelsPage/>
                <Route path=path!("/admin/sources") view=SourcesPage/>
                <Route path=path!("/admin/groups") view=GroupsPage/>
                <Route path=path!("/admin/epg") view=EpgPage/>
                <Route path=path!("/admin/sessions") view=SessionsPage/>
                <Route path=path!("/admin/plans") view=PlansPage/>
                <Route path=path!("/admin/admins") view=AdminsPage/>
                <Route path=path!("/admin/audit") view=AuditPage/>
                <Route path=path!("/admin/settings") view=SettingsPage/>
            </Routes>
        </Router>
    }
}

/// 后台布局：左侧深色导航栏（可折叠）+ 顶部 Toolbar。
#[component]
fn AdminLayout(
    #[prop(into)] title: String,
    active: &'static str,
    children: Children,
) -> impl IntoView {
    let collapsed = RwSignal::new(false);
    let items = vec![
        NavItem::new(
            t_or("admin.dashboard", "Dashboard"),
            "/admin",
            "▦",
            active == "dashboard",
        ),
        NavItem::new(
            t_or("admin.users", "Users"),
            "/admin/users",
            "☺",
            active == "users",
        ),
        NavItem::new(
            t_or("admin.tokens", "Tokens"),
            "/admin/tokens",
            "⚿",
            active == "tokens",
        ),
        NavItem::new(
            t_or("admin.channels", "Channels"),
            "/admin/channels",
            "▤",
            active == "channels",
        ),
        NavItem::new(
            t_or("admin.sources", "Sources"),
            "/admin/sources",
            "⇩",
            active == "sources",
        ),
        NavItem::new(
            t_or("admin.groups", "Groups"),
            "/admin/groups",
            "▧",
            active == "groups",
        ),
        NavItem::new(t_or("admin.epg", "EPG"), "/admin/epg", "▦", active == "epg"),
        NavItem::new(
            t_or("admin.sessions", "Sessions"),
            "/admin/sessions",
            "◉",
            active == "sessions",
        ),
        NavItem::new(
            t_or("admin.plans", "Plans"),
            "/admin/plans",
            "◆",
            active == "plans",
        ),
        NavItem::new(
            t_or("admin.admins", "Admins"),
            "/admin/admins",
            "♛",
            active == "admins",
        ),
        NavItem::new(
            t_or("admin.audit", "Audit log"),
            "/admin/audit",
            "≣",
            active == "audit",
        ),
        NavItem::new(
            t_or("admin.settings", "Settings"),
            "/admin/settings",
            "⚙",
            active == "settings",
        ),
    ];
    let toggle = Callback::new(move |_| collapsed.update(|v| *v = !*v));
    let role_text = admin_role()
        .map(|r| role_label(&format!("{:?}", r)))
        .unwrap_or_default();
    view! {
        <div class="app-shell">
            <Sidebar title=t_or("common.app-name", "IPTV Platform") items=items collapsed=collapsed.get() on_toggle=toggle/>
            <div class="app-main">
                <Toolbar title=title>
                    {(!role_text.is_empty()).then(|| view! { <Badge text=role_text.clone() tone=BadgeTone::Muted/> })}
                    <LanguageSwitcher action="/admin/locale" redirect_to="/admin"/>
                    <form method="post" action="/admin/logout" style="display:inline">
                        <button class="btn btn-plain" type="submit">{t_or("common.logout", "Sign out")}</button>
                    </form>
                </Toolbar>
                <div class="page">{children()}</div>
            </div>
        </div>
    }
}

#[component]
fn LoginPrompt() -> impl IntoView {
    view! {
        <EmptyState title=t_or("errors.e40102", "Admin sign-in failed") icon="⚿">
            <a class="btn btn-filled" href="/admin/login">{t_or("common.login", "Sign in")}</a>
        </EmptyState>
    }
}

#[component]
fn NotFoundPage() -> impl IntoView {
    view! {
        <div class="page">
            <EmptyState title=t_or("errors.e40401", "Not found") icon="?">
                <a class="btn btn-filled" href="/admin">{t_or("admin.dashboard", "Dashboard")}</a>
            </EmptyState>
        </div>
    }
}

#[component]
fn LoadingSkeleton() -> impl IntoView {
    view! { <p role="status">{t_or("common.loading", "Loading…")}</p> }
}

/// 操作结果提示（?saved=1 / ?error=…）。
#[component]
fn OpNotice() -> impl IntoView {
    let query = use_query_map();
    let saved = move || query.get().get("saved").is_some();
    let error = move || query.get().get("error").unwrap_or_default();
    view! {
        {move || saved().then(|| view! {
            <div class="notice notice-success" role="status">{t_or("common.saved", "Saved")}</div>
        })}
        {move || {
            let e = error();
            if e.is_empty() { view! { "" }.into_any() } else {
                view! { <div class="notice notice-error" role="alert">{e}</div> }.into_any()
            }
        }}
    }
}

fn page_of() -> u32 {
    use_query_map()
        .get()
        .get("page")
        .and_then(|s| s.parse().ok())
        .unwrap_or(1)
}

fn q_of() -> Option<String> {
    use_query_map().get().get("q")
}

/// 分页链接（上一页 / 下一页）。
#[component]
fn Pager(page: u32, total: u64, page_size: u32, base: String) -> impl IntoView {
    let total_pages = total.div_ceil(page_size as u64).max(1) as u32;
    view! {
        <div class="pagination">
            {if page > 1 {
                view! { <a class="btn btn-tinted" href=format!("{base}?page={}", page - 1)>{t_or("common.prev", "‹ Prev")}</a> }.into_any()
            } else {
                view! { "" }.into_any()
            }}
            <span>{t_args_or("common.pager", &[("page", page.to_string()), ("pages", total_pages.to_string()), ("total", total.to_string())], format!("{page} / {total_pages} · total {total}"))}</span>
            {if page < total_pages {
                view! { <a class="btn btn-tinted" href=format!("{base}?page={}", page + 1)>{t_or("common.next", "Next ›")}</a> }.into_any()
            } else {
                view! { "" }.into_any()
            }}
        </div>
    }
}

// ---------------------------------------------------------------------------
// §5.1 后台页面
// ---------------------------------------------------------------------------

/// `/admin/login` 管理员登录（Argon2 + 强制 TOTP，服务端校验）。
#[component]
fn LoginPage() -> impl IntoView {
    let query = use_query_map();
    let error = move || query.get().get("error").unwrap_or_default();
    view! {
        <div class="page">
            <div class="card glass" style="max-width:480px;margin:0 auto">
                <h1 style="font-size:var(--text-title1);margin-top:0">{t_or("admin.login-title", "Admin sign in")}</h1>
                {move || {
                    let e = error();
                    if e.is_empty() { view! { "" }.into_any() } else {
                        view! { <div class="notice notice-error" role="alert">{e}</div> }.into_any()
                    }
                }}
                <form method="post" action="/admin/login">
                    <TextField label=t_or("admin.username", "Username") name="username" required=true autocomplete="username"/>
                    <TextField label=t_or("admin.password", "Password") name="password" input_type="password" required=true autocomplete="current-password"/>
                    <TextField label=t_or("admin.totp", "TOTP code (if enabled)") name="totp_code" input_type="text" autocomplete="one-time-code"/>
                    <button class="btn btn-filled" type="submit" style="width:100%">{t_or("common.login", "Sign in")}</button>
                </form>
            </div>
        </div>
    }
}

/// `/admin` 仪表盘：在线用户、活跃会话、频道热度、同步状态、告警。
#[component]
fn DashboardPage() -> impl IntoView {
    let stats = Resource::new_blocking(|| (), |_| async move { admin_overview().await });
    view! {
        <AdminLayout title=t_or("admin.dashboard", "Dashboard") active="dashboard">
            <OpNotice/>
            <Suspense fallback=LoadingSkeleton>
                {move || stats.get().map(|res| match res {
                    Ok(Some(s)) => {
                        let sync = match s.last_sync_at {
                            Some(t) => format!("{} · {}", fmt_dt(t), s.last_sync_status.clone().unwrap_or_default()),
                            None => t_or("admin.never-synced", "Never synced"),
                        };
                        view! {
                            <div class="stat-grid">
                                <div class="stat-card"><div class="stat-value">{s.online_users}</div><div class="stat-label">{t_or("admin.online-users", "online users")}</div></div>
                                <div class="stat-card"><div class="stat-value">{s.active_sessions}</div><div class="stat-label">{t_or("admin.active-sessions", "active sessions")}</div></div>
                                <div class="stat-card"><div class="stat-value">{format!("{}/{}", s.available_channels, s.total_channels)}</div><div class="stat-label">{t_or("admin.available-total-channels", "available / total channels")}</div></div>
                                <div class="stat-card"><div class="stat-value">{format!("{}/{}", s.active_tokens, s.total_tokens)}</div><div class="stat-label">{t_or("admin.active-total-tokens", "active / total tokens")}</div></div>
                            </div>
                            <div class="card">
                                <h3 style="margin-top:0">{t_or("admin.sync-title", "Sync")}</h3>
                                <p>{sync}</p>
                                {if s.alerts.is_empty() {
                                    view! { <p style="color:var(--c-label-secondary)">{t_or("common.no-alerts", "No alerts")}</p> }.into_any()
                                } else {
                                    view! {
                                        <ul class="list">
                                            {s.alerts.into_iter().map(|a| view! { <li class="row"><span>{a}</span></li> }).collect_view()}
                                        </ul>
                                    }.into_any()
                                }}
                            </div>
                        }.into_any()
                    }
                    Ok(None) => view! { <LoginPrompt/> }.into_any(),
                    Err(e) => view! { <div class="notice notice-error" role="alert">{e.to_string()}</div> }.into_any(),
                })}
            </Suspense>
        </AdminLayout>
    }
}

/// `/admin/users` 用户列表与详情、启停、套餐调整、语言（§5.1）。
#[component]
fn UsersPage() -> impl IntoView {
    let page = page_of();
    let q = q_of();
    let q_res = q.clone();
    let users = Resource::new_blocking(
        move || (page, q_res.clone()),
        |(p, q)| async move { admin_users(p, q).await },
    );
    let can_write = admin_role().map(|r| r.can_write_users()).unwrap_or(false);
    view! {
        <AdminLayout title=t_or("admin.users", "Users") active="users">
            <OpNotice/>
            <form method="get" action="/admin/users" style="display:flex;gap:8px;margin-bottom:12px">
                <SearchField value=q.clone().unwrap_or_default() placeholder=t_or("common.search", "Search")/>
                <button class="btn btn-tinted" type="submit">{t_or("common.search", "Search")}</button>
            </form>
            {can_write.then(|| view! {
                <div class="card">
                    <h3 style="margin-top:0">{t_or("admin.create-user", "Create user")}</h3>
                    <form method="post" action="/admin/users">
                        <div class="form-grid">
                            <TextField label=t_or("portal.display-name", "Display name") name="display_name" required=true/>
                            <TextField label=t_or("common.email", "Email") name="email" input_type="email"/>
                            <TextField label=t_or("common.phone", "Phone") name="phone"/>
                            <TextField label=t_or("common.plan-id", "Plan ID") name="plan_id" input_type="number"/>
                            <div class="field">
                                <label for="locale-select">{t_or("common.language", "Language")}</label>
                                <select id="locale-select" name="locale" class="textfield">
                                    {iptv_i18n::I18n::supported_locales().into_iter().map(|l| {
                                        let sel = l == iptv_i18n::DEFAULT_LOCALE;
                                        view! { <option value=l selected=sel>{iptv_i18n::locale_display_name(l)}</option> }
                                    }).collect_view()}
                                </select>
                            </div>
                            <TextField label=t_or("common.remark", "Remark") name="remark"/>
                        </div>
                        <button class="btn btn-filled" type="submit">{t_or("common.save", "Save")}</button>
                    </form>
                </div>
            })}
            <Suspense fallback=LoadingSkeleton>
                {move || users.get().map(|res| match res {
                    Ok(Some(paged)) => {
                        let total = paged.total;
                        if paged.items.is_empty() {
                            view! { <EmptyState title=t_or("admin.no-users", "No users") icon="☺">{view! { <></> }}</EmptyState> }.into_any()
                        } else {
                            view! {
                                <div class="table-wrap">
                                    <table class="admin-table">
                                        <thead><tr><th>{t_or("common.id", "ID")}</th><th>{t_or("common.name", "Name")}</th><th>{t_or("common.email", "Email")}</th><th>{t_or("common.status", "Status")}</th><th>{t_or("common.locale", "Locale")}</th><th>{t_or("common.expires", "Expires")}</th><th>{t_or("common.actions", "Actions")}</th></tr></thead>
                                        <tbody>
                                            {paged.items.into_iter().map(|u| {
                                                let uid = u.id;
                                                let display_name = u.display_name.clone();
                                                let remark = u.remark.clone().unwrap_or_default();
                                                let plan_id = u.plan_id.map(|p| p.to_string()).unwrap_or_default();
                                                let expires_at = u.expires_at.map(|d| d.to_rfc3339()).unwrap_or_default();
                                                let status = u.status.clone();
                                                view! {
                                                    <tr>
                                                        <td>{u.id}</td>
                                                        <td>{u.display_name}</td>
                                                        <td>{u.email.unwrap_or_default()}</td>
                                                        <td>{status_label(&format!("{:?}", u.status))}</td>
                                                        <td>{iptv_i18n::locale_display_name(&u.locale)}</td>
                                                        <td>{u.expires_at.map(fmt_dt).unwrap_or_default()}</td>
                                                        <td>
                                                            {can_write.then(|| view! {
                                                                <div style="display:flex;gap:8px;flex-wrap:wrap;align-items:flex-start">
                                                                    <details>
                                                                        <summary>{t_or("admin.edit-user", "Edit user")}</summary>
                                                                        <form method="post" action=format!("/admin/users/{uid}/update")>
                                                                            <div class="form-grid">
                                                                                <div class="field"><label>{t_or("portal.display-name", "Display name")}</label><input class="textfield" name="display_name" value=display_name/></div>
                                                                                <div class="field"><label>{t_or("common.remark", "Remark")}</label><input class="textfield" name="remark" value=remark/></div>
                                                                                <div class="field"><label>{t_or("common.plan-id", "Plan ID")}</label><input class="textfield" name="plan_id" type="number" value=plan_id/></div>
                                                                                <div class="field"><label>{t_or("common.status", "Status")}</label><select name="status" class="textfield"><option value="active" selected=status == UserStatus::Active>{status_label("Active")}</option><option value="suspended" selected=status == UserStatus::Suspended>{status_label("Suspended")}</option><option value="expired" selected=status == UserStatus::Expired>{status_label("Expired")}</option></select></div>
                                                                                <div class="field"><label>{t_or("common.expires-at", "Expires at (RFC3339)")}</label><input class="textfield" name="expires_at" value=expires_at/></div>
                                                                            </div>
                                                                            <button class="btn" type="submit">{t_or("common.save", "Save")}</button>
                                                                        </form>
                                                                    </details>
                                                                    <form method="post" action=format!("/admin/users/{uid}/delete") style="display:inline">
                                                                        <button class="btn btn-destructive" type="submit">{t_or("common.delete", "Delete")}</button>
                                                                    </form>
                                                                </div>
                                                            })}
                                                        </td>
                                                    </tr>
                                                }
                                            }).collect_view()}
                                        </tbody>
                                    </table>
                                </div>
                                <Pager page=page total=total page_size=20 base="/admin/users".to_string()/>
                            }.into_any()
                        }
                    }
                    Ok(None) => view! { <LoginPrompt/> }.into_any(),
                    Err(e) => view! { <div class="notice notice-error" role="alert">{e.to_string()}</div> }.into_any(),
                })}
            </Suspense>
        </AdminLayout>
    }
}

/// `/admin/tokens` Token 管理（§5.1/§5.2：创建、禁用、并发数、有效期、
/// 频道授权、生成登录链接/二维码）。
#[component]
fn TokensPage() -> impl IntoView {
    let page = page_of();
    let q = q_of();
    let q_res = q.clone();
    let tokens = Resource::new_blocking(
        move || (page, q_res.clone()),
        |(p, q)| async move { admin_tokens(p, q).await },
    );
    let can_write = admin_role().map(|r| r.can_write_tokens()).unwrap_or(false);
    view! {
        <AdminLayout title=t_or("admin.tokens", "Tokens") active="tokens">
            <OpNotice/>
            <form method="get" action="/admin/tokens" style="display:flex;gap:8px;margin-bottom:12px">
                <SearchField value=q.clone().unwrap_or_default() placeholder=t_or("common.search", "Search")/>
                <button class="btn btn-tinted" type="submit">{t_or("common.search", "Search")}</button>
            </form>
            {can_write.then(|| view! {
                <div class="card">
                    <h3 style="margin-top:0">{t_or("admin.create-token", "Create Token")}</h3>
                    <p style="font-size:var(--text-footnote);color:var(--c-label-secondary)">{t_or("admin.plaintext-token-once", "The plaintext Token is shown only once after creation.")}</p>
                    <form method="post" action="/admin/tokens">
                        <div class="form-grid">
                            <TextField label=t_or("common.user-id", "User ID") name="user_id" input_type="number" required=true/>
                            <TextField label=t_or("common.name", "Name") name="name" required=true/>
                            <TextField label=t_or("common.device-type", "Device type") name="device_type" value="android_tv"/>
                            <TextField label=t_or("common.max-concurrent-sessions", "Max concurrent sessions") name="max_concurrent_sessions" input_type="number" value="2"/>
                            <TextField label=t_or("common.max-bound-devices", "Max bound devices") name="max_bound_devices" input_type="number" value="3"/>
                            <TextField label=t_or("common.expires-at", "Expires at (RFC3339)") name="expires_at"/>
                            <div class="field">
                                <label for="locale-select">{t_or("common.language", "Language")}</label>
                                <select id="locale-select" name="locale" class="textfield">
                                    {iptv_i18n::I18n::supported_locales().into_iter().map(|l| {
                                        let sel = l == iptv_i18n::DEFAULT_LOCALE;
                                        view! { <option value=l selected=sel>{iptv_i18n::locale_display_name(l)}</option> }
                                    }).collect_view()}
                                </select>
                            </div>
                            <TextField label=t_or("common.allowed-groups", "Allowed groups (comma separated)") name="allowed_groups"/>
                            <TextField label=t_or("common.max-quality", "Max quality") name="max_quality" value="1080p"/>
                        </div>
                        <div style="margin-bottom:12px">
                            <Toggle label=t_or("common.catchup", "Catchup") name="catchup" checked=true/>
                            <Toggle label=t_or("common.recording", "Recording") name="recording"/>
                        </div>
                        <button class="btn btn-filled" type="submit">{t_or("common.save", "Save")}</button>
                    </form>
                </div>
            })}
            <Suspense fallback=LoadingSkeleton>
                {move || tokens.get().map(|res| match res {
                    Ok(Some(paged)) => {
                        let total = paged.total;
                        if paged.items.is_empty() {
                            view! { <EmptyState title=t_or("admin.no-tokens", "No tokens") icon="⚿">{view! { <></> }}</EmptyState> }.into_any()
                        } else {
                            view! {
                                <div class="table-wrap">
                                    <table class="admin-table">
                                        <thead><tr><th>{t_or("common.id", "ID")}</th><th>{t_or("common.prefix", "Prefix")}</th><th>{t_or("common.name", "Name")}</th><th>{t_or("common.device", "Device")}</th><th>{t_or("common.concurrent", "Concurrent")}</th><th>{t_or("common.devices", "Devices")}</th><th>{t_or("common.status", "Status")}</th><th>{t_or("common.expires", "Expires")}</th><th>{t_or("common.actions", "Actions")}</th></tr></thead>
                                        <tbody>
                                            {paged.items.into_iter().map(|t| {
                                                let tid = t.id;
                                                let tok_name = t.name.clone();
                                                let tok_groups = t.allowed_groups.join(", ");
                                                let tok_expires = t.expires_at.map(|d| d.to_rfc3339()).unwrap_or_default();
                                                let tok_status = t.status.clone();
                                                view! {
                                                    <tr>
                                                        <td>{t.id}</td>
                                                        <td class="mono">{t.token_prefix}</td>
                                                        <td>{t.name}</td>
                                                        <td>{t.device_type.unwrap_or_default()}</td>
                                                        <td>{t.max_concurrent_sessions}</td>
                                                        <td>{t.max_bound_devices}</td>
                                                        <td>{status_label(&format!("{:?}", t.status))}</td>
                                                        <td>{t.expires_at.map(fmt_dt).unwrap_or_default()}</td>
                                                        <td>
                                                            <form method="post" action=format!("/admin/tokens/{tid}/ticket") style="display:inline">
                                                                <button class="btn btn-tinted" type="submit">{t_or("admin.login-link", "login link")}</button>
                                                            </form>
                                                            " "
                                                            <form method="post" action=format!("/admin/tokens/{tid}/revoke") style="display:inline">
                                                                <button class="btn btn-destructive" type="submit">{t_or("admin.revoke-sessions", "revoke sessions")}</button>
                                                            </form>
                                                            {can_write.then(|| view! {
                                                                <div style="display:flex;gap:8px;flex-wrap:wrap;align-items:flex-start">
                                                                    <details>
                                                                        <summary>{t_or("admin.edit-token", "Edit Token")}</summary>
                                                                        <form method="post" action=format!("/admin/tokens/{tid}/update")>
                                                                            <div class="form-grid">
                                                                                <div class="field"><label>{t_or("common.name", "Name")}</label><input class="textfield" name="name" value=tok_name/></div>
                                                                                <div class="field"><label>{t_or("common.max-concurrent-sessions", "Max concurrent sessions")}</label><input class="textfield" name="max_concurrent_sessions" type="number" value=t.max_concurrent_sessions.to_string()/></div>
                                                                                <div class="field"><label>{t_or("common.max-bound-devices", "Max bound devices")}</label><input class="textfield" name="max_bound_devices" type="number" value=t.max_bound_devices.to_string()/></div>
                                                                                <div class="field"><label>{t_or("common.status", "Status")}</label><select name="status" class="textfield"><option value="active" selected=tok_status == TokenStatus::Active>{status_label("Active")}</option><option value="disabled" selected=tok_status == TokenStatus::Disabled>{status_label("Disabled")}</option><option value="expired" selected=tok_status == TokenStatus::Expired>{status_label("Expired")}</option></select></div>
                                                                                <div class="field"><label>{t_or("common.expires-at", "Expires at (RFC3339)")}</label><input class="textfield" name="expires_at" value=tok_expires/></div>
                                                                                <div class="field"><label>{t_or("common.allowed-groups", "Allowed groups (comma separated)")}</label><input class="textfield" name="allowed_groups" value=tok_groups/></div>
                                                                            </div>
                                                                            <button class="btn" type="submit">{t_or("common.save", "Save")}</button>
                                                                        </form>
                                                                    </details>
                                                                    <form method="post" action=format!("/admin/tokens/{tid}/delete") style="display:inline">
                                                                        <button class="btn btn-destructive" type="submit">{t_or("common.delete", "Delete")}</button>
                                                                    </form>
                                                                </div>
                                                            })}
                                                        </td>
                                                    </tr>
                                                }
                                            }).collect_view()}
                                        </tbody>
                                    </table>
                                </div>
                                <Pager page=page total=total page_size=20 base="/admin/tokens".to_string()/>
                            }.into_any()
                        }
                    }
                    Ok(None) => view! { <LoginPrompt/> }.into_any(),
                    Err(e) => view! { <div class="notice notice-error" role="alert">{e.to_string()}</div> }.into_any(),
                })}
            </Suspense>
        </AdminLayout>
    }
}

/// `/admin/channels` 频道列表、编辑、排序、批量操作、可用性、备用源（§5.1）。
#[component]
fn ChannelsPage() -> impl IntoView {
    let page = page_of();
    let q = q_of();
    let q_res = q.clone();
    let channels = Resource::new_blocking(
        move || (page, q_res.clone()),
        |(p, q)| async move { admin_channels(p, q).await },
    );
    let can_write = admin_role()
        .map(|r| r.can_write_channels())
        .unwrap_or(false);
    view! {
        <AdminLayout title=t_or("admin.channels", "Channels") active="channels">
            <OpNotice/>
            <form method="get" action="/admin/channels" style="display:flex;gap:8px;margin-bottom:12px">
                <SearchField value=q.clone().unwrap_or_default() placeholder=t_or("common.search", "Search")/>
                <button class="btn btn-tinted" type="submit">{t_or("common.search", "Search")}</button>
            </form>
            {can_write.then(|| view! {
                <div class="card">
                    <h3 style="margin-top:0">{t_or("admin.add-channel", "Add channel")}</h3>
                    <form method="post" action="/admin/channels">
                        <div class="form-grid">
                            <TextField label=t_or("common.tvg-id", "tvg-id") name="tvg_id"/>
                            <TextField label=t_or("common.name", "Name") name="name" required=true/>
                            <TextField label=t_or("common.group", "Group") name="group_title"/>
                            <TextField label=t_or("common.logo-url", "Logo URL") name="logo_url"/>
                            <TextField label=t_or("common.play-url", "Play URL") name="play_url" required=true/>
                            <TextField label=t_or("common.backup-url", "Backup URL") name="backup_url"/>
                            <TextField label=t_or("common.protocol", "Protocol") name="protocol" value="hls"/>
                            <TextField label=t_or("common.sort-weight", "Sort weight") name="sort_weight" input_type="number" value="0"/>
                        </div>
                        <button class="btn btn-filled" type="submit">{t_or("common.save", "Save")}</button>
                    </form>
                </div>
            })}
            <Suspense fallback=LoadingSkeleton>
                {move || channels.get().map(|res| match res {
                    Ok(Some(paged)) => {
                        let total = paged.total;
                        if paged.items.is_empty() {
                            view! { <EmptyState title=t_or("admin.no-channels", "No channels") icon="▤">{view! { <></> }}</EmptyState> }.into_any()
                        } else {
                            view! {
                                <div class="table-wrap">
                                    <table class="admin-table">
                                        <thead><tr><th>{t_or("common.id", "ID")}</th><th>{t_or("common.tvg-id", "tvg-id")}</th><th>{t_or("common.name", "Name")}</th><th>{t_or("common.group", "Group")}</th><th>{t_or("common.status", "Status")}</th><th>{t_or("common.available", "Available")}</th><th>{t_or("common.actions", "Actions")}</th></tr></thead>
                                        <tbody>
                                            {paged.items.into_iter().map(|c| {
                                                let cid = c.id;
                                                view! {
                                                    <tr>
                                                        <td>{c.id}</td>
                                                        <td class="mono">{c.tvg_id.unwrap_or_default()}</td>
                                                        <td>{c.name}</td>
                                                        <td>{c.group_title.unwrap_or_default()}</td>
                                                        <td>{status_label(&format!("{:?}", c.status))}</td>
                                                        <td>{c.last_available.map(|b| if b { "●".to_string() } else { "○".to_string() }).unwrap_or_default()}</td>
                                                        <td>
                                                            {can_write.then(|| view! {
                                                                <form method="post" action=format!("/admin/channels/{cid}/delete") style="display:inline">
                                                                    <button class="btn btn-destructive" type="submit">{t_or("common.delete", "Delete")}</button>
                                                                </form>
                                                            })}
                                                        </td>
                                                    </tr>
                                                }
                                            }).collect_view()}
                                        </tbody>
                                    </table>
                                </div>
                                <Pager page=page total=total page_size=20 base="/admin/channels".to_string()/>
                            }.into_any()
                        }
                    }
                    Ok(None) => view! { <LoginPrompt/> }.into_any(),
                    Err(e) => view! { <div class="notice notice-error" role="alert">{e.to_string()}</div> }.into_any(),
                })}
            </Suspense>
        </AdminLayout>
    }
}

/// `/admin/sources` 订阅源：上传 M3U、远程 URL、同步周期、立即同步、日志、差异（§5.1/§7）。
#[component]
fn SourcesPage() -> impl IntoView {
    let query = use_query_map();
    let source_id = move || {
        query
            .get()
            .get("source")
            .and_then(|s| s.parse::<i64>().ok())
    };
    let sources = Resource::new_blocking(|| (), |_| async move { admin_sources().await });
    let diff = Resource::new_blocking(source_id, |id| async move {
        match id {
            Some(id) => admin_source_diff(id).await,
            None => Ok(None),
        }
    });
    let logs = Resource::new_blocking(
        move || {
            query
                .get()
                .get("source")
                .and_then(|s| s.parse::<i64>().ok())
        },
        |id| async move {
            match id {
                Some(id) => admin_source_logs(id).await,
                None => Ok(None),
            }
        },
    );
    let can_write = admin_role().map(|r| r.can_write_sources()).unwrap_or(false);
    view! {
        <AdminLayout title=t_or("admin.sources", "Sources") active="sources">
            <OpNotice/>
            {can_write.then(|| view! {
                <div class="card">
                    <h3 style="margin-top:0">{t_or("admin.add-source", "Add subscription source")}</h3>
                    <form method="post" action="/admin/sources">
                        <div class="form-grid">
                            <TextField label=t_or("common.name", "Name") name="name" required=true/>
                            <TextField label=t_or("common.url", "URL") name="url" required=true/>
                            <TextField label=t_or("common.sync-interval", "Sync interval (sec, min 300)") name="sync_interval_sec" input_type="number" value="3600"/>
                            <TextField label=t_or("common.epg-url", "EPG URL") name="epg_url"/>
                        </div>
                        <button class="btn btn-filled" type="submit">{t_or("common.save", "Save")}</button>
                    </form>
                    <h3>{t_or("admin.upload-m3u-title", "Upload local M3U (.m3u / .m3u8 / .txt, no size limit)")}</h3>
                    <form method="post" action="/admin/sources/upload" enctype="multipart/form-data">
                        <div class="field">
                            <label for="m3u-name">{t_or("common.source-name", "Source name")}</label>
                            <input class="textfield" id="m3u-name" name="name" required/>
                        </div>
                        <div class="field">
                            <label for="m3u-file">{t_or("common.m3u-file", "M3U file")}</label>
                            <input class="textfield" id="m3u-file" type="file" name="file" accept=".m3u,.m3u8,.txt" required/>
                        </div>
                        <button class="btn btn-filled" type="submit">{t_or("admin.upload-parse", "Upload & parse")}</button>
                    </form>
                </div>
            })}
            <Suspense fallback=LoadingSkeleton>
                {move || sources.get().map(|res| match res {
                    Ok(Some(list)) => {
                        if list.is_empty() {
                            view! { <EmptyState title=t_or("admin.no-sources", "No sources") icon="⇩">{view! { <></> }}</EmptyState> }.into_any()
                        } else {
                            view! {
                                <div class="table-wrap">
                                    <table class="admin-table">
                                        <thead><tr><th>{t_or("common.id", "ID")}</th><th>{t_or("common.name", "Name")}</th><th>{t_or("common.url", "URL")}</th><th>{t_or("common.interval", "Interval")}</th><th>{t_or("common.last-sync", "Last sync")}</th><th>{t_or("common.status", "Status")}</th><th>{t_or("common.actions", "Actions")}</th></tr></thead>
                                        <tbody>
                                            {list.into_iter().map(|s| {
                                                let sid = s.id;
                                                let src_name = s.name.clone();
                                                let src_url = s.url.clone().unwrap_or_default();
                                                let src_epg_url = s.epg_url.clone().unwrap_or_default();
                                                let src_on_missing = s.on_missing.clone();
                                                let src_enabled = s.enabled;
                                                view! {
                                                    <tr>
                                                        <td>{s.id}</td>
                                                        <td>{s.name}</td>
                                                        <td class="mono">{s.url.unwrap_or_default()}</td>
                                                        <td>{s.sync_interval_sec}</td>
                                                        <td>{s.last_sync_at.map(fmt_dt).unwrap_or_default()}</td>
                                                        <td>{s.last_status.unwrap_or_default()}</td>
                                                        <td>
                                                            <a class="btn btn-tinted" href=format!("/admin/sources?source={sid}")>{t_or("admin.diff-logs", "diff / logs")}</a>
                                                            " "
                                                            {can_write.then(|| view! {
                                                                <form method="post" action=format!("/admin/sources/{sid}/sync") style="display:inline">
                                                                    <button class="btn btn-filled" type="submit">{t_or("admin.sync-now", "Sync now")}</button>
                                                                </form>
                                                            })}
                                                            {can_write.then(|| view! {
                                                                <div style="display:flex;gap:8px;flex-wrap:wrap;align-items:flex-start">
                                                                    <details>
                                                                        <summary>{t_or("admin.edit-source", "Edit source")}</summary>
                                                                        <form method="post" action=format!("/admin/sources/{sid}/update")>
                                                                            <div class="form-grid">
                                                                                <div class="field"><label>{t_or("common.name", "Name")}</label><input class="textfield" name="name" value=src_name/></div>
                                                                                <div class="field"><label>{t_or("common.url", "URL")}</label><input class="textfield" name="url" value=src_url/></div>
                                                                                <div class="field"><label>{t_or("common.sync-interval", "Sync interval (sec, min 300)")}</label><input class="textfield" name="sync_interval_sec" type="number" value=s.sync_interval_sec.to_string()/></div>
                                                                                <div class="field"><label>{t_or("common.epg-url", "EPG URL")}</label><input class="textfield" name="epg_url" value=src_epg_url/></div>
                                                                                <div class="field"><label>{t_or("common.on-missing", "On missing")}</label><select name="on_missing" class="textfield"><option value="disable" selected=format!("{:?}", src_on_missing) == "Disable">{t_or("common.disable", "Disable")}</option><option value="delete" selected=format!("{:?}", src_on_missing) == "Delete">{t_or("common.delete", "Delete")}</option></select></div>
                                                                            </div>
                                                                            <Toggle label=t_or("common.enabled", "Enabled") name="enabled" checked=src_enabled/>
                                                                            <button class="btn" type="submit">{t_or("common.save", "Save")}</button>
                                                                        </form>
                                                                    </details>
                                                                    <form method="post" action=format!("/admin/sources/{sid}/delete") style="display:inline">
                                                                        <button class="btn btn-destructive" type="submit">{t_or("common.delete", "Delete")}</button>
                                                                    </form>
                                                                </div>
                                                            })}
                                                        </td>
                                                    </tr>
                                                }
                                            }).collect_view()}
                                        </tbody>
                                    </table>
                                </div>
                            }.into_any()
                        }
                    }
                    Ok(None) => view! { <LoginPrompt/> }.into_any(),
                    Err(e) => view! { <div class="notice notice-error" role="alert">{e.to_string()}</div> }.into_any(),
                })}
            </Suspense>
            <Suspense fallback=LoadingSkeleton>
                {move || diff.get().map(|res| match res {
                    Ok(Some(d)) => view! {
                        <div class="card">
                            <h3 style="margin-top:0">{t_or("admin.sync-diff", "Sync diff")}</h3>
                            <p>{format!("added {} · metadata changed {} · url changed {} · missing {}", d.added.len(), d.metadata_changed.len(), d.url_changed.len(), d.missing.len())}</p>
                            <ul class="list">
                                {d.added.into_iter().map(|c| view! { <li class="row"><span>{format!("+ {}", c.name)}</span></li> }).collect_view()}
                                {d.missing.into_iter().map(|c| view! { <li class="row"><span>{format!("− {}", c.name)}</span></li> }).collect_view()}
                            </ul>
                        </div>
                    }.into_any(),
                    Ok(None) => view! { "" }.into_any(),
                    Err(e) => view! { <div class="notice notice-error" role="alert">{e.to_string()}</div> }.into_any(),
                })}
            </Suspense>
            <Suspense fallback=LoadingSkeleton>
                {move || logs.get().map(|res| match res {
                    Ok(Some(paged)) => view! {
                        <div class="card">
                            <h3 style="margin-top:0">{t_or("admin.sync-logs", "Sync logs")}</h3>
                            <div class="table-wrap">
                                <table class="admin-table">
                                    <thead><tr><th>{t_or("common.started", "Started")}</th><th>{t_or("common.status", "Status")}</th><th>{t_or("common.added", "Added")}</th><th>{t_or("common.updated", "Updated")}</th><th>{t_or("common.removed", "Removed")}</th><th>{t_or("common.error", "Error")}</th></tr></thead>
                                    <tbody>
                                        {paged.items.into_iter().map(|l| view! {
                                            <tr>
                                                <td>{fmt_dt(l.started_at)}</td>
                                                <td>{l.status}</td>
                                                <td>{l.added}</td>
                                                <td>{l.updated}</td>
                                                <td>{l.removed}</td>
                                                <td>{l.error.unwrap_or_default()}</td>
                                            </tr>
                                        }).collect_view()}
                                    </tbody>
                                </table>
                            </div>
                        </div>
                    }.into_any(),
                    Ok(None) => view! { "" }.into_any(),
                    Err(e) => view! { <div class="notice notice-error" role="alert">{e.to_string()}</div> }.into_any(),
                })}
            </Suspense>
        </AdminLayout>
    }
}

/// `/admin/groups` 频道分组（§5.1）。
#[component]
fn GroupsPage() -> impl IntoView {
    let groups = Resource::new_blocking(|| (), |_| async move { admin_groups().await });
    let can_write = admin_role()
        .map(|r| r.can_write_channels())
        .unwrap_or(false);
    view! {
        <AdminLayout title=t_or("admin.groups", "Groups") active="groups">
            <OpNotice/>
            {can_write.then(|| view! {
                <div class="card">
                    <h3 style="margin-top:0">{t_or("admin.add-group", "Add group")}</h3>
                    <form method="post" action="/admin/groups">
                        <div class="form-grid">
                            <TextField label=t_or("common.name", "Name") name="name" required=true/>
                            <TextField label=t_or("common.sort-weight", "Sort weight") name="sort_weight" input_type="number" value="0"/>
                            <TextField label=t_or("common.icon", "Icon") name="icon"/>
                        </div>
                        <button class="btn btn-filled" type="submit">{t_or("common.save", "Save")}</button>
                    </form>
                </div>
            })}
            <Suspense fallback=LoadingSkeleton>
                {move || groups.get().map(|res| match res {
                    Ok(Some(list)) => {
                        if list.is_empty() {
                            view! { <EmptyState title=t_or("admin.no-groups", "No groups") icon="▧">{view! { <></> }}</EmptyState> }.into_any()
                        } else {
                            view! {
                                <div class="table-wrap">
                                    <table class="admin-table">
                                        <thead><tr><th>{t_or("common.id", "ID")}</th><th>{t_or("common.name", "Name")}</th><th>{t_or("common.sort", "Sort")}</th><th>{t_or("common.icon", "Icon")}</th><th>{t_or("common.default", "Default")}</th><th>{t_or("common.actions", "Actions")}</th></tr></thead>
                                        <tbody>
                                            {list.into_iter().map(|g| {
                                                let gid = g.id;
                                                let grp_name = g.name.clone();
                                                let grp_icon = g.icon.clone().unwrap_or_default();
                                                let grp_default = g.is_default;
                                                view! {
                                                    <tr>
                                                        <td>{g.id}</td>
                                                        <td>{g.name}</td>
                                                        <td>{g.sort_weight}</td>
                                                        <td>{g.icon.unwrap_or_default()}</td>
                                                        <td>{if g.is_default { "✓" } else { "" }}</td>
                                                        <td>
                                                            {can_write.then(|| view! {
                                                                <div style="display:flex;gap:8px;flex-wrap:wrap;align-items:flex-start">
                                                                    <details>
                                                                        <summary>{t_or("admin.edit-group", "Edit group")}</summary>
                                                                        <form method="post" action=format!("/admin/groups/{gid}/update")>
                                                                            <div class="form-grid">
                                                                                <div class="field"><label>{t_or("common.name", "Name")}</label><input class="textfield" name="name" value=grp_name/></div>
                                                                                <div class="field"><label>{t_or("common.sort-weight", "Sort weight")}</label><input class="textfield" name="sort_weight" type="number" value=g.sort_weight.to_string()/></div>
                                                                                <div class="field"><label>{t_or("common.icon", "Icon")}</label><input class="textfield" name="icon" value=grp_icon/></div>
                                                                            </div>
                                                                            <Toggle label=t_or("common.default", "Default") name="is_default" checked=grp_default/>
                                                                            <button class="btn" type="submit">{t_or("common.save", "Save")}</button>
                                                                        </form>
                                                                    </details>
                                                                    <form method="post" action=format!("/admin/groups/{gid}/delete") style="display:inline">
                                                                        <button class="btn btn-destructive" type="submit">{t_or("common.delete", "Delete")}</button>
                                                                    </form>
                                                                </div>
                                                            })}
                                                        </td>
                                                    </tr>
                                                }
                                            }).collect_view()}
                                        </tbody>
                                    </table>
                                </div>
                            }.into_any()
                        }
                    }
                    Ok(None) => view! { <LoginPrompt/> }.into_any(),
                    Err(e) => view! { <div class="notice notice-error" role="alert">{e.to_string()}</div> }.into_any(),
                })}
            </Suspense>
        </AdminLayout>
    }
}

/// `/admin/epg` EPG 源、频道匹配、节目预览、失败清单（§5.1）。
#[component]
fn EpgPage() -> impl IntoView {
    let query = use_query_map();
    let channel_id = move || {
        query
            .get()
            .get("channel_id")
            .and_then(|s| s.parse::<i64>().ok())
    };
    let sources = Resource::new_blocking(|| (), |_| async move { admin_epg_sources().await });
    let programs = Resource::new_blocking(channel_id, |id| async move {
        match id {
            Some(id) => admin_epg_programs(id).await,
            None => Ok(None),
        }
    });
    let can_write = admin_role().map(|r| r.can_write_sources()).unwrap_or(false);
    view! {
        <AdminLayout title=t_or("admin.epg", "EPG") active="epg">
            <OpNotice/>
            {can_write.then(|| view! {
                <div class="card">
                    <h3 style="margin-top:0">{t_or("admin.add-epg-source", "Add EPG source")}</h3>
                    <form method="post" action="/admin/epg/sources">
                        <div class="form-grid">
                            <TextField label=t_or("common.name", "Name") name="name" required=true/>
                            <TextField label=t_or("common.url", "URL") name="url" required=true/>
                        </div>
                        <button class="btn btn-filled" type="submit">{t_or("common.save", "Save")}</button>
                    </form>
                </div>
            })}
            <div class="card">
                <h3 style="margin-top:0">{t_or("admin.program-preview", "Program preview")}</h3>
                <form method="get" action="/admin/epg" style="display:flex;gap:8px">
                    <input class="textfield" type="number" name="channel_id" placeholder=t_or("common.channel-id", "Channel ID") style="flex:1"/>
                    <button class="btn btn-tinted" type="submit">{t_or("common.search", "Search")}</button>
                </form>
            </div>
            <Suspense fallback=LoadingSkeleton>
                {move || programs.get().map(|res| match res {
                    Ok(Some(list)) => {
                        if list.is_empty() {
                            view! { <EmptyState title=t_or("admin.no-programs", "No programs") icon="▦">{view! { <></> }}</EmptyState> }.into_any()
                        } else {
                            view! {
                                <InsetGroupedList title=t_or("admin.epg", "EPG")>
                                    {list.into_iter().take(30).map(|p| view! {
                                        <Row title=p.title subtitle=format!("{} – {}", fmt_dt(p.start_at), fmt_dt(p.stop_at)) value=p.category.unwrap_or_default()/>
                                    }).collect_view()}
                                </InsetGroupedList>
                            }.into_any()
                        }
                    }
                    Ok(None) => view! { "" }.into_any(),
                    Err(e) => view! { <div class="notice notice-error" role="alert">{e.to_string()}</div> }.into_any(),
                })}
            </Suspense>
            <Suspense fallback=LoadingSkeleton>
                {move || sources.get().map(|res| match res {
                    Ok(Some(list)) => {
                        if list.is_empty() {
                            view! { <EmptyState title=t_or("admin.no-epg-sources", "No EPG sources") icon="▦">{view! { <></> }}</EmptyState> }.into_any()
                        } else {
                            view! {
                                <div class="table-wrap">
                                    <table class="admin-table">
                                        <thead><tr><th>{t_or("common.id", "ID")}</th><th>{t_or("common.name", "Name")}</th><th>{t_or("common.url", "URL")}</th><th>{t_or("common.last-sync", "Last sync")}</th><th>{t_or("common.status", "Status")}</th><th>{t_or("common.actions", "Actions")}</th></tr></thead>
                                        <tbody>
                                            {list.into_iter().map(|s| {
                                                let eid = s.id;
                                                let epg_name = s.name.clone();
                                                let epg_url = s.url.clone();
                                                view! {
                                                    <tr>
                                                        <td>{s.id}</td>
                                                        <td>{s.name}</td>
                                                        <td class="mono">{s.url}</td>
                                                        <td>{s.last_sync_at.map(fmt_dt).unwrap_or_default()}</td>
                                                        <td>{s.last_status.unwrap_or_default()}</td>
                                                        <td>
                                                            {can_write.then(|| view! {
                                                                <div style="display:flex;gap:8px;flex-wrap:wrap;align-items:flex-start">
                                                                    <details>
                                                                        <summary>{t_or("admin.edit-epg-source", "Edit EPG source")}</summary>
                                                                        <form method="post" action=format!("/admin/epg/sources/{eid}/update")>
                                                                            <div class="form-grid">
                                                                                <div class="field"><label>{t_or("common.name", "Name")}</label><input class="textfield" name="name" value=epg_name/></div>
                                                                                <div class="field"><label>{t_or("common.url", "URL")}</label><input class="textfield" name="url" value=epg_url/></div>
                                                                            </div>
                                                                            <button class="btn" type="submit">{t_or("common.save", "Save")}</button>
                                                                        </form>
                                                                    </details>
                                                                    <form method="post" action=format!("/admin/epg/sources/{eid}/delete") style="display:inline">
                                                                        <button class="btn btn-destructive" type="submit">{t_or("common.delete", "Delete")}</button>
                                                                    </form>
                                                                </div>
                                                            })}
                                                        </td>
                                                    </tr>
                                                }
                                            }).collect_view()}
                                        </tbody>
                                    </table>
                                </div>
                            }.into_any()
                        }
                    }
                    Ok(None) => view! { <LoginPrompt/> }.into_any(),
                    Err(e) => view! { <div class="notice notice-error" role="alert">{e.to_string()}</div> }.into_any(),
                })}
            </Suspense>
        </AdminLayout>
    }
}

/// `/admin/sessions` 实时会话看板、强制下线、按 Token/IP 排查（§5.1）。
#[component]
fn SessionsPage() -> impl IntoView {
    let page = page_of();
    let sessions = Resource::new_blocking(move || page, |p| async move { admin_sessions(p).await });
    let can_kick = admin_role().map(|r| r.can_kick_session()).unwrap_or(false);
    view! {
        <AdminLayout title=t_or("admin.sessions", "Sessions") active="sessions">
            <OpNotice/>
            <Suspense fallback=LoadingSkeleton>
                {move || sessions.get().map(|res| match res {
                    Ok(Some(paged)) => {
                        let total = paged.total;
                        if paged.items.is_empty() {
                            view! { <EmptyState title=t_or("admin.no-sessions", "No active sessions") icon="◉">{view! { <></> }}</EmptyState> }.into_any()
                        } else {
                            view! {
                                <div class="table-wrap">
                                    <table class="admin-table">
                                        <thead><tr><th>{t_or("common.id", "ID")}</th><th>{t_or("common.session", "Session")}</th><th>{t_or("common.token", "Token")}</th><th>{t_or("common.channel", "Channel")}</th><th>{t_or("common.device", "Device")}</th><th>{t_or("common.started", "Started")}</th><th>{t_or("common.last-heartbeat", "Last heartbeat")}</th><th>{t_or("common.status", "Status")}</th><th>{t_or("common.actions", "Actions")}</th></tr></thead>
                                        <tbody>
                                            {paged.items.into_iter().map(|s| {
                                                let sid = s.id;
                                                view! {
                                                    <tr>
                                                        <td>{s.id}</td>
                                                        <td class="mono">{s.session_uid}</td>
                                                        <td>{s.token_id}</td>
                                                        <td>{s.channel_id}</td>
                                                        <td>{s.device_id}</td>
                                                        <td>{fmt_dt(s.started_at)}</td>
                                                        <td>{fmt_dt(s.last_heartbeat_at)}</td>
                                                        <td><StatusDot label=status_label(&s.status) kind=if s.status == "active" { StatusKind::Online } else { StatusKind::Offline }/></td>
                                                        <td>
                                                            {can_kick.then(|| view! {
                                                                <form method="post" action=format!("/admin/sessions/{sid}/kick") style="display:inline">
                                                                    <button class="btn btn-destructive" type="submit">{t_or("common.kick", "Kick")}</button>
                                                                </form>
                                                            })}
                                                        </td>
                                                    </tr>
                                                }
                                            }).collect_view()}
                                        </tbody>
                                    </table>
                                </div>
                                <Pager page=page total=total page_size=20 base="/admin/sessions".to_string()/>
                            }.into_any()
                        }
                    }
                    Ok(None) => view! { <LoginPrompt/> }.into_any(),
                    Err(e) => view! { <div class="notice notice-error" role="alert">{e.to_string()}</div> }.into_any(),
                })}
            </Suspense>
        </AdminLayout>
    }
}

/// `/admin/plans` 套餐与授权包（§5.1）。
#[component]
fn PlansPage() -> impl IntoView {
    let plans = Resource::new_blocking(|| (), |_| async move { admin_plans().await });
    let can_write = admin_role()
        .map(|r| r.can_write_settings())
        .unwrap_or(false);
    view! {
        <AdminLayout title=t_or("admin.plans", "Plans") active="plans">
            <OpNotice/>
            {can_write.then(|| view! {
                <div class="card">
                    <h3 style="margin-top:0">{t_or("admin.add-plan", "Add plan")}</h3>
                    <form method="post" action="/admin/plans">
                        <div class="form-grid">
                            <TextField label=t_or("common.name", "Name") name="name" required=true/>
                            <TextField label=t_or("common.max-concurrent-sessions", "Max concurrent sessions") name="max_concurrent_sessions" input_type="number" value="2"/>
                            <TextField label=t_or("common.max-bound-devices", "Max bound devices") name="max_bound_devices" input_type="number" value="3"/>
                            <TextField label=t_or("common.channel-groups", "Channel groups (comma separated)") name="channel_groups"/>
                            <TextField label=t_or("common.duration-days", "Duration (days)") name="duration_days" input_type="number" value="365"/>
                            <TextField label=t_or("common.price-cents", "Price (cents)") name="price_cents" input_type="number" value="0"/>
                            <TextField label=t_or("common.currency", "Currency") name="currency" value="USD"/>
                        </div>
                        <button class="btn btn-filled" type="submit">{t_or("common.save", "Save")}</button>
                    </form>
                </div>
            })}
            <Suspense fallback=LoadingSkeleton>
                {move || plans.get().map(|res| match res {
                    Ok(Some(list)) => {
                        if list.is_empty() {
                            view! { <EmptyState title=t_or("admin.no-plans", "No plans") icon="◆">{view! { <></> }}</EmptyState> }.into_any()
                        } else {
                            view! {
                                <div class="table-wrap">
                                    <table class="admin-table">
                                        <thead><tr><th>{t_or("common.id", "ID")}</th><th>{t_or("common.name", "Name")}</th><th>{t_or("common.concurrent", "Concurrent")}</th><th>{t_or("common.devices", "Devices")}</th><th>{t_or("common.groups", "Groups")}</th><th>{t_or("common.duration", "Duration")}</th><th>{t_or("common.price", "Price")}</th><th>{t_or("common.actions", "Actions")}</th></tr></thead>
                                        <tbody>
                                            {list.into_iter().map(|p| {
                                                let pid = p.id;
                                                let plan_name = p.name.clone();
                                                let plan_groups = p.channel_groups.join(", ");
                                                let plan_currency = p.currency.clone();
                                                view! {
                                                    <tr>
                                                        <td>{p.id}</td>
                                                        <td>{p.name}</td>
                                                        <td>{p.max_concurrent_sessions}</td>
                                                        <td>{p.max_bound_devices}</td>
                                                        <td>{p.channel_groups.join(", ")}</td>
                                                        <td>{p.duration_days}</td>
                                                        <td>{format!("{} {}", p.price_cents, p.currency)}</td>
                                                        <td>
                                                            {can_write.then(|| view! {
                                                                <div style="display:flex;gap:8px;flex-wrap:wrap;align-items:flex-start">
                                                                    <details>
                                                                        <summary>{t_or("admin.edit-plan", "Edit plan")}</summary>
                                                                        <form method="post" action=format!("/admin/plans/{pid}/update")>
                                                                            <div class="form-grid">
                                                                                <div class="field"><label>{t_or("common.name", "Name")}</label><input class="textfield" name="name" value=plan_name/></div>
                                                                                <div class="field"><label>{t_or("common.max-concurrent-sessions", "Max concurrent sessions")}</label><input class="textfield" name="max_concurrent_sessions" type="number" value=p.max_concurrent_sessions.to_string()/></div>
                                                                                <div class="field"><label>{t_or("common.max-bound-devices", "Max bound devices")}</label><input class="textfield" name="max_bound_devices" type="number" value=p.max_bound_devices.to_string()/></div>
                                                                                <div class="field"><label>{t_or("common.channel-groups", "Channel groups (comma separated)")}</label><input class="textfield" name="channel_groups" value=plan_groups/></div>
                                                                                <div class="field"><label>{t_or("common.duration-days", "Duration (days)")}</label><input class="textfield" name="duration_days" type="number" value=p.duration_days.to_string()/></div>
                                                                                <div class="field"><label>{t_or("common.price-cents", "Price (cents)")}</label><input class="textfield" name="price_cents" type="number" value=p.price_cents.to_string()/></div>
                                                                                <div class="field"><label>{t_or("common.currency", "Currency")}</label><input class="textfield" name="currency" value=plan_currency/></div>
                                                                            </div>
                                                                            <button class="btn" type="submit">{t_or("common.save", "Save")}</button>
                                                                        </form>
                                                                    </details>
                                                                    <form method="post" action=format!("/admin/plans/{pid}/delete") style="display:inline">
                                                                        <button class="btn btn-destructive" type="submit">{t_or("common.delete", "Delete")}</button>
                                                                    </form>
                                                                </div>
                                                            })}
                                                        </td>
                                                    </tr>
                                                }
                                            }).collect_view()}
                                        </tbody>
                                    </table>
                                </div>
                            }.into_any()
                        }
                    }
                    Ok(None) => view! { <LoginPrompt/> }.into_any(),
                    Err(e) => view! { <div class="notice notice-error" role="alert">{e.to_string()}</div> }.into_any(),
                })}
            </Suspense>
        </AdminLayout>
    }
}

/// `/admin/admins` 管理员、角色、权限矩阵（§5.1/§5.3）。
#[component]
fn AdminsPage() -> impl IntoView {
    let admins = Resource::new_blocking(|| (), |_| async move { admin_admins().await });
    let is_super = admin_role() == Some(AdminRole::SuperAdmin);
    view! {
        <AdminLayout title=t_or("admin.admins", "Admins") active="admins">
            <OpNotice/>
            <div class="card">
                <h3 style="margin-top:0">{t_or("admin.permission-matrix", "Permission matrix (§5.3)")}</h3>
                <div class="table-wrap">
                    <table class="admin-table">
                        <thead><tr><th>{t_or("common.role", "Role")}</th><th>{t_or("admin.users", "Users")}</th><th>{t_or("admin.tokens", "Tokens")}</th><th>{t_or("admin.channels", "Channels")}</th><th>{t_or("admin.sources", "Sources")}</th><th>{t_or("admin.settings", "Settings")}</th><th>{t_or("admin.audit", "Audit")}</th></tr></thead>
                        <tbody>
                            <tr><td>{t_or("common.role-super-admin", "Super admin")}</td><td>RW</td><td>RW</td><td>RW</td><td>RW</td><td>RW</td><td>R</td></tr>
                            <tr><td>{t_or("common.role-operator", "Operator")}</td><td>RW</td><td>RW</td><td>RW</td><td>R</td><td>{"—"}</td><td>{"—"}</td></tr>
                            <tr><td>{t_or("common.role-support", "Support")}</td><td>R</td><td>R + kick</td><td>R</td><td>{"—"}</td><td>{"—"}</td><td>{"—"}</td></tr>
                            <tr><td>{t_or("common.role-readonly", "Readonly")}</td><td>R</td><td>R (masked)</td><td>R</td><td>R</td><td>{"—"}</td><td>{"—"}</td></tr>
                        </tbody>
                    </table>
                </div>
                <p style="font-size:var(--text-footnote);color:var(--c-label-secondary)">{t_or("admin.permissions-note", "Permissions are enforced server-side; the frontend only hides actions.")}</p>
            </div>
            {is_super.then(|| view! {
                <div class="card">
                    <h3 style="margin-top:0">{t_or("admin.add-admin", "Add admin")}</h3>
                    <form method="post" action="/admin/admins">
                        <div class="form-grid">
                            <TextField label=t_or("admin.username", "Username") name="username" required=true/>
                            <TextField label=t_or("admin.password", "Password") name="password" input_type="password" required=true/>
                            <TextField label=t_or("admin.role-hint", "Role (super_admin / operator / support / readonly)") name="role" value="operator"/>
                        </div>
                        <div style="margin-bottom:12px">
                            <Toggle label=t_or("admin.require-totp", "Require TOTP") name="totp_required"/>
                        </div>
                        <button class="btn btn-filled" type="submit">{t_or("common.save", "Save")}</button>
                    </form>
                </div>
            })}
            <Suspense fallback=LoadingSkeleton>
                {move || admins.get().map(|res| match res {
                    Ok(Some(list)) => {
                        if list.is_empty() {
                            view! { <EmptyState title=t_or("admin.no-admins", "No admins") icon="♛">{view! { <></> }}</EmptyState> }.into_any()
                        } else {
                            view! {
                                <div class="table-wrap">
                                    <table class="admin-table">
                                        <thead><tr><th>{t_or("common.id", "ID")}</th><th>{t_or("admin.username", "Username")}</th><th>{t_or("common.role", "Role")}</th><th>{t_or("common.totp-status", "TOTP status")}</th><th>{t_or("common.totp-required", "TOTP required")}</th><th>{t_or("common.actions", "Actions")}</th></tr></thead>
                                        <tbody>
                                            {list.into_iter().map(|a| {
                                                // TOTP 状态：已启用 / 待验证（已配给、首次登录输码后启用）/ 未配给。
                                                let totp_status = if a.totp_enabled {
                                                    t_or("admin.totp-enabled", "Enabled")
                                                } else if a.totp_provisioned {
                                                    t_or("admin.totp-pending", "Pending verification")
                                                } else {
                                                    t_or("admin.totp-unprovisioned", "Not provisioned")
                                                };
                                                let is_super_admin = a.role == AdminRole::SuperAdmin;
                                                let is_operator = a.role == AdminRole::Operator;
                                                let is_support = a.role == AdminRole::Support;
                                                let is_readonly = a.role == AdminRole::Readonly;
                                                let required = a.totp_required;
                                                let id = a.id;
                                                view! {
                                                    <tr>
                                                        <td>{id}</td>
                                                        <td>{a.username}</td>
                                                        <td>{role_label(&format!("{:?}", a.role))}</td>
                                                        <td>{totp_status}</td>
                                                        <td>{if required { "✓" } else { "" }}</td>
                                                        <td>
                                                            {is_super.then(|| view! {
                                                                <div style="display:flex;gap:8px;flex-wrap:wrap;align-items:center">
                                                                    <form method="post" action=format!("/admin/admins/{id}/update") style="display:inline-flex;gap:6px;align-items:center">
                                                                        <select name="role">
                                                                            <option value="super_admin" selected=is_super_admin>{t_or("common.role-super-admin", "Super admin")}</option>
                                                                            <option value="operator" selected=is_operator>{t_or("common.role-operator", "Operator")}</option>
                                                                            <option value="support" selected=is_support>{t_or("common.role-support", "Support")}</option>
                                                                            <option value="readonly" selected=is_readonly>{t_or("common.role-readonly", "Readonly")}</option>
                                                                        </select>
                                                                        <label><input type="checkbox" name="totp_required" value="true" checked=required/>{t_or("admin.require-totp", "Require TOTP")}</label>
                                                                        <button class="btn" type="submit">{t_or("common.save", "Save")}</button>
                                                                    </form>
                                                                    <form method="post" action=format!("/admin/admins/{id}/reset-totp") style="display:inline">
                                                                        <button class="btn" type="submit">{t_or("admin.reset-totp", "Provision / reset TOTP")}</button>
                                                                    </form>
                                                                    <form method="post" action=format!("/admin/admins/{id}/delete") style="display:inline">
                                                                        <button class="btn btn-destructive" type="submit">{t_or("common.delete", "Delete")}</button>
                                                                    </form>
                                                                </div>
                                                            })}
                                                        </td>
                                                    </tr>
                                                }
                                            }).collect_view()}
                                        </tbody>
                                    </table>
                                </div>
                                <p style="font-size:var(--text-footnote);color:var(--c-label-secondary)">{t_or("admin.totp-note", "Accounts without TOTP provisioned can still sign in with a password (treated as incomplete setup, not locked out); when creating an admin with Require TOTP, the secret is shown only once — scan it immediately; entering one code on first sign-in after provisioning enables it.")}</p>
                            }.into_any()
                        }
                    }
                    Ok(None) => view! { <LoginPrompt/> }.into_any(),
                    Err(e) => view! { <div class="notice notice-error" role="alert">{e.to_string()}</div> }.into_any(),
                })}
            </Suspense>
        </AdminLayout>
    }
}

/// `/admin/audit` 审计检索与导出（§5.1/§3.2：操作者、对象、前后快照）。
#[component]
fn AuditPage() -> impl IntoView {
    let query = use_query_map();
    let actor = move || query.get().get("actor");
    let action = move || query.get().get("action");
    let actor_v = actor();
    let action_v = action();
    let audit = Resource::new_blocking(
        move || (actor_v.clone(), action_v.clone()),
        |(actor, action)| async move { admin_audit(actor, action).await },
    );
    let can_read = admin_role().map(|r| r.can_read_audit()).unwrap_or(false);
    view! {
        <AdminLayout title=t_or("admin.audit", "Audit log") active="audit">
            <OpNotice/>
            {if !can_read {
                view! {
                    <EmptyState title=t_or("errors.e40300", "Forbidden") icon="⚿">
                        <p>{t_or("admin.only-superadmin-audit", "Only SuperAdmin can read the audit log.")}</p>
                    </EmptyState>
                }.into_any()
            } else {
                view! {
                    <form method="get" action="/admin/audit" style="display:flex;gap:8px;margin-bottom:12px">
                        <input class="textfield" type="text" name="actor" placeholder=t_or("common.actor", "Actor") value=actor().unwrap_or_default() style="flex:1"/>
                        <input class="textfield" type="text" name="action" placeholder=t_or("common.action", "Action") value=action().unwrap_or_default() style="flex:1"/>
                        <button class="btn btn-tinted" type="submit">{t_or("common.search", "Search")}</button>
                    </form>
                    <Suspense fallback=LoadingSkeleton>
                        {move || audit.get().map(|res| match res {
                            Ok(Some(paged)) => {
                                if paged.items.is_empty() {
                                    view! { <EmptyState title=t_or("admin.no-audit", "No audit records") icon="≣">{view! { <></> }}</EmptyState> }.into_any()
                                } else {
                                    view! {
                                        <div class="table-wrap">
                                            <table class="admin-table">
                                                <thead><tr><th>{t_or("common.time", "Time")}</th><th>{t_or("common.actor", "Actor")}</th><th>{t_or("common.action", "Action")}</th><th>{t_or("common.object", "Object")}</th><th>{t_or("common.before-after", "Before → After")}</th><th>{t_or("common.ip", "IP")}</th></tr></thead>
                                                <tbody>
                                                    {paged.items.into_iter().map(|l| view! {
                                                        <tr>
                                                            <td>{fmt_dt(l.created_at)}</td>
                                                            <td>{l.actor}</td>
                                                            <td>{l.action}</td>
                                                            <td>{format!("{}:{}", l.object_type, l.object_id)}</td>
                                                            <td class="mono">{format!("{:?} → {:?}", l.before_json, l.after_json)}</td>
                                                            <td>{l.ip_address.unwrap_or_default()}</td>
                                                        </tr>
                                                    }).collect_view()}
                                                </tbody>
                                            </table>
                                        </div>
                                    }.into_any()
                                }
                            }
                            Ok(None) => view! { <LoginPrompt/> }.into_any(),
                            Err(e) => view! { <div class="notice notice-error" role="alert">{e.to_string()}</div> }.into_any(),
                        })}
                    </Suspense>
                }.into_any()
            }}
        </AdminLayout>
    }
}

/// `/admin/settings` 系统参数、默认语言、可用语言（§5.1/§3.2）。
/// M1：四个键暂无消费点，标记“预留”而非假装已接入——
/// `default_max_concurrent_sessions` / `default_max_bound_devices`（套餐与
/// Token 创建都强制显式填值，无默认值 fallback 的调用点）、
/// `single_ip_token_limit`（文档未定义它限制哪个操作）、`cdn_domain`
/// （播放地址是带签名的相对路径，文档未定义 CDN 域名如何拼）。
fn setting_status(key: &str) -> &'static str {
    match key {
        "default_max_concurrent_sessions"
        | "default_max_bound_devices"
        | "single_ip_token_limit"
        | "cdn_domain" => "预留（未接入）",
        _ => "已接入",
    }
}

#[component]
fn SettingsPage() -> impl IntoView {
    let settings = Resource::new_blocking(|| (), |_| async move { admin_settings().await });
    let can_write = admin_role()
        .map(|r| r.can_write_settings())
        .unwrap_or(false);
    view! {
        <AdminLayout title=t_or("admin.settings", "Settings") active="settings">
            <OpNotice/>
            {can_write.then(|| view! {
                <div class="card">
                    <h3 style="margin-top:0">{t_or("admin.update-setting", "Update setting")}</h3>
                    <form method="post" action="/admin/settings">
                        <div class="form-grid">
                            <TextField label=t_or("common.key", "Key") name="key" required=true/>
                            <TextField label=t_or("admin.value-json-hint", "Value (JSON, e.g. 2 or \"zh-Hant-TW\")") name="value" required=true/>
                        </div>
                        <button class="btn btn-filled" type="submit">{t_or("common.save", "Save")}</button>
                    </form>
                    <p style="font-size:var(--text-footnote);color:var(--c-label-secondary)">
                        {t_or("admin.settings-keys-hint", "Keys (§3.2): default_max_concurrent_sessions, default_max_bound_devices, heartbeat_interval_sec, session_timeout_sec, single_ip_token_limit, play_url_ttl_sec, cdn_domain, alert_threshold_failed_syncs, default_locale, available_locales, portal_session_ttl_days, remember_device_ttl_days, ticket_ttl_sec, audit_retention_days.")}
                    </p>
                </div>
            })}
            <Suspense fallback=LoadingSkeleton>
                {move || settings.get().map(|res| match res {
                    Ok(Some(list)) => {
                        if list.is_empty() {
                            view! { <EmptyState title=t_or("admin.no-settings", "No settings") icon="⚙">{view! { <></> }}</EmptyState> }.into_any()
                        } else {
                            view! {
                                <div class="table-wrap">
                                    <table class="admin-table">
                                        <thead><tr><th>{t_or("common.key", "Key")}</th><th>{t_or("common.value", "Value")}</th><th>{t_or("common.updated", "Updated")}</th><th>{t_or("common.status", "Status")}</th></tr></thead>
                                        <tbody>
                                            {list.into_iter().map(|s| view! {
                                                <tr>
                                                    <td class="mono">{s.key.clone()}</td>
                                                    <td class="mono">{s.value.to_string()}</td>
                                                    <td>{if s.updated_at.timestamp() == 0 { "—".to_string() } else { fmt_dt(s.updated_at) }}</td>
                                                    <td>{setting_status(&s.key)}</td>
                                                </tr>
                                            }).collect_view()}
                                        </tbody>
                                    </table>
                                </div>
                            }.into_any()
                        }
                    }
                    Ok(None) => view! { <LoginPrompt/> }.into_any(),
                    Err(e) => view! { <div class="notice notice-error" role="alert">{e.to_string()}</div> }.into_any(),
                })}
            </Suspense>
        </AdminLayout>
    }
}

// ---------------------------------------------------------------------------
// WASM hydrate 入口（B4）
// ---------------------------------------------------------------------------

/// hydrate 入口：cargo-leptos 生成的 `/pkg/iptv-admin.js` 在加载后调用它，
/// 把 SSR 出来的 HTML 挂上交互（`#[server]` 函数的客户端存根、资源重取）。
/// 只在 hydrate 构建里编译；服务端构建不包含它。
#[cfg(feature = "hydrate")]
#[wasm_bindgen::prelude::wasm_bindgen]
pub fn hydrate() {
    console_error_panic_hook::set_once();
    leptos::mount::hydrate_body(App);
}
