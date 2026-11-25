//! 管理后台 API（开发文档 §5.2 / §5.3 / §3.2）。
//!
//! 权限矩阵（§5.3）在服务端校验，前端仅做展示差异；所有写操作写审计日志
//! （操作者、对象、变更前后快照，§3.2）。
// 作者: Passwd Word

use crate::{ok, AdminAuth, ApiError, ApiResult, AppState};
use axum::extract::{ConnectInfo, Multipart, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{Duration, Utc};
use iptv_auth::{generate_totp_secret, hash_password, verify_password, Totp};
use iptv_core::{generate_token, hash_token, token_prefix};
use iptv_dto::{
    AdminLoginRequest, AdminRole, AdminUser, AppUser, AuditLog, Channel, ChannelGroup,
    CreateAdminRequest, CreateAdminResponse, CreateChannelRequest, CreateGroupRequest,
    CreatePlanRequest, CreateSourceRequest, CreateTokenRequest, CreateTokenResponse,
    CreateUserRequest, EpgSource, ErrorCode, M3uImportReport, PageQuery, Paged, Plan,
    PlaylistSource, StatsOverview, SyncDiff, SyncLog, SystemSetting, UpdateAdminRequest,
    UpdateChannelRequest, UpdateTokenRequest, UpdateUserRequest, UserToken,
};
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
    crate::client_ip(headers, None)
}

// ---------------------------------------------------------------------------
// 管理员登录（§5.1：Argon2 + 强制 TOTP）
// ---------------------------------------------------------------------------

/// POST /api/v1/admin/auth/login
pub async fn login(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<AdminLoginRequest>,
) -> Result<axum::response::Response, ApiError> {
    let ip = crate::client_ip(&headers, Some(peer));
    // M2：按“账号 + IP”复合 key 计（5/分钟）+ 按 IP 聚合（30/分钟），
    // 不再三类登录共用一个 "unknown" 桶。
    let ip_key = ip.clone().unwrap_or_else(|| "unknown".to_string());
    if !crate::check_login_limit(
        &state,
        &ip_key,
        &format!("admin-login:{}:{ip_key}", req.username),
    ) {
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
    // TOTP 门：以 totp_enabled（是否已启用）为准，不以 secret 是否存在为准。
    // create_admin 在 totp_required 时当场 stage 密钥，secret 从一开始就非 NULL；
    // 若仍按 secret 判 pending，刚建好的管理员在首次配给前会被码挡住而锁死。
    if admin.totp_enabled {
        // 已启用：必须校验码。
        let Some(secret) = totp_secret else {
            return Err(ApiError::new(ErrorCode::AdminAuthFailed, "bad totp secret"));
        };
        let code = req.totp_code.as_deref().unwrap_or("");
        let totp = Totp::new(&secret)
            .ok_or_else(|| ApiError::new(ErrorCode::AdminAuthFailed, "bad totp secret"))?;
        if !totp.verify(code, iptv_auth::now_unix()) {
            return Err(ApiError::new(
                ErrorCode::AdminAuthFailed,
                "invalid totp code",
            ));
        }
    } else if admin.totp_required {
        // 待验证态：密钥已 stage 但尚未启用。带码且校验通过 → 当场启用；
        // 不带码 → 放行密码登录并写审计（绝不锁死账号）；带错码 → 401，
        // 但密码登录本身不受影响（下次不带码即可登录）。
        match (
            totp_secret.as_deref(),
            req.totp_code.as_deref().filter(|c| !c.is_empty()),
        ) {
            (Some(secret), Some(code)) => {
                let totp = Totp::new(secret)
                    .ok_or_else(|| ApiError::new(ErrorCode::AdminAuthFailed, "bad totp secret"))?;
                if !totp.verify(code, iptv_auth::now_unix()) {
                    return Err(ApiError::new(
                        ErrorCode::AdminAuthFailed,
                        "invalid totp code",
                    ));
                }
                let _ = state.store.set_admin_totp(admin.id, secret).await;
            }
            _ => {
                let _ = state
                    .store
                    .write_audit(&NewAuditLog {
                        actor: req.username.clone(),
                        actor_role: Some(format!("{:?}", admin.role)),
                        action: "admin.login_totp_pending".to_string(),
                        object_type: "admin".to_string(),
                        object_id: admin.id.to_string(),
                        before_json: None,
                        after_json: Some(
                            serde_json::json!({"totp_required": admin.totp_required, "totp_provisioned": totp_secret.is_some()}),
                        ),
                        ip_address: ip.clone(),
                    })
                    .await;
            }
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
    admin: AdminAuth,
    Query(page): Query<PageQuery>,
) -> ApiResult<Paged<UserToken>> {
    let mut tokens = state
        .store
        .list_tokens(None, None, &page)
        .await
        .map_err(ApiError::from)?;
    // m4：只读角色看 Token 须脱敏（§5.3“读（脱敏）”）：token_prefix 只留
    // 前 4 位，其余打码（明文与哈希本就不在列表里）。
    if admin.role == AdminRole::Readonly {
        for t in &mut tokens.items {
            let p = &t.token_prefix;
            t.token_prefix = if p.len() > 4 {
                format!("{}****", &p[..4])
            } else {
                "****".to_string()
            };
        }
    }
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
    // M1：票据有效期读系统设置 `ticket_ttl_sec`（默认 60s）。
    let ticket_ttl = state.store.get_setting_i64("ticket_ttl_sec").await;
    let ticket = state.tickets.issue(
        token.id,
        None,
        std::time::Duration::from_secs(ticket_ttl.max(1) as u64),
    );
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
    let ticket_ttl = state.store.get_setting_i64("ticket_ttl_sec").await;
    let ticket = state.tickets.issue(
        id,
        None,
        std::time::Duration::from_secs(ticket_ttl.max(1) as u64),
    );
    let portal_url = format!(
        "{}/t/{ticket}",
        state.config.portal_base_url.trim_end_matches('/')
    );
    ok(
        serde_json::json!({ "ticket": ticket, "portal_url": portal_url, "qr_png": make_qr_png(&portal_url), "expires_in_sec": ticket_ttl }),
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

/// POST /api/v1/admin/sources/upload：本地 M3U 上传。
///
/// **不限大小**（用户 2026-11-25 亲口批准，与文档 §7.1 的 20 MB 相悖，
/// 见 README；远程订阅拉取仍限 20 MB）。安全兜底是流式：multipart field
/// 按 chunk 落临时文件（不整包进内存），再逐行解析；解析仍在请求内同步
/// 完成（§7.1 的“异步解析”未实现，见 docs/ACCEPTANCE.md）。
pub async fn upload_m3u(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> ApiResult<M3uImportReport> {
    require(&admin, &[AdminRole::SuperAdmin], "upload sources")?;
    use tokio::io::AsyncWriteExt;
    let mut tmp_path: Option<std::path::PathBuf> = None;
    let mut filename = "upload.m3u".to_string();
    let mut source_id: Option<i64> = None;
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|_| ApiError::new(ErrorCode::BadRequest, "bad multipart"))?
    {
        let name = field.name().unwrap_or("").to_string();
        if name == "file" {
            filename = field.file_name().unwrap_or("upload.m3u").to_string();
            // 流式落盘：chunk 边读边写临时文件，文件多大内存都不涨。
            let path = std::env::temp_dir().join(format!(
                "iptv-upload-{}-{}.m3u",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            let mut file = tokio::fs::File::create(&path)
                .await
                .map_err(|_| ApiError::new(ErrorCode::Internal, "create temp file failed"))?;
            loop {
                match field.chunk().await {
                    Ok(Some(chunk)) => {
                        if file.write_all(&chunk).await.is_err() {
                            let _ = tokio::fs::remove_file(&path).await;
                            return Err(ApiError::new(
                                ErrorCode::Internal,
                                "write temp file failed",
                            ));
                        }
                    }
                    Ok(None) => break,
                    Err(_) => {
                        let _ = tokio::fs::remove_file(&path).await;
                        return Err(ApiError::new(ErrorCode::BadRequest, "read upload failed"));
                    }
                }
            }
            if file.flush().await.is_err() {
                let _ = tokio::fs::remove_file(&path).await;
                return Err(ApiError::new(ErrorCode::Internal, "write temp file failed"));
            }
            drop(file);
            tmp_path = Some(path);
        } else if name == "source_id" {
            let v = field.text().await.unwrap_or_default();
            source_id = v.trim().parse().ok();
        }
    }
    let tmp_path =
        tmp_path.ok_or_else(|| ApiError::new(ErrorCode::BadRequest, "missing file field"))?;
    // 逐行解析在阻塞线程跑（大文件解析是纯 CPU 活，不占 async 运行时），
    // 临时文件随后删除。
    let outcome = tokio::task::spawn_blocking(move || {
        let file = std::fs::File::open(&tmp_path)?;
        let parsed = iptv_m3u::parse_m3u_reader(std::io::BufReader::new(file));
        let _ = std::fs::remove_file(&tmp_path);
        parsed
    })
    .await
    .map_err(|_| ApiError::new(ErrorCode::Internal, "parse upload failed"))?
    .map_err(|_| ApiError::new(ErrorCode::BadRequest, "read upload failed"))?;
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
        total_lines: outcome.total_lines,
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
    admin: AdminAuth,
    Path(id): Path<i64>,
) -> ApiResult<SyncDiff> {
    // m4：订阅源读按 §5.3 权限矩阵——客服无订阅源权限（与 list_sources 一致）。
    if !admin.role.can_read_sources() {
        return Err(ApiError::new(ErrorCode::Forbidden, "cannot read sources"));
    }
    let diff = state
        .store
        .get_sync_diff(id)
        .await
        .map_err(ApiError::from)?;
    ok(diff)
}

pub async fn source_logs(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    Path(id): Path<i64>,
    Query(page): Query<PageQuery>,
) -> ApiResult<Paged<SyncLog>> {
    if !admin.role.can_read_sources() {
        return Err(ApiError::new(ErrorCode::Forbidden, "cannot read sources"));
    }
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
    admin: AdminAuth,
) -> ApiResult<Vec<EpgSource>> {
    // m4：EPG 源是订阅源的一类，读权限与订阅源一致（客服无权）。
    if !admin.role.can_read_sources() {
        return Err(ApiError::new(ErrorCode::Forbidden, "cannot read sources"));
    }
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
) -> ApiResult<CreateAdminResponse> {
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
    // totp_required 时当场配给密钥（待验证态），密钥/URI/二维码只返回一次，
    // 调用方必须立即展示给用户扫码；丢了只能由超管重新配给。
    let (totp_secret, totp_uri, qr_png) = if req.totp_required {
        let secret = generate_totp_secret();
        state
            .store
            .stage_admin_totp_secret(created.id, &secret)
            .await
            .map_err(ApiError::from)?;
        let uri = Totp::new(&secret)
            .map(|t| t.provisioning_uri(&req.username, "Muse-IPTV"))
            .unwrap_or_default();
        let qr = make_qr_png(&uri);
        (Some(secret), Some(uri), Some(qr))
    } else {
        (None, None, None)
    };
    // 配给后重取：返回的 admin.totp_provisioned 须为 true。
    let created = state
        .store
        .get_admin_by_id(created.id)
        .await
        .map_err(ApiError::from)?
        .unwrap_or(created);
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
    ok(CreateAdminResponse {
        admin: created,
        totp_secret,
        totp_uri,
        qr_png,
    })
}

/// PATCH /api/v1/admin/admins/{id}：改角色 / 是否强制 TOTP（仅超管）。
pub async fn update_admin(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(req): Json<UpdateAdminRequest>,
) -> ApiResult<AdminUser> {
    require(&admin, &[AdminRole::SuperAdmin], "update admins")?;
    // 不改自己的角色（否则可能把自己踢出超管）；不把最后一个超管降级。
    if admin.admin_id == id && req.role.is_some() {
        return Err(ApiError::new(
            ErrorCode::Forbidden,
            "cannot change your own role",
        ));
    }
    let before = state
        .store
        .get_admin_by_id(id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::new(ErrorCode::NotFound, "admin not found"))?;
    if before.role == AdminRole::SuperAdmin
        && req
            .role
            .as_ref()
            .is_some_and(|r| *r != AdminRole::SuperAdmin)
        && state
            .store
            .count_super_admins()
            .await
            .map_err(ApiError::from)?
            <= 1
    {
        return Err(ApiError::new(
            ErrorCode::Forbidden,
            "cannot demote the last super admin",
        ));
    }
    let updated = state
        .store
        .update_admin(id, req.role, req.totp_required)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::new(ErrorCode::NotFound, "admin not found"))?;
    audit(
        &state,
        &admin,
        "admin.update",
        "admin",
        &id.to_string(),
        Some(serde_json::to_value(&before).unwrap_or_default()),
        Some(serde_json::to_value(&updated).unwrap_or_default()),
        ip_of(&headers).as_deref(),
    )
    .await;
    ok(updated)
}

/// DELETE /api/v1/admin/admins/{id}：删除管理员（仅超管；不删自己、
/// 不删最后一个超管）。
pub async fn delete_admin(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> ApiResult<serde_json::Value> {
    require(&admin, &[AdminRole::SuperAdmin], "delete admins")?;
    if admin.admin_id == id {
        return Err(ApiError::new(
            ErrorCode::Forbidden,
            "cannot delete yourself",
        ));
    }
    let before = state
        .store
        .get_admin_by_id(id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::new(ErrorCode::NotFound, "admin not found"))?;
    if before.role == AdminRole::SuperAdmin
        && state
            .store
            .count_super_admins()
            .await
            .map_err(ApiError::from)?
            <= 1
    {
        return Err(ApiError::new(
            ErrorCode::Forbidden,
            "cannot delete the last super admin",
        ));
    }
    let deleted = state.store.delete_admin(id).await.map_err(ApiError::from)?;
    if !deleted {
        return Err(ApiError::new(ErrorCode::NotFound, "admin not found"));
    }
    audit(
        &state,
        &admin,
        "admin.delete",
        "admin",
        &id.to_string(),
        Some(serde_json::to_value(&before).unwrap_or_default()),
        None,
        ip_of(&headers).as_deref(),
    )
    .await;
    ok(serde_json::json!({ "deleted": true }))
}

/// POST /api/v1/admin/admins/{id}/reset-totp：重新配给 TOTP 密钥（仅超管）。
/// 旧密钥立即作废，新密钥回到待验证态；密钥/URI/二维码只返回一次。
pub async fn reset_admin_totp(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> ApiResult<serde_json::Value> {
    require(&admin, &[AdminRole::SuperAdmin], "reset admin totp")?;
    let target = state
        .store
        .get_admin_by_id(id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::new(ErrorCode::NotFound, "admin not found"))?;
    let secret = generate_totp_secret();
    state
        .store
        .stage_admin_totp_secret(id, &secret)
        .await
        .map_err(ApiError::from)?;
    let uri = Totp::new(&secret)
        .map(|t| t.provisioning_uri(&target.username, "Muse-IPTV"))
        .unwrap_or_default();
    let qr_png = make_qr_png(&uri);
    audit(
        &state,
        &admin,
        "admin.reset_totp",
        "admin",
        &id.to_string(),
        None,
        Some(serde_json::json!({"totp_provisioned": true, "totp_enabled": false})),
        ip_of(&headers).as_deref(),
    )
    .await;
    ok(serde_json::json!({ "totp_secret": secret, "totp_uri": uri, "qr_png": qr_png }))
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

/// GET /api/v1/admin/audit/export：审计导出（m10，§5.1“审计检索与导出”）。
/// 与列表同款过滤条件，返回 CSV 文件下载；单页上限 100 行，逐页取到
/// 10000 行为止。仅超管可读（与列表同权限）。
pub async fn export_audit(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    Query(q): Query<AuditQuery>,
) -> Result<Response, ApiError> {
    if !admin.role.can_read_audit() {
        return Err(ApiError::new(ErrorCode::Forbidden, "cannot read audit"));
    }
    let mut csv = String::from("id,created_at,actor,actor_role,action,object_type,object_id,ip_address,before_json,after_json\n");
    let mut fetched: u64 = 0;
    let mut page_no: u32 = 1;
    loop {
        let page = PageQuery {
            page: page_no,
            page_size: 100,
            q: None,
        };
        let logs = state
            .store
            .list_audit(q.actor.as_deref(), q.action.as_deref(), q.from, q.to, &page)
            .await
            .map_err(ApiError::from)?;
        let n = logs.items.len();
        for l in logs.items {
            csv.push_str(&format!(
                "{},{},{},{},{},{},{},{},{},{}\n",
                l.id,
                l.created_at.to_rfc3339(),
                csv_field(&l.actor),
                csv_field(l.actor_role.as_deref().unwrap_or("")),
                csv_field(&l.action),
                csv_field(&l.object_type),
                csv_field(&l.object_id),
                csv_field(l.ip_address.as_deref().unwrap_or("")),
                csv_field(&l.before_json.map(|v| v.to_string()).unwrap_or_default()),
                csv_field(&l.after_json.map(|v| v.to_string()).unwrap_or_default()),
            ));
        }
        fetched += n as u64;
        if n < 100 || fetched >= 10_000 {
            break;
        }
        page_no += 1;
    }
    let filename = format!("audit-{}.csv", chrono::Utc::now().format("%Y%m%d-%H%M%S"));
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/csv; charset=utf-8")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        )
        .body(axum::body::Body::from(csv))
        .unwrap())
}

/// CSV 字段转义：含逗号/引号/换行时加双引号，内部双引号加倍。
fn csv_field(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
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
        // M1：会话超时改了立即同步到并发限制器（其余键在下次读取时生效，
        // 设置读有 60 秒缓存）。
        if k == "session_timeout_sec" {
            if let Some(n) = v.as_i64() {
                state.limiter.set_ttl_sec(n);
            }
        }
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
