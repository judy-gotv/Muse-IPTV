//! 频道与 EPG 公开查询（门户鉴权，§3.1 / §14.1）。
// 作者: Passwd Word

use crate::{ok, ApiError, ApiResult, AppState, PortalAuth};
use axum::extract::{Path, Query, State};
use chrono::{DateTime, Utc};
use iptv_dto::{Channel, EpgProgram, PageQuery, Paged};
use serde::Deserialize;
use std::sync::Arc;

/// GET /api/v1/channels（按 Token 授权过滤，见 store.list_channels_for_token）。
pub async fn list_channels(
    State(state): State<Arc<AppState>>,
    auth: PortalAuth,
    Query(page): Query<PageQuery>,
) -> ApiResult<Paged<Channel>> {
    let token = state
        .store
        .get_token(auth.token_id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| {
            ApiError::new(
                iptv_dto::ErrorCode::TokenDisabledOrExpired,
                "token not found",
            )
        })?;
    let channels = state
        .store
        .list_channels_for_token(&token, &page, None)
        .await
        .map_err(ApiError::from)?;
    ok(channels)
}

#[derive(Debug, Deserialize)]
pub struct EpgQuery {
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
}

/// GET /api/v1/epg/channels/{channel_id}/programs：7 天节目单（§14.1）。
pub async fn epg_programs(
    State(state): State<Arc<AppState>>,
    _auth: PortalAuth,
    Path(channel_id): Path<i64>,
    Query(q): Query<EpgQuery>,
) -> ApiResult<Vec<EpgProgram>> {
    let now = Utc::now();
    let from = q.from.unwrap_or(now);
    let to = q.to.unwrap_or(now + chrono::Duration::days(7));
    let programs = state
        .store
        .programs_for_channel(channel_id, from, to)
        .await
        .map_err(ApiError::from)?;
    ok(programs)
}

/// GET /api/v1/epg/now：当前节目（?channel_ids=1,2,3）。
#[derive(Debug, Deserialize)]
pub struct NowQuery {
    pub channel_ids: String,
}

pub async fn epg_now(
    State(state): State<Arc<AppState>>,
    _auth: PortalAuth,
    Query(q): Query<NowQuery>,
) -> ApiResult<Vec<EpgProgram>> {
    let now = Utc::now();
    let mut out = Vec::new();
    for part in q.channel_ids.split(',') {
        if let Ok(id) = part.trim().parse::<i64>() {
            if let Some(p) = state
                .store
                .current_program(id, now)
                .await
                .map_err(ApiError::from)?
            {
                out.push(p);
            }
        }
    }
    ok(out)
}
