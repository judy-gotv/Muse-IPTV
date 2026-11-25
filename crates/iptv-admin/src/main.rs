//! 管理后台二进制（开发文档 §5）。
//!
//! Leptos SSR：`leptos_routes` + `shell()`；没有 `pkg/` 时仍可 SSR 渲染。
//! 变更处理器是纯 Axum POST，无 hydrate 也能工作：管理员登录（Argon2 +
//! TOTP，服务端校验）、退出、语言切换、各资源的创建 / 删除 / 同步 / 下线；
//! 创建 Token 与生成登录票据后返回一次性展示页（明文 Token 只显示一次，§5.2）。
//!
//! 登录成功后除转发后端 `iptv_admin_sid`（Path=/admin）外，另写一份 Path=/
//! 的同值 Cookie，供 hydrate 后的 server fn 请求使用（请求走 `/api/...`
//! 路径，否则带 Path=/admin 的 Cookie 不会被浏览器发送）；角色另写
//! `iptv_admin_role` Cookie，仅作前端展示用，服务端另行校验。
//!
//! 监听地址由 Leptos 配置决定（`LEPTOS_SITE_ADDR`，默认 127.0.0.1:3000）；
//! 后端地址 `IPTV_API_BASE`，默认 http://127.0.0.1:8080。
// 作者: Passwd Word

use axum::{
    extract::{Form, Multipart, Path},
    http::{header, HeaderMap},
    response::{Html, Redirect},
    routing::{get, post},
    Router,
};
use iptv_admin::{api_base, cookie_value, shell, App};
use iptv_design::design_tokens_css;
use leptos_axum::{generate_route_list, LeptosRoutes};
use serde::Deserialize;

#[tokio::main]
async fn main() {
    // 先读工作目录的 .env（不存在则忽略；已有环境变量优先，不会被覆盖）。
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let conf = leptos::config::get_configuration(None).expect("leptos config");
    let leptos_options = conf.leptos_options;
    // 单端口部署时用专属变量定内部监听地址（.env 里门户/后台各写一行）；
    // 没设则回落到 Leptos 的 LEPTOS_SITE_ADDR（默认 127.0.0.1:3000）。
    let addr = std::env::var("IPTV_ADMIN_ADDR")
        .ok()
        .and_then(|s| s.parse::<std::net::SocketAddr>().ok())
        .unwrap_or(leptos_options.site_addr);
    let routes = generate_route_list(App);
    let app = Router::new()
        .route("/design.css", get(design_css))
        .route("/healthz", get(|| async { "ok" }))
        .route("/admin/login", post(login))
        .route("/admin/logout", post(logout))
        .route("/admin/locale", post(set_locale))
        .route("/admin/users", post(create_user))
        .route("/admin/tokens", post(create_token))
        .route("/admin/tokens/{id}/ticket", post(create_login_ticket))
        .route("/admin/tokens/{id}/revoke", post(revoke_token_sessions))
        .route("/admin/channels", post(create_channel))
        .route("/admin/channels/{id}/delete", post(delete_channel))
        .route("/admin/sources", post(create_source))
        .route("/admin/sources/upload", post(upload_m3u))
        .route("/admin/sources/{id}/sync", post(trigger_sync))
        .route("/admin/groups", post(create_group))
        .route("/admin/epg/sources", post(create_epg_source))
        .route("/admin/sessions/{id}/kick", post(kick_session))
        .route("/admin/plans", post(create_plan))
        .route("/admin/admins", post(create_admin))
        .route("/admin/settings", post(update_settings))
        .leptos_routes(&leptos_options, routes, {
            let leptos_options = leptos_options.clone();
            move || shell(leptos_options.clone())
        })
        .with_state(leptos_options);
    tracing::info!("admin listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app.into_make_service())
        .await
        .unwrap();
}

async fn design_css() -> ([(header::HeaderName, &'static str); 1], &'static str) {
    ([(header::CONTENT_TYPE, "text/css")], design_tokens_css())
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        // 与门户 http_client 同理：绝不能跟随重定向，否则 API 返回的 302
        // 会被跟到不存在的路径、Set-Cookie 也丢了。
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

fn cookie_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

fn admin_sid(headers: &HeaderMap) -> Option<String> {
    cookie_header(headers).and_then(|h| cookie_value(&h, "iptv_admin_sid"))
}

struct BackendResult {
    ok: bool,
    data: serde_json::Value,
    set_cookie: Option<String>,
    error: Option<String>,
}

async fn backend_json(
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
    sid: Option<&str>,
) -> BackendResult {
    let url = format!("{}{path}", api_base());
    let mut req = client().request(method, &url);
    if let Some(b) = body {
        req = req.json(&b);
    }
    if let Some(s) = sid {
        req = req.header("Cookie", format!("iptv_admin_sid={s}"));
    }
    let Ok(res) = req.send().await else {
        return BackendResult {
            ok: false,
            data: serde_json::Value::Null,
            set_cookie: None,
            error: Some("backend unreachable".to_string()),
        };
    };
    let set_cookie = res
        .headers()
        .get(reqwest::header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let env: serde_json::Value = res.json().await.unwrap_or(serde_json::Value::Null);
    let code = env.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        let msg = env
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("error")
            .to_string();
        return BackendResult {
            ok: false,
            data: serde_json::Value::Null,
            set_cookie,
            error: Some(msg),
        };
    }
    BackendResult {
        ok: true,
        data: env.get("data").cloned().unwrap_or(serde_json::Value::Null),
        set_cookie,
        error: None,
    }
}

fn redirect_with_cookies(to: &str, cookies: &[String]) -> (HeaderMap, Redirect) {
    let mut h = HeaderMap::new();
    for c in cookies {
        if let Ok(v) = c.parse() {
            h.append(header::SET_COOKIE, v);
        }
    }
    (h, Redirect::to(to))
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

fn ok_redirect(base: &str, r: &BackendResult) -> (HeaderMap, Redirect) {
    match &r.error {
        Some(e) => redirect_with_cookies(&format!("{base}?error={}", pct_encode(e)), &[]),
        None => redirect_with_cookies(&format!("{base}?saved=1"), &[]),
    }
}

fn opt_str(s: &Option<String>) -> Option<String> {
    s.as_ref().filter(|v| !v.trim().is_empty()).cloned()
}

fn split_csv(s: &Option<String>) -> Vec<String> {
    s.as_deref()
        .unwrap_or("")
        .split(',')
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .collect()
}

/// 一次性展示页（Token / 票据只显示一次，§5.2）。
fn onetime_page(title: &str, rows: &[(String, String)], qr_png: Option<&str>) -> Html<String> {
    let mut body = format!(
        "<!DOCTYPE html><html lang=\"zh-Hant\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><link rel=\"stylesheet\" href=\"/design.css\"><title>{title}</title></head><body><div class=\"page\"><div class=\"card\"><h1>{title}</h1><p style=\"color:var(--c-destructive)\">This secret is shown only once. Save it now.</p>"
    );
    for (k, v) in rows {
        body.push_str(&format!(
            "<p><strong>{k}:</strong> <span class=\"mono\">{v}</span></p>"
        ));
    }
    if let Some(qr) = qr_png {
        body.push_str(&format!(
            "<p><img class=\"qr-img\" src=\"{qr}\" alt=\"QR code\"></p>"
        ));
    }
    body.push_str("<p><a class=\"btn btn-filled\" href=\"/admin/tokens\">Back to Tokens</a></p></div></div></body></html>");
    Html(body)
}

// ---------------------------------------------------------------------------
// 登录 / 退出 / 语言
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct LoginForm {
    username: String,
    password: String,
    totp_code: Option<String>,
}

async fn login(Form(form): Form<LoginForm>) -> (HeaderMap, Redirect) {
    let body = serde_json::json!({
        "username": form.username,
        "password": form.password,
        "totp_code": opt_str(&form.totp_code),
    });
    let r = backend_json(
        reqwest::Method::POST,
        "/api/v1/admin/auth/login",
        Some(body),
        None,
    )
    .await;
    if !r.ok {
        let e = r.error.unwrap_or_else(|| "login failed".to_string());
        return redirect_with_cookies(&format!("/admin/login?error={}", pct_encode(&e)), &[]);
    }
    let role = r
        .data
        .get("role")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mut cookies = Vec::new();
    if let Some(sc) = r.set_cookie {
        cookies.push(sc.clone());
        // 同值写一份 Path=/ 的 Cookie，供 hydrate 后的 server fn 请求使用。
        if let Some(value) = sc.split(';').next().and_then(|kv| kv.split_once('=')) {
            cookies.push(format!(
                "{}={}; Path=/; HttpOnly; SameSite=Lax; Max-Age=43200",
                value.0, value.1
            ));
        }
    }
    if !role.is_empty() {
        cookies.push(format!(
            "iptv_admin_role={role}; Path=/admin; SameSite=Lax; Max-Age=43200"
        ));
    }
    redirect_with_cookies("/admin", &cookies)
}

async fn logout() -> (HeaderMap, Redirect) {
    redirect_with_cookies(
        "/admin/login",
        &[
            "iptv_admin_sid=; Path=/admin; HttpOnly; SameSite=Lax; Max-Age=0".to_string(),
            "iptv_admin_sid=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0".to_string(),
            "iptv_admin_role=; Path=/admin; SameSite=Lax; Max-Age=0".to_string(),
        ],
    )
}

#[derive(Deserialize)]
struct LocaleForm {
    locale: String,
    redirect_to: Option<String>,
}

async fn set_locale(Form(form): Form<LocaleForm>) -> (HeaderMap, Redirect) {
    let loc = if iptv_i18n::I18n::supported_locales().contains(&form.locale.as_str()) {
        form.locale.clone()
    } else {
        iptv_i18n::DEFAULT_LOCALE.to_string()
    };
    let to = form.redirect_to.unwrap_or_else(|| "/admin".to_string());
    let to = if to.starts_with('/') && !to.starts_with("//") {
        to
    } else {
        "/admin".to_string()
    };
    redirect_with_cookies(
        &to,
        &[format!(
            "iptv_locale={loc}; Path=/; SameSite=Lax; Max-Age=31536000"
        )],
    )
}

// ---------------------------------------------------------------------------
// 资源创建 / 操作
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct UserForm {
    display_name: String,
    email: Option<String>,
    phone: Option<String>,
    plan_id: Option<String>,
    locale: Option<String>,
    remark: Option<String>,
}

async fn create_user(headers: HeaderMap, Form(f): Form<UserForm>) -> (HeaderMap, Redirect) {
    let body = serde_json::json!({
        "display_name": f.display_name,
        "email": opt_str(&f.email),
        "phone": opt_str(&f.phone),
        "plan_id": f.plan_id.and_then(|v| v.parse::<i64>().ok()),
        "locale": opt_str(&f.locale),
        "remark": opt_str(&f.remark),
    });
    let r = backend_json(
        reqwest::Method::POST,
        "/api/v1/admin/users",
        Some(body),
        admin_sid(&headers).as_deref(),
    )
    .await;
    ok_redirect("/admin/users", &r)
}

#[derive(Deserialize)]
struct TokenForm {
    user_id: i64,
    name: String,
    device_type: Option<String>,
    max_concurrent_sessions: i32,
    max_bound_devices: i32,
    expires_at: Option<String>,
    locale: Option<String>,
    allowed_groups: Option<String>,
    max_quality: Option<String>,
    catchup: Option<String>,
    recording: Option<String>,
}

async fn create_token(headers: HeaderMap, Form(f): Form<TokenForm>) -> Html<String> {
    let body = serde_json::json!({
        "user_id": f.user_id,
        "name": f.name,
        "device_type": opt_str(&f.device_type),
        "max_concurrent_sessions": f.max_concurrent_sessions,
        "max_bound_devices": f.max_bound_devices,
        "expires_at": opt_str(&f.expires_at),
        "locale": opt_str(&f.locale),
        "allowed_groups": split_csv(&f.allowed_groups),
        "denied_channels": [],
        "features": {
            "catchup": f.catchup.as_deref() == Some("true"),
            "recording": f.recording.as_deref() == Some("true"),
            "max_quality": f.max_quality.unwrap_or_else(|| "1080p".to_string()),
        },
    });
    let r = backend_json(
        reqwest::Method::POST,
        "/api/v1/admin/tokens",
        Some(body),
        admin_sid(&headers).as_deref(),
    )
    .await;
    if !r.ok {
        let e = r.error.unwrap_or_else(|| "create token failed".to_string());
        return Html(format!("<!DOCTYPE html><html><body><p>{e}</p><p><a href=\"/admin/tokens\">Back</a></p></body></html>"));
    }
    let rows = vec![
        (
            "Token ID".to_string(),
            r.data.get("id").map(|v| v.to_string()).unwrap_or_default(),
        ),
        (
            "Token (shown once)".to_string(),
            r.data
                .get("token")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        ),
        (
            "Portal URL".to_string(),
            r.data
                .get("portal_url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        ),
    ];
    onetime_page(
        "Token created",
        &rows,
        r.data.get("qr_png").and_then(|v| v.as_str()),
    )
}

async fn create_login_ticket(headers: HeaderMap, Path(id): Path<i64>) -> Html<String> {
    let r = backend_json(
        reqwest::Method::POST,
        &format!("/api/v1/admin/tokens/{id}/login-ticket"),
        Some(serde_json::json!({})),
        admin_sid(&headers).as_deref(),
    )
    .await;
    if !r.ok {
        let e = r
            .error
            .unwrap_or_else(|| "create ticket failed".to_string());
        return Html(format!("<!DOCTYPE html><html><body><p>{e}</p><p><a href=\"/admin/tokens\">Back</a></p></body></html>"));
    }
    let rows = vec![
        (
            "Portal URL (60s, single-use)".to_string(),
            r.data
                .get("portal_url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        ),
        (
            "Ticket".to_string(),
            r.data
                .get("ticket")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        ),
    ];
    onetime_page(
        "Login ticket",
        &rows,
        r.data.get("qr_png").and_then(|v| v.as_str()),
    )
}

async fn revoke_token_sessions(headers: HeaderMap, Path(id): Path<i64>) -> (HeaderMap, Redirect) {
    let r = backend_json(
        reqwest::Method::POST,
        &format!("/api/v1/admin/tokens/{id}/revoke-sessions"),
        Some(serde_json::json!({})),
        admin_sid(&headers).as_deref(),
    )
    .await;
    ok_redirect("/admin/tokens", &r)
}

#[derive(Deserialize)]
struct ChannelForm {
    tvg_id: Option<String>,
    name: String,
    display_name: Option<String>,
    logo_url: Option<String>,
    group_title: Option<String>,
    language: Option<String>,
    country: Option<String>,
    play_url: String,
    backup_url: Option<String>,
    protocol: Option<String>,
    sort_weight: Option<String>,
    visibility: Option<String>,
}

async fn create_channel(headers: HeaderMap, Form(f): Form<ChannelForm>) -> (HeaderMap, Redirect) {
    let body = serde_json::json!({
        "tvg_id": opt_str(&f.tvg_id),
        "name": f.name,
        "display_name": opt_str(&f.display_name),
        "logo_url": opt_str(&f.logo_url),
        "group_title": opt_str(&f.group_title),
        "language": opt_str(&f.language),
        "country": opt_str(&f.country),
        "play_url": f.play_url,
        "backup_url": opt_str(&f.backup_url),
        "protocol": opt_str(&f.protocol),
        "sort_weight": f.sort_weight.and_then(|v| v.parse::<i32>().ok()),
        "visibility": opt_str(&f.visibility),
    });
    let r = backend_json(
        reqwest::Method::POST,
        "/api/v1/admin/channels",
        Some(body),
        admin_sid(&headers).as_deref(),
    )
    .await;
    ok_redirect("/admin/channels", &r)
}

async fn delete_channel(headers: HeaderMap, Path(id): Path<i64>) -> (HeaderMap, Redirect) {
    let r = backend_json(
        reqwest::Method::DELETE,
        &format!("/api/v1/admin/channels/{id}"),
        None,
        admin_sid(&headers).as_deref(),
    )
    .await;
    ok_redirect("/admin/channels", &r)
}

#[derive(Deserialize)]
struct SourceForm {
    name: String,
    url: String,
    sync_interval_sec: Option<String>,
    epg_url: Option<String>,
}

async fn create_source(headers: HeaderMap, Form(f): Form<SourceForm>) -> (HeaderMap, Redirect) {
    let body = serde_json::json!({
        "name": f.name,
        "url": f.url,
        "sync_interval_sec": f.sync_interval_sec.and_then(|v| v.parse::<i64>().ok()),
        "epg_url": opt_str(&f.epg_url),
    });
    let r = backend_json(
        reqwest::Method::POST,
        "/api/v1/admin/sources",
        Some(body),
        admin_sid(&headers).as_deref(),
    )
    .await;
    ok_redirect("/admin/sources", &r)
}

/// POST /admin/sources/upload：multipart 转发给后端（§7.1 本地 M3U 上传）。
async fn upload_m3u(headers: HeaderMap, mut multipart: Multipart) -> Html<String> {
    let sid = admin_sid(&headers);
    let mut name = "upload".to_string();
    let mut file_bytes: Vec<u8> = Vec::new();
    let mut filename = "upload.m3u".to_string();
    while let Ok(Some(field)) = multipart.next_field().await {
        let fname = field.name().unwrap_or("").to_string();
        if fname == "name" {
            if let Ok(t) = field.text().await {
                name = t;
            }
        } else if fname == "file" {
            filename = field.file_name().unwrap_or("upload.m3u").to_string();
            if let Ok(b) = field.bytes().await {
                file_bytes = b.to_vec();
            }
        }
    }
    let part = reqwest::multipart::Part::bytes(file_bytes).file_name(filename);
    let form = reqwest::multipart::Form::new()
        .text("name", name)
        .part("file", part);
    let url = format!("{}/api/v1/admin/sources/upload", api_base());
    let mut req = client().post(&url).multipart(form);
    if let Some(s) = sid {
        req = req.header("Cookie", format!("iptv_admin_sid={s}"));
    }
    let Ok(res) = req.send().await else {
        return Html("<!DOCTYPE html><html><body><p>backend unreachable</p><p><a href=\"/admin/sources\">Back</a></p></body></html>".to_string());
    };
    let env: serde_json::Value = res.json().await.unwrap_or(serde_json::Value::Null);
    let code = env.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        let msg = env
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("upload failed");
        return Html(format!("<!DOCTYPE html><html><body><p>{msg}</p><p><a href=\"/admin/sources\">Back</a></p></body></html>"));
    }
    let d = env.get("data").cloned().unwrap_or(serde_json::Value::Null);
    let parsed = d
        .get("channels_parsed")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let imported = d
        .get("channels_imported")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let errors = d
        .get("errors")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    Html(format!(
        "<!DOCTYPE html><html><head><link rel=\"stylesheet\" href=\"/design.css\"></head><body><div class=\"page\"><div class=\"card\"><h1>M3U import report</h1><p>parsed {parsed} · imported {imported} · line errors {errors}</p><p><a class=\"btn btn-filled\" href=\"/admin/sources\">Back to Sources</a></p></div></div></body></html>"
    ))
}

async fn trigger_sync(headers: HeaderMap, Path(id): Path<i64>) -> (HeaderMap, Redirect) {
    let r = backend_json(
        reqwest::Method::POST,
        &format!("/api/v1/admin/sources/{id}/sync"),
        Some(serde_json::json!({})),
        admin_sid(&headers).as_deref(),
    )
    .await;
    ok_redirect("/admin/sources", &r)
}

#[derive(Deserialize)]
struct GroupForm {
    name: String,
    sort_weight: Option<String>,
    icon: Option<String>,
    is_default: Option<String>,
}

async fn create_group(headers: HeaderMap, Form(f): Form<GroupForm>) -> (HeaderMap, Redirect) {
    let body = serde_json::json!({
        "name": f.name,
        "sort_weight": f.sort_weight.and_then(|v| v.parse::<i32>().ok()),
        "icon": opt_str(&f.icon),
        "is_default": f.is_default.as_deref() == Some("true"),
    });
    let r = backend_json(
        reqwest::Method::POST,
        "/api/v1/admin/groups",
        Some(body),
        admin_sid(&headers).as_deref(),
    )
    .await;
    ok_redirect("/admin/groups", &r)
}

#[derive(Deserialize)]
struct EpgSourceForm {
    name: String,
    url: String,
}

async fn create_epg_source(
    headers: HeaderMap,
    Form(f): Form<EpgSourceForm>,
) -> (HeaderMap, Redirect) {
    let body = serde_json::json!({ "name": f.name, "url": f.url });
    let r = backend_json(
        reqwest::Method::POST,
        "/api/v1/admin/epg/sources",
        Some(body),
        admin_sid(&headers).as_deref(),
    )
    .await;
    ok_redirect("/admin/epg", &r)
}

async fn kick_session(headers: HeaderMap, Path(id): Path<i64>) -> (HeaderMap, Redirect) {
    let r = backend_json(
        reqwest::Method::DELETE,
        &format!("/api/v1/admin/sessions/{id}"),
        None,
        admin_sid(&headers).as_deref(),
    )
    .await;
    ok_redirect("/admin/sessions", &r)
}

#[derive(Deserialize)]
struct PlanForm {
    name: String,
    max_concurrent_sessions: i32,
    max_bound_devices: i32,
    channel_groups: Option<String>,
    duration_days: i32,
    price_cents: i64,
    currency: Option<String>,
}

async fn create_plan(headers: HeaderMap, Form(f): Form<PlanForm>) -> (HeaderMap, Redirect) {
    let body = serde_json::json!({
        "name": f.name,
        "max_concurrent_sessions": f.max_concurrent_sessions,
        "max_bound_devices": f.max_bound_devices,
        "channel_groups": split_csv(&f.channel_groups),
        "duration_days": f.duration_days,
        "price_cents": f.price_cents,
        "currency": opt_str(&f.currency),
    });
    let r = backend_json(
        reqwest::Method::POST,
        "/api/v1/admin/plans",
        Some(body),
        admin_sid(&headers).as_deref(),
    )
    .await;
    ok_redirect("/admin/plans", &r)
}

#[derive(Deserialize)]
struct AdminForm {
    username: String,
    password: String,
    role: String,
    totp_required: Option<String>,
}

async fn create_admin(headers: HeaderMap, Form(f): Form<AdminForm>) -> (HeaderMap, Redirect) {
    let body = serde_json::json!({
        "username": f.username,
        "password": f.password,
        "role": f.role,
        "totp_required": f.totp_required.as_deref() == Some("true"),
    });
    let r = backend_json(
        reqwest::Method::POST,
        "/api/v1/admin/admins",
        Some(body),
        admin_sid(&headers).as_deref(),
    )
    .await;
    ok_redirect("/admin/admins", &r)
}

#[derive(Deserialize)]
struct SettingForm {
    key: String,
    value: String,
}

async fn update_settings(headers: HeaderMap, Form(f): Form<SettingForm>) -> (HeaderMap, Redirect) {
    let value: serde_json::Value =
        serde_json::from_str(&f.value).unwrap_or_else(|_| serde_json::json!(f.value));
    let body = serde_json::json!({ f.key: value });
    let r = backend_json(
        reqwest::Method::PATCH,
        "/api/v1/admin/settings",
        Some(body),
        admin_sid(&headers).as_deref(),
    )
    .await;
    ok_redirect("/admin/settings", &r)
}
