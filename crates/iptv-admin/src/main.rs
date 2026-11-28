//! 管理后台二进制（开发文档 §5）。
//!
//! Leptos SSR + hydrate：`leptos_routes` + `shell()`；hydrate 产物由
//! cargo-leptos 构建（见 Dockerfile / release.yml），打进镜像与 Release
//! 包的 `site/`，运行时由 `LEPTOS_SITE_ROOT` 指向（默认 `target/site`）。
//! 输出名 `iptv-admin` 见 `crates/iptv-admin/Cargo.toml` 的
//! `[package.metadata.leptos]`（此前缺失导致 shell 引用空名的 /pkg/.js）。
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
    extract::{Form, Multipart, Path, Query, Request},
    http::{header, HeaderMap, Method, StatusCode},
    middleware::Next,
    response::{Html, IntoResponse, Json, Redirect, Response},
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
    // m22：启动时读库装入 i18n 覆盖（与 API main.rs 同款；后台此前只在
    // 访问覆盖页的 server fn 里惰性装入，重启后失效。读库失败只记警告，
    // 不阻塞启动；无覆盖时 FTL 兜底）。
    load_i18n_overrides().await;
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
        .route("/admin/users/{id}/update", post(update_user))
        .route("/admin/users/{id}/delete", post(delete_user))
        .route("/admin/tokens", post(create_token))
        .route("/admin/tokens/{id}/ticket", post(create_login_ticket))
        .route("/admin/tokens/{id}/revoke", post(revoke_token_sessions))
        .route("/admin/tokens/{id}/update", post(update_token))
        .route("/admin/tokens/{id}/delete", post(delete_token))
        .route("/admin/channels", post(create_channel))
        .route("/admin/channels/{id}/delete", post(delete_channel))
        .route("/admin/sources", post(create_source))
        .route("/admin/sources/upload", post(upload_m3u))
        // M16：上传进度查询代理（浏览器轮询用，见 upload_task_status）。
        .route("/admin/upload-tasks/{id}", get(upload_task_status))
        .route("/admin/sources/paste", post(paste_m3u))
        .route("/admin/sources/{id}/sync", post(trigger_sync))
        .route("/admin/sources/{id}/update", post(update_source))
        .route("/admin/sources/{id}/delete", post(delete_source))
        .route("/admin/groups", post(create_group))
        .route("/admin/groups/{id}/update", post(update_group))
        .route("/admin/groups/{id}/delete", post(delete_group))
        .route("/admin/epg/sources", post(create_epg_source))
        .route("/admin/epg/sources/{id}/update", post(update_epg_source))
        .route("/admin/epg/sources/{id}/delete", post(delete_epg_source))
        .route("/admin/epg/maps", post(upsert_epg_channel_map))
        .route("/admin/epg/maps/{id}/delete", post(delete_epg_channel_map))
        .route("/admin/sessions/{id}/kick", post(kick_session))
        .route("/admin/plans", post(create_plan))
        .route("/admin/plans/{id}/update", post(update_plan))
        .route("/admin/plans/{id}/delete", post(delete_plan))
        .route("/admin/admins", post(create_admin))
        .route("/admin/admins/{id}/update", post(update_admin))
        .route("/admin/admins/{id}/delete", post(delete_admin))
        .route("/admin/admins/{id}/reset-totp", post(reset_admin_totp))
        .route("/admin/settings", post(update_settings))
        .route("/admin/i18n-overrides", post(upsert_i18n_override))
        .route("/admin/i18n-overrides/delete", post(delete_i18n_override))
        .leptos_routes(&leptos_options, routes, {
            let leptos_options = leptos_options.clone();
            move || shell(leptos_options.clone())
        })
        // B4：leptos_routes 只注册 leptos 路由与 server fn，不带静态文件
        // 服务；/pkg/*.js 与 /pkg/*.wasm 必须由这个 fallback 经 ServeDir
        // 从 LEPTOS_SITE_ROOT 指出，否则 hydrate 产物永远 404（空体）。
        .fallback(leptos_axum::file_and_error_handler(shell))
        // m14：所有 POST 先过 CSRF 中间件（浏览器→SSR 的第一道防线，
        // 见 `csrf_middleware`）；再由各 handler 把 token 转发给 API。
        .layer(axum::middleware::from_fn(csrf_middleware))
        .with_state(leptos_options);
    tracing::info!("admin listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app.into_make_service())
        .await
        .unwrap();
}

/// m22：启动时把 `i18n_override` 表装入进程内覆盖表（与 API main.rs
/// 同款；`DATABASE_URL` 未设时默认 `sqlite://iptv.db`，与 API 的
/// `Config::from_env` 一致）。读库/连库失败只记警告，不阻塞启动。
async fn load_i18n_overrides() {
    let database_url =
        std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite://iptv.db".to_string());
    load_i18n_overrides_from(&database_url).await;
}

/// m22：`load_i18n_overrides` 的可测形态（单测传临时库 URL）。
async fn load_i18n_overrides_from(database_url: &str) {
    match iptv_store::Store::connect(database_url).await {
        Ok(store) => match store.i18n_overrides_map().await {
            Ok(map) => {
                let n = map.len();
                iptv_i18n::I18n::set_overrides(map);
                tracing::info!("loaded {n} i18n overrides");
            }
            Err(e) => tracing::warn!("failed to load i18n overrides: {e}"),
        },
        Err(e) => tracing::warn!("failed to connect database for i18n overrides: {e}"),
    }
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

/// m14：入站请求里的双提交 token（`iptv_csrf` Cookie 值），供转发给 API 用。
fn csrf_token(headers: &HeaderMap) -> Option<String> {
    cookie_header(headers).and_then(|h| cookie_value(&h, "iptv_csrf"))
}

/// m14：签发一个随机双提交 token（256 bit 随机，hex 编码）。
fn new_csrf_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let mut s = String::with_capacity(64);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// m14 双提交 Cookie（§13）：与门户 `main.rs` 的 `csrf_middleware` 同款，
/// 差别只在会话 Cookie 名（`iptv_admin_sid`）与有效期（12h，跟管理员会话
/// 的 43200s 一致）。浏览器→SSR 的纯 Axum POST 表单（全部后台 CRUD）发不
/// 出自定义头，故走 Origin 比对 + `Sec-Fetch-Site: cross-site` 直接拒；
/// 带 `X-CSRF-Token` 头的请求走双提交比对；响应时给有会话无 token 的
/// 浏览器补发 token（旧会话无需重新登录）。退出时 `logout` 清掉该 Cookie。
async fn csrf_middleware(req: Request, next: Next) -> Response {
    let headers = req.headers().clone();
    if matches!(
        *req.method(),
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    ) {
        if let Some(token) = headers.get("x-csrf-token").and_then(|v| v.to_str().ok()) {
            let ok = csrf_token(&headers).is_some_and(|c| {
                !token.is_empty() && {
                    let (a, b) = (token.as_bytes(), c.as_bytes());
                    a.len() == b.len()
                        && a.iter()
                            .zip(b.iter())
                            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
                            == 0
                }
            });
            if !ok {
                return (StatusCode::FORBIDDEN, "cross-site write rejected").into_response();
            }
        } else {
            if headers
                .get("sec-fetch-site")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.trim().eq_ignore_ascii_case("cross-site"))
            {
                return (StatusCode::FORBIDDEN, "cross-site write rejected").into_response();
            }
            if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
                let origin_host = origin.split("://").nth(1).unwrap_or("");
                let host_ok = headers
                    .get("x-forwarded-host")
                    .and_then(|v| v.to_str().ok())
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| s.trim())
                    .or_else(|| headers.get(header::HOST).and_then(|v| v.to_str().ok()))
                    .is_some_and(|host| {
                        origin_host.trim_end_matches('/') == host.trim_end_matches('/')
                    });
                if !host_ok {
                    return (StatusCode::FORBIDDEN, "cross-site write rejected").into_response();
                }
            }
        }
    }
    let had_session = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| cookie_value(c, "iptv_admin_sid"))
        .is_some();
    let had_csrf = csrf_token(&headers).is_some();
    let mut resp = next.run(req).await;
    if had_session && !had_csrf {
        let token = new_csrf_token();
        if let Ok(v) = header::HeaderValue::from_str(&format!(
            "iptv_csrf={token}; Path=/; SameSite=Lax; Max-Age=43200"
        )) {
            resp.headers_mut().append(header::SET_COOKIE, v);
        }
    }
    resp
}

struct BackendResult {
    ok: bool,
    data: serde_json::Value,
    set_cookie: Option<String>,
    error: Option<String>,
}

/// 从入站请求头取原始客户端 IP（网关写好的 X-Forwarded-For 首项，或
/// X-Real-IP），调 API 时原样转发，API 才能按真实 IP 做限流分桶（M2）。
fn forwarded_for(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            headers
                .get("x-real-ip")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
}

/// m14：`csrf` 为入站请求的 `iptv_csrf` Cookie 值：有则同时以
/// `X-CSRF-Token` 头 + `Cookie` 头里的 `iptv_csrf` 发给 API，走双提交
/// 比对；无则只发会话 Cookie，API 退回 Origin 兜底（管理员登录尚无
/// token 时就是这个形态）。
/// m22：另转发浏览器的 `iptv_locale` Cookie 与 `Accept-Language`，API
/// 的错误本地化中间件才能按管理员真实语言渲染 message（此前只转
/// 发会话/Cookie，API 一律回退系统默认简体中文）。
async fn backend_json(
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
    sid: Option<&str>,
    xff: Option<&str>,
    csrf: Option<&str>,
    headers: &HeaderMap,
) -> BackendResult {
    let url = format!("{}{path}", api_base());
    let mut req = client().request(method, &url);
    if let Some(b) = body {
        req = req.json(&b);
    }
    let mut cookie = String::new();
    if let Some(s) = sid {
        cookie.push_str(&format!("iptv_admin_sid={s}"));
    }
    if let Some(c) = csrf {
        if !cookie.is_empty() {
            cookie.push_str("; ");
        }
        cookie.push_str(&format!("iptv_csrf={c}"));
        req = req.header("x-csrf-token", c);
    }
    // m22：语言转发（Cookie 优先，Accept-Language 兜底；API 中间件按
    // Cookie `iptv_locale` → Accept-Language → 默认的顺序协商）。
    let locale = cookie_header(headers)
        .and_then(|h| cookie_value(&h, "iptv_locale"))
        .filter(|l| iptv_i18n::normalize_locale(l).is_some());
    if let Some(l) = &locale {
        if !cookie.is_empty() {
            cookie.push_str("; ");
        }
        cookie.push_str(&format!("iptv_locale={l}"));
    }
    if let Some(al) = headers
        .get(header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.trim().is_empty())
    {
        req = req.header(header::ACCEPT_LANGUAGE, al);
    }
    if !cookie.is_empty() {
        req = req.header("Cookie", cookie);
    }
    if let Some(x) = xff {
        req = req.header("X-Forwarded-For", x);
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

/// m22：`?error=` 跳转前用当前 locale 把 SSR 自产 / 任务英文串翻成
/// 本地文案（API 已本地化的 message 原样透传，见 `localize_backend_error`）。
fn ok_redirect(base: &str, r: &BackendResult, headers: &HeaderMap) -> (HeaderMap, Redirect) {
    match &r.error {
        Some(e) => {
            let locale = locale_from_headers(headers);
            let e = localize_backend_error(&locale, e);
            redirect_with_cookies(&format!("{base}?error={}", pct_encode(&e)), &[])
        }
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

/// 从请求头 Cookie 读 `iptv_locale`，非法/缺失时回退 `DEFAULT_LOCALE`（m13）。
fn locale_from_headers(headers: &HeaderMap) -> String {
    cookie_header(headers)
        .and_then(|h| cookie_value(&h, "iptv_locale"))
        .filter(|l| iptv_i18n::normalize_locale(l).is_some())
        .unwrap_or_else(|| iptv_i18n::DEFAULT_LOCALE.to_string())
}

/// N1：main.rs 内的文案走 FTL（与 lib.rs 的 `t_or` 同 key 体系；此处无
/// `LocaleCtx`，locale 由调用方从 Cookie 取后传入）。缺 key 时回退英文
/// 兜底并打警告（与 `iptv-ui::t_or` 同规则）；`cargo xtask i18n-check`
/// 的扫描器覆盖本文件的 `t_or(`/`t_args_or(` 调用（locale 在首参）。
fn t_or(locale: &str, key: &str, fallback: &str) -> String {
    let v = iptv_i18n::I18n::new().t_simple(locale, key);
    if v == key {
        tracing::warn!("i18n 缺 key：`{key}`（locale `{locale}`），回退英文兜底");
        fallback.to_string()
    } else {
        v
    }
}

/// N1：带参数的文案（同 `t_or`，`args` 为 `($name, value)` 对）。
fn t_args_or(locale: &str, key: &str, args: &[(&str, String)], fallback: String) -> String {
    let i18n = iptv_i18n::I18n::new();
    let mut fa = fluent_bundle::FluentArgs::new();
    for (k, v) in args {
        fa.set(*k, v.clone());
    }
    let v = i18n.t(locale, key, Some(&fa));
    if v == key {
        tracing::warn!("i18n 缺 key：`{key}`（locale `{locale}`），回退英文兜底");
        fallback
    } else {
        v
    }
}

/// N1：`backend_json` 在拿不到后端 message 时填的英文哨兵（`backend
/// unreachable` 等）在展示时翻成本地文案；其余 message 已由 API 的
/// 本地化中间件翻过，原样透传。
/// m22：覆盖面从 7 串扩到全量——新增上传任务 `p.error` 的 6 条英文串
/// （`run_upload_task` 的失败分支，见 `upload_task_status` 的 SSR 侧
/// 映射；`handlers/admin.rs` 里的错误串本身不动）。带 `: {detail}` 后
/// 缀的三条走前缀匹配，detail 原样保留在 `$detail` 占位符里。
/// 注意：key 必须写字面量（不走 `(key, fallback)` 元组），`cargo xtask
/// i18n-check` 的引用扫描只认字面量 key。
fn localize_backend_error(locale: &str, msg: &str) -> String {
    // 带 detail 后缀的三条：前缀匹配（精确串里没有这三条的全形）。
    if let Some(detail) = msg.strip_prefix("read source failed: ") {
        return t_args_or(
            locale,
            "admin.upload-task-read-source-failed",
            &[("detail", detail.to_string())],
            format!("Failed to read source: {detail}"),
        );
    }
    if let Some(detail) = msg.strip_prefix("create source failed: ") {
        return t_args_or(
            locale,
            "admin.upload-task-create-source-failed",
            &[("detail", detail.to_string())],
            format!("Failed to create source: {detail}"),
        );
    }
    if let Some(detail) = msg.strip_prefix("import channels failed: ") {
        return t_args_or(
            locale,
            "admin.upload-task-import-failed",
            &[("detail", detail.to_string())],
            format!("Failed to import channels: {detail}"),
        );
    }
    match msg {
        "backend unreachable" => t_or(
            locale,
            "admin.onetime-error-backend-unreachable",
            "Backend unreachable",
        ),
        "login failed" => t_or(locale, "admin.onetime-error-login-failed", "Sign-in failed"),
        "create token failed" => t_or(
            locale,
            "admin.onetime-error-create-token-failed",
            "Failed to create token",
        ),
        "create ticket failed" => t_or(
            locale,
            "admin.onetime-error-create-ticket-failed",
            "Failed to create login ticket",
        ),
        "reset totp failed" => t_or(
            locale,
            "admin.onetime-error-reset-totp-failed",
            "Failed to reset TOTP",
        ),
        "upload failed" => t_or(locale, "admin.onetime-error-upload-failed", "Upload failed"),
        "read upload failed" => t_or(
            locale,
            "admin.upload-task-read-failed",
            "Failed to read upload",
        ),
        "parse upload failed" => t_or(
            locale,
            "admin.upload-task-parse-failed",
            "Failed to parse upload",
        ),
        "source not found" => t_or(
            locale,
            "admin.upload-task-source-not-found",
            "Source not found",
        ),
        "error" => t_or(locale, "errors.e50000", "Internal server error"),
        _ => msg.to_string(),
    }
}

/// N1：错误页（message + 返回链接，文案走 FTL；`back_key` 如
/// `common.back`，`back_href` 为返回路径）。
fn error_page(
    locale: &str,
    msg: &str,
    back_href: &str,
    back_key: &str,
    back_fallback: &str,
) -> Html<String> {
    let back = t_or(locale, back_key, back_fallback);
    Html(format!(
        "<!DOCTYPE html><html lang=\"{locale}\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><link rel=\"stylesheet\" href=\"/design.css\"></head><body><div class=\"page\"><div class=\"card\"><p>{msg}</p><p><a class=\"btn btn-filled\" href=\"{back_href}\">{back}</a></p></div></div></body></html>"
    ))
}

/// 一次性展示页（Token / 票据 / TOTP 密钥只显示一次，§5.2）。
/// `back` 为返回链接（路径，文案）。
fn onetime_page(
    title: &str,
    rows: &[(String, String)],
    qr_png: Option<&str>,
    back: (&str, &str),
    locale: &str,
) -> Html<String> {
    // N1：警告语与二维码 alt 文案走 FTL（标题/行标签/返回链接由调用方
    // 在传参前翻好）。
    let warning = t_or(
        locale,
        "admin.onetime-secret-warning",
        "This secret is shown only once. Save it now.",
    );
    let qr_alt = t_or(locale, "admin.onetime-qr-code", "QR code");
    let mut body = format!(
        "<!DOCTYPE html><html lang=\"{locale}\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><link rel=\"stylesheet\" href=\"/design.css\"><title>{title}</title></head><body><div class=\"page\"><div class=\"card\"><h1>{title}</h1><p style=\"color:var(--c-destructive)\">{warning}</p>"
    );
    for (k, v) in rows {
        body.push_str(&format!(
            "<p><strong>{k}:</strong> <span class=\"mono\">{v}</span></p>"
        ));
    }
    if let Some(qr) = qr_png {
        body.push_str(&format!(
            "<p><img class=\"qr-img\" src=\"{qr}\" alt=\"{qr_alt}\"></p>"
        ));
    }
    body.push_str(&format!(
        "<p><a class=\"btn btn-filled\" href=\"{}\">{}</a></p></div></div></body></html>",
        back.0, back.1
    ));
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

async fn login(headers: HeaderMap, Form(form): Form<LoginForm>) -> (HeaderMap, Redirect) {
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
        forwarded_for(&headers).as_deref(),
        csrf_token(&headers).as_deref(),
        &headers,
    )
    .await;
    if !r.ok {
        // N1：登录失败的兜底文案走 FTL（后端 message 已由 API 本地化
        // 中间件翻过，原样透传；只有哨兵才在这里翻）。
        let locale = locale_from_headers(&headers);
        let e = r
            .error
            .map(|e| localize_backend_error(&locale, &e))
            .unwrap_or_else(|| {
                t_or(
                    &locale,
                    "admin.onetime-error-login-failed",
                    "Sign-in failed",
                )
            });
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
            "iptv_csrf=; Path=/; SameSite=Lax; Max-Age=0".to_string(),
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
        forwarded_for(&headers).as_deref(),
        csrf_token(&headers).as_deref(),
        &headers,
    )
    .await;
    ok_redirect("/admin/users", &r, &headers)
}

/// M19a：行内编辑表单提交 → PATCH /api/v1/admin/users/{id}。
/// 空串字段一律按“不改”处理（opt_str → null，后端 None 即保持原值）。
#[derive(Deserialize)]
struct UserUpdateForm {
    display_name: Option<String>,
    remark: Option<String>,
    plan_id: Option<String>,
    status: Option<String>,
    expires_at: Option<String>,
}

async fn update_user(
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(f): Form<UserUpdateForm>,
) -> (HeaderMap, Redirect) {
    let body = serde_json::json!({
        "display_name": opt_str(&f.display_name),
        "remark": opt_str(&f.remark),
        "plan_id": f.plan_id.and_then(|v| v.parse::<i64>().ok()),
        "status": opt_str(&f.status),
        "expires_at": opt_str(&f.expires_at),
    });
    let r = backend_json(
        reqwest::Method::PATCH,
        &format!("/api/v1/admin/users/{id}"),
        Some(body),
        admin_sid(&headers).as_deref(),
        forwarded_for(&headers).as_deref(),
        csrf_token(&headers).as_deref(),
        &headers,
    )
    .await;
    ok_redirect("/admin/users", &r, &headers)
}

async fn delete_user(headers: HeaderMap, Path(id): Path<i64>) -> (HeaderMap, Redirect) {
    let r = backend_json(
        reqwest::Method::DELETE,
        &format!("/api/v1/admin/users/{id}"),
        None,
        admin_sid(&headers).as_deref(),
        forwarded_for(&headers).as_deref(),
        csrf_token(&headers).as_deref(),
        &headers,
    )
    .await;
    ok_redirect("/admin/users", &r, &headers)
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
        forwarded_for(&headers).as_deref(),
        csrf_token(&headers).as_deref(),
        &headers,
    )
    .await;
    if !r.ok {
        let locale = locale_from_headers(&headers);
        let e = r
            .error
            .map(|e| localize_backend_error(&locale, &e))
            .unwrap_or_else(|| {
                t_or(
                    &locale,
                    "admin.onetime-error-create-token-failed",
                    "Failed to create token",
                )
            });
        return error_page(&locale, &e, "/admin/tokens", "common.back", "Back");
    }
    let locale = locale_from_headers(&headers);
    let rows = vec![
        (
            t_or(&locale, "admin.onetime-token-id", "Token ID"),
            r.data.get("id").map(|v| v.to_string()).unwrap_or_default(),
        ),
        (
            t_or(&locale, "admin.onetime-token-value", "Token (shown once)"),
            r.data
                .get("token")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        ),
        (
            t_or(&locale, "admin.onetime-portal-url", "Portal URL"),
            r.data
                .get("portal_url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        ),
    ];
    onetime_page(
        &t_or(&locale, "admin.onetime-token-created", "Token created"),
        &rows,
        r.data.get("qr_png").and_then(|v| v.as_str()),
        (
            "/admin/tokens",
            &t_or(&locale, "admin.onetime-back-tokens", "Back to Tokens"),
        ),
        &locale,
    )
}

/// M19b：行内编辑表单提交 → PATCH /api/v1/admin/tokens/{id}。
#[derive(Deserialize)]
struct TokenUpdateForm {
    name: Option<String>,
    max_concurrent_sessions: Option<String>,
    max_bound_devices: Option<String>,
    status: Option<String>,
    expires_at: Option<String>,
    allowed_groups: Option<String>,
}

async fn update_token(
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(f): Form<TokenUpdateForm>,
) -> (HeaderMap, Redirect) {
    let body = serde_json::json!({
        "name": opt_str(&f.name),
        "max_concurrent_sessions": f.max_concurrent_sessions.and_then(|v| v.parse::<i32>().ok()),
        "max_bound_devices": f.max_bound_devices.and_then(|v| v.parse::<i32>().ok()),
        "status": opt_str(&f.status),
        "expires_at": opt_str(&f.expires_at),
        "allowed_groups": split_csv(&f.allowed_groups),
    });
    let r = backend_json(
        reqwest::Method::PATCH,
        &format!("/api/v1/admin/tokens/{id}"),
        Some(body),
        admin_sid(&headers).as_deref(),
        forwarded_for(&headers).as_deref(),
        csrf_token(&headers).as_deref(),
        &headers,
    )
    .await;
    ok_redirect("/admin/tokens", &r, &headers)
}

async fn delete_token(headers: HeaderMap, Path(id): Path<i64>) -> (HeaderMap, Redirect) {
    let r = backend_json(
        reqwest::Method::DELETE,
        &format!("/api/v1/admin/tokens/{id}"),
        None,
        admin_sid(&headers).as_deref(),
        forwarded_for(&headers).as_deref(),
        csrf_token(&headers).as_deref(),
        &headers,
    )
    .await;
    ok_redirect("/admin/tokens", &r, &headers)
}

async fn create_login_ticket(headers: HeaderMap, Path(id): Path<i64>) -> Html<String> {
    let r = backend_json(
        reqwest::Method::POST,
        &format!("/api/v1/admin/tokens/{id}/login-ticket"),
        Some(serde_json::json!({})),
        admin_sid(&headers).as_deref(),
        forwarded_for(&headers).as_deref(),
        csrf_token(&headers).as_deref(),
        &headers,
    )
    .await;
    if !r.ok {
        let locale = locale_from_headers(&headers);
        let e = r
            .error
            .map(|e| localize_backend_error(&locale, &e))
            .unwrap_or_else(|| {
                t_or(
                    &locale,
                    "admin.onetime-error-create-ticket-failed",
                    "Failed to create login ticket",
                )
            });
        return error_page(&locale, &e, "/admin/tokens", "common.back", "Back");
    }
    let locale = locale_from_headers(&headers);
    let rows = vec![
        (
            t_or(
                &locale,
                "admin.onetime-portal-url-ticket",
                "Portal URL (60s, single-use)",
            ),
            r.data
                .get("portal_url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        ),
        (
            t_or(&locale, "admin.onetime-ticket", "Ticket"),
            r.data
                .get("ticket")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        ),
    ];
    onetime_page(
        &t_or(&locale, "admin.onetime-login-ticket", "Login ticket"),
        &rows,
        r.data.get("qr_png").and_then(|v| v.as_str()),
        (
            "/admin/tokens",
            &t_or(&locale, "admin.onetime-back-tokens", "Back to Tokens"),
        ),
        &locale,
    )
}

async fn revoke_token_sessions(headers: HeaderMap, Path(id): Path<i64>) -> (HeaderMap, Redirect) {
    let r = backend_json(
        reqwest::Method::POST,
        &format!("/api/v1/admin/tokens/{id}/revoke-sessions"),
        Some(serde_json::json!({})),
        admin_sid(&headers).as_deref(),
        forwarded_for(&headers).as_deref(),
        csrf_token(&headers).as_deref(),
        &headers,
    )
    .await;
    ok_redirect("/admin/tokens", &r, &headers)
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
        forwarded_for(&headers).as_deref(),
        csrf_token(&headers).as_deref(),
        &headers,
    )
    .await;
    ok_redirect("/admin/channels", &r, &headers)
}

/// M19c：行内编辑表单提交 → PATCH /api/v1/admin/sources/{id}。
/// enabled 是 checkbox：未勾选即不提交，proxy 按 false 送（表单按当前值预选）。
#[derive(Deserialize)]
struct SourceUpdateForm {
    name: Option<String>,
    url: Option<String>,
    sync_interval_sec: Option<String>,
    epg_url: Option<String>,
    on_missing: Option<String>,
    enabled: Option<String>,
}

async fn update_source(
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(f): Form<SourceUpdateForm>,
) -> (HeaderMap, Redirect) {
    let body = serde_json::json!({
        "name": opt_str(&f.name),
        "url": opt_str(&f.url),
        "sync_interval_sec": f.sync_interval_sec.and_then(|v| v.parse::<i64>().ok()),
        "epg_url": opt_str(&f.epg_url),
        "on_missing": opt_str(&f.on_missing),
        "enabled": f.enabled.as_deref() == Some("true"),
    });
    let r = backend_json(
        reqwest::Method::PATCH,
        &format!("/api/v1/admin/sources/{id}"),
        Some(body),
        admin_sid(&headers).as_deref(),
        forwarded_for(&headers).as_deref(),
        csrf_token(&headers).as_deref(),
        &headers,
    )
    .await;
    ok_redirect("/admin/sources", &r, &headers)
}

async fn delete_source(headers: HeaderMap, Path(id): Path<i64>) -> (HeaderMap, Redirect) {
    let r = backend_json(
        reqwest::Method::DELETE,
        &format!("/api/v1/admin/sources/{id}"),
        None,
        admin_sid(&headers).as_deref(),
        forwarded_for(&headers).as_deref(),
        csrf_token(&headers).as_deref(),
        &headers,
    )
    .await;
    ok_redirect("/admin/sources", &r, &headers)
}

async fn delete_channel(headers: HeaderMap, Path(id): Path<i64>) -> (HeaderMap, Redirect) {
    let r = backend_json(
        reqwest::Method::DELETE,
        &format!("/api/v1/admin/channels/{id}"),
        None,
        admin_sid(&headers).as_deref(),
        forwarded_for(&headers).as_deref(),
        csrf_token(&headers).as_deref(),
        &headers,
    )
    .await;
    ok_redirect("/admin/channels", &r, &headers)
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
        forwarded_for(&headers).as_deref(),
        csrf_token(&headers).as_deref(),
        &headers,
    )
    .await;
    ok_redirect("/admin/sources", &r, &headers)
}

/// POST /admin/sources/paste：粘贴 M3U 文本导入（m18，表单字段见
/// `lib.rs` 的粘贴表单：`name`/`content`），转发给后端
/// `POST /api/v1/admin/sources/paste`。
/// 注：m14 修复时补——路由与表单已在工作树里，handler 缺失导致
/// `iptv-admin` 整个编不过；此处只按 `create_source` 同款补一个
/// 最小转发，m18 的展示/文案仍归原负责人。
/// N8（v0.1.6）：后端粘贴已改为异步（与上传同款 `{ task_id }`），
/// 故成功时不再 `ok_redirect` 回源列表，而是走 `m3u_report_page`
///（其 task_id 分支出上传进度页，JS 轮询任务进度）；失败时出本地化
/// 错误页（与上传 handler 同款）。
#[derive(Deserialize)]
struct PasteForm {
    name: Option<String>,
    content: String,
}

async fn paste_m3u(headers: HeaderMap, Form(f): Form<PasteForm>) -> Html<String> {
    let body = serde_json::json!({
        "name": opt_str(&f.name),
        "content": f.content,
    });
    let r = backend_json(
        reqwest::Method::POST,
        "/api/v1/admin/sources/paste",
        Some(body),
        admin_sid(&headers).as_deref(),
        forwarded_for(&headers).as_deref(),
        csrf_token(&headers).as_deref(),
        &headers,
    )
    .await;
    if let Some(e) = &r.error {
        let locale = locale_from_headers(&headers);
        let msg = localize_backend_error(&locale, e);
        return error_page(&locale, &msg, "/admin/sources", "common.back", "Back");
    }
    // `backend_json` 已把信封的 `data` 拆出，`m3u_report_page` 要的是
    // 整个信封，在此包回 `{ data }`（与上传 handler 传整信封一致）。
    let env = serde_json::json!({ "data": r.data });
    m3u_report_page(&headers, &env)
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
    // m14：multipart 转发也带双提交（与 backend_json 同款）。
    // m22：同 backend_json，另转发 `iptv_locale` Cookie 与 Accept-Language。
    let mut cookie = String::new();
    if let Some(s) = sid {
        cookie.push_str(&format!("iptv_admin_sid={s}"));
    }
    if let Some(c) = csrf_token(&headers) {
        if !cookie.is_empty() {
            cookie.push_str("; ");
        }
        cookie.push_str(&format!("iptv_csrf={c}"));
        req = req.header("x-csrf-token", c);
    }
    if let Some(l) = cookie_header(&headers)
        .and_then(|h| cookie_value(&h, "iptv_locale"))
        .filter(|l| iptv_i18n::normalize_locale(l).is_some())
    {
        if !cookie.is_empty() {
            cookie.push_str("; ");
        }
        cookie.push_str(&format!("iptv_locale={l}"));
    }
    if let Some(al) = headers
        .get(header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.trim().is_empty())
    {
        req = req.header(header::ACCEPT_LANGUAGE, al);
    }
    if !cookie.is_empty() {
        req = req.header("Cookie", cookie);
    }
    let Ok(res) = req.send().await else {
        // N1：同款 backend unreachable 错误页（文案走 FTL）。
        let locale = locale_from_headers(&headers);
        let msg = t_or(
            &locale,
            "admin.onetime-error-backend-unreachable",
            "Backend unreachable",
        );
        return error_page(&locale, &msg, "/admin/sources", "common.back", "Back");
    };
    let env: serde_json::Value = res.json().await.unwrap_or(serde_json::Value::Null);
    let code = env.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        let locale = locale_from_headers(&headers);
        let msg = env
            .get("message")
            .and_then(|m| m.as_str())
            .map(|m| localize_backend_error(&locale, m))
            .unwrap_or_else(|| {
                t_or(
                    &locale,
                    "admin.onetime-error-upload-failed",
                    "Upload failed",
                )
            });
        return error_page(&locale, &msg, "/admin/sources", "common.back", "Back");
    }
    m3u_report_page(&headers, &env)
}

/// m18：M3U 导入报告页（上传与粘贴共用；标题/行文案/返回链接走 FTL）。
/// `env` 为后端信封：文件上传（M16）与粘贴导入（m18，N8 起异步）均
/// 为异步 `{ data: { task_id } }`，走进度页分支；同步
/// `{ data: M3uImportReport }` 分支仅作兼容保留（现无调用方返回该形）。
fn m3u_report_page(headers: &HeaderMap, env: &serde_json::Value) -> Html<String> {
    let d = env.get("data").cloned().unwrap_or(serde_json::Value::Null);
    // M16/N8：上传与粘贴均走异步任务（data 含 task_id）→ 进度页；
    // 同步 M3uImportReport 形仅作兼容保留。
    if let Some(task_id) = d.get("task_id").and_then(|v| v.as_str()) {
        return upload_progress_page(headers, task_id);
    }
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
    // N1：M3U 导入报告页（标题/行文案/返回链接走 FTL）。
    let locale = locale_from_headers(headers);
    let title = t_or(
        &locale,
        "admin.onetime-m3u-report-title",
        "M3U import report",
    );
    let report = t_args_or(
        &locale,
        "admin.onetime-m3u-report",
        &[
            ("parsed", parsed.to_string()),
            ("imported", imported.to_string()),
            ("errors", errors.to_string()),
        ],
        format!("parsed {parsed} · imported {imported} · line errors {errors}"),
    );
    let back = t_or(&locale, "admin.onetime-back-sources", "Back to Sources");
    Html(format!(
        "<!DOCTYPE html><html lang=\"{locale}\"><head><link rel=\"stylesheet\" href=\"/design.css\"></head><body><div class=\"page\"><div class=\"card\"><h1>{title}</h1><p>{report}</p><p><a class=\"btn btn-filled\" href=\"/admin/sources\">{back}</a></p></div></div></body></html>"
    ))
}

/// M16：异步上传进度页（任务 id 已定；数字由 JS 轮询
/// GET /admin/upload-tasks/{id} 后拼接片段 key 展示）。
fn upload_progress_page(headers: &HeaderMap, task_id: &str) -> Html<String> {
    // N1：上传进度页（标题/文案/返回链接走 FTL；数字由 JS 轮询
    // GET /admin/upload-tasks/{id} 后拼接片段 key 展示，见下）。
    let locale = locale_from_headers(headers);
    let title = t_or(
        &locale,
        "admin.onetime-m3u-report-title",
        "M3U import report",
    );
    let back = t_or(&locale, "admin.onetime-back-sources", "Back to Sources");
    let labels = [
        (
            "queued",
            t_or(
                &locale,
                "admin.upload-task-queued",
                "Queued, waiting to parse…",
            ),
        ),
        (
            "running",
            t_or(&locale, "admin.upload-task-running", "Parsing…"),
        ),
        ("done", t_or(&locale, "admin.upload-task-done", "Done")),
        (
            "failed",
            t_or(
                &locale,
                "admin.onetime-error-upload-failed",
                "Upload failed",
            ),
        ),
        (
            "lines",
            t_or(&locale, "admin.upload-task-lines", "lines read"),
        ),
        (
            "channels",
            t_or(&locale, "admin.upload-task-channels", "channels parsed"),
        ),
        (
            "imported",
            t_or(&locale, "admin.upload-task-imported", "imported"),
        ),
        (
            "errors",
            t_or(&locale, "admin.upload-task-errors", "line errors"),
        ),
    ];
    let mut labels_json = String::from("{");
    for (i, (k, v)) in labels.iter().enumerate() {
        if i > 0 {
            labels_json.push(',');
        }
        labels_json.push_str(&serde_json::to_string(k).unwrap_or_default());
        labels_json.push(':');
        labels_json.push_str(&serde_json::to_string(v).unwrap_or_default());
    }
    labels_json.push('}');
    // M16：进度页模板（__TOKEN__ 由下替换；JS 每秒轮询一次进度代理）。
    const PROGRESS_PAGE: &str = r##"<!DOCTYPE html><html lang="__LOCALE__"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><link rel="stylesheet" href="/design.css"><title>__TITLE__</title></head><body><div class="page"><div class="card"><h1>__TITLE__</h1><div role="progressbar" aria-valuemin="0" aria-valuemax="100" aria-valuenow="0" style="border:1px solid #888;border-radius:6px;overflow:hidden"><div id="upload-bar" style="width:5%;height:12px;background:#2f7cf6"></div></div><p id="upload-status">__QUEUED__</p><p><a class="btn btn-filled" href="/admin/sources">__BACK__</a></p></div></div><script>const TASK_ID = __TASK_ID_JSON__; const L = __LABELS_JSON__; const bar = document.getElementById('upload-bar'); const statusEl = document.getElementById('upload-status'); function fmt(p) { return p.total_lines + ' ' + L.lines + ' · ' + p.channels_parsed + ' ' + L.channels + ' · ' + p.channels_imported + ' ' + L.imported + ' · ' + p.error_count + ' ' + L.errors; } async function poll() { let res; try { res = await fetch('/admin/upload-tasks/' + encodeURIComponent(TASK_ID)); } catch (e) { return; } let env; try { env = await res.json(); } catch (e) { return; } const p = env && env.data; if (!p) { statusEl.textContent = L.failed + ': ' + (env && env.message ? env.message : ''); clearInterval(timer); return; } if (p.status === 'done') { bar.style.width = '100%'; statusEl.textContent = L.done + ' — ' + fmt(p); clearInterval(timer); } else if (p.status === 'failed') { bar.style.width = '100%'; statusEl.textContent = L.failed + ': ' + (p.error || ''); clearInterval(timer); } else if (p.status === 'running') { bar.style.width = Math.min(90, 10 + Math.floor(p.total_lines / 2000)) + '%'; statusEl.textContent = L.running + ' ' + fmt(p); } else { bar.style.width = '5%'; statusEl.textContent = L.queued; } } const timer = setInterval(poll, 1000); poll();</script></body></html>"##;
    Html(
        PROGRESS_PAGE
            .replace("__LOCALE__", &locale)
            .replace("__TITLE__", &title)
            .replace("__QUEUED__", &labels[0].1)
            .replace("__BACK__", &back)
            .replace(
                "__TASK_ID_JSON__",
                &serde_json::to_string(task_id).unwrap_or_default(),
            )
            .replace("__LABELS_JSON__", &labels_json),
    )
}

/// GET /admin/upload-tasks/{id}（M16）：进度查询代理——把浏览器每秒轮询
/// 转发给后端 GET /api/v1/admin/upload-tasks/{id}（带 iptv_admin_sid），
/// 信封原样回透；后端 404 时同样回透（前端据 code 展示失败）。
/// m22：`p.error` 的 6 条英文串由 API 的任务注册表原样存着（`handlers/
/// admin.rs` 的串本身不动），SSR 侧在回透前用当前 locale 调
/// `localize_backend_error` 映射，进度页 JS 展示到的即本地文案；同时
/// 转发 `iptv_locale` Cookie 与 Accept-Language，让后端按管理员语言
/// 渲染信封 message。
async fn upload_task_status(headers: HeaderMap, Path(id): Path<String>) -> Json<serde_json::Value> {
    let locale = locale_from_headers(&headers);
    let url = format!("{}/api/v1/admin/upload-tasks/{id}", api_base());
    let mut req = client().get(&url);
    let mut cookie = String::new();
    if let Some(s) = admin_sid(&headers) {
        cookie.push_str(&format!("iptv_admin_sid={s}"));
    }
    if let Some(l) = cookie_header(&headers)
        .and_then(|h| cookie_value(&h, "iptv_locale"))
        .filter(|l| iptv_i18n::normalize_locale(l).is_some())
    {
        if !cookie.is_empty() {
            cookie.push_str("; ");
        }
        cookie.push_str(&format!("iptv_locale={l}"));
    }
    if !cookie.is_empty() {
        req = req.header("Cookie", cookie);
    }
    if let Some(al) = headers
        .get(header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.trim().is_empty())
    {
        req = req.header(header::ACCEPT_LANGUAGE, al);
    }
    let Ok(res) = req.send().await else {
        let msg = t_or(
            &locale,
            "admin.onetime-error-backend-unreachable",
            "Backend unreachable",
        );
        return Json(serde_json::json!({ "code": 50000, "message": msg, "data": null }));
    };
    let mut v: serde_json::Value = res.json().await.unwrap_or(serde_json::Value::Null);
    // m22：SSR 侧映射任务错误串（JS 只原样展示 `p.error`，见 PROGRESS_PAGE）。
    if let Some(err) = v
        .get("data")
        .and_then(|d| d.get("error"))
        .and_then(|e| e.as_str())
        .map(|s| s.to_string())
    {
        let localized = localize_backend_error(&locale, &err);
        if let Some(data) = v.get_mut("data") {
            data["error"] = serde_json::json!(localized);
        }
    }
    Json(v)
}

/// M19e：行内编辑表单提交 → PATCH /api/v1/admin/groups/{id}。
#[derive(Deserialize)]
struct GroupUpdateForm {
    name: Option<String>,
    sort_weight: Option<String>,
    icon: Option<String>,
    is_default: Option<String>,
}

async fn update_group(
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(f): Form<GroupUpdateForm>,
) -> (HeaderMap, Redirect) {
    let body = serde_json::json!({
        "name": opt_str(&f.name),
        "sort_weight": f.sort_weight.and_then(|v| v.parse::<i32>().ok()),
        "icon": opt_str(&f.icon),
        "is_default": f.is_default.as_deref() == Some("true"),
    });
    let r = backend_json(
        reqwest::Method::PATCH,
        &format!("/api/v1/admin/groups/{id}"),
        Some(body),
        admin_sid(&headers).as_deref(),
        forwarded_for(&headers).as_deref(),
        csrf_token(&headers).as_deref(),
        &headers,
    )
    .await;
    ok_redirect("/admin/groups", &r, &headers)
}

async fn delete_group(headers: HeaderMap, Path(id): Path<i64>) -> (HeaderMap, Redirect) {
    let r = backend_json(
        reqwest::Method::DELETE,
        &format!("/api/v1/admin/groups/{id}"),
        None,
        admin_sid(&headers).as_deref(),
        forwarded_for(&headers).as_deref(),
        csrf_token(&headers).as_deref(),
        &headers,
    )
    .await;
    ok_redirect("/admin/groups", &r, &headers)
}

/// M19f：行内编辑表单提交 → PATCH /api/v1/admin/epg/sources/{id}。
#[derive(Deserialize)]
struct EpgSourceUpdateForm {
    name: Option<String>,
    url: Option<String>,
}

async fn update_epg_source(
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(f): Form<EpgSourceUpdateForm>,
) -> (HeaderMap, Redirect) {
    let body = serde_json::json!({
        "name": opt_str(&f.name),
        "url": opt_str(&f.url),
    });
    let r = backend_json(
        reqwest::Method::PATCH,
        &format!("/api/v1/admin/epg/sources/{id}"),
        Some(body),
        admin_sid(&headers).as_deref(),
        forwarded_for(&headers).as_deref(),
        csrf_token(&headers).as_deref(),
        &headers,
    )
    .await;
    ok_redirect("/admin/epg", &r, &headers)
}

async fn delete_epg_source(headers: HeaderMap, Path(id): Path<i64>) -> (HeaderMap, Redirect) {
    let r = backend_json(
        reqwest::Method::DELETE,
        &format!("/api/v1/admin/epg/sources/{id}"),
        None,
        admin_sid(&headers).as_deref(),
        forwarded_for(&headers).as_deref(),
        csrf_token(&headers).as_deref(),
        &headers,
    )
    .await;
    ok_redirect("/admin/epg", &r, &headers)
}

async fn trigger_sync(headers: HeaderMap, Path(id): Path<i64>) -> (HeaderMap, Redirect) {
    let r = backend_json(
        reqwest::Method::POST,
        &format!("/api/v1/admin/sources/{id}/sync"),
        Some(serde_json::json!({})),
        admin_sid(&headers).as_deref(),
        forwarded_for(&headers).as_deref(),
        csrf_token(&headers).as_deref(),
        &headers,
    )
    .await;
    ok_redirect("/admin/sources", &r, &headers)
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
        forwarded_for(&headers).as_deref(),
        csrf_token(&headers).as_deref(),
        &headers,
    )
    .await;
    ok_redirect("/admin/groups", &r, &headers)
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
        forwarded_for(&headers).as_deref(),
        csrf_token(&headers).as_deref(),
        &headers,
    )
    .await;
    ok_redirect("/admin/epg", &r, &headers)
}

async fn kick_session(headers: HeaderMap, Path(id): Path<i64>) -> (HeaderMap, Redirect) {
    let r = backend_json(
        reqwest::Method::DELETE,
        &format!("/api/v1/admin/sessions/{id}"),
        None,
        admin_sid(&headers).as_deref(),
        forwarded_for(&headers).as_deref(),
        csrf_token(&headers).as_deref(),
        &headers,
    )
    .await;
    ok_redirect("/admin/sessions", &r, &headers)
}

/// m19：映射表单提交 → PUT /api/v1/admin/epg/sources/{id}/maps。
#[derive(Deserialize)]
struct EpgChannelMapForm {
    epg_source_id: i64,
    tvg_id: String,
    channel_id: i64,
}

async fn upsert_epg_channel_map(
    headers: HeaderMap,
    Form(f): Form<EpgChannelMapForm>,
) -> (HeaderMap, Redirect) {
    let body = serde_json::json!({
        "tvg_id": f.tvg_id,
        "channel_id": f.channel_id,
    });
    let r = backend_json(
        reqwest::Method::PUT,
        &format!("/api/v1/admin/epg/sources/{}/maps", f.epg_source_id),
        Some(body),
        admin_sid(&headers).as_deref(),
        forwarded_for(&headers).as_deref(),
        csrf_token(&headers).as_deref(),
        &headers,
    )
    .await;
    ok_redirect("/admin/epg", &r, &headers)
}

/// m19：映射删除 → DELETE /api/v1/admin/epg/maps/{id}。
async fn delete_epg_channel_map(headers: HeaderMap, Path(id): Path<i64>) -> (HeaderMap, Redirect) {
    let r = backend_json(
        reqwest::Method::DELETE,
        &format!("/api/v1/admin/epg/maps/{id}"),
        None,
        admin_sid(&headers).as_deref(),
        forwarded_for(&headers).as_deref(),
        csrf_token(&headers).as_deref(),
        &headers,
    )
    .await;
    ok_redirect("/admin/epg", &r, &headers)
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
        forwarded_for(&headers).as_deref(),
        csrf_token(&headers).as_deref(),
        &headers,
    )
    .await;
    ok_redirect("/admin/plans", &r, &headers)
}

#[derive(Deserialize)]
struct AdminForm {
    username: String,
    password: String,
    role: String,
    totp_required: Option<String>,
}

async fn create_admin(headers: HeaderMap, Form(f): Form<AdminForm>) -> Response {
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
        forwarded_for(&headers).as_deref(),
        csrf_token(&headers).as_deref(),
        &headers,
    )
    .await;
    if !r.ok {
        return ok_redirect("/admin/admins", &r, &headers).into_response();
    }
    // totp_required 时后端当场配给密钥：只显示一次（密钥 + otpauth URI + 二维码），
    // 用户须立即用验证器 App 扫码；首次登录输入一枚码后 TOTP 才启用。
    if let Some(secret) = r.data.get("totp_secret").and_then(|v| v.as_str()) {
        let locale = locale_from_headers(&headers);
        let rows = vec![
            (
                t_or(&locale, "admin.onetime-username", "Username"),
                r.data
                    .get("admin")
                    .and_then(|a| a.get("username"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            ),
            (
                t_or(
                    &locale,
                    "admin.onetime-totp-secret",
                    "TOTP secret (shown once)",
                ),
                secret.to_string(),
            ),
            (
                t_or(&locale, "admin.onetime-otpauth-uri", "otpauth URI"),
                r.data
                    .get("totp_uri")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            ),
        ];
        return onetime_page(
            &t_or(
                &locale,
                "admin.onetime-totp-provisioned",
                "TOTP provisioned",
            ),
            &rows,
            r.data.get("qr_png").and_then(|v| v.as_str()),
            (
                "/admin/admins",
                &t_or(&locale, "admin.onetime-back-admins", "Back to Admins"),
            ),
            &locale,
        )
        .into_response();
    }
    ok_redirect("/admin/admins", &r, &headers).into_response()
}

#[derive(Deserialize)]
struct AdminUpdateForm {
    role: String,
    totp_required: Option<String>,
}

async fn update_admin(
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(f): Form<AdminUpdateForm>,
) -> (HeaderMap, Redirect) {
    let body = serde_json::json!({
        "role": f.role,
        "totp_required": f.totp_required.as_deref() == Some("true"),
    });
    let r = backend_json(
        reqwest::Method::PATCH,
        &format!("/api/v1/admin/admins/{id}"),
        Some(body),
        admin_sid(&headers).as_deref(),
        forwarded_for(&headers).as_deref(),
        csrf_token(&headers).as_deref(),
        &headers,
    )
    .await;
    ok_redirect("/admin/admins", &r, &headers)
}

async fn delete_admin(headers: HeaderMap, Path(id): Path<i64>) -> (HeaderMap, Redirect) {
    let r = backend_json(
        reqwest::Method::DELETE,
        &format!("/api/v1/admin/admins/{id}"),
        None,
        admin_sid(&headers).as_deref(),
        forwarded_for(&headers).as_deref(),
        csrf_token(&headers).as_deref(),
        &headers,
    )
    .await;
    ok_redirect("/admin/admins", &r, &headers)
}

async fn reset_admin_totp(headers: HeaderMap, Path(id): Path<i64>) -> Html<String> {
    let r = backend_json(
        reqwest::Method::POST,
        &format!("/api/v1/admin/admins/{id}/reset-totp"),
        Some(serde_json::json!({})),
        admin_sid(&headers).as_deref(),
        forwarded_for(&headers).as_deref(),
        csrf_token(&headers).as_deref(),
        &headers,
    )
    .await;
    if !r.ok {
        let locale = locale_from_headers(&headers);
        let e = r
            .error
            .map(|e| localize_backend_error(&locale, &e))
            .unwrap_or_else(|| {
                t_or(
                    &locale,
                    "admin.onetime-error-reset-totp-failed",
                    "Failed to reset TOTP",
                )
            });
        return error_page(&locale, &e, "/admin/admins", "common.back", "Back");
    }
    let locale = locale_from_headers(&headers);
    let rows = vec![
        (
            t_or(
                &locale,
                "admin.onetime-totp-secret",
                "TOTP secret (shown once)",
            ),
            r.data
                .get("totp_secret")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        ),
        (
            t_or(&locale, "admin.onetime-otpauth-uri", "otpauth URI"),
            r.data
                .get("totp_uri")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        ),
    ];
    onetime_page(
        &t_or(
            &locale,
            "admin.onetime-totp-reprovisioned",
            "TOTP re-provisioned",
        ),
        &rows,
        r.data.get("qr_png").and_then(|v| v.as_str()),
        (
            "/admin/admins",
            &t_or(&locale, "admin.onetime-back-admins", "Back to Admins"),
        ),
        &locale,
    )
}

/// M19d：行内编辑表单提交 → PATCH /api/v1/admin/plans/{id}。
#[derive(Deserialize)]
struct PlanUpdateForm {
    name: Option<String>,
    max_concurrent_sessions: Option<String>,
    max_bound_devices: Option<String>,
    channel_groups: Option<String>,
    duration_days: Option<String>,
    price_cents: Option<String>,
    currency: Option<String>,
}

async fn update_plan(
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(f): Form<PlanUpdateForm>,
) -> (HeaderMap, Redirect) {
    let body = serde_json::json!({
        "name": opt_str(&f.name),
        "max_concurrent_sessions": f.max_concurrent_sessions.and_then(|v| v.parse::<i32>().ok()),
        "max_bound_devices": f.max_bound_devices.and_then(|v| v.parse::<i32>().ok()),
        "channel_groups": split_csv(&f.channel_groups),
        "duration_days": f.duration_days.and_then(|v| v.parse::<i32>().ok()),
        "price_cents": f.price_cents.and_then(|v| v.parse::<i64>().ok()),
        "currency": opt_str(&f.currency),
    });
    let r = backend_json(
        reqwest::Method::PATCH,
        &format!("/api/v1/admin/plans/{id}"),
        Some(body),
        admin_sid(&headers).as_deref(),
        forwarded_for(&headers).as_deref(),
        csrf_token(&headers).as_deref(),
        &headers,
    )
    .await;
    ok_redirect("/admin/plans", &r, &headers)
}

async fn delete_plan(headers: HeaderMap, Path(id): Path<i64>) -> (HeaderMap, Redirect) {
    let r = backend_json(
        reqwest::Method::DELETE,
        &format!("/api/v1/admin/plans/{id}"),
        None,
        admin_sid(&headers).as_deref(),
        forwarded_for(&headers).as_deref(),
        csrf_token(&headers).as_deref(),
        &headers,
    )
    .await;
    ok_redirect("/admin/plans", &r, &headers)
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
        forwarded_for(&headers).as_deref(),
        csrf_token(&headers).as_deref(),
        &headers,
    )
    .await;
    ok_redirect("/admin/settings", &r, &headers)
}

/// m22：文案覆盖编辑表单 → PUT /api/v1/admin/i18n-overrides。
#[derive(Deserialize)]
struct I18nOverrideForm {
    locale: String,
    key: String,
    value: String,
}

async fn upsert_i18n_override(
    headers: HeaderMap,
    Form(f): Form<I18nOverrideForm>,
) -> (HeaderMap, Redirect) {
    let body = serde_json::json!({
        "locale": f.locale,
        "key": f.key,
        "value": f.value,
    });
    let r = backend_json(
        reqwest::Method::PUT,
        "/api/v1/admin/i18n-overrides",
        Some(body),
        admin_sid(&headers).as_deref(),
        forwarded_for(&headers).as_deref(),
        csrf_token(&headers).as_deref(),
        &headers,
    )
    .await;
    ok_redirect("/admin/settings", &r, &headers)
}

/// m22：文案覆盖删除表单 → DELETE /api/v1/admin/i18n-overrides?locale=&key=。
/// 表单把 locale/key 放在 action 的查询串里（无 body 字段），故这里取 Query。
#[derive(Deserialize)]
struct I18nOverrideDeleteForm {
    locale: String,
    key: String,
}

async fn delete_i18n_override(
    headers: HeaderMap,
    Query(f): Query<I18nOverrideDeleteForm>,
) -> (HeaderMap, Redirect) {
    // locale/key 经后端校验（支持语言 + 点分 key 形），此处只做 URL 编码。
    let path = format!(
        "/api/v1/admin/i18n-overrides?locale={}&key={}",
        pct_encode(&f.locale),
        pct_encode(&f.key)
    );
    let r = backend_json(
        reqwest::Method::DELETE,
        &path,
        None,
        admin_sid(&headers).as_deref(),
        forwarded_for(&headers).as_deref(),
        csrf_token(&headers).as_deref(),
        &headers,
    )
    .await;
    ok_redirect("/admin/settings", &r, &headers)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// m22：`localize_backend_error` 对新增 6 条上传任务串的映射（en）。
    #[test]
    fn localize_backend_error_upload_tasks_en() {
        assert_eq!(
            localize_backend_error("en", "read upload failed"),
            "Failed to read upload"
        );
        assert_eq!(
            localize_backend_error("en", "parse upload failed"),
            "Failed to parse upload"
        );
        assert_eq!(
            localize_backend_error("en", "source not found"),
            "Source not found"
        );
        // Fluent 把 `{ $detail }` 插值包在 U+2068/U+2069 双向隔离符里
        // （与现有 `$args` 文案同款行为），断言按真实渲染值写。
        assert_eq!(
            localize_backend_error("en", "read source failed: boom"),
            "Failed to read source: \u{2068}boom\u{2069}"
        );
        assert_eq!(
            localize_backend_error("en", "create source failed: boom"),
            "Failed to create source: \u{2068}boom\u{2069}"
        );
        assert_eq!(
            localize_backend_error("en", "import channels failed: boom"),
            "Failed to import channels: \u{2068}boom\u{2069}"
        );
        assert_eq!(
            localize_backend_error("en", "backend unreachable"),
            "Backend unreachable"
        );
        // 未知串原样透传（已由 API 本地化中间件翻过的 message）。
        assert_eq!(
            localize_backend_error("en", "totally unknown"),
            "totally unknown"
        );
    }

    /// m22：同一批串在简体中文下的映射（含 `$detail` 占位符）。
    #[test]
    fn localize_backend_error_upload_tasks_zh() {
        assert_eq!(
            localize_backend_error("zh-Hans-CN", "read upload failed"),
            "读取上传失败"
        );
        assert_eq!(
            localize_backend_error("zh-Hans-CN", "import channels failed: boom"),
            "导入频道失败：\u{2068}boom\u{2069}"
        );
        assert_eq!(
            localize_backend_error("zh-Hans-CN", "read source failed: boom"),
            "读取订阅源失败：\u{2068}boom\u{2069}"
        );
    }
}
