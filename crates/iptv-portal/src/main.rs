//! 用户端门户二进制（开发文档 §4）。
//!
//! Leptos SSR + hydrate：`leptos_routes` + `shell()`；hydrate 产物由
//! cargo-leptos 构建（见 Dockerfile / release.yml），打进镜像与 Release
//! 包的 `site/`，运行时由 `LEPTOS_SITE_ROOT` 指向（默认 `target/site`）。
//! 输出名 `iptv-portal` 见 `crates/iptv-portal/Cargo.toml` 的
//! `[package.metadata.leptos]`（此前缺失导致 shell 引用空名的 /pkg/.js）。
//! 变更处理器是纯 Axum POST，无 hydrate 也能工作，包括登录（粘贴 Token，
//! §4.2 方式 B）、退出、语言切换（写 Cookie + PATCH /api/v1/me/settings，
//! §12.4）、设备下线、改设置、切清晰度。
//!
//! 监听地址由 Leptos 配置决定（`LEPTOS_SITE_ADDR`，默认 127.0.0.1:3000）；
//! 后端地址 `IPTV_API_BASE`，默认 http://127.0.0.1:8080。
// 作者: Passwd Word

use axum::{
    extract::{Form, Path, Request},
    http::{header, HeaderMap, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
    Router,
};
use iptv_design::design_tokens_css;
use iptv_portal::{api_base, cookie_value, shell, App};
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
    let addr = std::env::var("IPTV_PORTAL_ADDR")
        .ok()
        .and_then(|s| s.parse::<std::net::SocketAddr>().ok())
        .unwrap_or(leptos_options.site_addr);
    let routes = generate_route_list(App);
    let app = Router::new()
        .route("/design.css", get(design_css))
        .route("/vendor/hls.min.js", get(hls_vendor_js))
        .route("/healthz", get(|| async { "ok" }))
        .route("/login", post(login))
        .route("/logout", post(logout))
        .route("/logout-all", post(logout_all))
        .route("/locale", post(set_locale))
        .route("/me/devices/{session_id}/kick", post(kick_device))
        .route("/me/settings", post(save_settings))
        .route("/watch/{channel_id}/quality", post(change_quality))
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
    tracing::info!("portal listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app.into_make_service())
        .await
        .unwrap();
}

async fn design_css() -> ([(header::HeaderName, &'static str); 1], &'static str) {
    ([(header::CONTENT_TYPE, "text/css")], design_tokens_css())
}

/// m16：`GET /vendor/hls.min.js`——hls.js 1.5.13 打进二进制（`include_bytes!`），
/// 播放器非 Safari 时经 MSE 接它；不走运行时 CDN 外链，离线部署不断链。
/// 文件在 `crates/iptv-portal/vendor/hls.min.js`（固定版本，见该目录 README）。
/// 网关把 `/{*rest}` 全量代理到门户，故经 8081 同样可达。
async fn hls_vendor_js() -> ([(header::HeaderName, &'static str); 2], &'static [u8]) {
    (
        [
            (header::CONTENT_TYPE, "text/javascript"),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        include_bytes!("../vendor/hls.min.js"),
    )
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

fn cookie_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

fn portal_sid(headers: &HeaderMap) -> Option<String> {
    cookie_header(headers).and_then(|h| cookie_value(&h, "iptv_portal_sid"))
}

/// m14：入站请求里的双提交 token（`iptv_csrf` Cookie 值），供转发给 API 用。
fn csrf_token(headers: &HeaderMap) -> Option<String> {
    cookie_header(headers).and_then(|h| cookie_value(&h, "iptv_csrf"))
}

/// m14 双提交 Cookie（§13）：浏览器写请求的第一道防线在 SSR 层。
///
/// 浏览器→SSR 的写请求分两类，都过这个中间件：
/// - 纯 Axum POST 表单（登录/退出/语言/设置/下线设备/切清晰度）与 Leptos
///   server fn POST：表单发不出自定义头，故不走双提交字段，改走
///   Origin 比对（有 Origin 时必须与 `X-Forwarded-Host` 或 `Host` 同源，
///   网关会覆盖写 XFH）+ 辅助信号 `Sec-Fetch-Site: cross-site` 直接拒。
///   现代浏览器对 POST 一定带 Origin / Sec-Fetch-Site，跨站表单 POST
///   两项全中、必然被拦；同源请求两项都过。
/// - 带 `X-CSRF-Token` 头的请求（如将来浏览器直调 API 的 fetch）：头值
///   必须与 `iptv_csrf` Cookie 等时一致，否则 403。
///
/// 另：响应时若请求带会话 Cookie 却没有 `iptv_csrf` Cookie，就签发一个
/// （256 bit 换算：uuid v4 122 bit 熵，CSRF token 够用；非 HttpOnly，
/// 供前端/SSR 读取；`SameSite=Lax`）。这样登录成功后的首个页面响应就把
/// token 交给浏览器，之后的 SSR→API 转发才能带上双提交头；旧会话
/// （部署前已登录、尚无该 Cookie）也在下一次响应里自动补发，无需重新登录。
/// 退出时由各 logout handler 清掉该 Cookie（见下）。
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
        .and_then(|c| cookie_value(c, "iptv_portal_sid"))
        .is_some();
    let had_csrf = csrf_token(&headers).is_some();
    let mut resp = next.run(req).await;
    if had_session && !had_csrf {
        let token = uuid::Uuid::new_v4().simple().to_string();
        if let Ok(v) = header::HeaderValue::from_str(&format!(
            "iptv_csrf={token}; Path=/; SameSite=Lax; Max-Age=7776000"
        )) {
            resp.headers_mut().append(header::SET_COOKIE, v);
        }
    }
    resp
}

/// POST 后端 JSON 接口；返回 `(http_status, data, set_cookie)`。
/// `csrf` 为入站请求的 `iptv_csrf` Cookie 值：有则同时以
/// `X-CSRF-Token` 头 + `Cookie` 头里的 `iptv_csrf` 发给 API，走双提交
/// 比对；无则只发会话 Cookie，API 退回 Origin 兜底（登录/票据兑换尚无
/// token 时就是这个形态）。
async fn backend_post(
    path: &str,
    body: serde_json::Value,
    sid: Option<&str>,
    csrf: Option<&str>,
) -> (
    reqwest::StatusCode,
    Option<serde_json::Value>,
    Option<String>,
) {
    let url = format!("{}{path}", api_base());
    let mut req = client().post(&url).json(&body);
    let mut cookie = String::new();
    if let Some(s) = sid {
        cookie.push_str(&format!("iptv_portal_sid={s}"));
    }
    if let Some(c) = csrf {
        if !cookie.is_empty() {
            cookie.push_str("; ");
        }
        cookie.push_str(&format!("iptv_csrf={c}"));
        req = req.header("x-csrf-token", c);
    }
    if !cookie.is_empty() {
        req = req.header("Cookie", cookie);
    }
    let Ok(res) = req.send().await else {
        return (reqwest::StatusCode::BAD_GATEWAY, None, None);
    };
    let status = res.status();
    let set_cookie = res
        .headers()
        .get(reqwest::header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let env: serde_json::Value = res.json().await.unwrap_or(serde_json::Value::Null);
    let data = env.get("data").cloned();
    let code = env.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        let msg = env
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("error")
            .to_string();
        return (
            status,
            Some(serde_json::json!({ "error": msg })),
            set_cookie,
        );
    }
    (status, data, set_cookie)
}

async fn backend_delete(path: &str, sid: Option<&str>, csrf: Option<&str>) -> bool {
    let url = format!("{}{path}", api_base());
    let mut req = client().delete(&url);
    let mut cookie = String::new();
    if let Some(s) = sid {
        cookie.push_str(&format!("iptv_portal_sid={s}"));
    }
    if let Some(c) = csrf {
        if !cookie.is_empty() {
            cookie.push_str("; ");
        }
        cookie.push_str(&format!("iptv_csrf={c}"));
        req = req.header("x-csrf-token", c);
    }
    if !cookie.is_empty() {
        req = req.header("Cookie", cookie);
    }
    req.send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
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

fn safe_redirect(to: &str, fallback: &str) -> String {
    if to.starts_with('/') && !to.starts_with("//") {
        to.to_string()
    } else {
        fallback.to_string()
    }
}

#[derive(Deserialize)]
struct LoginForm {
    token: String,
    remember_device: Option<String>,
}

/// POST /login：粘贴 Token 登录（§4.2 方式 B）。成功转发会话 Cookie 去 /me。
async fn login(headers: HeaderMap, Form(form): Form<LoginForm>) -> (HeaderMap, Redirect) {
    let mut cookies = Vec::new();
    let device_id = cookie_header(&headers)
        .and_then(|h| cookie_value(&h, "iptv_device_id"))
        .unwrap_or_else(|| {
            let d = format!("web-{}", uuid::Uuid::new_v4());
            cookies.push(format!(
                "iptv_device_id={d}; Path=/; HttpOnly; SameSite=Lax; Max-Age=7776000"
            ));
            d
        });
    let body = serde_json::json!({
        "token": form.token.trim(),
        "device_fingerprint": device_id,
        "remember_device": form.remember_device.as_deref() == Some("true"),
    });
    let (_status, data, set_cookie) =
        backend_post("/api/v1/auth/token-login", body, None, None).await;
    if let Some(sc) = set_cookie {
        cookies.push(sc);
    }
    if let Some(err) = data
        .as_ref()
        .and_then(|d| d.get("error"))
        .and_then(|e| e.as_str())
    {
        return redirect_with_cookies(&format!("/?error={}", pct_encode(err)), &cookies);
    }
    redirect_with_cookies("/me", &cookies)
}

/// POST /logout：调后端退出并清除会话 Cookie。
async fn logout(headers: HeaderMap) -> (HeaderMap, Redirect) {
    let sid = portal_sid(&headers);
    let csrf = csrf_token(&headers);
    let _ = backend_post(
        "/api/v1/auth/logout",
        serde_json::json!({}),
        sid.as_deref(),
        csrf.as_deref(),
    )
    .await;
    redirect_with_cookies(
        "/",
        &[
            "iptv_portal_sid=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0".to_string(),
            "iptv_csrf=; Path=/; SameSite=Lax; Max-Age=0".to_string(),
        ],
    )
}

/// POST /logout-all（M14）：调后端吊销该 Token 全部会话（含当前），并清除 Cookie。
async fn logout_all(headers: HeaderMap) -> (HeaderMap, Redirect) {
    let sid = portal_sid(&headers);
    let csrf = csrf_token(&headers);
    let _ = backend_post(
        "/api/v1/auth/logout-all",
        serde_json::json!({}),
        sid.as_deref(),
        csrf.as_deref(),
    )
    .await;
    redirect_with_cookies(
        "/",
        &[
            "iptv_portal_sid=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0".to_string(),
            "iptv_csrf=; Path=/; SameSite=Lax; Max-Age=0".to_string(),
        ],
    )
}

#[derive(Deserialize)]
struct LocaleForm {
    locale: String,
    redirect_to: Option<String>,
}

/// POST /locale：写 `iptv_locale` Cookie（持久化）+ PATCH 后端设置（§12.4）。
async fn set_locale(headers: HeaderMap, Form(form): Form<LocaleForm>) -> (HeaderMap, Redirect) {
    let loc = if iptv_i18n::I18n::supported_locales().contains(&form.locale.as_str()) {
        form.locale.clone()
    } else {
        iptv_i18n::DEFAULT_LOCALE.to_string()
    };
    let cookies = vec![format!(
        "iptv_locale={loc}; Path=/; SameSite=Lax; Max-Age=31536000"
    )];
    let sid = portal_sid(&headers);
    if sid.is_some() {
        let body = serde_json::json!({ "locale": loc });
        let csrf = csrf_token(&headers);
        let (_, _, _) =
            backend_post("/api/v1/me/settings", body, sid.as_deref(), csrf.as_deref()).await;
    }
    let to = safe_redirect(form.redirect_to.as_deref().unwrap_or("/me"), "/me");
    redirect_with_cookies(&to, &cookies)
}

/// POST /me/devices/{id}/kick：下线自己的设备（服务端只认本 token 的会话）。
async fn kick_device(headers: HeaderMap, Path(session_id): Path<i64>) -> Redirect {
    let sid = portal_sid(&headers);
    let csrf = csrf_token(&headers);
    let _ = backend_delete(
        &format!("/api/v1/me/devices/{session_id}"),
        sid.as_deref(),
        csrf.as_deref(),
    )
    .await;
    Redirect::to("/me/devices")
}

#[derive(Deserialize)]
struct SettingsForm {
    display_name: Option<String>,
    locale: Option<String>,
    theme: Option<String>,
}

/// POST /me/settings：PATCH 后端设置；主题写 Cookie（供 Shell 的 data-theme）。
async fn save_settings(
    headers: HeaderMap,
    Form(form): Form<SettingsForm>,
) -> (HeaderMap, Redirect) {
    let sid = portal_sid(&headers);
    let mut body = serde_json::Map::new();
    if let Some(d) = &form.display_name {
        if !d.trim().is_empty() {
            body.insert("display_name".to_string(), serde_json::json!(d));
        }
    }
    if let Some(l) = &form.locale {
        if iptv_i18n::I18n::supported_locales().contains(&l.as_str()) {
            body.insert("locale".to_string(), serde_json::json!(l));
        }
    }
    if let Some(t) = &form.theme {
        body.insert("theme".to_string(), serde_json::json!(t));
    }
    if sid.is_some() && !body.is_empty() {
        let csrf = csrf_token(&headers);
        let _ = backend_post(
            "/api/v1/me/settings",
            serde_json::Value::Object(body),
            sid.as_deref(),
            csrf.as_deref(),
        )
        .await;
    }
    let mut cookies = Vec::new();
    if let Some(t) = &form.theme {
        if ["light", "dark", "system"].contains(&t.as_str()) {
            cookies.push(format!(
                "iptv_theme={t}; Path=/; SameSite=Lax; Max-Age=31536000"
            ));
        }
    }
    if let Some(l) = &form.locale {
        if iptv_i18n::I18n::supported_locales().contains(&l.as_str()) {
            cookies.push(format!(
                "iptv_locale={l}; Path=/; SameSite=Lax; Max-Age=31536000"
            ));
        }
    }
    redirect_with_cookies("/settings?saved=1", &cookies)
}

#[derive(Deserialize)]
struct QualityForm {
    quality: String,
    session_uid: String,
}

/// POST /watch/{id}/quality：先释放旧会话，再 302 回播放页（带新清晰度参数）。
async fn change_quality(
    headers: HeaderMap,
    Path(channel_id): Path<i64>,
    Form(form): Form<QualityForm>,
) -> Redirect {
    let sid = portal_sid(&headers);
    let csrf = csrf_token(&headers);
    let body = serde_json::json!({ "session_uid": form.session_uid });
    let _ = backend_post(
        "/api/v1/playback/stop",
        body,
        sid.as_deref(),
        csrf.as_deref(),
    )
    .await;
    Redirect::to(&format!(
        "/watch/{channel_id}?quality={}",
        pct_encode(&form.quality)
    ))
}
