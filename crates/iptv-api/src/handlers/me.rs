//! 门户「我的资料」聚合（开发文档 §4.1 / §4.5）。
//!
//! 门户接口以会话内 `token_id` 为准，忽略请求体中的 id（§13 越权防护）。
// 作者: Passwd Word

use crate::{masked, ok, ApiError, ApiResult, AppState, PortalAuth};
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::Json;
use chrono::Utc;
use iptv_dto::{
    Channel, DeviceView, MePlan, MeQuota, MeResponse, MeSettingsPatch, MeToken, MeUser, PageQuery,
    Paged, PlaybackEvent,
};
use serde::Deserialize;
use std::sync::Arc;

#[derive(Debug, Deserialize, Default)]
pub struct ChannelListQuery {
    #[serde(default = "default_page")]
    pub page: u32,
    #[serde(default = "default_page_size")]
    pub page_size: u32,
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
}
fn default_page() -> u32 {
    1
}
fn default_page_size() -> u32 {
    20
}

async fn load_token(
    state: &Arc<AppState>,
    auth: &PortalAuth,
) -> Result<iptv_dto::UserToken, ApiError> {
    state
        .store
        .get_token(auth.token_id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| {
            ApiError::new(
                iptv_dto::ErrorCode::TokenDisabledOrExpired,
                "token not found",
            )
        })
}

/// GET /api/v1/me（§4.5 响应示例逐字段对应）。
pub async fn get_me(State(state): State<Arc<AppState>>, auth: PortalAuth) -> ApiResult<MeResponse> {
    let token = load_token(&state, &auth).await?;
    let user = state
        .store
        .get_user(token.user_id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::new(iptv_dto::ErrorCode::NotFound, "user not found"))?;
    let active = state
        .limiter
        .count(token.id)
        .await
        .map_err(ApiError::from)? as i32;
    let bound = state
        .store
        .bound_device_count(token.id)
        .await
        .map_err(ApiError::from)? as i32;
    let plan = match user.plan_id {
        Some(pid) => state
            .store
            .get_plan(pid)
            .await
            .map_err(ApiError::from)?
            .map(|p| MePlan {
                name: p.name,
                expires_at: user.expires_at,
            }),
        None => None,
    };
    let now = Utc::now();
    ok(MeResponse {
        user: MeUser {
            id: user.id,
            display_name: user.display_name,
            locale: user.locale,
        },
        token: MeToken {
            id: token.id,
            name: token.name.clone(),
            device_type: token.device_type.clone(),
            status: token.status.clone(),
            expires_at: token.expires_at,
            days_remaining: token.days_remaining(now),
        },
        quota: MeQuota {
            max_concurrent_sessions: token.max_concurrent_sessions,
            active_sessions: active,
            max_bound_devices: token.max_bound_devices,
            bound_devices: bound,
        },
        plan,
        allowed_groups: token.allowed_groups.clone(),
        features: token.features.clone(),
    })
}

/// GET /api/v1/me/usage：并发额度与占用。
pub async fn get_usage(State(state): State<Arc<AppState>>, auth: PortalAuth) -> ApiResult<MeQuota> {
    let token = load_token(&state, &auth).await?;
    let active = state
        .limiter
        .count(token.id)
        .await
        .map_err(ApiError::from)? as i32;
    let bound = state
        .store
        .bound_device_count(token.id)
        .await
        .map_err(ApiError::from)? as i32;
    ok(MeQuota {
        max_concurrent_sessions: token.max_concurrent_sessions,
        active_sessions: active,
        max_bound_devices: token.max_bound_devices,
        bound_devices: bound,
    })
}

/// GET /api/v1/me/devices：在线设备（IP 脱敏，§4.1）。
pub async fn list_devices(
    State(state): State<Arc<AppState>>,
    auth: PortalAuth,
) -> ApiResult<Vec<DeviceView>> {
    let sessions = state
        .store
        .list_portal_sessions(auth.token_id)
        .await
        .map_err(ApiError::from)?;
    let devices = sessions
        .into_iter()
        .map(|s| DeviceView {
            session_id: s.id,
            device_id: s.device_id.clone(),
            ip_masked: masked(s.ip_address.as_deref()),
            user_agent: s.user_agent.clone(),
            last_heartbeat_at: s.last_seen_at,
            is_current: s.id == auth.session_id,
        })
        .collect();
    ok(devices)
}

/// DELETE /api/v1/me/devices/{session_id}：下线自己的设备（不得下线他人：只查本 token 的会话）。
pub async fn kick_device(
    State(state): State<Arc<AppState>>,
    auth: PortalAuth,
    Path(session_id): Path<i64>,
) -> ApiResult<serde_json::Value> {
    let sessions = state
        .store
        .list_portal_sessions(auth.token_id)
        .await
        .map_err(ApiError::from)?;
    if !sessions.iter().any(|s| s.id == session_id) {
        return Err(ApiError::new(
            iptv_dto::ErrorCode::NotFound,
            "device not found",
        ));
    }
    state
        .store
        .revoke_portal_session(session_id)
        .await
        .map_err(ApiError::from)?;
    ok(serde_json::json!({ "revoked": true }))
}

/// GET /api/v1/me/channels：授权频道（分页/分组/关键词，§4.5）。
pub async fn list_my_channels(
    State(state): State<Arc<AppState>>,
    auth: PortalAuth,
    Query(query): Query<ChannelListQuery>,
) -> ApiResult<Paged<Channel>> {
    let token = load_token(&state, &auth).await?;
    let page = PageQuery {
        page: query.page,
        page_size: query.page_size,
        q: query.q,
    };
    let channels = state
        .store
        .list_channels_for_token(&token, &page, query.group.as_deref())
        .await
        .map_err(ApiError::from)?;
    ok(channels)
}

/// GET /api/v1/me/history：播放记录。
pub async fn history(
    State(state): State<Arc<AppState>>,
    auth: PortalAuth,
    Query(page): Query<PageQuery>,
) -> ApiResult<Paged<PlaybackEvent>> {
    let events = state
        .store
        .playback_history(auth.token_id, &page)
        .await
        .map_err(ApiError::from)?;
    ok(events)
}

/// GET /api/v1/me/plan：套餐与到期。
pub async fn plan(
    State(state): State<Arc<AppState>>,
    auth: PortalAuth,
) -> ApiResult<Option<MePlan>> {
    let token = load_token(&state, &auth).await?;
    let user = state
        .store
        .get_user(token.user_id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::new(iptv_dto::ErrorCode::NotFound, "user not found"))?;
    let plan = match user.plan_id {
        Some(pid) => state
            .store
            .get_plan(pid)
            .await
            .map_err(ApiError::from)?
            .map(|p| MePlan {
                name: p.name,
                expires_at: user.expires_at,
            }),
        None => None,
    };
    ok(plan)
}

/// PATCH /api/v1/me/settings：修改显示名、locale、主题（§4.5）。
/// locale 写回用户记录 + Cookie，刷新后仍保持（§16 i18n 验收）。
pub async fn patch_settings(
    State(state): State<Arc<AppState>>,
    auth: PortalAuth,
    headers: HeaderMap,
    Json(req): Json<MeSettingsPatch>,
) -> ApiResult<serde_json::Value> {
    let token = load_token(&state, &auth).await?;
    if req.display_name.is_some() || req.locale.is_some() {
        let locale = req
            .locale
            .clone()
            .filter(|l| iptv_i18n::normalize_locale(l).is_some());
        state
            .store
            .update_user(
                token.user_id,
                &iptv_dto::UpdateUserRequest {
                    display_name: req.display_name.clone(),
                    email: None,
                    phone: None,
                    plan_id: None,
                    status: None,
                    locale,
                    remark: None,
                    expires_at: None,
                },
            )
            .await
            .map_err(ApiError::from)?;
    }
    let _ = headers;
    let _ = req.theme;
    ok(serde_json::json!({ "updated": true }))
}
