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
    extract::{Form, Path},
    http::{header, HeaderMap},
    response::Redirect,
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

/// POST 后端 JSON 接口；返回 `(http_status, data, set_cookie)`。
async fn backend_post(
    path: &str,
    body: serde_json::Value,
    sid: Option<&str>,
) -> (
    reqwest::StatusCode,
    Option<serde_json::Value>,
    Option<String>,
) {
    let url = format!("{}{path}", api_base());
    let mut req = client().post(&url).json(&body);
    if let Some(s) = sid {
        req = req.header("Cookie", format!("iptv_portal_sid={s}"));
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

async fn backend_delete(path: &str, sid: Option<&str>) -> bool {
    let url = format!("{}{path}", api_base());
    let mut req = client().delete(&url);
    if let Some(s) = sid {
        req = req.header("Cookie", format!("iptv_portal_sid={s}"));
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
    let (_status, data, set_cookie) = backend_post("/api/v1/auth/token-login", body, None).await;
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
    let _ = backend_post("/api/v1/auth/logout", serde_json::json!({}), sid.as_deref()).await;
    redirect_with_cookies(
        "/",
        &["iptv_portal_sid=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0".to_string()],
    )
}

/// POST /logout-all（M14）：调后端吊销该 Token 全部会话（含当前），并清除 Cookie。
async fn logout_all(headers: HeaderMap) -> (HeaderMap, Redirect) {
    let sid = portal_sid(&headers);
    let _ = backend_post(
        "/api/v1/auth/logout-all",
        serde_json::json!({}),
        sid.as_deref(),
    )
    .await;
    redirect_with_cookies(
        "/",
        &["iptv_portal_sid=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0".to_string()],
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
        let (_, _, _) = backend_post("/api/v1/me/settings", body, sid.as_deref()).await;
    }
    let to = safe_redirect(form.redirect_to.as_deref().unwrap_or("/me"), "/me");
    redirect_with_cookies(&to, &cookies)
}

/// POST /me/devices/{id}/kick：下线自己的设备（服务端只认本 token 的会话）。
async fn kick_device(headers: HeaderMap, Path(session_id): Path<i64>) -> Redirect {
    let sid = portal_sid(&headers);
    let _ = backend_delete(&format!("/api/v1/me/devices/{session_id}"), sid.as_deref()).await;
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
        let _ = backend_post(
            "/api/v1/me/settings",
            serde_json::Value::Object(body),
            sid.as_deref(),
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
    let body = serde_json::json!({ "session_uid": form.session_uid });
    let _ = backend_post("/api/v1/playback/stop", body, sid.as_deref()).await;
    Redirect::to(&format!(
        "/watch/{channel_id}?quality={}",
        pct_encode(&form.quality)
    ))
}
