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
use iptv_core::{pin_target, resolve_checked_ip, resolve_segment_ip, verify_play_url};
use serde::Deserialize;
use std::sync::Arc;

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct PlayQuery {
    pub exp: i64,
    pub session: String,
    pub sig: String,
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
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
#[utoipa::path(get, path = "/p/{channel_id}.m3u8", tag = "playback", params(PlayQuery), responses((status = 200, description = "签名播放清单代理（HMAC，300s）")))]
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
    let resp = fetch_checked(&upstream).await?;
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

/// 带逐跳 SSRF 校验的 GET（M17①② + N2）：每跳先 `resolve_checked_ip`
///（解析一次、校验该组 IP 后取其中一个），再用 `pinned_client` 把连
/// 接固定到该 IP（`ClientBuilder::resolve` 覆盖本次解析，reqwest 不再
/// 二次解析同一主机名，DNS 重绑定无窗口；TLS SNI/Host 仍用原主机名）；
/// 3xx 带 Location 时解析相对地址后继续，最多跟 3 跳。无 Location 的
/// 3xx 直接返回响应。客户端每跳新建：重定向目标 host 不同，固定值也
/// 不同。重定向仍手动跟（Policy::none）：`Policy::limited` 只限跳数，
/// 302 跳到内网仍会被跟进。
async fn fetch_checked(url: &str) -> Result<reqwest::Response, ApiError> {
    let mut current = url.to_string();
    for _ in 0..=3 {
        let ip = resolve_checked_ip(&current).await.map_err(|_| {
            ApiError::new(iptv_dto::ErrorCode::Forbidden, "upstream url not allowed")
        })?;
        let parsed = reqwest::Url::parse(&current).map_err(|_| {
            ApiError::new(iptv_dto::ErrorCode::SourceUnavailable, "bad upstream url")
        })?;
        let client = pinned_client(&parsed, ip, 15);
        let resp = client.get(&current).send().await.map_err(|_| {
            ApiError::new(
                iptv_dto::ErrorCode::SourceUnavailable,
                "upstream fetch failed",
            )
        })?;
        if resp.status().is_redirection() {
            let Some(loc) = resp
                .headers()
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
            else {
                return Ok(resp);
            };
            let base = reqwest::Url::parse(&current).map_err(|_| {
                ApiError::new(iptv_dto::ErrorCode::SourceUnavailable, "bad upstream url")
            })?;
            current = base.join(&loc).map(|u| u.to_string()).map_err(|_| {
                ApiError::new(iptv_dto::ErrorCode::SourceUnavailable, "bad redirect url")
            })?;
            continue;
        }
        return Ok(resp);
    }
    Err(ApiError::new(
        iptv_dto::ErrorCode::SourceUnavailable,
        "too many redirects",
    ))
}

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
#[utoipa::path(get, path = "/p/seg", tag = "playback", params(SegQuery), responses((status = 200, description = "签名分片代理（Range 透传，SSRF 校验）")))]
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
    // SSRF 校验（§13/M17③）：目标 host、scheme、端口必须与频道主地址
    // 一致（只比 host 时，http↔https 或非标端口可绕过）；再把 host 解析
    // 成 IP 比对（见 `check_segment_target`），只比字符串会被 DNS 重绑定
    // 绕过（同一主机名在清单拉取时解析到公网、拉分片时解析到内网）。
    let upstream = primary_url(&state, session.channel_id).await?;
    let upstream_url = reqwest::Url::parse(&upstream)
        .map_err(|_| ApiError::new(iptv_dto::ErrorCode::SourceUnavailable, "bad upstream url"))?;
    let target = reqwest::Url::parse(&q.u)
        .map_err(|_| ApiError::new(iptv_dto::ErrorCode::BadRequest, "bad segment url"))?;
    check_segment_target(&upstream_url, &target).await?;
    let range = headers.get(header::RANGE).cloned();
    let resp = fetch_segment_checked(&upstream_url, target, range).await?;
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

/// 分片目标校验（M17③ + N2）：规则实现在 core 的 `resolve_segment_ip`
///（字符串层 host/scheme/端口一致 + 解析成 IP 比对），此处只做错误
/// 映射；只比字符串会被 DNS 重绑定绕过（同一主机名在清单拉取时解析
/// 到公网、拉分片时解析到内网）。
/// IP 层两条硬规则：① 目标解析出的任一 IP 落在禁段即拒绝（DNS 解析
/// 失败也按拒绝处理，不按放行）；② 两侧都只解析出一个 IP 时要求严格
/// 相等（同一主机名单 IP 场景，改解析即拦）。多 IP 场景（CDN 轮询、
/// 两次解析返回的子集不同）只执行规则 ①，避免误杀正常轮询。
/// 开关打开时跳过 IP 层两条硬规则（字符串层 host/scheme/端口一致性
/// 仍保留）：本地 stub 走 127.0.0.1，否则分片永远 403。
async fn check_segment_target(
    upstream: &reqwest::Url,
    target: &reqwest::Url,
) -> Result<(), ApiError> {
    resolve_segment_ip(upstream, target)
        .await
        .map(|_| ())
        .map_err(|_| ApiError::new(iptv_dto::ErrorCode::Forbidden, "segment host not allowed"))
}

/// N2：按校验过的 IP 建客户端（`ClientBuilder::resolve` 覆盖本次 DNS
/// 解析；字面 IP 主机无需固定，见 `pin_target`）。重定向目标 host
/// 不同故每跳新建。
fn pinned_client(url: &reqwest::Url, ip: std::net::IpAddr, timeout_secs: u64) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .user_agent(format!("Muse-IPTV/{}", iptv_core::VERSION))
        // M-E：强制直连，禁用环境代理（reqwest 默认读 http_proxy 等；
        // 走 CONNECT 隧道时 DNS 由代理做，`resolve` 的固定值不生效，
        // N2 的 TOCTOU 保护会被静默绕过）。上游只能经代理到达时走
        // 应用自身的订阅代理（`Subscription.proxyId`），不依赖环境代理。
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none());
    if let Some((host, addr)) = pin_target(url, ip) {
        builder = builder.resolve(&host, addr);
    }
    builder.build().expect("http client")
}

/// 带逐跳分片校验的 GET（M17① + N2）：每跳都重做 `resolve_segment_ip`
///（解析一次、校验后返回固定用的 IP），再用 `pinned_client` 把连接固
/// 定到该 IP（不再二次解析，DNS 重绑定无窗口）；Range 头透传；3xx
/// 带 Location 时解析相对地址后继续，最多跟 3 跳。无 Location 的 3xx
/// 直接返回响应。
async fn fetch_segment_checked(
    upstream: &reqwest::Url,
    target: reqwest::Url,
    range: Option<header::HeaderValue>,
) -> Result<reqwest::Response, ApiError> {
    let mut current = target;
    for _ in 0..=3 {
        let ip = resolve_segment_ip(upstream, &current).await.map_err(|_| {
            ApiError::new(iptv_dto::ErrorCode::Forbidden, "segment host not allowed")
        })?;
        let client = pinned_client(&current, ip, 30);
        let mut req = client.get(current.clone());
        if let Some(range) = &range {
            req = req.header(header::RANGE, range.clone());
        }
        let resp = req.send().await.map_err(|_| {
            ApiError::new(
                iptv_dto::ErrorCode::SourceUnavailable,
                "upstream fetch failed",
            )
        })?;
        if resp.status().is_redirection() {
            let Some(loc) = resp
                .headers()
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
            else {
                return Ok(resp);
            };
            current = current.join(&loc).map_err(|_| {
                ApiError::new(iptv_dto::ErrorCode::SourceUnavailable, "bad redirect url")
            })?;
            continue;
        }
        return Ok(resp);
    }
    Err(ApiError::new(
        iptv_dto::ErrorCode::SourceUnavailable,
        "too many redirects",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// 起一个本地 HTTP stub：每个连接读到请求头结束（`\r\n\r\n`）后
    /// 回固定响应并关闭连接。返回监听地址。
    async fn spawn_stub(ip: std::net::IpAddr, response: String) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind((ip, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let response = response.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let mut req = Vec::new();
                    loop {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => {
                                req.extend_from_slice(&buf[..n]);
                                if req.windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                            }
                        }
                    }
                    let _ = sock.write_all(response.as_bytes()).await;
                });
            }
        });
        addr
    }

    fn http_ok(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
    }

    /// 分片 302 专用 stub：`/seg0.ts` → 302 到同端口相对路径
    /// `/seg1.ts`，`/seg1.ts` → 200。跨端口重定向本就被 M17③ 的
    /// 端口一致规则拒绝，故测例只覆盖同端口跳。
    async fn spawn_segment_redirect_stub(ip: std::net::IpAddr) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind((ip, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let mut req = Vec::new();
                    loop {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => {
                                req.extend_from_slice(&buf[..n]);
                                if req.windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                            }
                        }
                    }
                    let text = String::from_utf8_lossy(&req);
                    let path = text
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("");
                    let resp = if path == "/seg0.ts" {
                        "HTTP/1.1 302 Found\r\nLocation: /seg1.ts\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                    } else {
                        http_ok("redirect-ok")
                    };
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        addr
    }

    /// 进程环境变量（代理开关/NO_PROXY）是全局共享的：改动它们的测试
    /// 必须串行，否则一个测试恢复环境变量会关掉另一个正在跑的测试的
    /// 开关（M-E 回归测试引入第二个 `TestSwitchGuard` 使用方后，
    /// `fetch_checked_and_segment_loopback` 在并行跑时可能被关开关，
    /// 实测在 worker 同款测试上撞上）。持锁的测试全程独占环境变量。
    static ENV_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// 测试开关的 RAII 守卫：`IPTV_TEST_ALLOW_PRIVATE_UPSTREAMS` 是进
    /// 程环境变量，测试 panic 时也要恢复，避免污染同进程其他测试。
    struct TestSwitchGuard {
        prev: Option<String>,
    }

    impl TestSwitchGuard {
        fn set() -> Self {
            let prev = std::env::var("IPTV_TEST_ALLOW_PRIVATE_UPSTREAMS").ok();
            std::env::set_var("IPTV_TEST_ALLOW_PRIVATE_UPSTREAMS", "1");
            Self { prev }
        }
    }

    impl Drop for TestSwitchGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var("IPTV_TEST_ALLOW_PRIVATE_UPSTREAMS", v),
                None => std::env::remove_var("IPTV_TEST_ALLOW_PRIVATE_UPSTREAMS"),
            }
        }
    }

    /// 把域名追加进 `NO_PROXY`/`no_proxy` 的 RAII 守卫（只追加已有
    /// 值、不替换，drop 时恢复原值）。reqwest 在 `Client::build` 时
    /// 读代理环境变量，守卫须在建客户端之前生效。
    struct NoProxyGuard {
        prev_upper: Option<String>,
        prev_lower: Option<String>,
    }

    impl NoProxyGuard {
        fn append(domain: &str) -> Self {
            let prev_upper = std::env::var("NO_PROXY").ok();
            let prev_lower = std::env::var("no_proxy").ok();
            let add = |prev: &Option<String>| match prev {
                Some(v) if !v.is_empty() => format!("{v},{domain}"),
                _ => domain.to_string(),
            };
            std::env::set_var("NO_PROXY", add(&prev_upper));
            std::env::set_var("no_proxy", add(&prev_lower));
            Self {
                prev_upper,
                prev_lower,
            }
        }
    }

    impl Drop for NoProxyGuard {
        fn drop(&mut self) {
            match &self.prev_upper {
                Some(v) => std::env::set_var("NO_PROXY", v),
                None => std::env::remove_var("NO_PROXY"),
            }
            match &self.prev_lower {
                Some(v) => std::env::set_var("no_proxy", v),
                None => std::env::remove_var("no_proxy"),
            }
        }
    }

    /// N2 核心行为：固定后的客户端实际连向校验过的 IP。
    /// `pinned.test` 在 DNS 里不存在：未经固定的请求必然失败；固定
    /// 到回环 stub 的 IP 后，请求必须打到本地 stub——证明
    /// `ClientBuilder::resolve` 的覆盖真的生效，而不是又走了一次 DNS。
    /// 注意：沙箱设了出口代理环境变量（`http_proxy` 等），reqwest
    /// 默认会走代理——`pinned.test` 不在 `NO_PROXY` 里时请求根本到
    /// 不了本地 stub。测试内把该域名追加进 `NO_PROXY`/`no_proxy`
    /// （只追加、不替换已有项，同进程并行的其他测试不受影响）。
    #[tokio::test]
    async fn pinned_client_connects_to_validated_ip() {
        let _env = ENV_TEST_LOCK.lock().await;
        let _no_proxy = NoProxyGuard::append("pinned.test");
        let loopback = std::net::IpAddr::from([127, 0, 0, 1]);
        let addr = spawn_stub(loopback, http_ok("pinned-ok")).await;
        let url = reqwest::Url::parse(&format!("http://pinned.test:{}/x", addr.port())).unwrap();
        let client = pinned_client(&url, addr.ip(), 15);
        let resp = client.get(url).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "pinned-ok");
    }

    /// M-E 回归测试用的最小 CONNECT 代理 stub：监听回环地址；收到
    /// `CONNECT host:port` 后记下目标、回 `200` 并把字节流中继到真实
    /// 目标（于是即使同进程并行测试的请求误走本代理也能正常完成，
    /// 只影响 CONNECT 记录的断言本身）。
    async fn spawn_connect_recorder() -> (std::net::SocketAddr, Arc<std::sync::Mutex<Vec<String>>>)
    {
        let listener = tokio::net::TcpListener::bind((std::net::IpAddr::from([127, 0, 0, 1]), 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let targets: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = targets.clone();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let seen = seen.clone();
                tokio::spawn(async move {
                    handle_proxy_conn(sock, seen).await;
                });
            }
        });
        (addr, targets)
    }

    async fn handle_proxy_conn(
        mut sock: tokio::net::TcpStream,
        seen: Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        let mut buf = [0u8; 4096];
        let mut req = Vec::new();
        loop {
            match sock.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    req.extend_from_slice(&buf[..n]);
                    if req.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
            }
        }
        let text = String::from_utf8_lossy(&req);
        let first = text.lines().next().unwrap_or("");
        let mut parts = first.split_whitespace();
        if parts.next() != Some("CONNECT") {
            // 非 CONNECT（绝对形式 GET 等）：直接关闭，调用方会报错。
            return;
        }
        let Some(target) = parts.next().map(|s| s.to_string()) else {
            return;
        };
        seen.lock().unwrap().push(target.clone());
        let Ok(mut upstream) = tokio::net::TcpStream::connect(target.as_str()).await else {
            let _ = sock
                .write_all(
                    b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
            return;
        };
        if sock
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .is_err()
        {
            return;
        }
        let _ = tokio::io::copy_bidirectional(&mut sock, &mut upstream).await;
    }

    /// M-E 回归测试用的代理环境守卫：把 `http_proxy`/`https_proxy`
    ///（大小写与 `ALL_PROXY`）指向记录 CONNECT 的 stub，并把 `NO_PROXY`
    /// 置空（沙箱默认 `NO_PROXY` 含 127.0.0.1，否则回环目标会绕过代理、
    /// 测不到 CONNECT）。drop 时恢复原值。
    struct HttpProxyGuard {
        prev: Vec<(&'static str, Option<String>)>,
    }

    impl HttpProxyGuard {
        fn set(proxy_addr: std::net::SocketAddr) -> Self {
            const KEYS: [&str; 8] = [
                "http_proxy",
                "https_proxy",
                "HTTP_PROXY",
                "HTTPS_PROXY",
                "ALL_PROXY",
                "all_proxy",
                "NO_PROXY",
                "no_proxy",
            ];
            let proxy_url = format!("http://127.0.0.1:{}/", proxy_addr.port());
            let mut prev = Vec::new();
            for k in KEYS {
                prev.push((k, std::env::var(k).ok()));
                let v = if k == "NO_PROXY" || k == "no_proxy" {
                    String::new()
                } else {
                    proxy_url.clone()
                };
                std::env::set_var(k, v);
            }
            Self { prev }
        }
    }

    impl Drop for HttpProxyGuard {
        fn drop(&mut self) {
            for (k, v) in &self.prev {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    /// M-E 回归：`http_proxy` 已设置时，清单/分片拉取仍直连校验过的 IP，
    /// 不向代理发 `CONNECT`。目标为回环 stub（开关放行私网 IP）；代理
    /// stub 记录 CONNECT 目标并中继——修复前会记录到
    /// `CONNECT 127.0.0.1:<port>`，修复后记录为空且响应仍为直连内容。
    #[tokio::test]
    async fn pinned_client_ignores_http_proxy() {
        let _env = ENV_TEST_LOCK.lock().await;
        let _switch = TestSwitchGuard::set();
        let v4 = std::net::IpAddr::from([127, 0, 0, 1]);
        let addr = spawn_stub(v4, http_ok("segment-ok")).await;
        let (proxy_addr, connects) = spawn_connect_recorder().await;
        let _proxy = HttpProxyGuard::set(proxy_addr);
        let target = format!("127.0.0.1:{}", addr.port());

        // 清单拉取（`fetch_checked` → `pinned_client`）。
        let manifest = format!("http://127.0.0.1:{}/live.m3u8", addr.port());
        let resp = fetch_checked(&manifest).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "segment-ok");

        // 分片拉取（`fetch_segment_checked` → `pinned_client`）。
        let upstream = reqwest::Url::parse(&manifest).unwrap();
        let seg =
            reqwest::Url::parse(&format!("http://127.0.0.1:{}/seg0.ts", addr.port())).unwrap();
        assert!(check_segment_target(&upstream, &seg).await.is_ok());
        let resp = fetch_segment_checked(&upstream, seg, None).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "segment-ok");

        let seen = connects.lock().unwrap();
        assert!(
            !seen.iter().any(|t| t == &target),
            "pinned client must connect directly, but proxy saw CONNECT: {seen:?}"
        );
    }

    /// N2 接线：清单拉取与分片拉取都走“解析+校验+固定”全程，且开关
    /// 语义不变。同一测试函数内顺序执行各阶段，避免多测试并行改进
    /// 程环境变量竞态：① 开关开时清单拉取成功；② 同 host 分片拉取
    /// 成功（`check_segment_target` 的委托也一并覆盖）；③ 分片 302
    /// 跳到另一回环端口，逐跳重新固定后终点可达；④ 开关关时清单与
    /// 分片目标双双被拒。
    #[tokio::test]
    async fn fetch_checked_and_segment_loopback() {
        let _env = ENV_TEST_LOCK.lock().await;
        let v4 = std::net::IpAddr::from([127, 0, 0, 1]);
        let _guard = TestSwitchGuard::set();

        // ① 清单拉取成功。
        let addr = spawn_stub(v4, http_ok("manifest-ok")).await;
        let manifest = format!("http://127.0.0.1:{}/live.m3u8", addr.port());
        let resp = fetch_checked(&manifest).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "manifest-ok");

        // ② 分片拉取成功（上游与目标同 host/scheme/端口）。
        let seg_addr = spawn_stub(v4, http_ok("segment-ok")).await;
        let upstream =
            reqwest::Url::parse(&format!("http://127.0.0.1:{}/live.m3u8", seg_addr.port()))
                .unwrap();
        let target =
            reqwest::Url::parse(&format!("http://127.0.0.1:{}/seg0.ts", seg_addr.port())).unwrap();
        assert!(check_segment_target(&upstream, &target).await.is_ok());
        let resp = fetch_segment_checked(&upstream, target, None)
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "segment-ok");
        // 目标 host 与上游不一致仍拒绝（字符串层保留）。
        let other =
            reqwest::Url::parse(&format!("http://127.0.0.2:{}/seg0.ts", seg_addr.port())).unwrap();
        assert!(check_segment_target(&upstream, &other).await.is_err());

        // ③ 分片 302 跳到同端口另一路径：每跳重新解析+校验+固定。
        // 注意：M17③ 要求目标端口与上游端口一致，故重定向必须留在同
        // 端口（跨端口 302 本就应被拒，见 `resolve_segment_ip`）。
        let redir_addr = spawn_segment_redirect_stub(v4).await;
        let upstream2 =
            reqwest::Url::parse(&format!("http://127.0.0.1:{}/live.m3u8", redir_addr.port()))
                .unwrap();
        let target2 =
            reqwest::Url::parse(&format!("http://127.0.0.1:{}/seg0.ts", redir_addr.port()))
                .unwrap();
        let resp = fetch_segment_checked(&upstream2, target2, None)
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "redirect-ok");

        // ④ 开关关时清单与分片目标双双被拒（开关只放行 IP 段）。
        drop(_guard);
        assert!(fetch_checked(&manifest).await.is_err());
        assert!(check_segment_target(&upstream, &upstream).await.is_err());
    }
}
