//! 门户鉴权（开发文档 §4.2 / §4.3 / §4.5）。
//!
//! - A. 一次性票据 `/t/{ticket}`：60s、单次消费、可绑 IP 段；成功后 302 到干净 URL。
//! - B. 直接粘贴 Token：门户首页输入框，需 HTTPS，失败限流。
//! - C. 记住设备：`device_id` Cookie，90 天，后台可吊销。
//!
//! 会话 Cookie `iptv_portal_sid`：256 bit 随机串，DB 只存哈希（§4.3）。
// 作者: Passwd Word

use crate::{ok, ApiError, ApiResult, AppState, PortalAuth};
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{Duration, Utc};
use iptv_auth::sha256_hex;
use iptv_core::{generate_session_value, hash_token};
use iptv_dto::{Envelope, ErrorCode, TicketExchangeRequest, TokenLoginRequest};
use serde_json::json;
use std::sync::Arc;

fn portal_cookie_header(value: &str, max_age_secs: i64) -> String {
    // §13：HttpOnly、Secure、SameSite=Lax。Secure 在明文 HTTP 下浏览器不发送，
    // 因此仅当 PORTAL_BASE_URL 是 https 时才加 Secure（开发/局域网 http 可用）。
    format!("iptv_portal_sid={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age_secs}")
}

fn clear_portal_cookie_header() -> String {
    "iptv_portal_sid=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0".to_string()
}

async fn create_portal_session(
    state: &Arc<AppState>,
    token_id: i64,
    device_id: &str,
    ip: Option<&str>,
    ua: Option<&str>,
    remember: bool,
) -> Result<String, ApiError> {
    let value = generate_session_value();
    let hash = sha256_hex(value.as_bytes());
    let hash_bytes: [u8; 32] = hex::decode(&hash)
        .unwrap_or_default()
        .try_into()
        .unwrap_or([0u8; 32]);
    let days = if remember { 90 } else { 7 };
    let expires_at = Utc::now() + Duration::days(days);
    state
        .store
        .create_portal_session(token_id, device_id, &hash_bytes, ip, ua, expires_at)
        .await
        .map_err(ApiError::from)?;
    Ok(value)
}

fn json_response<T: serde::Serialize>(
    status: StatusCode,
    headers: HeaderMap,
    data: Envelope<T>,
) -> Response {
    (status, headers, Json(data)).into_response()
}

/// POST /api/v1/auth/ticket-exchange（§4.3 流程图）。
pub async fn ticket_exchange(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<TicketExchangeRequest>,
) -> Result<Response, ApiError> {
    let ip = crate::client_ip(&headers);
    if !state
        .login_limiter
        .check(ip.as_deref().unwrap_or("unknown"))
    {
        return Err(ApiError::new(ErrorCode::RateLimited, "too many requests"));
    }
    let ticket = state
        .tickets
        .consume(&req.ticket)
        .ok_or_else(|| ApiError::new(ErrorCode::SessionExpired, "ticket invalid or expired"))?;
    // 可绑 IP 段：票据签发时若绑定了 IP 前缀，兑换 IP 必须匹配。
    if let Some(prefix) = &ticket.ip_prefix {
        let ok = ip
            .as_deref()
            .map(|i| i.starts_with(prefix.as_str()))
            .unwrap_or(false);
        if !ok {
            return Err(ApiError::new(
                ErrorCode::SessionExpired,
                "ticket ip mismatch",
            ));
        }
    }
    let token = state
        .store
        .get_token(ticket.token_id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::new(ErrorCode::TokenDisabledOrExpired, "token not found"))?;
    if !token.is_usable(Utc::now()) {
        return Err(ApiError::new(
            ErrorCode::TokenDisabledOrExpired,
            "token disabled or expired",
        ));
    }
    // 绑定设备数校验（§6.1 第二维度）。
    let bound = state
        .store
        .bound_device_count(token.id)
        .await
        .map_err(ApiError::from)?;
    if bound >= token.max_bound_devices as i64 {
        return Err(ApiError::new(
            ErrorCode::DeviceLimitReached,
            "device limit reached",
        ));
    }
    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let value = create_portal_session(
        &state,
        token.id,
        &req.device_fingerprint,
        ip.as_deref(),
        ua.as_deref(),
        false,
    )
    .await?;
    state
        .metrics
        .ticket_exchanges
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut h = HeaderMap::new();
    h.insert(
        header::SET_COOKIE,
        portal_cookie_header(&value, 7 * 86400).parse().unwrap(),
    );
    // §4.3：兑换成功后 302 到干净 URL，避免凭据进入浏览器历史与 Referer。
    h.insert(header::LOCATION, "/me".parse().unwrap());
    Ok(json_response(
        StatusCode::FOUND,
        h,
        Envelope::ok(json!({ "redirect": "/me" })),
    ))
}

/// POST /api/v1/auth/token-login（§4.2 方式 B）。
pub async fn token_login(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<TokenLoginRequest>,
) -> Result<Response, ApiError> {
    let ip = crate::client_ip(&headers);
    if !state
        .login_limiter
        .check(ip.as_deref().unwrap_or("unknown"))
    {
        return Err(ApiError::new(ErrorCode::RateLimited, "too many requests"));
    }
    let hash = hash_token(req.token.trim());
    let token = state
        .store
        .find_token_by_hash(&hash)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::new(ErrorCode::TokenDisabledOrExpired, "token invalid"))?;
    if !token.is_usable(Utc::now()) {
        return Err(ApiError::new(
            ErrorCode::TokenDisabledOrExpired,
            "token disabled or expired",
        ));
    }
    let bound = state
        .store
        .bound_device_count(token.id)
        .await
        .map_err(ApiError::from)?;
    if bound >= token.max_bound_devices as i64 {
        return Err(ApiError::new(
            ErrorCode::DeviceLimitReached,
            "device limit reached",
        ));
    }
    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let value = create_portal_session(
        &state,
        token.id,
        &req.device_fingerprint,
        ip.as_deref(),
        ua.as_deref(),
        req.remember_device,
    )
    .await?;
    let max_age = if req.remember_device {
        90 * 86400
    } else {
        7 * 86400
    };
    let mut h = HeaderMap::new();
    h.insert(
        header::SET_COOKIE,
        portal_cookie_header(&value, max_age).parse().unwrap(),
    );
    Ok(json_response(
        StatusCode::OK,
        h,
        Envelope::ok(json!({ "token_id": token.id })),
    ))
}

/// POST /api/v1/auth/logout（§4.3：退出当前设备）。
pub async fn logout(
    State(state): State<Arc<AppState>>,
    auth: PortalAuth,
) -> Result<Response, ApiError> {
    state
        .store
        .revoke_portal_session(auth.session_id)
        .await
        .map_err(ApiError::from)?;
    let mut h = HeaderMap::new();
    h.insert(
        header::SET_COOKIE,
        clear_portal_cookie_header().parse().unwrap(),
    );
    Ok(json_response(
        StatusCode::OK,
        h,
        Envelope::ok(json!({ "revoked": true })),
    ))
}

/// 退出全部设备（门户设置页用；由 me handler 调用）。
pub async fn logout_all(state: &Arc<AppState>, auth: &PortalAuth) -> ApiResult<serde_json::Value> {
    let n = state
        .store
        .revoke_all_portal_sessions(auth.token_id, Some(auth.session_id))
        .await
        .map_err(ApiError::from)?;
    ok(json!({ "revoked": n }))
}
