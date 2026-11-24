//! 管理后台 API（开发文档 §5.2 / §5.3 / §3.2）。
//!
//! 权限矩阵（§5.3）在服务端校验，前端仅做展示差异；所有写操作写审计日志
//! （操作者、对象、变更前后快照，§3.2）。
// 作者: Passwd Word

use crate::{ok, AdminAuth, ApiError, ApiResult, AppState};
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{header, HeaderMap};
use axum::response::IntoResponse;
use axum::Json;
use chrono::{Duration, Utc};
use iptv_auth::{hash_password, verify_password, Totp};
use iptv_core::{generate_token, hash_token, token_prefix};
use iptv_dto::{
    AdminLoginRequest, AdminRole, AdminUser, AppUser, AuditLog, Channel, ChannelGroup,
    CreateAdminRequest, CreateChannelRequest, CreateGroupRequest, CreatePlanRequest,
    CreateSourceRequest, CreateTokenRequest, CreateTokenResponse, CreateUserRequest, EpgSource,
    ErrorCode, M3uImportReport, PageQuery, Paged, Plan, PlaylistSource, StatsOverview, SyncDiff,
    SyncLog, SystemSetting, UpdateChannelRequest, UpdateTokenRequest, UpdateUserRequest, UserToken,
};
use iptv_m3u::parse_m3u;
use iptv_store::NewAuditLog;
use std::sync::Arc;

fn require(admin: &AdminAuth, ok_roles: &[AdminRole], what: &str) -> Result<(), ApiError> {
    if ok_roles.contains(&admin.role) {
        Ok(())
    } else {
        Err(ApiError::new(
            ErrorCode::Forbidden,
            format!("role {:?} cannot {what}", admin.role),
        ))
    }
}

// 审计 helper 有意保留 8 个位置参数：一条审计记录的字段缺一不可，
// 调用点全部同形，改 struct 反而让 13 处调用更难读。
#[allow(clippy::too_many_arguments)]
async fn audit(
    state: &Arc<AppState>,
    admin: &AdminAuth,
    action: &str,
    object_type: &str,
    object_id: &str,
    before: Option<serde_json::Value>,
    after: Option<serde_json::Value>,
    ip: Option<&str>,
) {
    let _ = state
        .store
        .write_audit(&NewAuditLog {
            actor: admin.username.clone(),
            actor_role: Some(format!("{:?}", admin.role)),
            action: action.to_string(),
            object_type: object_type.to_string(),
            object_id: object_id.to_string(),
            before_json: before,
            after_json: after,
            ip_address: ip.map(|s| s.to_string()),
        })
        .await;
}

fn ip_of(headers: &HeaderMap) -> Option<String> {
    crate::client_ip(headers)
}

// ---------------------------------------------------------------------------
// 管理员登录（§5.1：Argon2 + 强制 TOTP）
// ---------------------------------------------------------------------------

/// POST /api/v1/admin/auth/login
pub async fn login(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<AdminLoginRequest>,
) -> Result<axum::response::Response, ApiError> {
    let ip = ip_of(&headers);
    if !state
        .login_limiter
        .check(ip.as_deref().unwrap_or("unknown"))
    {
        return Err(ApiError::new(ErrorCode::RateLimited, "too many requests"));
    }
    let found = state
        .store
        .find_admin_by_username(&req.username)
        .await
        .map_err(ApiError::from)?;
    let Some((admin, password_hash, totp_secret)) = found else {
        return Err(ApiError::new(
            ErrorCode::AdminAuthFailed,
            "invalid username or password",
        ));
    };
    if !verify_password(&password_hash, &req.password) {
        return Err(ApiError::new(
            ErrorCode::AdminAuthFailed,
            "invalid username or password",
        ));
    }
    if admin.totp_required || admin.totp_enabled {
        let code = req.totp_code.as_deref().unwrap_or("");
        let secret = totp_secret
            .ok_or_else(|| ApiError::new(ErrorCode::AdminAuthFailed, "totp required"))?;
        let totp = Totp::new(&secret)
            .ok_or_else(|| ApiError::new(ErrorCode::AdminAuthFailed, "bad totp secret"))?;
        if !totp.verify(code, iptv_auth::now_unix()) {
            return Err(ApiError::new(
                ErrorCode::AdminAuthFailed,
                "invalid totp code",
            ));
        }
    }
    let exp = Utc::now() + Duration::hours(12);
    let cookie = crate::sign_admin_cookie(&state.config.admin_secret, admin.id, exp.timestamp());
    let mut h = HeaderMap::new();
    h.insert(
        header::SET_COOKIE,
        // Path 必须是 /：API 在 /api/v1/admin/* 下，Path=/admin 时浏览器不会把 Cookie 发给 API。
        format!("iptv_admin_sid={cookie}; Path=/; HttpOnly; SameSite=Lax; Max-Age=43200")
            .parse()
            .unwrap(),
    );
    Ok((
        axum::http::StatusCode::OK,
        h,
        Json(iptv_dto::Envelope::ok(
            serde_json::json!({ "admin_id": admin.id, "role": admin.role }),
        )),
    )
        .into_response())
}

/// GET /api/v1/admin/stats/overview
pub async fn stats_overview(
    State(state): State<Arc<AppState>>,
    _admin: AdminAuth,
) -> ApiResult<StatsOverview> {
    let stats = state.store.stats_overview().await.map_err(ApiError::from)?;
    ok(stats)
}

// ---------------------------------------------------------------------------
// 用户（§3.2 / §5.1）
// ---------------------------------------------------------------------------

pub async fn list_users(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    Query(page): Query<PageQuery>,
) -> ApiResult<Paged<AppUser>> {
    let _ = admin;
    let users = state
        .store
        .list_users(&page, None)
        .await
        .map_err(ApiError::from)?;
    ok(users)
}

pub async fn create_user(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    headers: HeaderMap,
    Json(req): Json<CreateUserRequest>,
) -> ApiResult<AppUser> {
    require(
        &admin,
        &[AdminRole::SuperAdmin, AdminRole::Operator],
        "create users",
    )?;
    if req.display_name.trim().is_empty() {
        return Err(ApiError::new(
            ErrorCode::ValidationFailed,
            "display_name required",
        ));
    }
    let user = state
        .store
        .create_user(&req)
        .await
        .map_err(ApiError::from)?;
    audit(
        &state,
        &admin,
        "user.create",
        "user",
        &user.id.to_string(),
        None,
        Some(serde_json::to_value(&user).unwrap_or_default()),
        ip_of(&headers).as_deref(),
    )
    .await;
    ok(user)
}

pub async fn get_user(
    State(state): State<Arc<AppState>>,
    _admin: AdminAuth,
    Path(id): Path<i64>,
) -> ApiResult<AppUser> {
    let user = state
        .store
        .get_user(id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::new(ErrorCode::NotFound, "user not found"))?;
    ok(user)
}

pub async fn update_user(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(req): Json<UpdateUserRequest>,
) -> ApiResult<AppUser> {
    require(
        &admin,
        &[AdminRole::SuperAdmin, AdminRole::Operator],
        "update users",
    )?;
    let before = state.store.get_user(id).await.map_err(ApiError::from)?;
    let user = state
        .store
        .update_user(id, &req)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::new(ErrorCode::NotFound, "user not found"))?;
    audit(
        &state,
        &admin,
        "user.update",
        "user",
        &id.to_string(),
        before.map(|u| serde_json::to_value(u).unwrap_or_default()),
        Some(serde_json::to_value(&user).unwrap_or_default()),
        ip_of(&headers).as_deref(),
    )
    .await;
    ok(user)
}

// ---------------------------------------------------------------------------
// Token（§5.2）
// ---------------------------------------------------------------------------

pub async fn list_tokens(
    State(state): State<Arc<AppState>>,
    _admin: AdminAuth,
    Query(page): Query<PageQuery>,
) -> ApiResult<Paged<UserToken>> {
    let tokens = state
        .store
        .list_tokens(None, None, &page)
        .await
        .map_err(ApiError::from)?;
    ok(tokens)
}

pub async fn create_token(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    headers: HeaderMap,
    Json(req): Json<CreateTokenRequest>,
) -> ApiResult<CreateTokenResponse> {
    require(
        &admin,
        &[AdminRole::SuperAdmin, AdminRole::Operator],
        "create tokens",
    )?;
    if req.max_concurrent_sessions < 1 || req.max_bound_devices < 1 {
        return Err(ApiError::new(
            ErrorCode::ValidationFailed,
            "max_concurrent_sessions and max_bound_devices must be >= 1",
        ));
    }
    let _ = state
        .store
        .get_user(req.user_id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::new(ErrorCode::NotFound, "user not found"))?;
    let plain = generate_token();
    let hash = hash_token(&plain);
    let prefix = token_prefix(&plain);
    let token = state
        .store
        .create_token(req.user_id, &prefix, &hash, &req)
        .await
        .map_err(ApiError::from)?;
    audit(
        &state,
        &admin,
        "token.create",
        "token",
        &token.id.to_string(),
        None,
        Some(serde_json::to_value(&token).unwrap_or_default()),
        ip_of(&headers).as_deref(),
    )
    .await;
    // 登录票据 + 二维码（§5.2：portal_url 与 qr_png）。
    let ticket = state
        .tickets
        .issue(token.id, None, std::time::Duration::from_secs(60));
    let portal_url = format!(
        "{}/t/{ticket}",
        state.config.portal_base_url.trim_end_matches('/')
    );
    let qr_png = make_qr_png(&portal_url);
    ok(CreateTokenResponse {
        id: token.id,
        token: plain,
        portal_url,
        qr_png,
    })
}

fn make_qr_png(url: &str) -> String {
    use qrcode::QrCode;
    let code = match QrCode::new(url.as_bytes()) {
        Ok(c) => c,
        Err(_) => return String::new(),
    };
    let image = code.render::<image::Luma<u8>>().build();
    let mut buf = Vec::new();
    if image::DynamicImage::ImageLuma8(image)
        .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .is_err()
    {
        return String::new();
    }
    format!(
        "data:image/png;base64,{}",
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, buf)
    )
}

pub async fn update_token(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(req): Json<UpdateTokenRequest>,
) -> ApiResult<UserToken> {
    require(
        &admin,
        &[AdminRole::SuperAdmin, AdminRole::Operator],
        "update tokens",
    )?;
    let before = state.store.get_token(id).await.map_err(ApiError::from)?;
    let token = state
        .store
        .update_token(id, &req)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::new(ErrorCode::NotFound, "token not found"))?;
    audit(
        &state,
        &admin,
        "token.update",
        "token",
        &id.to_string(),
        before.map(|t| serde_json::to_value(t).unwrap_or_default()),
        Some(serde_json::to_value(&token).unwrap_or_default()),
        ip_of(&headers).as_deref(),
    )
    .await;
    ok(token)
}

/// POST /api/v1/admin/tokens/{id}/login-ticket：生成一次性登录链接/二维码。
pub async fn create_login_ticket(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    Path(id): Path<i64>,
) -> ApiResult<serde_json::Value> {
    require(
        &admin,
        &[AdminRole::SuperAdmin, AdminRole::Operator],
        "create login tickets",
    )?;
    let _ = state
        .store
        .get_token(id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::new(ErrorCode::NotFound, "token not found"))?;
    let ticket = state
        .tickets
        .issue(id, None, std::time::Duration::from_secs(60));
    let portal_url = format!(
        "{}/t/{ticket}",
        state.config.portal_base_url.trim_end_matches('/')
    );
    ok(
        serde_json::json!({ "ticket": ticket, "portal_url": portal_url, "qr_png": make_qr_png(&portal_url), "expires_in_sec": 60 }),
    )
}

/// POST /api/v1/admin/tokens/{id}/revoke-sessions：吊销该 Token 全部会话。
pub async fn revoke_token_sessions(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    Path(id): Path<i64>,
) -> ApiResult<serde_json::Value> {
    if !admin.role.can_kick_session() {
        return Err(ApiError::new(
            ErrorCode::Forbidden,
            "cannot revoke sessions",
        ));
    }
    let n = state
        .store
        .revoke_all_portal_sessions(id, None)
        .await
        .map_err(ApiError::from)?;
    // 同步释放 Redis 并发额度（会话下线后其播放也应停止）。
    for s in state
        .store
        .active_sessions_for_token(id)
        .await
        .map_err(ApiError::from)?
    {
        let _ = state.limiter.release(id, &s.session_uid).await;
        let _ = state
            .store
            .end_session(&s.session_uid, Utc::now(), "revoked")
            .await;
    }
    ok(serde_json::json!({ "revoked": n }))
}

// ---------------------------------------------------------------------------
// 频道（§5.1 / §3.2）
// ---------------------------------------------------------------------------

pub async fn list_channels(
    State(state): State<Arc<AppState>>,
    _admin: AdminAuth,
    Query(page): Query<PageQuery>,
) -> ApiResult<Paged<Channel>> {
    let channels = state
        .store
        .list_channels(&page, None, None)
        .await
        .map_err(ApiError::from)?;
    ok(channels)
}

pub async fn create_channel(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    headers: HeaderMap,
    Json(req): Json<CreateChannelRequest>,
) -> ApiResult<Channel> {
    require(
        &admin,
        &[AdminRole::SuperAdmin, AdminRole::Operator],
        "create channels",
    )?;
    if req.name.trim().is_empty() || req.play_url.trim().is_empty() {
        return Err(ApiError::new(
            ErrorCode::ValidationFailed,
            "name and play_url required",
        ));
    }
    let channel = state
        .store
        .create_channel(&req)
        .await
        .map_err(ApiError::from)?;
    audit(
        &state,
        &admin,
        "channel.create",
        "channel",
        &channel.id.to_string(),
        None,
        Some(serde_json::to_value(&channel).unwrap_or_default()),
        ip_of(&headers).as_deref(),
    )
    .await;
    ok(channel)
}

pub async fn update_channel(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(req): Json<UpdateChannelRequest>,
) -> ApiResult<Channel> {
    require(
        &admin,
        &[AdminRole::SuperAdmin, AdminRole::Operator],
        "update channels",
    )?;
    let before = state.store.get_channel(id).await.map_err(ApiError::from)?;
    let channel = state
        .store
        .update_channel(id, &req)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::new(ErrorCode::NotFound, "channel not found"))?;
    audit(
        &state,
        &admin,
        "channel.update",
        "channel",
        &id.to_string(),
        before.map(|c| serde_json::to_value(c).unwrap_or_default()),
        Some(serde_json::to_value(&channel).unwrap_or_default()),
        ip_of(&headers).as_deref(),
    )
    .await;
    ok(channel)
}

pub async fn delete_channel(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> ApiResult<serde_json::Value> {
    require(
        &admin,
        &[AdminRole::SuperAdmin, AdminRole::Operator],
        "delete channels",
    )?;
    let before = state.store.get_channel(id).await.map_err(ApiError::from)?;
    let deleted = state
        .store
        .delete_channel(id)
        .await
        .map_err(ApiError::from)?;
    if !deleted {
        return Err(ApiError::new(ErrorCode::NotFound, "channel not found"));
    }
    audit(
        &state,
        &admin,
        "channel.delete",
        "channel",
        &id.to_string(),
        before.map(|c| serde_json::to_value(c).unwrap_or_default()),
        None,
        ip_of(&headers).as_deref(),
    )
    .await;
    ok(serde_json::json!({ "deleted": true }))
}

// ---------------------------------------------------------------------------
// 订阅源（§7）
// ---------------------------------------------------------------------------

pub async fn list_sources(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
) -> ApiResult<Vec<PlaylistSource>> {
    if !admin.role.can_read_sources() {
        return Err(ApiError::new(ErrorCode::Forbidden, "cannot read sources"));
    }
    let sources = state.store.list_sources().await.map_err(ApiError::from)?;
    ok(sources)
}

pub async fn create_source(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    headers: HeaderMap,
    Json(req): Json<CreateSourceRequest>,
) -> ApiResult<PlaylistSource> {
    require(&admin, &[AdminRole::SuperAdmin], "create sources")?;
    if let Some(interval) = req.sync_interval_sec {
        if interval < 300 {
            return Err(ApiError::new(
                ErrorCode::ValidationFailed,
                "sync_interval_sec must be >= 300",
            ));
        }
    }
    let source = state
        .store
        .create_source(&req)
        .await
        .map_err(ApiError::from)?;
    audit(
        &state,
        &admin,
        "source.create",
        "source",
        &source.id.to_string(),
        None,
        Some(serde_json::to_value(&source).unwrap_or_default()),
        ip_of(&headers).as_deref(),
    )
    .await;
    ok(source)
}

/// POST /api/v1/admin/sources/upload：本地 M3U 上传（§7.1，限 20 MB，异步解析→此处同步解析后入库）。
pub async fn upload_m3u(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> ApiResult<M3uImportReport> {
    require(&admin, &[AdminRole::SuperAdmin], "upload sources")?;
    let mut text: Option<String> = None;
    let mut filename = "upload.m3u".to_string();
    let mut source_id: Option<i64> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| ApiError::new(ErrorCode::BadRequest, "bad multipart"))?
    {
        let name = field.name().unwrap_or("").to_string();
        if name == "file" {
            filename = field.file_name().unwrap_or("upload.m3u").to_string();
            let bytes = field
                .bytes()
                .await
                .map_err(|_| ApiError::new(ErrorCode::BadRequest, "read upload failed"))?;
            if bytes.len() > 20 * 1024 * 1024 {
                return Err(ApiError::new(
                    ErrorCode::ValidationFailed,
                    "file too large (max 20 MB)",
                ));
            }
            text = Some(String::from_utf8_lossy(&bytes).to_string());
        } else if name == "source_id" {
            let v = field.text().await.unwrap_or_default();
            source_id = v.trim().parse().ok();
        }
    }
    let text = text.ok_or_else(|| ApiError::new(ErrorCode::BadRequest, "missing file field"))?;
    let outcome = parse_m3u(&text);
    let source = match source_id {
        Some(id) => state
            .store
            .get_source(id)
            .await
            .map_err(ApiError::from)?
            .ok_or_else(|| ApiError::new(ErrorCode::NotFound, "source not found"))?,
        None => state
            .store
            .create_source(&CreateSourceRequest {
                name: filename.clone(),
                url: None,
                sync_interval_sec: None,
                http_headers: None,
                epg_url: outcome.x_tvg_url.clone(),
                on_missing: None,
                auto_enable_new: None,
                enabled: Some(true),
            })
            .await
            .map_err(ApiError::from)?,
    };
    let sync = state
        .store
        .upsert_channels(source.id, outcome.channels.clone())
        .await
        .map_err(ApiError::from)?;
    audit(
        &state,
        &admin,
        "source.upload",
        "source",
        &source.id.to_string(),
        None,
        Some(serde_json::json!({ "added": sync.added, "updated": sync.updated })),
        ip_of(&headers).as_deref(),
    )
    .await;
    ok(M3uImportReport {
        total_lines: text.lines().count(),
        channels_parsed: outcome.channels.len(),
        channels_imported: (sync.added + sync.updated) as usize,
        errors: outcome.errors,
    })
}

/// POST /api/v1/admin/sources/{id}/sync：立即同步（把 next_sync_at 置为现在，worker 拾取）。
pub async fn trigger_sync(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    Path(id): Path<i64>,
) -> ApiResult<serde_json::Value> {
    require(&admin, &[AdminRole::SuperAdmin], "trigger sync")?;
    let _ = state
        .store
        .get_source(id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::new(ErrorCode::NotFound, "source not found"))?;
    state
        .store
        .update_source_sync(id, Utc::now(), "pending", None)
        .await
        .map_err(ApiError::from)?;
    state
        .metrics
        .sync_triggers
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    ok(serde_json::json!({ "triggered": true }))
}

pub async fn source_diff(
    State(state): State<Arc<AppState>>,
    _admin: AdminAuth,
    Path(id): Path<i64>,
) -> ApiResult<SyncDiff> {
    let diff = state
        .store
        .get_sync_diff(id)
        .await
        .map_err(ApiError::from)?;
    ok(diff)
}

pub async fn source_logs(
    State(state): State<Arc<AppState>>,
    _admin: AdminAuth,
    Path(id): Path<i64>,
    Query(page): Query<PageQuery>,
) -> ApiResult<Paged<SyncLog>> {
    let logs = state
        .store
        .list_sync_logs(id, &page)
        .await
        .map_err(ApiError::from)?;
    ok(logs)
}

// ---------------------------------------------------------------------------
// 分组 / EPG / 会话 / 套餐 / 管理员 / 审计 / 设置
// ---------------------------------------------------------------------------

pub async fn list_groups(
    State(state): State<Arc<AppState>>,
    _admin: AdminAuth,
) -> ApiResult<Vec<ChannelGroup>> {
    let groups = state.store.list_groups().await.map_err(ApiError::from)?;
    ok(groups)
}

pub async fn create_group(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    headers: HeaderMap,
    Json(req): Json<CreateGroupRequest>,
) -> ApiResult<ChannelGroup> {
    require(
        &admin,
        &[AdminRole::SuperAdmin, AdminRole::Operator],
        "create groups",
    )?;
    let group = state
        .store
        .create_group(&req)
        .await
        .map_err(ApiError::from)?;
    audit(
        &state,
        &admin,
        "group.create",
        "group",
        &group.id.to_string(),
        None,
        Some(serde_json::to_value(&group).unwrap_or_default()),
        ip_of(&headers).as_deref(),
    )
    .await;
    ok(group)
}

pub async fn list_epg_sources(
    State(state): State<Arc<AppState>>,
    _admin: AdminAuth,
) -> ApiResult<Vec<EpgSource>> {
    let sources = state
        .store
        .list_epg_sources()
        .await
        .map_err(ApiError::from)?;
    ok(sources)
}

pub async fn create_epg_source(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    Json(req): Json<serde_json::Value>,
) -> ApiResult<EpgSource> {
    require(&admin, &[AdminRole::SuperAdmin], "create epg sources")?;
    let name = req
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let url = req
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if name.is_empty() || url.is_empty() {
        return Err(ApiError::new(
            ErrorCode::ValidationFailed,
            "name and url required",
        ));
    }
    let source = state
        .store
        .create_epg_source(&name, &url)
        .await
        .map_err(ApiError::from)?;
    ok(source)
}

pub async fn list_sessions(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    Query(page): Query<PageQuery>,
) -> ApiResult<Paged<iptv_dto::PlaybackSession>> {
    if !admin.role.can_kick_session() {
        return Err(ApiError::new(ErrorCode::Forbidden, "cannot list sessions"));
    }
    let sessions = state
        .store
        .list_active_playback_sessions(&page)
        .await
        .map_err(ApiError::from)?;
    ok(sessions)
}

pub async fn kick_session(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    Path(id): Path<i64>,
) -> ApiResult<serde_json::Value> {
    if !admin.role.can_kick_session() {
        return Err(ApiError::new(ErrorCode::Forbidden, "cannot kick sessions"));
    }
    // id 在这里是 playback_session 的主键；逐页扫描活跃会话直到找到。
    let mut found = None;
    for page_no in 1..=20u32 {
        let page = state
            .store
            .list_active_playback_sessions(&PageQuery {
                page: page_no,
                page_size: 100,
                q: None,
            })
            .await
            .map_err(ApiError::from)?;
        if page.items.is_empty() {
            break;
        }
        if let Some(s) = page.items.into_iter().find(|s| s.id == id) {
            found = Some(s);
            break;
        }
    }
    let Some(session) = found else {
        return Err(ApiError::new(ErrorCode::NotFound, "session not found"));
    };
    let _ = state
        .limiter
        .release(session.token_id, &session.session_uid)
        .await;
    state
        .store
        .end_session(&session.session_uid, Utc::now(), "kicked")
        .await
        .map_err(ApiError::from)?;
    ok(serde_json::json!({ "kicked": true }))
}

pub async fn list_plans(
    State(state): State<Arc<AppState>>,
    _admin: AdminAuth,
) -> ApiResult<Vec<Plan>> {
    let plans = state.store.list_plans().await.map_err(ApiError::from)?;
    ok(plans)
}

pub async fn create_plan(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    headers: HeaderMap,
    Json(req): Json<CreatePlanRequest>,
) -> ApiResult<Plan> {
    require(&admin, &[AdminRole::SuperAdmin], "create plans")?;
    let plan = state
        .store
        .create_plan(&req)
        .await
        .map_err(ApiError::from)?;
    audit(
        &state,
        &admin,
        "plan.create",
        "plan",
        &plan.id.to_string(),
        None,
        Some(serde_json::to_value(&plan).unwrap_or_default()),
        ip_of(&headers).as_deref(),
    )
    .await;
    ok(plan)
}

pub async fn list_admins(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
) -> ApiResult<Vec<AdminUser>> {
    require(&admin, &[AdminRole::SuperAdmin], "list admins")?;
    let admins = state.store.list_admins().await.map_err(ApiError::from)?;
    ok(admins)
}

pub async fn create_admin(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    headers: HeaderMap,
    Json(req): Json<CreateAdminRequest>,
) -> ApiResult<AdminUser> {
    require(&admin, &[AdminRole::SuperAdmin], "create admins")?;
    if req.username.trim().is_empty() || req.password.len() < 8 {
        return Err(ApiError::new(
            ErrorCode::ValidationFailed,
            "username required and password must be >= 8 chars",
        ));
    }
    let hash = hash_password(&req.password)
        .map_err(|_| ApiError::new(ErrorCode::Internal, "hash password failed"))?;
    let created = state
        .store
        .create_admin(&req.username, &hash, req.role, req.totp_required)
        .await
        .map_err(ApiError::from)?;
    audit(
        &state,
        &admin,
        "admin.create",
        "admin",
        &created.id.to_string(),
        None,
        Some(serde_json::to_value(&created).unwrap_or_default()),
        ip_of(&headers).as_deref(),
    )
    .await;
    ok(created)
}

#[derive(Debug, serde::Deserialize)]
pub struct AuditQuery {
    pub actor: Option<String>,
    pub action: Option<String>,
    pub from: Option<chrono::DateTime<Utc>>,
    pub to: Option<chrono::DateTime<Utc>>,
    #[serde(default = "default_page")]
    pub page: u32,
    #[serde(default = "default_page_size")]
    pub page_size: u32,
}
fn default_page() -> u32 {
    1
}
fn default_page_size() -> u32 {
    20
}

pub async fn list_audit(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    Query(q): Query<AuditQuery>,
) -> ApiResult<Paged<AuditLog>> {
    if !admin.role.can_read_audit() {
        return Err(ApiError::new(ErrorCode::Forbidden, "cannot read audit"));
    }
    let page = PageQuery {
        page: q.page,
        page_size: q.page_size,
        q: None,
    };
    let logs = state
        .store
        .list_audit(q.actor.as_deref(), q.action.as_deref(), q.from, q.to, &page)
        .await
        .map_err(ApiError::from)?;
    ok(logs)
}

pub async fn list_settings(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
) -> ApiResult<Vec<SystemSetting>> {
    if !admin.role.can_write_settings() {
        return Err(ApiError::new(ErrorCode::Forbidden, "cannot read settings"));
    }
    let settings = state.store.list_settings().await.map_err(ApiError::from)?;
    ok(settings)
}

pub async fn update_settings(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    headers: HeaderMap,
    Json(req): Json<serde_json::Value>,
) -> ApiResult<Vec<SystemSetting>> {
    require(&admin, &[AdminRole::SuperAdmin], "update settings")?;
    let obj = req
        .as_object()
        .ok_or_else(|| ApiError::new(ErrorCode::ValidationFailed, "object required"))?;
    for (k, v) in obj {
        // 只允许文档 §3.2 列出的键（防乱写）。
        let allowed = iptv_core::default_settings()
            .into_iter()
            .map(|(k, _)| k)
            .collect::<Vec<_>>();
        if !allowed.iter().any(|a| a == k) {
            return Err(ApiError::new(
                ErrorCode::ValidationFailed,
                format!("unknown setting: {k}"),
            ));
        }
        state
            .store
            .set_setting(k, v)
            .await
            .map_err(ApiError::from)?;
    }
    audit(
        &state,
        &admin,
        "settings.update",
        "settings",
        "system",
        None,
        Some(req.clone()),
        ip_of(&headers).as_deref(),
    )
    .await;
    let settings = state.store.list_settings().await.map_err(ApiError::from)?;
    ok(settings)
}
