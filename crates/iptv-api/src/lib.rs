//! 后端 API 服务（开发文档 §3、§4.5、§5.2、§12.5、§13）。
//!
//! 路由总表见 [`AppState::router`]；错误统一为 `Envelope { code, message, data }`，
//! `message` 按 `Accept-Language` 本地化，`code` 永远不变（§12.5）。
//! 门户接口以会话内 `token_id` 为准，忽略请求体中的 id（§13 越权防护）。
// 作者: Passwd Word

pub mod handlers;
pub mod proxy;
pub mod state;
pub mod tickets;
pub mod upload_tasks;

pub use state::{AppState, Config, Metrics};

use axum::extract::{FromRequestParts, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use iptv_auth::{mask_ip, sha256_hex};
use iptv_dto::{Envelope, ErrorCode, HealthResponse};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use utoipa::OpenApi as _;

// ---------------------------------------------------------------------------
// 错误类型：业务层只抛错误码 + 参数（§12.5），这里统一序列化。
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
    pub args: Vec<(String, serde_json::Value)>,
    /// N1：FTL 点分 key（如 `errors.plan-in-use`）；有 key 时序列化层
    /// 按 locale 渲染该 key（带 `args`），`message` 只作英文兜底。
    pub message_key: Option<String>,
}

impl ApiError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            args: vec![],
            message_key: None,
        }
    }
    /// N1：带本地化 key 的错误（§12.5：业务层只抛错误码与文案参数）。
    /// `message` 为英文兜底（key 缺失或中间件未渲染时用）。
    pub fn with_key(
        code: ErrorCode,
        message_key: impl Into<String>,
        message: impl Into<String>,
        args: Vec<(String, serde_json::Value)>,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            args,
            message_key: Some(message_key.into()),
        }
    }
    /// 参数以 JSON Map 形式给出（供序列化层渲染 `message_key` 用）。
    fn args_map(&self) -> serde_json::Map<String, serde_json::Value> {
        self.args.iter().cloned().collect()
    }
    /// 按 locale 渲染 message：有 `message_key` 时渲染该 key（带 args），
    /// 无 key 时按 `errors.e<code>` 渲染，文案缺失时回退业务 message。
    fn render_message(&self, state: &AppState, locale: &str) -> String {
        if let Some(key) = &self.message_key {
            let rendered = state.i18n.t_json_args(locale, key, &self.args_map());
            if rendered != *key {
                return rendered;
            }
        }
        let message = state.i18n.error_message(locale, self.code as i32);
        if message == format!("errors.e{}", self.code as i32) {
            self.message.clone()
        } else {
            message
        }
    }
    pub fn from_core(e: iptv_core::CoreError) -> Self {
        match e {
            iptv_core::CoreError::Coded {
                code,
                message,
                args,
            } => Self {
                code,
                message,
                args,
                message_key: None,
            },
            iptv_core::CoreError::Internal(m) => Self::new(ErrorCode::Internal, m),
        }
    }
    fn localized(self, state: &AppState, locale: &str) -> (StatusCode, Json<Envelope<()>>) {
        let message = self.render_message(state, locale);
        let args = self.args_map();
        let message_key = self.message_key;
        let env = match message_key {
            Some(key) => Envelope::<()>::err_with_key(self.code, message, key, args),
            None => Envelope::<()>::err(self.code, message),
        };
        (
            StatusCode::from_u16(self.code.http_status()).unwrap_or(StatusCode::BAD_REQUEST),
            Json(env),
        )
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        // M10：store 层唯一约束冲突（重名）不再是 500，映射为 40900。
        if iptv_store::is_unique_violation_error(&e) {
            return Self::new(ErrorCode::Conflict, "resource already exists");
        }
        Self::new(ErrorCode::Internal, e.to_string())
    }
}

/// 上游拉流失败 → §6.4 频道源不可用（50301）。
impl From<reqwest::Error> for ApiError {
    fn from(e: reqwest::Error) -> Self {
        Self::new(
            ErrorCode::SourceUnavailable,
            format!("upstream fetch failed: {e}"),
        )
    }
}

/// §12.5：前端优先用 `code` 查本地文案，`message` 仅作兜底；
/// 因此错误响应直接用业务 message，不在这里猜 locale。
/// N1：带 `message_key` 的错误把 key + args 一并进包，供外层
/// `localize_errors_middleware` 按请求 locale 渲染（`message` 仍为
/// 英文兜底，中间件渲染失败时保留）。
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.code.http_status()).unwrap_or(StatusCode::BAD_REQUEST);
        let args = self.args_map();
        let message_key = self.message_key;
        let env = match message_key {
            Some(key) => Envelope::<()>::err_with_key(self.code, self.message, key, args),
            None => Envelope::<()>::err(self.code, self.message),
        };
        (status, Json(env)).into_response()
    }
}

pub type ApiResult<T> = Result<Json<Envelope<T>>, ApiError>;

/// 成功包。
pub fn ok<T: serde::Serialize>(data: T) -> ApiResult<T> {
    Ok(Json(Envelope::ok(data)))
}

// ---------------------------------------------------------------------------
// 请求上下文：locale + 客户端 IP
// ---------------------------------------------------------------------------

/// 解析 locale（§12.2）：Cookie `iptv_locale` → Accept-Language → 默认。
pub fn negotiate_locale(headers: &HeaderMap, user_locale: Option<&str>) -> String {
    let cookie_locale = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| {
            c.split(';').find_map(|part| {
                let (k, v) = part.trim().split_once('=')?;
                if k.trim() == "iptv_locale" {
                    Some(v.trim().to_string())
                } else {
                    None
                }
            })
        });
    let accept = headers
        .get(header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok());
    iptv_i18n::I18n::negotiate(user_locale, cookie_locale.as_deref(), accept)
}

/// 同 `negotiate_locale`，但最后的“默认”读系统设置 `default_locale`，
/// 且结果不在系统设置 `available_locales` 里时回退到默认语言（M1）。
/// 读设置走 60 秒缓存，中间件每请求调一次无额外负担。
async fn negotiate_locale_with_settings(
    state: &AppState,
    headers: &HeaderMap,
    user_locale: Option<&str>,
) -> String {
    let cookie_locale = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| {
            c.split(';').find_map(|part| {
                let (k, v) = part.trim().split_once('=')?;
                if k.trim() == "iptv_locale" {
                    Some(v.trim().to_string())
                } else {
                    None
                }
            })
        });
    let accept = headers
        .get(header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok());
    let default = state.store.get_setting_string("default_locale").await;
    let locale = iptv_i18n::I18n::negotiate_with_default(
        user_locale,
        cookie_locale.as_deref(),
        accept,
        &default,
    );
    let allowed = state
        .store
        .get_setting_string_list("available_locales")
        .await;
    if !allowed.is_empty() && !allowed.iter().any(|a| a == &locale) {
        return default;
    }
    locale
}

/// 客户端 IP：X-Forwarded-For 首项 → X-Real-IP → 直连对端 IP。
/// 单端口部署下请求必经网关，网关把 XFF 写成直连对端 IP（客户端自带
/// 的 XFF 不可信，网关默认覆盖，见 iptv-gateway）；直连 API 时没有 XFF，
/// 回落到 ConnectInfo 的对端地址，避免全站共用 "unknown" 桶。
pub fn client_ip(headers: &HeaderMap, peer: Option<std::net::SocketAddr>) -> Option<String> {
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
        .or_else(|| peer.map(|p| p.ip().to_string()))
}

/// 登录口限流（M2，§13 三维）：先过按 IP 聚合的桶（30 次/分钟），再过
/// 复合 key 的桶（5 次/分钟），两者都过才放行。`key` 由调用方按登录类型
/// 拼（`admin-login:{username}:{ip}` / `ticket-exchange:{ip}` /
/// `token-login:{prefix}:{ip}`），避免三类登录共用一个 "unknown" 桶。
pub fn check_login_limit(state: &AppState, ip: &str, key: &str) -> bool {
    state.login_ip_limiter.check(&format!("ip:{ip}")) && state.login_limiter.check(key)
}

/// m25：成功登录后重置该复合 key 的计数——成功登录不占限流桶。
/// 只重置复合 key（`login_limiter`），按 IP 聚合的桶（`login_ip_limiter`）
/// 不动：同一 IP 多账号轮换刷登录仍受 30 次/分钟聚合桶约束。
pub fn reset_login_limit(state: &AppState, key: &str) {
    state.login_limiter.reset(key);
}

/// m14 CSRF 双提交 Cookie（§13）+ Origin / Sec-Fetch-Site 兜底。
///
/// 双提交：会话建立后，SSR 层给浏览器发 `iptv_csrf` Cookie（非 HttpOnly、
/// 可读，`SameSite=Lax`，见门户/后台 `main.rs` 的签发与续发）；SSR 向 API
/// 转发写请求时，把该 Cookie 值同时放进 `X-CSRF-Token` 请求头与
/// `Cookie` 头。中间件比对两者一致才放行——跨站攻击者读不到该 Cookie
///（同源策略），也就伪造不出与 Cookie 一致的请求头。
///
/// 逐类防线（见报告）：
/// - 带 `X-CSRF-Token` 头的写请求：只走双提交比对（头与 Cookie 等时比较，
///   不一致/缺 Cookie 即 403），不看 Origin。
/// - 无头的写请求：走 Origin 兜底（与旧 `csrf_origin_middleware` 同款：
///   有 Origin 时必须与 `X-Forwarded-Host` 或 `Host` 同源）+ 辅助信号
///   `Sec-Fetch-Site: cross-site` 时直接拒。两类：
///   （a）登录/票据兑换（尚未建会话、无 token 可带）；（b）服务端对服务端
///   与 API 客户端（无 Origin、无 Cookie，浏览器发不出这种请求）。
/// - 读请求（GET/HEAD/OPTIONS）与无 Cookie 的请求不受影响。
///
/// 注意：`Sec-Fetch-Site` 只是辅助信号，从不单独放行——无头请求仍须过
/// Origin 比对（有 Origin 时）或本身就是无 Origin 的服务端调用。
pub const CSRF_COOKIE: &str = "iptv_csrf";
pub const CSRF_HEADER: &str = "x-csrf-token";

/// 从 `Cookie` 头里取指定 Cookie 的值。
fn cookie_value(header: &str, name: &str) -> Option<String> {
    header.split(';').find_map(|part| {
        let (k, v) = part.trim().split_once('=')?;
        if k.trim() == name {
            Some(v.trim().to_string())
        } else {
            None
        }
    })
}

/// 等时比较（普通 `==` 会泄漏时序；与 play_url / admin Cookie 验签同款）。
fn tokens_equal(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    a.as_bytes().ct_eq(b.as_bytes()).unwrap_u8() == 1
}

/// 写请求是否通过 CSRF 检查（纯函数，供单测；`csrf_middleware` 的决策层）。
/// 返回 `false` 即 403。
fn csrf_check(headers: &HeaderMap, method: &Method) -> bool {
    if !matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    ) {
        return true;
    }
    let cookies = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    // 1. 双提交：有头就必须与 Cookie 一致（头在、Cookie 缺失也算不一致）。
    if let Some(token) = headers.get(CSRF_HEADER).and_then(|v| v.to_str().ok()) {
        return cookie_value(cookies, CSRF_COOKIE)
            .is_some_and(|cookie| !token.is_empty() && tokens_equal(token, &cookie));
    }
    // 2. 辅助信号：浏览器明确标注 cross-site 的写请求直接拒（即使 Origin 缺失）。
    if headers
        .get("sec-fetch-site")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("cross-site"))
    {
        return false;
    }
    // 3. Origin 兜底：有 Origin 时必须同源；无 Origin 的放行（服务端调用）。
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        let origin_host = origin.split("://").nth(1).unwrap_or("");
        // 经网关来的请求：reqwest 按上游 URL 把 Host 重写成内网
        // 地址（127.0.0.1:8080），与浏览器 Origin 里的网关地址
        // 必然失配；网关会把入站 Host 原样写进 X-Forwarded-Host
        // （覆盖写入，客户端自带的值不可信），优先用它比对。
        // 直连 API 时没有 XFH，退回 Host。
        // 信任前提：API 只绑本机（:8080 不对外），唯一入口是网关。
        let host_ok = headers
            .get("x-forwarded-host")
            .and_then(|v| v.to_str().ok())
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.trim())
            .or_else(|| headers.get(header::HOST).and_then(|v| v.to_str().ok()))
            .is_some_and(|host| origin_host.trim_end_matches('/') == host.trim_end_matches('/'));
        if !host_ok {
            return false;
        }
    }
    true
}

pub async fn csrf_middleware(req: Request, next: Next) -> Response {
    if !csrf_check(req.headers(), req.method()) {
        return ApiError::new(ErrorCode::Forbidden, "cross-origin write rejected").into_response();
    }
    next.run(req).await
}

/// m14 CORS（§13）：`allow_origin(Any)` 改为可配置允许源列表。
/// - 空列表（默认）：不放行任何跨域源——预检收不到
///   `Access-Control-Allow-Origin`，浏览器拦截跨域读；同源请求不受影响
///   （部署是单端口网关同源，浏览器根本不走 CORS）。
/// - 配了源：只放行列表里的源；`allow_credentials` 永远不配（会话 Cookie
///   不走跨域，见 `Config::cors_origins`）。
/// - `X-CSRF-Token` 必须在 `allow_headers` 里，否则已配源的跨域客户端
///   在预检阶段就发不出双提交头。
///
/// 与 CSRF 中间件不打架：CORS 层在最外层先处理 OPTIONS 预检（预检不是写
/// 方法，不会进 CSRF 检查）；真正写请求上的 Origin 由 CSRF 中间件再验一次。
pub fn build_cors_layer(origins: &[String]) -> tower_http::cors::CorsLayer {
    use tower_http::cors::{AllowHeaders, AllowMethods, AllowOrigin, CorsLayer};
    let allow_origin = if origins.is_empty() {
        AllowOrigin::predicate(|_, _| false)
    } else {
        AllowOrigin::list(origins.iter().filter_map(|o| o.parse::<HeaderValue>().ok()))
    };
    CorsLayer::new()
        .allow_origin(allow_origin)
        .allow_methods(AllowMethods::list([
            Method::GET,
            Method::POST,
            Method::PATCH,
            Method::DELETE,
        ]))
        .allow_headers(AllowHeaders::list([
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            header::ACCEPT_LANGUAGE,
            header::HeaderName::from_static(CSRF_HEADER),
        ]))
}

/// m2 安全响应头（§4.3）：所有响应加 `Referrer-Policy: no-referrer`；
/// `/api/v1/*` 全部动态接口再加 `Cache-Control: no-store`（此前只有
/// /p 代理响应有 no-store，/me 等身份响应可被缓存）。
async fn security_headers_middleware(req: Request, next: Next) -> Response {
    let is_api = req.uri().path().starts_with("/api/v1/");
    let mut resp = next.run(req).await;
    resp.headers_mut()
        .insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    if is_api {
        resp.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    resp
}

/// 全局中间件：计数 + locale 进 extensions。
async fn metrics_middleware(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    state.metrics.http_requests.fetch_add(1, Ordering::Relaxed);
    next.run(req).await
}

/// §16：错误提示随 Accept-Language 本地化，`code` 不变。
/// `ApiError::into_response` 拿不到 AppState 与请求头，故在外层对 4xx/5xx 的
/// Envelope 做 message 替换；该 code 无对应文案时保留业务 message。
async fn localize_errors_middleware(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    let locale = negotiate_locale_with_settings(&state, req.headers(), None).await;
    let mut resp = next.run(req).await;
    let status = resp.status();
    if !(status.is_client_error() || status.is_server_error()) {
        return resp;
    }
    // to_bytes 按值取走 Body：先换一个空 Body 进去。
    let body = std::mem::replace(resp.body_mut(), axum::body::Body::empty());
    let body_bytes = match axum::body::to_bytes(body, 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return resp,
    };
    let mut v: serde_json::Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(_) => return rebuild(status, resp.headers().clone(), body_bytes.to_vec()),
    };
    if let Some(code) = v.get("code").and_then(|c| c.as_i64()) {
        if code != 0 {
            // N1：带 message_key 的错误渲染该 key（带 args，如删套餐被
            // N 人引用）；无 key 时按 errors.e<code> 渲染；文案缺失时
            // 保留业务 message。
            let rendered = match v.get("message_key").and_then(|k| k.as_str()) {
                Some(key) => {
                    let args = v
                        .get("args")
                        .and_then(|a| a.as_object())
                        .cloned()
                        .unwrap_or_default();
                    let rendered = state.i18n.t_json_args(&locale, key, &args);
                    if rendered == key {
                        None
                    } else {
                        Some(rendered)
                    }
                }
                None => {
                    let localized = state.i18n.error_message(&locale, code as i32);
                    if localized != format!("errors.e{code}") {
                        Some(localized)
                    } else {
                        None
                    }
                }
            };
            if let Some(rendered) = rendered {
                v["message"] = serde_json::Value::String(rendered);
            }
        }
    }
    let new_body = serde_json::to_vec(&v).unwrap_or_else(|_| body_bytes.to_vec());
    rebuild(status, resp.headers().clone(), new_body)
}

/// 用新 body 重建响应（去掉旧 Content-Length，由 hyper 按实际长度重算）。
fn rebuild(status: StatusCode, mut headers: HeaderMap, body: Vec<u8>) -> Response {
    headers.remove(header::CONTENT_LENGTH);
    let mut r = Response::new(axum::body::Body::from(body));
    *r.status_mut() = status;
    *r.headers_mut() = headers;
    r
}

// ---------------------------------------------------------------------------
// 鉴权提取器
// ---------------------------------------------------------------------------

/// 门户会话（Cookie `iptv_portal_sid`，§4.3）。
pub struct PortalAuth {
    pub token_id: i64,
    pub user_id: i64,
    pub session_id: i64,
    pub device_id: String,
    pub locale: String,
}

impl FromRequestParts<Arc<AppState>> for PortalAuth {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let headers = &parts.headers;
        // 会话 locale 以 Token 上存的为准（token.locale），Accept-Language 只用于错误文案本地化。
        let cookie = headers
            .get(header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .and_then(|c| {
                c.split(';').find_map(|part| {
                    let (k, v) = part.trim().split_once('=')?;
                    if k.trim() == "iptv_portal_sid" {
                        Some(v.trim().to_string())
                    } else {
                        None
                    }
                })
            })
            .ok_or_else(|| ApiError::new(ErrorCode::SessionExpired, "missing portal session"))?;
        let hash = sha256_hex(cookie.as_bytes());
        let hash_bytes: [u8; 32] = hex::decode(&hash)
            .unwrap_or_default()
            .try_into()
            .unwrap_or([0u8; 32]);
        let session = state
            .store
            .find_portal_session_by_hash(&hash_bytes)
            .await
            .map_err(ApiError::from)?
            .ok_or_else(|| ApiError::new(ErrorCode::SessionExpired, "session not found"))?;
        if session.revoked_at.is_some() || session.expires_at < chrono::Utc::now() {
            return Err(ApiError::new(ErrorCode::SessionExpired, "session expired"));
        }
        let token = state
            .store
            .get_token(session.token_id)
            .await
            .map_err(ApiError::from)?
            .ok_or_else(|| ApiError::new(ErrorCode::TokenDisabledOrExpired, "token not found"))?;
        // m3：滑动续期——每次鉴权成功延长 `portal_session_ttl_days`
        // （默认 7 天），但总有效期不超过首次登录起
        // `remember_device_ttl_days`（默认 90 天）；此前每次都重置为
        // now+7 天，“记住设备 90 天”名不副实，会话可无限续期。
        let now = chrono::Utc::now();
        let slide_days = state.store.get_setting_i64("portal_session_ttl_days").await;
        let cap_days = state
            .store
            .get_setting_i64("remember_device_ttl_days")
            .await;
        let cap = session.created_at + chrono::Duration::days(cap_days.max(1));
        let new_exp = (now + chrono::Duration::days(slide_days.max(1))).min(cap);
        let _ = state
            .store
            .touch_portal_session(session.id, now, new_exp)
            .await;
        Ok(Self {
            token_id: session.token_id,
            user_id: token.user_id,
            session_id: session.id,
            device_id: session.device_id.clone(),
            locale: token.locale.clone(),
        })
    }
}

/// 管理员会话（Cookie `iptv_admin_sid` = base64(admin_id|exp|hmac)）。
pub struct AdminAuth {
    pub admin_id: i64,
    pub username: String,
    pub role: iptv_dto::AdminRole,
}

impl FromRequestParts<Arc<AppState>> for AdminAuth {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let headers = &parts.headers;
        let cookie = headers
            .get(header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .and_then(|c| {
                c.split(';').find_map(|part| {
                    let (k, v) = part.trim().split_once('=')?;
                    if k.trim() == "iptv_admin_sid" {
                        Some(v.trim().to_string())
                    } else {
                        None
                    }
                })
            })
            .ok_or_else(|| ApiError::new(ErrorCode::Unauthorized, "missing admin session"))?;
        let admin_id = verify_admin_cookie(&state.config.admin_secret, &cookie)
            .ok_or_else(|| ApiError::new(ErrorCode::Unauthorized, "invalid admin session"))?;
        let admins = state.store.list_admins().await.map_err(ApiError::from)?;
        let admin = admins
            .into_iter()
            .find(|a| a.id == admin_id)
            .ok_or_else(|| ApiError::new(ErrorCode::Unauthorized, "admin not found"))?;
        Ok(Self {
            admin_id,
            username: admin.username,
            role: admin.role,
        })
    }
}

/// 签发管理员会话 Cookie 值。
pub fn sign_admin_cookie(secret: &str, admin_id: i64, exp_epoch: i64) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let msg = format!("{admin_id}|{exp_epoch}");
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac key");
    mac.update(msg.as_bytes());
    let sig = hex::encode(mac.finalize().into_bytes());
    base64::Engine::encode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        format!("{msg}|{sig}").as_bytes(),
    )
}

/// 验签管理员会话 Cookie，返回 admin_id（角色以 DB 为准，调用方再查）。
pub fn verify_admin_cookie(secret: &str, cookie: &str) -> Option<i64> {
    use base64::Engine;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cookie)
        .ok()?;
    let text = String::from_utf8(raw).ok()?;
    let mut parts = text.split('|');
    let admin_id: i64 = parts.next()?.parse().ok()?;
    let exp: i64 = parts.next()?.parse().ok()?;
    let sig = parts.next()?;
    if chrono::Utc::now().timestamp() > exp {
        return None;
    }
    let msg = format!("{admin_id}|{exp}");
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(msg.as_bytes());
    let expected = mac.finalize().into_bytes();
    let Ok(given) = hex::decode(sig) else {
        return None;
    };
    // m11：等时比较（与 play_url 的验签一致；普通字符串比较会泄漏时序）。
    use subtle::ConstantTimeEq;
    if expected.as_slice().ct_eq(given.as_slice()).unwrap_u8() != 1 {
        return None;
    }
    Some(admin_id)
}

// ---------------------------------------------------------------------------
// 公开端点：健康、指标、OpenAPI
// ---------------------------------------------------------------------------

/// m21：OpenAPI 文档由 utoipa 从 `#[utoipa::path]` 标注的代码生成
/// （`#[derive(OpenApi)]`），不再是静态 `openapi.json` 文件。
/// `version` 取 Cargo 包版本（`env!("CARGO_PKG_VERSION")`），随发版自动
/// 走，不会再过期（v0.1.4 时静态文件仍写 0.1.0）。
#[derive(utoipa::OpenApi)]
#[openapi(
    info(
        title = "Muse-IPTV API",
        description = "IPTV 播放平台后端 API（开发文档 v1.2 §3/§4.5/§5.2）。统一响应包 { code, message, data }，code==0 成功；message 按 Accept-Language 本地化，code 永远不变。门户用 Cookie iptv_portal_sid，后台用 Cookie iptv_admin_sid。",
        version = env!("CARGO_PKG_VERSION")
    ),
    paths(
        handlers::auth::ticket_exchange,
        handlers::auth::token_login,
        handlers::auth::logout,
        handlers::auth::logout_all,
        handlers::me::get_me,
        handlers::me::get_usage,
        handlers::me::list_devices,
        handlers::me::kick_device,
        handlers::me::list_my_channels,
        handlers::me::history,
        handlers::me::plan,
        handlers::me::patch_settings,
        handlers::playback::start,
        handlers::playback::heartbeat,
        handlers::playback::stop,
        handlers::public::list_channels,
        handlers::public::epg_programs,
        handlers::public::epg_now,
        handlers::admin::login,
        handlers::admin::stats_overview,
        handlers::admin::list_users,
        handlers::admin::create_user,
        handlers::admin::get_user,
        handlers::admin::update_user,
        handlers::admin::delete_user,
        handlers::admin::list_tokens,
        handlers::admin::create_token,
        handlers::admin::update_token,
        handlers::admin::delete_token,
        handlers::admin::create_login_ticket,
        handlers::admin::revoke_token_sessions,
        handlers::admin::list_channels,
        handlers::admin::create_channel,
        handlers::admin::update_channel,
        handlers::admin::delete_channel,
        handlers::admin::list_sources,
        handlers::admin::create_source,
        handlers::admin::update_source,
        handlers::admin::delete_source,
        handlers::admin::upload_m3u,
        handlers::admin::get_upload_task,
        handlers::admin::trigger_sync,
        handlers::admin::source_diff,
        handlers::admin::source_logs,
        handlers::admin::list_groups,
        handlers::admin::create_group,
        handlers::admin::update_group,
        handlers::admin::delete_group,
        handlers::admin::list_epg_sources,
        handlers::admin::create_epg_source,
        handlers::admin::update_epg_source,
        handlers::admin::delete_epg_source,
        handlers::admin::list_sessions,
        handlers::admin::kick_session,
        handlers::admin::list_plans,
        handlers::admin::create_plan,
        handlers::admin::update_plan,
        handlers::admin::delete_plan,
        handlers::admin::list_admins,
        handlers::admin::create_admin,
        handlers::admin::update_admin,
        handlers::admin::delete_admin,
        handlers::admin::reset_admin_totp,
        handlers::admin::list_audit,
        handlers::admin::export_audit,
        handlers::admin::list_settings,
        handlers::admin::update_settings,
        handlers::admin::list_i18n_overrides,
        handlers::admin::upsert_i18n_override,
        handlers::admin::delete_i18n_override,
        handlers::m18_m19::paste_m3u,
        handlers::m18_m19::list_epg_channel_maps_for_source,
        handlers::m18_m19::list_epg_channel_maps,
        handlers::m18_m19::upsert_epg_channel_map,
        handlers::m18_m19::delete_epg_channel_map,
        proxy::proxy_manifest,
        proxy::proxy_segment,
        health,
        metrics,
        openapi_json
    ),
    components(schemas(
        iptv_dto::TicketExchangeRequest,
        iptv_dto::TokenLoginRequest,
        iptv_dto::AdminLoginRequest,
        iptv_dto::CreateUserRequest,
        iptv_dto::UpdateUserRequest,
        iptv_dto::CreateTokenRequest,
        iptv_dto::UpdateTokenRequest,
        iptv_dto::CreateChannelRequest,
        iptv_dto::UpdateChannelRequest,
        iptv_dto::CreateSourceRequest,
        iptv_dto::UpdateSourceRequest,
        iptv_dto::CreateGroupRequest,
        iptv_dto::UpdateGroupRequest,
        iptv_dto::UpdateEpgSourceRequest,
        iptv_dto::CreatePlanRequest,
        iptv_dto::UpdatePlanRequest,
        iptv_dto::CreateAdminRequest,
        iptv_dto::UpdateAdminRequest,
        iptv_dto::MeSettingsPatch,
        iptv_dto::PlaybackStartRequest,
        iptv_dto::PlaybackHeartbeatRequest,
        iptv_dto::PlaybackStopRequest,
        iptv_dto::UpsertI18nOverrideRequest,
        iptv_dto::PasteM3uRequest,
        iptv_dto::UpsertEpgChannelMapRequest
    )),
    tags(
        (name = "auth", description = "门户鉴权"),
        (name = "me", description = "我的资料"),
        (name = "playback", description = "播放会话与并发"),
        (name = "channels", description = "频道与 EPG"),
        (name = "admin", description = "管理后台")
    )
)]
pub struct ApiDoc;

#[utoipa::path(
    get,
    path = "/healthz",
    tag = "auth",
    responses((status = 200, description = "健康检查"))
)]
async fn health(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Json<HealthResponse> {
    let db_ok = state.store.get_setting("default_locale").await.is_ok();
    Json(HealthResponse {
        status: if db_ok {
            "ok".into()
        } else {
            "degraded".into()
        },
        version: iptv_core::VERSION.into(),
        database: state.store.dialect().to_string(),
        // M15：票据已落库（`login_ticket` 表），不再依赖 Redis/内存；
        // `redis` 字段只报告并发限流器的 Redis 配置状态：配了且连上为
        // `connected`，未配为 `not-configured`（限流器走内存兜底，见 state.rs）。
        redis: if state.limiter.uses_redis() {
            "connected".into()
        } else {
            "not-configured".into()
        },
    })
}

#[utoipa::path(get, path = "/metrics", tag = "auth", responses((status = 200, description = "Prometheus 指标")))]
async fn metrics(axum::extract::State(state): axum::extract::State<Arc<AppState>>) -> Response {
    let body = format!(
        "# HELP iptv_http_requests_total Total HTTP requests\n# TYPE iptv_http_requests_total counter\niptv_http_requests_total {}\n# HELP iptv_playback_starts_total Playback starts\n# TYPE iptv_playback_starts_total counter\niptv_playback_starts_total {}\n# HELP iptv_concurrent_limit_hits_total Concurrent-limit rejections (42901)\n# TYPE iptv_concurrent_limit_hits_total counter\niptv_concurrent_limit_hits_total {}\n# HELP iptv_ticket_exchanges_total Ticket exchanges\n# TYPE iptv_ticket_exchanges_total counter\niptv_ticket_exchanges_total {}\n",
        state.metrics.http_requests.load(Ordering::Relaxed),
        state.metrics.playback_starts.load(Ordering::Relaxed),
        state.metrics.concurrent_limit_hits.load(Ordering::Relaxed),
        state.metrics.ticket_exchanges.load(Ordering::Relaxed),
    );
    Response::builder()
        .header(header::CONTENT_TYPE, "text/plain; version=0.0.4")
        .body(axum::body::Body::from(body))
        .unwrap()
}

#[utoipa::path(get, path = "/api/v1/openapi.json", tag = "auth", responses((status = 200, description = "OpenAPI 规范")))]
async fn openapi_json() -> Json<utoipa::openapi::OpenApi> {
    // m21：返回 utoipa 从代码生成的文档（`ApiDoc::openapi()`），
    // 版本号取 Cargo 包版本，不再读静态文件。
    Json(ApiDoc::openapi())
}

// ---------------------------------------------------------------------------
// 路由总表（§3.1 / §4.5 / §5.2）
// ---------------------------------------------------------------------------

impl AppState {
    pub fn router(state: Arc<Self>) -> Router {
        let api = Router::new()
            // §4.5 门户鉴权
            .route(
                "/auth/ticket-exchange",
                post(handlers::auth::ticket_exchange),
            )
            .route("/auth/token-login", post(handlers::auth::token_login))
            .route("/auth/logout", post(handlers::auth::logout))
            .route("/auth/logout-all", post(handlers::auth::logout_all))
            // §4.5 我的资料
            .route("/me", get(handlers::me::get_me))
            .route("/me/usage", get(handlers::me::get_usage))
            .route("/me/devices", get(handlers::me::list_devices))
            .route(
                "/me/devices/{session_id}",
                delete(handlers::me::kick_device),
            )
            .route("/me/channels", get(handlers::me::list_my_channels))
            .route("/me/history", get(handlers::me::history))
            .route("/me/plan", get(handlers::me::plan))
            .route("/me/settings", patch(handlers::me::patch_settings))
            // §4.5 / §6 播放
            .route("/playback/start", post(handlers::playback::start))
            .route("/playback/heartbeat", post(handlers::playback::heartbeat))
            .route("/playback/stop", post(handlers::playback::stop))
            // §3.1 频道 / EPG（门户鉴权）
            .route("/channels", get(handlers::public::list_channels))
            .route("/epg/now", get(handlers::public::epg_now))
            .route(
                "/epg/channels/{channel_id}/programs",
                get(handlers::public::epg_programs),
            )
            // §5.2 后台
            .route("/admin/auth/login", post(handlers::admin::login))
            .route(
                "/admin/stats/overview",
                get(handlers::admin::stats_overview),
            )
            .route(
                "/admin/users",
                get(handlers::admin::list_users).post(handlers::admin::create_user),
            )
            .route(
                "/admin/users/{id}",
                get(handlers::admin::get_user)
                    .patch(handlers::admin::update_user)
                    .delete(handlers::admin::delete_user),
            )
            .route(
                "/admin/tokens",
                get(handlers::admin::list_tokens).post(handlers::admin::create_token),
            )
            .route(
                "/admin/tokens/{id}",
                patch(handlers::admin::update_token).delete(handlers::admin::delete_token),
            )
            .route(
                "/admin/tokens/{id}/login-ticket",
                post(handlers::admin::create_login_ticket),
            )
            .route(
                "/admin/tokens/{id}/revoke-sessions",
                post(handlers::admin::revoke_token_sessions),
            )
            .route(
                "/admin/channels",
                get(handlers::admin::list_channels).post(handlers::admin::create_channel),
            )
            .route(
                "/admin/channels/{id}",
                patch(handlers::admin::update_channel).delete(handlers::admin::delete_channel),
            )
            .route(
                "/admin/sources",
                get(handlers::admin::list_sources).post(handlers::admin::create_source),
            )
            .route(
                "/admin/sources/{id}",
                patch(handlers::admin::update_source).delete(handlers::admin::delete_source),
            )
            // 上传不限大小（用户 2026-11-25 亲口批准，与 §7.1 的 20 MB 相悖）：
            // 只对本路由取消 axum 默认 2 MB 体限；handler 内流式落盘+逐行
            // 解析，不整包进内存。其他接口仍受默认体限保护。
            .route(
                "/admin/sources/upload",
                post(handlers::admin::upload_m3u).layer(axum::extract::DefaultBodyLimit::disable()),
            )
            // M16：异步上传任务进度查询（§7.1）。
            .route(
                "/admin/upload-tasks/{id}",
                get(handlers::admin::get_upload_task),
            )
            // m18：粘贴 M3U 文本导入（与上传同一解析器、同一入库路径；
            // JSON 体仍受 axum 默认 2 MB 体限保护，超大文本请走上传）。
            .route("/admin/sources/paste", post(handlers::m18_m19::paste_m3u))
            .route(
                "/admin/sources/{id}/sync",
                post(handlers::admin::trigger_sync),
            )
            .route(
                "/admin/sources/{id}/diff",
                get(handlers::admin::source_diff),
            )
            .route(
                "/admin/sources/{id}/logs",
                get(handlers::admin::source_logs),
            )
            .route(
                "/admin/groups",
                get(handlers::admin::list_groups).post(handlers::admin::create_group),
            )
            .route(
                "/admin/groups/{id}",
                patch(handlers::admin::update_group).delete(handlers::admin::delete_group),
            )
            .route(
                "/admin/epg/sources",
                get(handlers::admin::list_epg_sources).post(handlers::admin::create_epg_source),
            )
            .route(
                "/admin/epg/sources/{id}",
                patch(handlers::admin::update_epg_source)
                    .delete(handlers::admin::delete_epg_source),
            )
            // m19：EPG 频道映射可配置（先查映射表，再回落 tvg-id/名称匹配）。
            .route(
                "/admin/epg/sources/{id}/maps",
                get(handlers::m18_m19::list_epg_channel_maps_for_source)
                    .put(handlers::m18_m19::upsert_epg_channel_map),
            )
            .route(
                "/admin/epg/maps",
                get(handlers::m18_m19::list_epg_channel_maps),
            )
            .route(
                "/admin/epg/maps/{id}",
                delete(handlers::m18_m19::delete_epg_channel_map),
            )
            .route("/admin/sessions", get(handlers::admin::list_sessions))
            .route(
                "/admin/sessions/{id}",
                delete(handlers::admin::kick_session),
            )
            .route(
                "/admin/plans",
                get(handlers::admin::list_plans).post(handlers::admin::create_plan),
            )
            .route(
                "/admin/plans/{id}",
                patch(handlers::admin::update_plan).delete(handlers::admin::delete_plan),
            )
            .route(
                "/admin/admins",
                get(handlers::admin::list_admins).post(handlers::admin::create_admin),
            )
            .route(
                "/admin/admins/{id}",
                patch(handlers::admin::update_admin).delete(handlers::admin::delete_admin),
            )
            .route(
                "/admin/admins/{id}/reset-totp",
                post(handlers::admin::reset_admin_totp),
            )
            .route("/admin/audit", get(handlers::admin::list_audit))
            .route("/admin/audit/export", get(handlers::admin::export_audit))
            .route(
                "/admin/settings",
                get(handlers::admin::list_settings).patch(handlers::admin::update_settings),
            )
            .route(
                "/admin/i18n-overrides",
                get(handlers::admin::list_i18n_overrides)
                    .put(handlers::admin::upsert_i18n_override)
                    .delete(handlers::admin::delete_i18n_override),
            );

        Router::new()
            .route("/healthz", get(health))
            .route("/metrics", get(metrics))
            .route("/api/v1/openapi.json", get(openapi_json))
            // axum 0.8 不允许一段路径里参数与 `.m3u8` 后缀混写，故用通配
            // 路由接 `/p/{channel_id}.m3u8`，id 由 handler 从 rest 里拆。
            .route("/p/{*rest}", get(proxy::proxy_manifest))
            .route("/p/seg", get(proxy::proxy_segment))
            .nest("/api/v1", api)
            // 注意：from_fn 的中间件拿不到 with_state 的 state（metrics 计数
            // 因此长期是 0），必须用 from_fn_with_state 把 state 显式传进去。
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                metrics_middleware,
            ))
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                localize_errors_middleware,
            ))
            .layer(axum::middleware::from_fn(csrf_middleware))
            .layer(axum::middleware::from_fn(security_headers_middleware))
            .layer(build_cors_layer(&state.config.cors_origins))
            .with_state(state)
    }
}

/// 响应 helper：把 ApiError 按请求 locale 本地化。
/// 各 handler 返回 `Result<Json<Envelope<T>>, ApiError>` 时由 axum 的
/// `IntoResponse` 兜底；这里提供一个显式的转换供需要的地方用。
pub fn error_response(state: &AppState, locale: &str, err: ApiError) -> Response {
    let (status, body) = err.localized(state, locale);
    (status, body).into_response()
}

/// 脱敏 IP（门户设备列表用，§4.1）。
pub fn masked(ip: Option<&str>) -> String {
    ip.map(mask_ip).unwrap_or_else(|| "***".into())
}

#[cfg(test)]
mod csrf_tests {
    use super::*;
    use crate::state::parse_cors_origins;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn double_submit_match_passes() {
        let h = headers(&[
            ("cookie", "iptv_portal_sid=abc; iptv_csrf=tok123"),
            ("x-csrf-token", "tok123"),
            ("origin", "https://evil.example.com"),
        ]);
        assert!(csrf_check(&h, &Method::POST));
    }

    #[test]
    fn double_submit_mismatch_rejected() {
        let h = headers(&[
            ("cookie", "iptv_portal_sid=abc; iptv_csrf=tok123"),
            ("x-csrf-token", "other"),
        ]);
        assert!(!csrf_check(&h, &Method::POST));
    }

    #[test]
    fn double_submit_header_without_cookie_rejected() {
        let h = headers(&[
            ("cookie", "iptv_portal_sid=abc"),
            ("x-csrf-token", "tok123"),
        ]);
        assert!(!csrf_check(&h, &Method::POST));
    }

    #[test]
    fn origin_match_passes_without_token() {
        // SSR 转发前的旧形态 / 无 token 的服务端调用：同源 Origin 放行。
        let h = headers(&[
            ("cookie", "iptv_portal_sid=abc"),
            ("origin", "http://127.0.0.1:8081"),
            ("x-forwarded-host", "127.0.0.1:8081"),
        ]);
        assert!(csrf_check(&h, &Method::POST));
    }

    #[test]
    fn origin_mismatch_rejected_without_token() {
        let h = headers(&[
            ("cookie", "iptv_portal_sid=abc"),
            ("origin", "https://evil.example.com"),
            ("x-forwarded-host", "127.0.0.1:8081"),
        ]);
        assert!(!csrf_check(&h, &Method::POST));
    }

    #[test]
    fn sec_fetch_site_cross_site_rejected_even_without_origin() {
        let h = headers(&[
            ("cookie", "iptv_portal_sid=abc"),
            ("sec-fetch-site", "cross-site"),
        ]);
        assert!(!csrf_check(&h, &Method::POST));
    }

    #[test]
    fn no_origin_no_token_server_to_server_passes() {
        // reqwest 服务端调用：无 Origin、无头、无 Sec-Fetch-Site，放行。
        let h = headers(&[("cookie", "iptv_portal_sid=abc")]);
        assert!(csrf_check(&h, &Method::POST));
    }

    #[test]
    fn get_requests_unaffected() {
        let h = headers(&[
            ("cookie", "iptv_portal_sid=abc"),
            ("origin", "https://evil.example.com"),
            ("sec-fetch-site", "cross-site"),
        ]);
        assert!(csrf_check(&h, &Method::GET));
    }

    #[test]
    fn parse_cors_origins_trims_and_drops_empties() {
        assert_eq!(
            parse_cors_origins("https://a.example.com/, http://b:8080 , ,"),
            vec![
                "https://a.example.com".to_string(),
                "http://b:8080".to_string()
            ]
        );
        assert!(parse_cors_origins("").is_empty());
        assert!(parse_cors_origins("   ").is_empty());
    }

    #[test]
    fn tokens_equal_is_exact() {
        assert!(tokens_equal("abc", "abc"));
        assert!(!tokens_equal("abc", "abd"));
        assert!(!tokens_equal("abc", "abcd"));
        assert!(!tokens_equal("", "abc"));
    }
}
