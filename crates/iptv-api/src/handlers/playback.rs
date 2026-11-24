//! 播放会话（开发文档 §6、§8）。
//!
//! 判定顺序（§6.4）：会话 → Token 状态与有效期 → 频道鉴权（分组/黑名单/
//! 清晰度上限）→ Redis Lua 扣减 → 签发短时效播放地址（HMAC，300s）。
//! 第 N+1 路返回 429 + `42901`（§16 验收）。
// 作者: Passwd Word

use crate::{ok, ApiError, ApiResult, AppState, PortalAuth};
use axum::extract::State;
use axum::Json;
use chrono::Utc;
use iptv_core::{authorize_channel, sign_play_url};
use iptv_dto::{
    ErrorCode, PlaybackHeartbeatRequest, PlaybackStartRequest, PlaybackStartResponse,
    PlaybackStopRequest,
};
use iptv_store::NewPlaybackEvent;
use std::sync::atomic::Ordering;
use std::sync::Arc;

fn session_uid() -> String {
    format!("ps_{}", uuid::Uuid::new_v4().as_simple())
}

/// POST /api/v1/playback/start
pub async fn start(
    State(state): State<Arc<AppState>>,
    auth: PortalAuth,
    Json(req): Json<PlaybackStartRequest>,
) -> ApiResult<PlaybackStartResponse> {
    let token = state
        .store
        .get_token(auth.token_id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::new(ErrorCode::TokenDisabledOrExpired, "token not found"))?;
    let channel = state
        .store
        .get_channel(req.channel_id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::new(ErrorCode::NotFound, "channel not found"))?;
    // 1-3 步：Token + 频道鉴权 + 清晰度上限（纯规则，见 iptv-core）。
    let decision = authorize_channel(&token, &channel, req.quality.as_deref(), Utc::now())
        .map_err(ApiError::from_core)?;
    // 4 步：Redis Lua 原子扣减。
    let uid = session_uid();
    let outcome = state
        .limiter
        .acquire(token.id, &uid, token.max_concurrent_sessions)
        .await
        .map_err(ApiError::from)?;
    if !outcome.acquired {
        state
            .metrics
            .concurrent_limit_hits
            .fetch_add(1, Ordering::Relaxed);
        return Err(ApiError::from_core(
            iptv_core::CoreError::coded(
                ErrorCode::ConcurrentLimitReached,
                "concurrent limit reached",
            )
            .with_arg("used", serde_json::json!(outcome.current))
            .with_arg("max", serde_json::json!(token.max_concurrent_sessions)),
        ));
    }
    // 5-6 步：DB 记流水 + 签发播放地址。
    let now = Utc::now();
    state
        .store
        .create_playback_session(
            token.id,
            channel.id,
            &req.device_id,
            &uid,
            Some(decision.quality.as_str()),
            None,
        )
        .await
        .map_err(ApiError::from)?;
    let _ = state
        .store
        .record_event(&NewPlaybackEvent {
            session_uid: uid.clone(),
            token_id: token.id,
            channel_id: channel.id,
            event_type: "start".into(),
            detail: serde_json::json!({ "quality": decision.quality }),
        })
        .await;
    let exp = now.timestamp() + 300;
    let play_url = sign_play_url(
        state.config.play_secret.as_bytes(),
        &format!("/p/{}.m3u8", channel.id),
        &uid,
        channel.id,
        exp,
    );
    state
        .metrics
        .playback_starts
        .fetch_add(1, Ordering::Relaxed);
    ok(PlaybackStartResponse {
        session_uid: uid,
        play_url,
        expires_in_sec: 300,
    })
}

/// POST /api/v1/playback/heartbeat（30s 一次，刷新 Redis score 与 DB 心跳）。
pub async fn heartbeat(
    State(state): State<Arc<AppState>>,
    auth: PortalAuth,
    Json(req): Json<PlaybackHeartbeatRequest>,
) -> ApiResult<serde_json::Value> {
    let session = state
        .store
        .get_playback_session(&req.session_uid)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::new(ErrorCode::NotFound, "playback session not found"))?;
    if session.token_id != auth.token_id {
        return Err(ApiError::new(ErrorCode::Forbidden, "not your session"));
    }
    let alive = state
        .limiter
        .heartbeat(session.token_id, &req.session_uid)
        .await
        .map_err(ApiError::from)?;
    if !alive {
        return Err(ApiError::new(
            ErrorCode::SessionExpired,
            "playback session expired",
        ));
    }
    state
        .store
        .heartbeat_session(&req.session_uid, Utc::now())
        .await
        .map_err(ApiError::from)?;
    ok(serde_json::json!({ "ok": true }))
}

/// POST /api/v1/playback/stop（释放并发额度）。
pub async fn stop(
    State(state): State<Arc<AppState>>,
    auth: PortalAuth,
    Json(req): Json<PlaybackStopRequest>,
) -> ApiResult<serde_json::Value> {
    let session = state
        .store
        .get_playback_session(&req.session_uid)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::new(ErrorCode::NotFound, "playback session not found"))?;
    if session.token_id != auth.token_id {
        return Err(ApiError::new(ErrorCode::Forbidden, "not your session"));
    }
    state
        .limiter
        .release(session.token_id, &req.session_uid)
        .await
        .map_err(ApiError::from)?;
    state
        .store
        .end_session(&req.session_uid, Utc::now(), "ended")
        .await
        .map_err(ApiError::from)?;
    let _ = state
        .store
        .record_event(&NewPlaybackEvent {
            session_uid: req.session_uid.clone(),
            token_id: session.token_id,
            channel_id: session.channel_id,
            event_type: "stop".into(),
            detail: serde_json::json!({}),
        })
        .await;
    ok(serde_json::json!({ "ok": true }))
}
