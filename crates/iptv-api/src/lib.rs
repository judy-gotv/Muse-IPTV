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

pub use state::{AppState, Config, Metrics};

use axum::extract::{FromRequestParts, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use iptv_auth::{mask_ip, sha256_hex};
use iptv_dto::{Envelope, ErrorCode, HealthResponse};
use std::sync::atomic::Ordering;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// 错误类型：业务层只抛错误码 + 参数（§12.5），这里统一序列化。
// ---------------------------------------------------------------------------

pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
    pub args: Vec<(String, serde_json::Value)>,
}

impl ApiError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            args: vec![],
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
            },
            iptv_core::CoreError::Internal(m) => Self::new(ErrorCode::Internal, m),
        }
    }
    fn localized(self, state: &AppState, locale: &str) -> (StatusCode, Json<Envelope<()>>) {
        let message = state.i18n.error_message(locale, self.code as i32);
        let message = if message == format!("errors.e{}", self.code as i32) {
            // 无本地化文案时回退业务 message（§12.5：message 仅作兜底）。
            self.message
        } else {
            message
        };
        let env = Envelope::<()>::err(self.code, message);
        (
            StatusCode::from_u16(self.code.http_status()).unwrap_or(StatusCode::BAD_REQUEST),
            Json(env),
        )
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
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
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.code.http_status()).unwrap_or(StatusCode::BAD_REQUEST);
        (status, Json(Envelope::<()>::err(self.code, self.message))).into_response()
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

pub fn client_ip(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(|s| s.trim().to_string())
        .or_else(|| {
            headers
                .get("x-real-ip")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
        })
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
    let locale = negotiate_locale(req.headers(), None);
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
            let localized = state.i18n.error_message(&locale, code as i32);
            if localized != format!("errors.e{code}") {
                v["message"] = serde_json::Value::String(localized);
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
        // 滑动续期：7 天（§4.3），每次鉴权成功延长。
        let now = chrono::Utc::now();
        let new_exp = now + chrono::Duration::days(7);
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
    let expected = hex::encode(mac.finalize().into_bytes());
    if expected != sig {
        return None;
    }
    Some(admin_id)
}

// ---------------------------------------------------------------------------
// 公开端点：健康、指标、OpenAPI
// ---------------------------------------------------------------------------

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
        redis: if state.limiter.uses_redis() {
            "connected".into()
        } else {
            "memory-fallback".into()
        },
    })
}

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

async fn openapi_json() -> Response {
    let spec = include_str!("../openapi.json");
    Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(spec.to_string()))
        .unwrap()
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
                get(handlers::admin::get_user).patch(handlers::admin::update_user),
            )
            .route(
                "/admin/tokens",
                get(handlers::admin::list_tokens).post(handlers::admin::create_token),
            )
            .route("/admin/tokens/{id}", patch(handlers::admin::update_token))
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
            .route("/admin/sources/upload", post(handlers::admin::upload_m3u))
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
                "/admin/epg/sources",
                get(handlers::admin::list_epg_sources).post(handlers::admin::create_epg_source),
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
                "/admin/admins",
                get(handlers::admin::list_admins).post(handlers::admin::create_admin),
            )
            .route("/admin/audit", get(handlers::admin::list_audit))
            .route(
                "/admin/settings",
                get(handlers::admin::list_settings).patch(handlers::admin::update_settings),
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
            .layer(
                tower_http::cors::CorsLayer::new()
                    .allow_origin(tower_http::cors::Any)
                    .allow_methods([
                        axum::http::Method::GET,
                        axum::http::Method::POST,
                        axum::http::Method::PATCH,
                        axum::http::Method::DELETE,
                    ])
                    .allow_headers([
                        header::AUTHORIZATION,
                        header::CONTENT_TYPE,
                        header::ACCEPT_LANGUAGE,
                    ]),
            )
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
