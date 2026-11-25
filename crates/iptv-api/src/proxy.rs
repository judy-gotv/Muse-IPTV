//! 播放代理（开发文档 §8）。
//!
//! `GET /p/{channel_id}.m3u8?exp&session&sig`：验签 → 取频道主地址 → 拉取
//! 上游 manifest → 把相对分片地址改写为同签名的 `/p/seg` 代理地址。
//! 上游真实地址永不下发给客户端（§8）。
//! `GET /p/seg?u=<urlencoded>&exp&session&sig`：验签 + SSRF 校验（目标 host
//! 必须与频道主地址 host 一致）→ 透传 Range 流式回源。
// 作者: Passwd Word

use crate::{ApiError, AppState};
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::Response;
use iptv_core::verify_play_url;
use serde::Deserialize;
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub struct PlayQuery {
    pub exp: i64,
    pub session: String,
    pub sig: String,
}

#[derive(Debug, Deserialize)]
pub struct SegQuery {
    pub u: String,
    pub exp: i64,
    pub session: String,
    pub sig: String,
}

fn verify(state: &AppState, channel_id: i64, q: &PlayQuery) -> Result<(), ApiError> {
    let ok = verify_play_url(
        state.config.play_secret.as_bytes(),
        &q.session,
        channel_id,
        q.exp,
        &q.sig,
        chrono::Utc::now().timestamp(),
    );
    if !ok {
        return Err(ApiError::new(
            iptv_dto::ErrorCode::SessionExpired,
            "invalid or expired play url",
        ));
    }
    Ok(())
}

/// m7：播放会话须仍是 active——签名本身 300s 有效，stop 后若不查状态，
/// 停播的地址在剩余有效期内仍能拉流。返回会话的 channel_id 供调用方复用。
async fn require_active_session(
    state: &AppState,
    session_uid: &str,
) -> Result<iptv_dto::PlaybackSession, ApiError> {
    let session = state
        .store
        .get_playback_session(session_uid)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| {
            ApiError::new(
                iptv_dto::ErrorCode::SessionExpired,
                "playback session not found",
            )
        })?;
    if session.status != "active" {
        return Err(ApiError::new(
            iptv_dto::ErrorCode::SessionExpired,
            "playback session not active",
        ));
    }
    Ok(session)
}

async fn primary_url(state: &Arc<AppState>, channel_id: i64) -> Result<String, ApiError> {
    // 只校验频道存在（404），地址取主端点记录。
    state
        .store
        .get_channel(channel_id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::new(iptv_dto::ErrorCode::NotFound, "channel not found"))?;
    // 主地址存在 channel_endpoint（is_primary）；没有端点记录时频道不可用。
    let endpoints = state
        .store
        .list_endpoints(channel_id)
        .await
        .map_err(ApiError::from)?;
    endpoints
        .into_iter()
        .find(|e| e.is_primary)
        .map(|e| e.url)
        .ok_or_else(|| {
            ApiError::new(
                iptv_dto::ErrorCode::SourceUnavailable,
                "no playable endpoint",
            )
        })
}

/// GET /p/{channel_id}.m3u8（路由为 `/p/{*rest}`，此处拆 id 与后缀）。
pub async fn proxy_manifest(
    State(state): State<Arc<AppState>>,
    Path(rest): Path<String>,
    Query(q): Query<PlayQuery>,
) -> Result<Response, ApiError> {
    let channel_id: i64 = rest
        .strip_suffix(".m3u8")
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| ApiError::new(iptv_dto::ErrorCode::NotFound, "bad manifest path"))?;
    verify(&state, channel_id, &q)?;
    // m7：会话 stop/过期后，签名剩余有效期内的清单地址须失效。
    let session = require_active_session(&state, &q.session).await?;
    if session.channel_id != channel_id {
        return Err(ApiError::new(
            iptv_dto::ErrorCode::SessionExpired,
            "playback session channel mismatch",
        ));
    }
    let upstream = primary_url(&state, channel_id).await?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent(format!("Muse-IPTV/{}", iptv_core::VERSION))
        // M3：重定向最多跟 3 跳。
        .redirect(reqwest::redirect::Policy::limited(3))
        .build()
        .map_err(ApiError::from)?;
    let resp = client.get(&upstream).send().await.map_err(|_| {
        ApiError::new(
            iptv_dto::ErrorCode::SourceUnavailable,
            "upstream fetch failed",
        )
    })?;
    // M4：流式读、边读边累计上限（20 MB），超限立即断流，不先整包进内存。
    let bytes = read_capped(resp, MANIFEST_MAX_BYTES).await?;
    let body = String::from_utf8_lossy(&bytes).into_owned();
    let base = reqwest::Url::parse(&upstream)
        .map_err(|_| ApiError::new(iptv_dto::ErrorCode::SourceUnavailable, "bad upstream url"))?;
    let rewritten = rewrite_manifest(&body, &base, &q);
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")
        .header(header::CACHE_CONTROL, "no-store")
        .body(axum::body::Body::from(rewritten))
        .unwrap())
}

/// 上游清单单响应上限（M4）。清单本身很小，20 MB 是安全兜底。
const MANIFEST_MAX_BYTES: usize = 20 * 1024 * 1024;

/// 流式读响应体，累计超 `limit` 立即报错中断，不继续消费上游。
async fn read_capped(resp: reqwest::Response, limit: usize) -> Result<Vec<u8>, ApiError> {
    use futures_util::StreamExt;
    let mut buf = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| {
            ApiError::new(
                iptv_dto::ErrorCode::SourceUnavailable,
                "upstream read failed",
            )
        })?;
        if buf.len() + chunk.len() > limit {
            return Err(ApiError::new(
                iptv_dto::ErrorCode::SourceUnavailable,
                "upstream response too large",
            ));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// 把 manifest 里的相对 URI 改写为 `/p/seg` 代理地址（带同一签名）。
fn rewrite_manifest(body: &str, base: &reqwest::Url, q: &PlayQuery) -> String {
    let mut out = String::new();
    for line in body.lines() {
        if line.starts_with('#') {
            // #EXT-X-KEY 等行内的 URI="..." 也要改写。
            if let Some(rest) = line.strip_prefix("#EXT-X-KEY:") {
                out.push_str("#EXT-X-KEY:");
                out.push_str(&rewrite_key_line(rest, base, q));
                out.push('\n');
            } else if let Some(rest) = line.strip_prefix("#EXT-X-MAP:") {
                out.push_str("#EXT-X-MAP:");
                out.push_str(&rewrite_key_line(rest, base, q));
                out.push('\n');
            } else {
                out.push_str(line);
                out.push('\n');
            }
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            out.push('\n');
            continue;
        }
        let abs = base
            .join(trimmed)
            .map(|u| u.to_string())
            .unwrap_or_else(|_| trimmed.to_string());
        out.push_str(&seg_url(&abs, q));
        out.push('\n');
    }
    out
}

fn rewrite_key_line(rest: &str, base: &reqwest::Url, q: &PlayQuery) -> String {
    // URI="..." 形式
    if let Some(start) = rest.find("URI=\"") {
        let after = &rest[start + 5..];
        if let Some(end) = after.find('"') {
            let uri = &after[..end];
            let abs = base
                .join(uri)
                .map(|u| u.to_string())
                .unwrap_or_else(|_| uri.to_string());
            return format!(
                "{}URI=\"{}\"{}",
                &rest[..start],
                seg_url(&abs, q),
                &after[end + 1..]
            );
        }
    }
    rest.to_string()
}

fn seg_url(abs: &str, q: &PlayQuery) -> String {
    use url::form_urlencoded;
    let u: String = form_urlencoded::byte_serialize(abs.as_bytes()).collect();
    format!(
        "/p/seg?u={u}&exp={}&session={}&sig={}",
        q.exp, q.session, q.sig
    )
}

/// GET /p/seg?u=...&exp&session&sig
pub async fn proxy_segment(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Query(q): Query<SegQuery>,
) -> Result<Response, ApiError> {
    // session_uid 里的 channel_id 无法从签名反推：验签时用 session 对应的播放会话查 channel。
    // m7：先查会话状态（stop/过期后分片地址立即失效），再验签名。
    let session = require_active_session(&state, &q.session).await?;
    let pq = PlayQuery {
        exp: q.exp,
        session: q.session.clone(),
        sig: q.sig.clone(),
    };
    verify(&state, session.channel_id, &pq)?;
    // SSRF 校验（§13）：目标 host、scheme、端口必须与频道主地址一致
    //（只比 host 时，http↔https 或非标端口可绕过；重定向另由客户端限 3 跳）。
    let upstream = primary_url(&state, session.channel_id).await?;
    let upstream_url = reqwest::Url::parse(&upstream)
        .map_err(|_| ApiError::new(iptv_dto::ErrorCode::SourceUnavailable, "bad upstream url"))?;
    let target = reqwest::Url::parse(&q.u)
        .map_err(|_| ApiError::new(iptv_dto::ErrorCode::BadRequest, "bad segment url"))?;
    if target.host_str() != upstream_url.host_str()
        || target.scheme() != upstream_url.scheme()
        || target.port_or_known_default() != upstream_url.port_or_known_default()
    {
        return Err(ApiError::new(
            iptv_dto::ErrorCode::Forbidden,
            "segment host not allowed",
        ));
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent(format!("Muse-IPTV/{}", iptv_core::VERSION))
        // M3：重定向最多跟 3 跳。
        .redirect(reqwest::redirect::Policy::limited(3))
        .build()
        .map_err(ApiError::from)?;
    let mut req = client.get(target);
    if let Some(range) = headers.get(header::RANGE) {
        req = req.header(header::RANGE, range.clone());
    }
    let resp = req.send().await.map_err(|_| {
        ApiError::new(
            iptv_dto::ErrorCode::SourceUnavailable,
            "upstream fetch failed",
        )
    })?;
    let status = resp.status();
    let content_type = resp.headers().get(header::CONTENT_TYPE).cloned();
    let content_length = resp.headers().get(header::CONTENT_LENGTH).cloned();
    // M4：流式转发（Body::from_stream），不再整段缓冲后才返回。
    let stream = resp.bytes_stream();
    let mut builder = Response::builder()
        .status(status)
        .header(header::CACHE_CONTROL, "no-store");
    if let Some(v) = content_type {
        builder = builder.header(header::CONTENT_TYPE, v);
    }
    if let Some(v) = content_length {
        builder = builder.header(header::CONTENT_LENGTH, v);
    }
    Ok(builder.body(axum::body::Body::from_stream(stream)).unwrap())
}
