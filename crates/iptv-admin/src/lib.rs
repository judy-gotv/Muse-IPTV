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
    AdminRole, AdminUser, AppUser, AuditLog, Channel, ChannelGroup, Envelope, EpgProgram,
    EpgSource, Paged, Plan, PlaybackSession, PlaylistSource, StatsOverview, SyncDiff, SyncLog,
    SystemSetting, UserToken,
};
use iptv_ui::{fmt_dt, t_or, LanguageSwitcher, LocaleCtx};
use leptos::prelude::*;
use leptos_meta::{provide_meta_context, MetaTags, Title};
use leptos_router::components::{Route, Router, Routes};
use leptos_router::hooks::use_query_map;
use leptos_router::path;
use serde::de::DeserializeOwned;

// ---------------------------------------------------------------------------
// 后端 API 客户端（server fn 与 Axum 处理器共用，§5.2）
// ---------------------------------------------------------------------------

/// 后端地址：`IPTV_API_BASE`，默认 `http://127.0.0.1:8080`。
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

fn parts_cookie(name: &str) -> Option<String> {
    let parts = use_context::<axum::http::request::Parts>()?;
    let header = parts
        .headers
        .get(axum::http::header::COOKIE)?
        .to_str()
        .ok()?;
    cookie_value(header, name)
}

fn parts_accept_language() -> Option<String> {
    let parts = use_context::<axum::http::request::Parts>()?;
    parts
        .headers
        .get(axum::http::header::ACCEPT_LANGUAGE)?
        .to_str()
        .ok()
        .map(|s| s.to_string())
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// GET 后端接口；401 返回 `Ok(None)`（未登录）。
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
        <html lang="zh-Hant" data-theme=data_theme>
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
    let role_label = admin_role().map(|r| format!("{:?}", r)).unwrap_or_default();
    view! {
        <div class="app-shell">
            <Sidebar title=t_or("common.app-name", "IPTV Platform") items=items collapsed=collapsed.get() on_toggle=toggle/>
            <div class="app-main">
                <Toolbar title=title>
                    {(!role_label.is_empty()).then(|| view! { <Badge text=role_label.clone() tone=BadgeTone::Muted/> })}
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
                view! { <a class="btn btn-tinted" href=format!("{base}?page={}", page - 1)>"‹ Prev"</a> }.into_any()
            } else {
                view! { "" }.into_any()
            }}
            <span>{format!("{page} / {total_pages} · total {total}")}</span>
            {if page < total_pages {
                view! { <a class="btn btn-tinted" href=format!("{base}?page={}", page + 1)>"Next ›"</a> }.into_any()
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
                                <div class="stat-card"><div class="stat-value">{s.online_users}</div><div class="stat-label">"online users"</div></div>
                                <div class="stat-card"><div class="stat-value">{s.active_sessions}</div><div class="stat-label">"active sessions"</div></div>
                                <div class="stat-card"><div class="stat-value">{format!("{}/{}", s.available_channels, s.total_channels)}</div><div class="stat-label">"available / total channels"</div></div>
                                <div class="stat-card"><div class="stat-value">{format!("{}/{}", s.active_tokens, s.total_tokens)}</div><div class="stat-label">"active / total tokens"</div></div>
                            </div>
                            <div class="card">
                                <h3 style="margin-top:0">"sync"</h3>
                                <p>{sync}</p>
                                {if s.alerts.is_empty() {
                                    view! { <p style="color:var(--c-label-secondary)">"no alerts"</p> }.into_any()
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
                            <TextField label="Email" name="email" input_type="email"/>
                            <TextField label="Phone" name="phone"/>
                            <TextField label="Plan ID" name="plan_id" input_type="number"/>
                            <TextField label=t_or("common.language", "Language") name="locale" value="zh-Hant-TW"/>
                            <TextField label="Remark" name="remark"/>
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
                                        <thead><tr><th>ID</th><th>Name</th><th>Email</th><th>Status</th><th>Locale</th><th>Expires</th></tr></thead>
                                        <tbody>
                                            {paged.items.into_iter().map(|u| view! {
                                                <tr>
                                                    <td>{u.id}</td>
                                                    <td>{u.display_name}</td>
                                                    <td>{u.email.unwrap_or_default()}</td>
                                                    <td>{format!("{:?}", u.status)}</td>
                                                    <td>{u.locale}</td>
                                                    <td>{u.expires_at.map(fmt_dt).unwrap_or_default()}</td>
                                                </tr>
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
                    <p style="font-size:var(--text-footnote);color:var(--c-label-secondary)">"The plaintext Token is shown only once after creation."</p>
                    <form method="post" action="/admin/tokens">
                        <div class="form-grid">
                            <TextField label="User ID" name="user_id" input_type="number" required=true/>
                            <TextField label="Name" name="name" required=true/>
                            <TextField label="Device type" name="device_type" value="android_tv"/>
                            <TextField label="Max concurrent sessions" name="max_concurrent_sessions" input_type="number" value="2"/>
                            <TextField label="Max bound devices" name="max_bound_devices" input_type="number" value="3"/>
                            <TextField label="Expires at (RFC3339)" name="expires_at"/>
                            <TextField label=t_or("common.language", "Language") name="locale" value="zh-Hant-TW"/>
                            <TextField label="Allowed groups (comma separated)" name="allowed_groups"/>
                            <TextField label="Max quality" name="max_quality" value="1080p"/>
                        </div>
                        <div style="margin-bottom:12px">
                            <Toggle label="Catchup" name="catchup" checked=true/>
                            <Toggle label="Recording" name="recording"/>
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
                                        <thead><tr><th>ID</th><th>Prefix</th><th>Name</th><th>Device</th><th>Concurrent</th><th>Devices</th><th>Status</th><th>Expires</th><th>Actions</th></tr></thead>
                                        <tbody>
                                            {paged.items.into_iter().map(|t| {
                                                let tid = t.id;
                                                view! {
                                                    <tr>
                                                        <td>{t.id}</td>
                                                        <td class="mono">{t.token_prefix}</td>
                                                        <td>{t.name}</td>
                                                        <td>{t.device_type.unwrap_or_default()}</td>
                                                        <td>{t.max_concurrent_sessions}</td>
                                                        <td>{t.max_bound_devices}</td>
                                                        <td>{format!("{:?}", t.status)}</td>
                                                        <td>{t.expires_at.map(fmt_dt).unwrap_or_default()}</td>
                                                        <td>
                                                            <form method="post" action=format!("/admin/tokens/{tid}/ticket") style="display:inline">
                                                                <button class="btn btn-tinted" type="submit">"login link"</button>
                                                            </form>
                                                            " "
                                                            <form method="post" action=format!("/admin/tokens/{tid}/revoke") style="display:inline">
                                                                <button class="btn btn-destructive" type="submit">"revoke sessions"</button>
                                                            </form>
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
                    <h3 style="margin-top:0">"Add channel"</h3>
                    <form method="post" action="/admin/channels">
                        <div class="form-grid">
                            <TextField label="tvg-id" name="tvg_id"/>
                            <TextField label="Name" name="name" required=true/>
                            <TextField label="Group" name="group_title"/>
                            <TextField label="Logo URL" name="logo_url"/>
                            <TextField label="Play URL" name="play_url" required=true/>
                            <TextField label="Backup URL" name="backup_url"/>
                            <TextField label="Protocol" name="protocol" value="hls"/>
                            <TextField label="Sort weight" name="sort_weight" input_type="number" value="0"/>
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
                                        <thead><tr><th>ID</th><th>tvg-id</th><th>Name</th><th>Group</th><th>Status</th><th>Available</th><th>Actions</th></tr></thead>
                                        <tbody>
                                            {paged.items.into_iter().map(|c| {
                                                let cid = c.id;
                                                view! {
                                                    <tr>
                                                        <td>{c.id}</td>
                                                        <td class="mono">{c.tvg_id.unwrap_or_default()}</td>
                                                        <td>{c.name}</td>
                                                        <td>{c.group_title.unwrap_or_default()}</td>
                                                        <td>{format!("{:?}", c.status)}</td>
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
                    <h3 style="margin-top:0">"Add subscription source"</h3>
                    <form method="post" action="/admin/sources">
                        <div class="form-grid">
                            <TextField label="Name" name="name" required=true/>
                            <TextField label="URL" name="url" required=true/>
                            <TextField label="Sync interval (sec, min 300)" name="sync_interval_sec" input_type="number" value="3600"/>
                            <TextField label="EPG URL" name="epg_url"/>
                        </div>
                        <button class="btn btn-filled" type="submit">{t_or("common.save", "Save")}</button>
                    </form>
                    <h3>"Upload local M3U (.m3u / .m3u8 / .txt, max 20 MB)"</h3>
                    <form method="post" action="/admin/sources/upload" enctype="multipart/form-data">
                        <div class="field">
                            <label for="m3u-name">"Source name"</label>
                            <input class="textfield" id="m3u-name" name="name" required/>
                        </div>
                        <div class="field">
                            <label for="m3u-file">"M3U file"</label>
                            <input class="textfield" id="m3u-file" type="file" name="file" accept=".m3u,.m3u8,.txt" required/>
                        </div>
                        <button class="btn btn-filled" type="submit">"Upload & parse"</button>
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
                                        <thead><tr><th>ID</th><th>Name</th><th>URL</th><th>Interval</th><th>Last sync</th><th>Status</th><th>Actions</th></tr></thead>
                                        <tbody>
                                            {list.into_iter().map(|s| {
                                                let sid = s.id;
                                                view! {
                                                    <tr>
                                                        <td>{s.id}</td>
                                                        <td>{s.name}</td>
                                                        <td class="mono">{s.url.unwrap_or_default()}</td>
                                                        <td>{s.sync_interval_sec}</td>
                                                        <td>{s.last_sync_at.map(fmt_dt).unwrap_or_default()}</td>
                                                        <td>{s.last_status.unwrap_or_default()}</td>
                                                        <td>
                                                            <a class="btn btn-tinted" href=format!("/admin/sources?source={sid}")>"diff / logs"</a>
                                                            " "
                                                            {can_write.then(|| view! {
                                                                <form method="post" action=format!("/admin/sources/{sid}/sync") style="display:inline">
                                                                    <button class="btn btn-filled" type="submit">{t_or("admin.sync-now", "Sync now")}</button>
                                                                </form>
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
                            <h3 style="margin-top:0">"Sync diff"</h3>
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
                            <h3 style="margin-top:0">"Sync logs"</h3>
                            <div class="table-wrap">
                                <table class="admin-table">
                                    <thead><tr><th>Started</th><th>Status</th><th>Added</th><th>Updated</th><th>Removed</th><th>Error</th></tr></thead>
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
                    <h3 style="margin-top:0">"Add group"</h3>
                    <form method="post" action="/admin/groups">
                        <div class="form-grid">
                            <TextField label="Name" name="name" required=true/>
                            <TextField label="Sort weight" name="sort_weight" input_type="number" value="0"/>
                            <TextField label="Icon" name="icon"/>
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
                                        <thead><tr><th>ID</th><th>Name</th><th>Sort</th><th>Icon</th><th>Default</th></tr></thead>
                                        <tbody>
                                            {list.into_iter().map(|g| view! {
                                                <tr>
                                                    <td>{g.id}</td>
                                                    <td>{g.name}</td>
                                                    <td>{g.sort_weight}</td>
                                                    <td>{g.icon.unwrap_or_default()}</td>
                                                    <td>{if g.is_default { "✓" } else { "" }}</td>
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
                    <h3 style="margin-top:0">"Add EPG source"</h3>
                    <form method="post" action="/admin/epg/sources">
                        <div class="form-grid">
                            <TextField label="Name" name="name" required=true/>
                            <TextField label="URL" name="url" required=true/>
                        </div>
                        <button class="btn btn-filled" type="submit">{t_or("common.save", "Save")}</button>
                    </form>
                </div>
            })}
            <div class="card">
                <h3 style="margin-top:0">"Program preview"</h3>
                <form method="get" action="/admin/epg" style="display:flex;gap:8px">
                    <input class="textfield" type="number" name="channel_id" placeholder="channel id" style="flex:1"/>
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
                                        <thead><tr><th>ID</th><th>Name</th><th>URL</th><th>Last sync</th><th>Status</th></tr></thead>
                                        <tbody>
                                            {list.into_iter().map(|s| view! {
                                                <tr>
                                                    <td>{s.id}</td>
                                                    <td>{s.name}</td>
                                                    <td class="mono">{s.url}</td>
                                                    <td>{s.last_sync_at.map(fmt_dt).unwrap_or_default()}</td>
                                                    <td>{s.last_status.unwrap_or_default()}</td>
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
                                        <thead><tr><th>ID</th><th>Session</th><th>Token</th><th>Channel</th><th>Device</th><th>Started</th><th>Last heartbeat</th><th>Status</th><th>Actions</th></tr></thead>
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
                                                        <td><StatusDot label=s.status.clone() kind=if s.status == "active" { StatusKind::Online } else { StatusKind::Offline }/></td>
                                                        <td>
                                                            {can_kick.then(|| view! {
                                                                <form method="post" action=format!("/admin/sessions/{sid}/kick") style="display:inline">
                                                                    <button class="btn btn-destructive" type="submit">"kick"</button>
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
                    <h3 style="margin-top:0">"Add plan"</h3>
                    <form method="post" action="/admin/plans">
                        <div class="form-grid">
                            <TextField label="Name" name="name" required=true/>
                            <TextField label="Max concurrent sessions" name="max_concurrent_sessions" input_type="number" value="2"/>
                            <TextField label="Max bound devices" name="max_bound_devices" input_type="number" value="3"/>
                            <TextField label="Channel groups (comma separated)" name="channel_groups"/>
                            <TextField label="Duration (days)" name="duration_days" input_type="number" value="365"/>
                            <TextField label="Price (cents)" name="price_cents" input_type="number" value="0"/>
                            <TextField label="Currency" name="currency" value="USD"/>
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
                                        <thead><tr><th>ID</th><th>Name</th><th>Concurrent</th><th>Devices</th><th>Groups</th><th>Duration</th><th>Price</th></tr></thead>
                                        <tbody>
                                            {list.into_iter().map(|p| view! {
                                                <tr>
                                                    <td>{p.id}</td>
                                                    <td>{p.name}</td>
                                                    <td>{p.max_concurrent_sessions}</td>
                                                    <td>{p.max_bound_devices}</td>
                                                    <td>{p.channel_groups.join(", ")}</td>
                                                    <td>{p.duration_days}</td>
                                                    <td>{format!("{} {}", p.price_cents, p.currency)}</td>
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

/// `/admin/admins` 管理员、角色、权限矩阵（§5.1/§5.3）。
#[component]
fn AdminsPage() -> impl IntoView {
    let admins = Resource::new_blocking(|| (), |_| async move { admin_admins().await });
    let is_super = admin_role() == Some(AdminRole::SuperAdmin);
    view! {
        <AdminLayout title=t_or("admin.admins", "Admins") active="admins">
            <OpNotice/>
            <div class="card">
                <h3 style="margin-top:0">"Permission matrix (§5.3)"</h3>
                <div class="table-wrap">
                    <table class="admin-table">
                        <thead><tr><th>Role</th><th>Users</th><th>Tokens</th><th>Channels</th><th>Sources</th><th>Settings</th><th>Audit</th></tr></thead>
                        <tbody>
                            <tr><td>SuperAdmin</td><td>RW</td><td>RW</td><td>RW</td><td>RW</td><td>RW</td><td>R</td></tr>
                            <tr><td>Operator</td><td>RW</td><td>RW</td><td>RW</td><td>R</td><td>{"—"}</td><td>{"—"}</td></tr>
                            <tr><td>Support</td><td>R</td><td>R + kick</td><td>R</td><td>{"—"}</td><td>{"—"}</td><td>{"—"}</td></tr>
                            <tr><td>Readonly</td><td>R</td><td>R (masked)</td><td>R</td><td>R</td><td>{"—"}</td><td>{"—"}</td></tr>
                        </tbody>
                    </table>
                </div>
                <p style="font-size:var(--text-footnote);color:var(--c-label-secondary)">"Permissions are enforced server-side; the frontend only hides actions."</p>
            </div>
            {is_super.then(|| view! {
                <div class="card">
                    <h3 style="margin-top:0">"Add admin"</h3>
                    <form method="post" action="/admin/admins">
                        <div class="form-grid">
                            <TextField label=t_or("admin.username", "Username") name="username" required=true/>
                            <TextField label=t_or("admin.password", "Password") name="password" input_type="password" required=true/>
                            <TextField label="Role (super_admin / operator / support / readonly)" name="role" value="operator"/>
                        </div>
                        <div style="margin-bottom:12px">
                            <Toggle label="Require TOTP" name="totp_required"/>
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
                                        <thead><tr><th>ID</th><th>Username</th><th>Role</th><th>TOTP enabled</th><th>TOTP required</th></tr></thead>
                                        <tbody>
                                            {list.into_iter().map(|a| view! {
                                                <tr>
                                                    <td>{a.id}</td>
                                                    <td>{a.username}</td>
                                                    <td>{format!("{:?}", a.role)}</td>
                                                    <td>{if a.totp_enabled { "✓" } else { "" }}</td>
                                                    <td>{if a.totp_required { "✓" } else { "" }}</td>
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
                        <p>"Only SuperAdmin can read the audit log."</p>
                    </EmptyState>
                }.into_any()
            } else {
                view! {
                    <form method="get" action="/admin/audit" style="display:flex;gap:8px;margin-bottom:12px">
                        <input class="textfield" type="text" name="actor" placeholder="actor" value=actor().unwrap_or_default() style="flex:1"/>
                        <input class="textfield" type="text" name="action" placeholder="action" value=action().unwrap_or_default() style="flex:1"/>
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
                                                <thead><tr><th>Time</th><th>Actor</th><th>Action</th><th>Object</th><th>{"Before → After"}</th><th>IP</th></tr></thead>
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
                    <h3 style="margin-top:0">"Update setting"</h3>
                    <form method="post" action="/admin/settings">
                        <div class="form-grid">
                            <TextField label="Key" name="key" required=true/>
                            <TextField label="Value (JSON, e.g. 2 or \"zh-Hant-TW\")" name="value" required=true/>
                        </div>
                        <button class="btn btn-filled" type="submit">{t_or("common.save", "Save")}</button>
                    </form>
                    <p style="font-size:var(--text-footnote);color:var(--c-label-secondary)">
                        "Keys (§3.2): default_max_concurrent_sessions, heartbeat_interval_sec, session_timeout_sec, max_tokens_per_ip, signature_ttl_sec, cdn_domain, alert_thresholds, default_locale, available_locales."
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
                                        <thead><tr><th>Key</th><th>Value</th><th>Updated</th></tr></thead>
                                        <tbody>
                                            {list.into_iter().map(|s| view! {
                                                <tr>
                                                    <td class="mono">{s.key}</td>
                                                    <td class="mono">{s.value.to_string()}</td>
                                                    <td>{fmt_dt(s.updated_at)}</td>
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
