//! 门户鉴权（开发文档 §4.2 / §4.3 / §4.5）。
//!
//! - A. 一次性票据 `/t/{ticket}`：60s、单次消费、可绑 IP 段；成功后 302 到干净 URL。
//! - B. 直接粘贴 Token：门户首页输入框，需 HTTPS，失败限流。
//! - C. 记住设备：`device_id` Cookie，90 天，后台可吊销。
//!
//! 会话 Cookie `iptv_portal_sid`：256 bit 随机串，DB 只存哈希（§4.3）。
// 作者: Passwd Word

use crate::{ApiError, AppState, PortalAuth};
use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{Duration, Utc};
use iptv_auth::sha256_hex;
use iptv_core::{generate_session_value, hash_token};
use iptv_dto::{Envelope, ErrorCode, TicketExchangeRequest, TokenLoginRequest};
use iptv_store::NewAuditLog;
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
    // M1：会话有效期读系统设置（记住设备 `remember_device_ttl_days`，
    // 否则 `portal_session_ttl_days`；默认 90 / 7 天）。
    let days = if remember {
        state
            .store
            .get_setting_i64("remember_device_ttl_days")
            .await
    } else {
        state.store.get_setting_i64("portal_session_ttl_days").await
    };
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
#[utoipa::path(
    post,
    path = "/api/v1/auth/ticket-exchange",
    tag = "auth",
    request_body = TicketExchangeRequest,
    responses(
        (status = 200, description = "票据换会话（60s 单次消费），成功后 302 到 /me"),
        (status = 401, description = "票据无效/过期（40101）"),
        (status = 429, description = "限流（42903）")
    )
)]
pub async fn ticket_exchange(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<TicketExchangeRequest>,
) -> Result<Response, ApiError> {
    // m25：拿不到客户端 IP 时无法按 IP 限流，直接拒绝，不落 "unknown" 桶。
    let Some(ip) = crate::client_ip(&headers, Some(peer)) else {
        return Err(ApiError::new(
            ErrorCode::RateLimited,
            "client ip unavailable",
        ));
    };
    if !crate::check_login_limit(&state, &ip, &format!("ticket-exchange:{ip}")) {
        return Err(ApiError::new(ErrorCode::RateLimited, "too many requests"));
    }
    let ticket = state
        .tickets
        .consume(&req.ticket)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::new(ErrorCode::SessionExpired, "ticket invalid or expired"))?;
    // 可绑 IP 段：票据签发时若绑定了 IP 前缀，兑换 IP 必须匹配。
    if let Some(prefix) = &ticket.ip_prefix {
        if !ip.starts_with(prefix.as_str()) {
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
    // 绑定设备数校验（§6.1 第二维度）：已绑定设备重登录不计入上限——
    // 上限只限制不同设备数，logout-all 后同一台设备重登不能被 42902 挡住。
    if !state
        .store
        .is_device_bound(token.id, &req.device_fingerprint)
        .await
        .map_err(ApiError::from)?
    {
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
    }
    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let value = create_portal_session(
        &state,
        token.id,
        &req.device_fingerprint,
        Some(ip.as_str()),
        ua.as_deref(),
        false,
    )
    .await?;
    state
        .metrics
        .ticket_exchanges
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // m25：兑换成功，重置该复合 key 的计数——成功登录不占限流桶。
    crate::reset_login_limit(&state, &format!("ticket-exchange:{ip}"));
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
#[utoipa::path(post, path = "/api/v1/auth/token-login", tag = "auth", request_body = TokenLoginRequest, responses((status = 200, description = "ok，下发 iptv_portal_sid Cookie"), (status = 429, description = "设备数达上限（42902）")))]
pub async fn token_login(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<TokenLoginRequest>,
) -> Result<Response, ApiError> {
    // m25：拿不到客户端 IP 时无法按 IP 限流，直接拒绝，不落 "unknown" 桶。
    let Some(ip) = crate::client_ip(&headers, Some(peer)) else {
        return Err(ApiError::new(
            ErrorCode::RateLimited,
            "client ip unavailable",
        ));
    };
    // 按 Token 前缀 + IP 计：同一 Token 被多 IP 轮换刷时各 key 独立，
    // 再叠加按 IP 聚合的桶防单 IP 横扫多 Token。
    let prefix = iptv_core::token_prefix(req.token.trim());
    if !crate::check_login_limit(&state, &ip, &format!("token-login:{prefix}:{ip}")) {
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
    // 绑定设备数校验（§6.1 第二维度）：已绑定设备重登录不计入上限——
    // 上限只限制不同设备数，logout-all 后同一台设备重登不能被 42902 挡住。
    if !state
        .store
        .is_device_bound(token.id, &req.device_fingerprint)
        .await
        .map_err(ApiError::from)?
    {
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
    }
    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let value = create_portal_session(
        &state,
        token.id,
        &req.device_fingerprint,
        Some(ip.as_str()),
        ua.as_deref(),
        req.remember_device,
    )
    .await?;
    // m25：登录成功，重置该复合 key 的计数——成功登录不占限流桶。
    crate::reset_login_limit(&state, &format!("token-login:{prefix}:{ip}"));
    // M1：Cookie 有效期与会话 DB 有效期同源（系统设置，默认 90 / 7 天）。
    let max_age = if req.remember_device {
        state
            .store
            .get_setting_i64("remember_device_ttl_days")
            .await
            * 86400
    } else {
        state.store.get_setting_i64("portal_session_ttl_days").await * 86400
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
#[utoipa::path(post, path = "/api/v1/auth/logout", tag = "auth", responses((status = 200, description = "ok")))]
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
    // m14：退出时一并清除双提交 Cookie（SSR 层下次有会话时会重新签发）。
    h.append(
        header::SET_COOKIE,
        "iptv_csrf=; Path=/; SameSite=Lax; Max-Age=0"
            .parse()
            .unwrap(),
    );
    Ok(json_response(
        StatusCode::OK,
        h,
        Envelope::ok(json!({ "revoked": true })),
    ))
}

/// POST /api/v1/auth/logout-all（M14：退出全部设备）。
/// 传 `None`：吊销该 Token 的全部会话，含当前会话；写审计；并清除会话 Cookie。
#[utoipa::path(post, path = "/api/v1/auth/logout-all", tag = "auth", responses((status = 200, description = "ok")))]
pub async fn logout_all(
    State(state): State<Arc<AppState>>,
    auth: PortalAuth,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let n = state
        .store
        .revoke_all_portal_sessions(auth.token_id, None)
        .await
        .map_err(ApiError::from)?;
    // 审计失败不影响退出本身（`let _ =` 吞掉）；actor 用 token 标识，
    // 门户侧无管理员身份。
    let _ = state
        .store
        .write_audit(&NewAuditLog {
            actor: format!("token:{}", auth.token_id),
            actor_role: Some("portal".to_string()),
            action: "auth.logout_all".to_string(),
            object_type: "portal_session".to_string(),
            object_id: auth.token_id.to_string(),
            before_json: None,
            after_json: Some(json!({ "revoked": n })),
            ip_address: crate::client_ip(&headers, None),
        })
        .await;
    let mut h = HeaderMap::new();
    h.insert(
        header::SET_COOKIE,
        clear_portal_cookie_header().parse().unwrap(),
    );
    // m14：退出时一并清除双提交 Cookie（SSR 层下次有会话时会重新签发）。
    h.append(
        header::SET_COOKIE,
        "iptv_csrf=; Path=/; SameSite=Lax; Max-Age=0"
            .parse()
            .unwrap(),
    );
    Ok(json_response(
        StatusCode::OK,
        h,
        Envelope::ok(json!({ "revoked": n })),
    ))
}
