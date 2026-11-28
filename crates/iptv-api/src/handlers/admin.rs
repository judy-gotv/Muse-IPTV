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
use iptv_core::{
    generate_token, hash_token, mirror_logo_if_configured, token_prefix, S3Config,
    LOGO_MIRROR_PER_SYNC,
};
use iptv_dto::{
    AdminLoginRequest, AdminRole, AdminUser, AppUser, AuditLog, Channel, ChannelGroup,
    CreateAdminRequest, CreateAdminResponse, CreateChannelRequest, CreateEpgSourceRequest,
    CreateGroupRequest, CreatePlanRequest, CreateSourceRequest, CreateTokenRequest,
    CreateTokenResponse, CreateUserRequest, EpgSource, ErrorCode, PageQuery, Paged, Plan,
    PlaylistSource, StatsOverview, SyncDiff, SyncLog, SystemSetting, UpdateAdminRequest,
    UpdateChannelRequest, UpdateEpgSourceRequest, UpdateGroupRequest, UpdatePlanRequest,
    UpdateSettingsRequest, UpdateSourceRequest, UpdateTokenRequest, UpdateUserRequest,
    UploadM3uRequest, UploadTaskCreated, UploadTaskProgress, UploadTaskStatus, UserToken,
};
use iptv_store::NewAuditLog;
use std::future::Future;
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

/// m9 台标转存（频道创建 / M3U 上传共用）：未配置存储时直接返回；
/// 单张转存失败只记日志、保留直链；成功才写回 `logo_url` 并更新
/// 传入的频道对象（创建接口的返回体随之是转存后地址）。
async fn mirror_channel_logos(state: &Arc<AppState>, channels: &mut [Channel]) {
    let Some(cfg) = S3Config::from_env() else {
        return;
    };
    let mut seen = std::collections::HashSet::new();
    let mut n = 0;
    for ch in channels.iter_mut() {
        let Some(logo_url) = ch.logo_url.clone() else {
            continue;
        };
        if logo_url.trim().is_empty() || !seen.insert(logo_url.clone()) {
            continue;
        }
        if n >= LOGO_MIRROR_PER_SYNC {
            break;
        }
        n += 1;
        match mirror_logo_if_configured(Some(&cfg), &logo_url).await {
            Ok(Some(new_url)) => {
                if let Err(e) = state.store.update_channel_logo_url(ch.id, &new_url).await {
                    tracing::warn!("update logo url channel {} failed: {e:#}", ch.id);
                } else {
                    ch.logo_url = Some(new_url);
                }
            }
            Ok(None) => {}
            Err(e) => tracing::warn!("logo mirror channel {} failed: {e:#}", ch.id),
        }
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
#[utoipa::path(post, path = "/api/v1/admin/auth/login", tag = "admin", request_body = AdminLoginRequest, responses((status = 200, description = "ok，下发 iptv_admin_sid Cookie")))]
pub async fn login(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<AdminLoginRequest>,
) -> Result<axum::response::Response, ApiError> {
    // m25：拿不到客户端 IP 时无法按 IP 限流，直接拒绝，不落 "unknown" 桶。
    let Some(ip) = crate::client_ip(&headers, Some(peer)) else {
        return Err(ApiError::new(
            ErrorCode::RateLimited,
            "client ip unavailable",
        ));
    };
    // M2：按“账号 + IP”复合 key 计（5/分钟）+ 按 IP 聚合（30/分钟），
    // 不再三类登录共用一个 "unknown" 桶。
    if !crate::check_login_limit(&state, &ip, &format!("admin-login:{}:{ip}", req.username)) {
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
                        ip_address: Some(ip.clone()),
                    })
                    .await;
            }
        }
    }
    let exp = Utc::now() + Duration::hours(12);
    let cookie = crate::sign_admin_cookie(&state.config.admin_secret, admin.id, exp.timestamp());
    // m25：登录成功，重置该复合 key 的计数——成功登录不占限流桶。
    crate::reset_login_limit(&state, &format!("admin-login:{}:{ip}", req.username));
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
#[utoipa::path(get, path = "/api/v1/admin/stats/overview", tag = "admin", responses((status = 200, description = "仪表盘总览（StatsOverview）")))]
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

#[utoipa::path(get, path = "/api/v1/admin/users", tag = "admin", params(PageQuery), responses((status = 200, description = "用户列表，Paged<AppUser>")))]
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

#[utoipa::path(post, path = "/api/v1/admin/users", tag = "admin", request_body = CreateUserRequest, responses((status = 200, description = "新建用户（AppUser）")))]
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

#[utoipa::path(get, path = "/api/v1/admin/users/{id}", tag = "admin", responses((status = 200, description = "用户详情（AppUser）")))]
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

#[utoipa::path(patch, path = "/api/v1/admin/users/{id}", tag = "admin", request_body = UpdateUserRequest, responses((status = 200, description = "更新用户（AppUser）")))]
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

/// DELETE /api/v1/admin/users/{id}（M19a）：删用户——先释放其全部 Token
/// 的播放会话并吊销门户会话、禁用 Token（store 层），再删用户行。
#[utoipa::path(delete, path = "/api/v1/admin/users/{id}", tag = "admin", responses((status = 200, description = "ok")))]
pub async fn delete_user(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> ApiResult<serde_json::Value> {
    require(
        &admin,
        &[AdminRole::SuperAdmin, AdminRole::Operator],
        "delete users",
    )?;
    let before = state.store.get_user(id).await.map_err(ApiError::from)?;
    let tokens = state
        .store
        .list_tokens(Some(id), None, &PageQuery::default())
        .await
        .map_err(ApiError::from)?;
    for t in &tokens.items {
        let _ = revoke_token_sessions_inner(&state, t.id).await;
    }
    let deleted = state.store.delete_user(id).await.map_err(ApiError::from)?;
    if !deleted {
        return Err(ApiError::new(ErrorCode::NotFound, "user not found"));
    }
    audit(
        &state,
        &admin,
        "user.delete",
        "user",
        &id.to_string(),
        before.map(|u| serde_json::to_value(u).unwrap_or_default()),
        None,
        ip_of(&headers).as_deref(),
    )
    .await;
    ok(serde_json::json!({ "deleted": true }))
}

// ---------------------------------------------------------------------------
// Token（§5.2）
// ---------------------------------------------------------------------------

#[utoipa::path(get, path = "/api/v1/admin/tokens", tag = "admin", params(PageQuery), responses((status = 200, description = "Token 列表，Paged<UserToken>")))]
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
    // m4/m26：只读与客服角色看 Token 须脱敏（§5.3“读（脱敏）”）：
    // token_prefix 只留前 4 位，其余打码（明文与哈希本就不在列表里）。
    if matches!(admin.role, AdminRole::Readonly | AdminRole::Support) {
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

#[utoipa::path(post, path = "/api/v1/admin/tokens", tag = "admin", request_body = CreateTokenRequest, responses((status = 200, description = "创建 Token（明文只返回一次），CreateTokenResponse")))]
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
    let ticket = state
        .tickets
        .issue(
            token.id,
            None,
            std::time::Duration::from_secs(ticket_ttl.max(1) as u64),
        )
        .await
        .map_err(ApiError::from)?;
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

#[utoipa::path(patch, path = "/api/v1/admin/tokens/{id}", tag = "admin", request_body = UpdateTokenRequest, responses((status = 200, description = "更新 Token（UserToken）")))]
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

/// DELETE /api/v1/admin/tokens/{id}（M19b）：删 Token——先释放其播放会话
/// 并吊销门户会话，再删 Token 行。
#[utoipa::path(delete, path = "/api/v1/admin/tokens/{id}", tag = "admin", responses((status = 200, description = "ok")))]
pub async fn delete_token(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> ApiResult<serde_json::Value> {
    require(
        &admin,
        &[AdminRole::SuperAdmin, AdminRole::Operator],
        "delete tokens",
    )?;
    let before = state.store.get_token(id).await.map_err(ApiError::from)?;
    let _ = revoke_token_sessions_inner(&state, id).await;
    let deleted = state.store.delete_token(id).await.map_err(ApiError::from)?;
    if !deleted {
        return Err(ApiError::new(ErrorCode::NotFound, "token not found"));
    }
    audit(
        &state,
        &admin,
        "token.delete",
        "token",
        &id.to_string(),
        before.map(|t| serde_json::to_value(t).unwrap_or_default()),
        None,
        ip_of(&headers).as_deref(),
    )
    .await;
    ok(serde_json::json!({ "deleted": true }))
}

/// POST /api/v1/admin/tokens/{id}/login-ticket：生成一次性登录链接/二维码。
#[utoipa::path(post, path = "/api/v1/admin/tokens/{id}/login-ticket", tag = "admin", responses((status = 200, description = "ticket + portal_url + qr_png")))]
pub async fn create_login_ticket(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    headers: HeaderMap,
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
    let ticket = state
        .tickets
        .issue(
            id,
            None,
            std::time::Duration::from_secs(ticket_ttl.max(1) as u64),
        )
        .await
        .map_err(ApiError::from)?;
    let portal_url = format!(
        "{}/t/{ticket}",
        state.config.portal_base_url.trim_end_matches('/')
    );
    // M20：签发票据是写操作，写审计（票据明文不进审计快照；库里只存其 SHA-256 哈希）。
    audit(
        &state,
        &admin,
        "token.login_ticket",
        "token",
        &id.to_string(),
        None,
        Some(serde_json::json!({"issued": true})),
        ip_of(&headers).as_deref(),
    )
    .await;
    ok(
        serde_json::json!({ "ticket": ticket, "portal_url": portal_url, "qr_png": make_qr_png(&portal_url), "expires_in_sec": ticket_ttl }),
    )
}

/// 吊销一个 Token 的全部会话并释放播放并发额度（M19a/b 与
/// `revoke_token_sessions` 共用）：先吊销门户会话，再释放 limiter 额度并
/// 收尾播放会话。
async fn revoke_token_sessions_inner(
    state: &Arc<AppState>,
    token_id: i64,
) -> Result<u64, ApiError> {
    let n = state
        .store
        .revoke_all_portal_sessions(token_id, None)
        .await
        .map_err(ApiError::from)?;
    for s in state
        .store
        .active_sessions_for_token(token_id)
        .await
        .map_err(ApiError::from)?
    {
        let _ = state.limiter.release(token_id, &s.session_uid).await;
        let _ = state
            .store
            .end_session(&s.session_uid, Utc::now(), "revoked")
            .await;
    }
    Ok(n)
}

/// POST /api/v1/admin/tokens/{id}/revoke-sessions：吊销该 Token 全部会话。
#[utoipa::path(post, path = "/api/v1/admin/tokens/{id}/revoke-sessions", tag = "admin", responses((status = 200, description = "revoked 计数")))]
pub async fn revoke_token_sessions(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> ApiResult<serde_json::Value> {
    if !admin.role.can_kick_session() {
        return Err(ApiError::new(
            ErrorCode::Forbidden,
            "cannot revoke sessions",
        ));
    }
    let n = revoke_token_sessions_inner(&state, id).await?;
    // M20：吊销会话是写操作，写审计。
    audit(
        &state,
        &admin,
        "token.revoke_sessions",
        "token",
        &id.to_string(),
        None,
        Some(serde_json::json!({"revoked": n})),
        ip_of(&headers).as_deref(),
    )
    .await;
    ok(serde_json::json!({ "revoked": n }))
}

// ---------------------------------------------------------------------------
// 频道（§5.1 / §3.2）
// ---------------------------------------------------------------------------

#[utoipa::path(get, path = "/api/v1/admin/channels", tag = "admin", params(PageQuery), responses((status = 200, description = "频道列表，Paged<Channel>")))]
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

#[utoipa::path(post, path = "/api/v1/admin/channels", tag = "admin", request_body = CreateChannelRequest, responses((status = 200, description = "新建频道（Channel）")))]
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
    // M10：建前预查 tvg_id（store 层唯一约束兜底见 From<anyhow::Error>）。
    if let Some(tvg) = req.tvg_id.as_deref().filter(|t| !t.trim().is_empty()) {
        let page = state
            .store
            .list_channels(
                &PageQuery {
                    page: 1,
                    page_size: 20,
                    q: Some(tvg.to_string()),
                },
                None,
                None,
            )
            .await
            .map_err(ApiError::from)?;
        if page.items.iter().any(|c| c.tvg_id.as_deref() == Some(tvg)) {
            return Err(ApiError::new(ErrorCode::Conflict, "tvg_id already exists"));
        }
    }
    let mut channel = state
        .store
        .create_channel(&req)
        .await
        .map_err(ApiError::from)?;
    // m9：带台标且配了对象存储时转存；失败保留直链，不影响创建。
    mirror_channel_logos(&state, std::slice::from_mut(&mut channel)).await;
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

#[utoipa::path(patch, path = "/api/v1/admin/channels/{id}", tag = "admin", request_body = UpdateChannelRequest, responses((status = 200, description = "编辑频道（Channel）")))]
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
    let mut channel = state
        .store
        .update_channel(id, &req)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::new(ErrorCode::NotFound, "channel not found"))?;
    // m9：本次改了台标且配了对象存储时转存；失败保留直链，不影响更新。
    // 只在 logo_url 本次被改写时转存，避免改名等无关更新重复下载。
    let logo_changed = req
        .logo_url
        .as_ref()
        .is_some_and(|u| before.as_ref().and_then(|c| c.logo_url.as_ref()) != Some(u));
    if logo_changed {
        mirror_channel_logos(&state, std::slice::from_mut(&mut channel)).await;
    }
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

#[utoipa::path(delete, path = "/api/v1/admin/channels/{id}", tag = "admin", responses((status = 200, description = "ok")))]
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

#[utoipa::path(get, path = "/api/v1/admin/sources", tag = "admin", responses((status = 200, description = "订阅源列表，PlaylistSource[]")))]
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

#[utoipa::path(post, path = "/api/v1/admin/sources", tag = "admin", request_body = CreateSourceRequest, responses((status = 200, description = "新建订阅源（PlaylistSource）")))]
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

/// PATCH /api/v1/admin/sources/{id}（M19c）：改订阅源（仅超管）。
#[utoipa::path(patch, path = "/api/v1/admin/sources/{id}", tag = "admin", request_body = UpdateSourceRequest, responses((status = 200, description = "更新订阅源（PlaylistSource）")))]
pub async fn update_source(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(req): Json<UpdateSourceRequest>,
) -> ApiResult<PlaylistSource> {
    require(&admin, &[AdminRole::SuperAdmin], "update sources")?;
    if let Some(interval) = req.sync_interval_sec {
        if interval < 300 {
            return Err(ApiError::new(
                ErrorCode::ValidationFailed,
                "sync_interval_sec must be >= 300",
            ));
        }
    }
    let before = state.store.get_source(id).await.map_err(ApiError::from)?;
    let source = state
        .store
        .update_source(id, &req)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::new(ErrorCode::NotFound, "source not found"))?;
    audit(
        &state,
        &admin,
        "source.update",
        "source",
        &id.to_string(),
        before.map(|s| serde_json::to_value(s).unwrap_or_default()),
        Some(serde_json::to_value(&source).unwrap_or_default()),
        ip_of(&headers).as_deref(),
    )
    .await;
    ok(source)
}

/// DELETE /api/v1/admin/sources/{id}（M19c）：删订阅源（仅超管；同步日志
/// 随源清理，该源同步出的频道保留）。
#[utoipa::path(delete, path = "/api/v1/admin/sources/{id}", tag = "admin", responses((status = 200, description = "ok")))]
pub async fn delete_source(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> ApiResult<serde_json::Value> {
    require(&admin, &[AdminRole::SuperAdmin], "delete sources")?;
    let before = state.store.get_source(id).await.map_err(ApiError::from)?;
    let deleted = state
        .store
        .delete_source(id)
        .await
        .map_err(ApiError::from)?;
    if !deleted {
        return Err(ApiError::new(ErrorCode::NotFound, "source not found"));
    }
    audit(
        &state,
        &admin,
        "source.delete",
        "source",
        &id.to_string(),
        before.map(|s| serde_json::to_value(s).unwrap_or_default()),
        None,
        ip_of(&headers).as_deref(),
    )
    .await;
    ok(serde_json::json!({ "deleted": true }))
}

/// POST /api/v1/admin/sources/upload：本地 M3U 上传。
///
/// **不限大小**（用户 2026-11-25 亲口批准，与文档 §7.1 的 20 MB 相悖，
/// 见 README；远程订阅拉取仍限 20 MB）。安全兜底是流式：multipart field
/// 按 chunk 落临时文件（不整包进内存）。
///
/// M16（§7.1 异步解析）：本接口只收文件、注册任务后立即返回
/// `{ task_id }`；解析 + 入库在后台 tokio 任务里做，进度见
/// GET /api/v1/admin/upload-tasks/{id}。旧的同步返回体
/// `M3uImportReport`（total_lines / channels_parsed / channels_imported /
/// errors）不再由此接口返回，终态数字改从进度接口的同名字段读。
#[utoipa::path(post, path = "/api/v1/admin/sources/upload", tag = "admin", request_body(content = UploadM3uRequest, content_type = "multipart/form-data"), responses((status = 200, description = "上传 M3U（multipart，异步解析，返回任务 id），UploadTaskCreated")))]
pub async fn upload_m3u(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> ApiResult<UploadTaskCreated> {
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
    // M16：只注册任务、起后台任务，立即返回任务 id，不等解析完成。
    // m-uptask：任务行落库（V8 `upload_task` 表），重启后仍可查进度。
    let task_id = uuid::Uuid::new_v4().to_string();
    state
        .upload_tasks
        .create_queued(task_id.clone(), filename.clone(), source_id)
        .await
        .map_err(ApiError::from)?;
    let bg_state = state.clone();
    let bg_admin = AdminAuth {
        admin_id: admin.admin_id,
        username: admin.username.clone(),
        role: admin.role,
    };
    let bg_ip = ip_of(&headers);
    tokio::spawn(run_upload_task(
        bg_state,
        task_id.clone(),
        UploadTaskInput::File(tmp_path),
        filename,
        source_id,
        bg_admin,
        bg_ip,
        "source.upload",
    ));
    ok(UploadTaskCreated { task_id })
}

/// GET /api/v1/admin/upload-tasks/{id}（M16）：查异步上传任务进度。
/// 未知 id 回 404；m-uptask：任务落库，重启后仍可查；已过期终态任务也 404。
#[utoipa::path(get, path = "/api/v1/admin/upload-tasks/{id}", tag = "admin", responses((status = 200, description = "上传任务进度（UploadTaskProgress）")))]
pub async fn get_upload_task(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    Path(id): Path<String>,
) -> ApiResult<UploadTaskProgress> {
    require(&admin, &[AdminRole::SuperAdmin], "read upload tasks")?;
    match state.upload_tasks.get(&id).await.map_err(ApiError::from)? {
        Some(p) => ok(p),
        None => Err(ApiError::new(ErrorCode::NotFound, "upload task not found")),
    }
}

/// N8：后台导入任务的输入——文件上传落盘后的临时文件，或粘贴的
/// M3U 文本。两种输入走同一解析器（`parse_m3u_reader_progress`，
/// 共用 `M3uParser` 状态机）与同一任务执行体，不复制解析逻辑。
#[derive(Debug)]
pub(crate) enum UploadTaskInput {
    File(std::path::PathBuf),
    Text(String),
}

/// M16：后台上传任务——解析（阻塞线程跑，边解析边上报进度）→ 建源/取源
/// → upsert 入库 → 台标转存 → 审计 → 置 done；任一步失败置 failed 并记错。
/// 解析完即删临时文件；入库是单次 `upsert_channels` 全量调用（与同步版
/// 完全一致的语义），故入库阶段进度停在“已解析终态、已入库 0”，完成后
/// 一次跳到终态数字——不把 upsert 拆批，避免每批重读全源频道导致二次方开销。
/// N8：粘贴导入复用本任务（`UploadTaskInput::Text`）；`audit_action`
/// 区分审计动作（上传 `source.upload`、粘贴 `source.paste`）。
#[allow(clippy::too_many_arguments)]
/// 终态写库带退避重试（3 次）：任务终态与源同步状态写失败时不得静默
/// 丢弃，否则任务永久卡在 running（v0.1.6 收尾验证：磁盘满时终态更新
/// 的 `let _ =` 吞掉错误，任务停在 running 无恢复）。最终失败记 error
/// 日志（调用方继续走原流程，任务行以最后一次成功写为准）。
async fn retry_terminal_write<F, Fut>(label: &str, mut attempt: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    let mut delay = std::time::Duration::from_millis(100);
    for i in 1..=3 {
        match attempt().await {
            Ok(()) => return,
            Err(e) if i < 3 => {
                tracing::warn!("{label} 第 {i}/3 次写失败，{delay:?} 后重试：{e:#}");
                tokio::time::sleep(delay).await;
                delay *= 2;
            }
            Err(e) => {
                tracing::error!("{label} 3 次重试均失败（终态可能未落库）：{e:#}");
            }
        }
    }
}

/// 8 参数是上传任务的固有输入（状态/任务/输入/文件名/源/操作者/IP/审计
/// 动作），两处调用点（上传、粘贴）共用同一签名，故放行。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_upload_task(
    state: Arc<AppState>,
    task_id: String,
    input: UploadTaskInput,
    filename: String,
    source_id: Option<i64>,
    admin: AdminAuth,
    ip: Option<String>,
    audit_action: &'static str,
) {
    let tasks = state.upload_tasks.clone();
    let _ = tasks
        .update(&task_id, |p| p.status = UploadTaskStatus::Running)
        .await;
    // 解析：阻塞线程跑（纯 CPU 活），每 2000 行回写一次进度。
    let parse_tasks = tasks.clone();
    let parse_id = task_id.clone();
    let parsed = tokio::task::spawn_blocking(move || {
        // 解析回调跑在 spawn_blocking 线程内：用阻塞版行级 UPDATE
        // 写库（见 UploadTaskRegistry::update_progress_blocking）。
        let mut report = |l: usize, c: usize, e: usize| {
            parse_tasks.update_progress_blocking(&parse_id, l, c, e);
        };
        match input {
            UploadTaskInput::File(tmp_path) => {
                let file = std::fs::File::open(&tmp_path)?;
                let out =
                    iptv_m3u::parse_m3u_reader_progress(std::io::BufReader::new(file), &mut report);
                let _ = std::fs::remove_file(&tmp_path);
                out
            }
            // 文本经 Cursor 走同一流式解析器（BOM 只剥首行、进度回调
            // 每 2000 行触发，与文件路径行为一致）。
            UploadTaskInput::Text(text) => iptv_m3u::parse_m3u_reader_progress(
                std::io::BufReader::new(std::io::Cursor::new(text.into_bytes())),
                &mut report,
            ),
        }
    })
    .await;
    let outcome = match parsed {
        Ok(Ok(o)) => o,
        Ok(Err(_)) => {
            retry_terminal_write("upload task fail: read upload failed", || async {
                tasks
                    .fail(&task_id, "read upload failed".to_string())
                    .await
                    .map(|_| ())
            })
            .await;
            return;
        }
        Err(_) => {
            retry_terminal_write("upload task fail: parse upload failed", || async {
                tasks
                    .fail(&task_id, "parse upload failed".to_string())
                    .await
                    .map(|_| ())
            })
            .await;
            return;
        }
    };
    let _ = tasks
        .update(&task_id, |p| {
            p.total_lines = outcome.total_lines;
            p.channels_parsed = outcome.channels.len();
            p.error_count = outcome.errors.len();
        })
        .await;
    let source = match source_id {
        Some(id) => match state.store.get_source(id).await {
            Ok(Some(s)) => s,
            Ok(None) => {
                retry_terminal_write("upload task fail: source not found", || async {
                    tasks
                        .fail(&task_id, "source not found".to_string())
                        .await
                        .map(|_| ())
                })
                .await;
                return;
            }
            Err(e) => {
                let msg = format!("read source failed: {e}");
                retry_terminal_write("upload task fail: read source failed", || async {
                    tasks.fail(&task_id, msg.clone()).await.map(|_| ())
                })
                .await;
                return;
            }
        },
        None => match state
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
        {
            Ok(s) => s,
            Err(e) => {
                let msg = format!("create source failed: {e}");
                retry_terminal_write("upload task fail: create source failed", || async {
                    tasks.fail(&task_id, msg.clone()).await.map(|_| ())
                })
                .await;
                return;
            }
        },
    };
    let _ = tasks
        .update(&task_id, |p| p.source_id = Some(source.id))
        .await;
    // M-D（v0.1.6）：上传与 Worker 同步共用同一认领（`claim_source_sync`，
    // 与 Worker `sync_source` 同一个原子 UPDATE）。认领窗口只包住下面的
    // `upsert_channels`：解析（可能数分钟）与台标转存不占锁；认领成功后
    // 无论 upsert 成败都立即释放（`update_source_sync` 把 `last_status`
    // 写回 ok/failed，`next_sync_at` 为现在 + 同步周期，与 Worker 一致）。
    // 未认领到说明 Worker 正在同步该源、或另一次上传在跑：任务直接失败，
    // 原因写进任务的 `error` 字段，不静默丢弃、不排队（排队会让任务长时
    // 间挂在 running，调用方无法区分卡住与进行中；重试由调用方发起）。
    let next_sync_at = Utc::now() + Duration::seconds(source.sync_interval_sec);
    let claimed = match state.store.claim_source_sync(source.id, next_sync_at).await {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("claim source failed: {e}");
            retry_terminal_write("upload task fail: claim source failed", || async {
                tasks.fail(&task_id, msg.clone()).await.map(|_| ())
            })
            .await;
            return;
        }
    };
    if !claimed {
        retry_terminal_write("upload task fail: source already syncing", || async {
            tasks
                .fail(
                    &task_id,
                    "source is already syncing (another upload or worker sync is running)"
                        .to_string(),
                )
                .await
                .map(|_| ())
        })
        .await;
        return;
    }
    let sync = match state
        .store
        .upsert_channels(source.id, outcome.channels.clone())
        .await
    {
        Ok(s) => {
            retry_terminal_write("update_source_sync ok", || async {
                state
                    .store
                    .update_source_sync(
                        source.id,
                        Utc::now() + Duration::seconds(source.sync_interval_sec),
                        "ok",
                        None,
                    )
                    .await
            })
            .await;
            s
        }
        Err(e) => {
            retry_terminal_write("update_source_sync failed", || async {
                state
                    .store
                    .update_source_sync(
                        source.id,
                        Utc::now() + Duration::seconds(source.sync_interval_sec),
                        "failed",
                        Some(&e.to_string()),
                    )
                    .await
            })
            .await;
            let msg = format!("import channels failed: {e}");
            retry_terminal_write("upload task fail: import channels failed", || async {
                tasks.fail(&task_id, msg.clone()).await.map(|_| ())
            })
            .await;
            return;
        }
    };
    // m9：新增/变更频道的台标转存对象存储；未配置存储或转存失败时
    // 保留直链，不影响导入结果。
    let mut to_mirror: Vec<Channel> = sync
        .added_channels
        .iter()
        .chain(sync.metadata_changed.iter())
        .chain(sync.url_changed.iter())
        .cloned()
        .collect();
    mirror_channel_logos(&state, &mut to_mirror).await;
    audit(
        &state,
        &admin,
        audit_action,
        "source",
        &source.id.to_string(),
        None,
        Some(serde_json::json!({ "added": sync.added, "updated": sync.updated })),
        ip.as_deref(),
    )
    .await;
    retry_terminal_write("upload task done", || async {
        tasks
            .update(&task_id, |p| {
                p.status = UploadTaskStatus::Done;
                p.channels_imported = (sync.added + sync.updated) as usize;
                p.error_count = outcome.errors.len();
            })
            .await
            .map(|_| ())
    })
    .await;
}

/// POST /api/v1/admin/sources/{id}/sync：立即同步（把 next_sync_at 置为现在，worker 拾取）。
/// M-D（v0.1.6）：源正被上传/Worker 认领同步中时回 409（`is_source_syncing`，
/// 与 `claim_source_sync` 同一条 15 分钟陈旧规则），不再无条件把状态置为
/// pending——旧写法会让手动触发与飞行中的上传互相覆盖认领状态。
#[utoipa::path(post, path = "/api/v1/admin/sources/{id}/sync", tag = "admin", responses((status = 200, description = "triggered")))]
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
    if state
        .store
        .is_source_syncing(id)
        .await
        .map_err(ApiError::from)?
    {
        return Err(ApiError::new(
            ErrorCode::Conflict,
            "source is already syncing",
        ));
    }
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

#[utoipa::path(get, path = "/api/v1/admin/sources/{id}/diff", tag = "admin", responses((status = 200, description = "同步差异（新增/变更/失效），SyncDiff")))]
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

#[utoipa::path(get, path = "/api/v1/admin/sources/{id}/logs", tag = "admin", params(PageQuery), responses((status = 200, description = "同步日志，Paged<SyncLog>")))]
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

#[utoipa::path(get, path = "/api/v1/admin/groups", tag = "admin", responses((status = 200, description = "频道分组，ChannelGroup[]")))]
pub async fn list_groups(
    State(state): State<Arc<AppState>>,
    _admin: AdminAuth,
) -> ApiResult<Vec<ChannelGroup>> {
    let groups = state.store.list_groups().await.map_err(ApiError::from)?;
    ok(groups)
}

#[utoipa::path(post, path = "/api/v1/admin/groups", tag = "admin", request_body = CreateGroupRequest, responses((status = 200, description = "新建分组（ChannelGroup）")))]
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
    // M10：建前预查分组名（store 层唯一约束兜底见 From<anyhow::Error>）。
    let groups = state.store.list_groups().await.map_err(ApiError::from)?;
    if groups.iter().any(|g| g.name == req.name) {
        return Err(ApiError::new(
            ErrorCode::Conflict,
            "group name already exists",
        ));
    }
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

/// PATCH /api/v1/admin/groups/{id}（M19e）：改分组。
#[utoipa::path(patch, path = "/api/v1/admin/groups/{id}", tag = "admin", request_body = UpdateGroupRequest, responses((status = 200, description = "更新分组（ChannelGroup）")))]
pub async fn update_group(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(req): Json<UpdateGroupRequest>,
) -> ApiResult<ChannelGroup> {
    require(
        &admin,
        &[AdminRole::SuperAdmin, AdminRole::Operator],
        "update groups",
    )?;
    // 改名时与建分组同款预查：重名 → 409（store 层唯一约束兜底）。
    if let Some(name) = &req.name {
        let groups = state.store.list_groups().await.map_err(ApiError::from)?;
        if groups.iter().any(|g| g.name == *name && g.id != id) {
            return Err(ApiError::new(
                ErrorCode::Conflict,
                "group name already exists",
            ));
        }
    }
    let before = state.store.get_group(id).await.map_err(ApiError::from)?;
    let group = state
        .store
        .update_group(id, &req)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::new(ErrorCode::NotFound, "group not found"))?;
    audit(
        &state,
        &admin,
        "group.update",
        "group",
        &id.to_string(),
        before.map(|g| serde_json::to_value(g).unwrap_or_default()),
        Some(serde_json::to_value(&group).unwrap_or_default()),
        ip_of(&headers).as_deref(),
    )
    .await;
    ok(group)
}

/// DELETE /api/v1/admin/groups/{id}（M19e）：删分组——`channel_group_map`
/// 里的映射随分组清理（频道本身保留）。
#[utoipa::path(delete, path = "/api/v1/admin/groups/{id}", tag = "admin", responses((status = 200, description = "ok")))]
pub async fn delete_group(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> ApiResult<serde_json::Value> {
    require(
        &admin,
        &[AdminRole::SuperAdmin, AdminRole::Operator],
        "delete groups",
    )?;
    let before = state.store.get_group(id).await.map_err(ApiError::from)?;
    let deleted = state.store.delete_group(id).await.map_err(ApiError::from)?;
    if !deleted {
        return Err(ApiError::new(ErrorCode::NotFound, "group not found"));
    }
    audit(
        &state,
        &admin,
        "group.delete",
        "group",
        &id.to_string(),
        before.map(|g| serde_json::to_value(g).unwrap_or_default()),
        None,
        ip_of(&headers).as_deref(),
    )
    .await;
    ok(serde_json::json!({ "deleted": true }))
}

#[utoipa::path(get, path = "/api/v1/admin/epg/sources", tag = "admin", responses((status = 200, description = "EPG 源列表，EpgSource[]")))]
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

#[utoipa::path(post, path = "/api/v1/admin/epg/sources", tag = "admin", request_body = CreateEpgSourceRequest, responses((status = 200, description = "新建 EPG 源（EpgSource）")))]
pub async fn create_epg_source(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    headers: HeaderMap,
    Json(req): Json<CreateEpgSourceRequest>,
) -> ApiResult<EpgSource> {
    require(&admin, &[AdminRole::SuperAdmin], "create epg sources")?;
    // m-openapi：typed DTO 已保证 name/url 存在；空串仍按原校验报 422。
    let name = req.name;
    let url = req.url;
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
    // M20：建 EPG 源是写操作，写审计。
    audit(
        &state,
        &admin,
        "epg_source.create",
        "epg_source",
        &source.id.to_string(),
        None,
        Some(serde_json::to_value(&source).unwrap_or_default()),
        ip_of(&headers).as_deref(),
    )
    .await;
    ok(source)
}

/// PATCH /api/v1/admin/epg/sources/{id}（M19f）：改 EPG 源（仅超管）。
#[utoipa::path(patch, path = "/api/v1/admin/epg/sources/{id}", tag = "admin", request_body = UpdateEpgSourceRequest, responses((status = 200, description = "更新 EPG 源（EpgSource）")))]
pub async fn update_epg_source(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(req): Json<UpdateEpgSourceRequest>,
) -> ApiResult<EpgSource> {
    require(&admin, &[AdminRole::SuperAdmin], "update epg sources")?;
    let before = state
        .store
        .get_epg_source(id)
        .await
        .map_err(ApiError::from)?;
    let source = state
        .store
        .update_epg_source(id, &req)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::new(ErrorCode::NotFound, "epg source not found"))?;
    audit(
        &state,
        &admin,
        "epg_source.update",
        "epg_source",
        &id.to_string(),
        before.map(|s| serde_json::to_value(s).unwrap_or_default()),
        Some(serde_json::to_value(&source).unwrap_or_default()),
        ip_of(&headers).as_deref(),
    )
    .await;
    ok(source)
}

/// DELETE /api/v1/admin/epg/sources/{id}（M19f）：删 EPG 源（仅超管；
/// `epg_channel_map` 里的节目映射随源清理，已生成的节目行保留）。
#[utoipa::path(delete, path = "/api/v1/admin/epg/sources/{id}", tag = "admin", responses((status = 200, description = "ok")))]
pub async fn delete_epg_source(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> ApiResult<serde_json::Value> {
    require(&admin, &[AdminRole::SuperAdmin], "delete epg sources")?;
    let before = state
        .store
        .get_epg_source(id)
        .await
        .map_err(ApiError::from)?;
    let deleted = state
        .store
        .delete_epg_source(id)
        .await
        .map_err(ApiError::from)?;
    if !deleted {
        return Err(ApiError::new(ErrorCode::NotFound, "epg source not found"));
    }
    audit(
        &state,
        &admin,
        "epg_source.delete",
        "epg_source",
        &id.to_string(),
        before.map(|s| serde_json::to_value(s).unwrap_or_default()),
        None,
        ip_of(&headers).as_deref(),
    )
    .await;
    ok(serde_json::json!({ "deleted": true }))
}

#[utoipa::path(get, path = "/api/v1/admin/sessions", tag = "admin", params(PageQuery), responses((status = 200, description = "实时会话看板，Paged<PlaybackSession>")))]
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

#[utoipa::path(delete, path = "/api/v1/admin/sessions/{id}", tag = "admin", responses((status = 200, description = "ok")))]
pub async fn kick_session(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    headers: HeaderMap,
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
    // M20：踢会话是写操作，写审计（before 为踢前的会话快照）。
    let before = serde_json::to_value(&session).unwrap_or_default();
    let _ = state
        .limiter
        .release(session.token_id, &session.session_uid)
        .await;
    state
        .store
        .end_session(&session.session_uid, Utc::now(), "kicked")
        .await
        .map_err(ApiError::from)?;
    audit(
        &state,
        &admin,
        "session.kick",
        "playback_session",
        &id.to_string(),
        Some(before),
        Some(serde_json::json!({"kicked": true, "ended_reason": "kicked"})),
        ip_of(&headers).as_deref(),
    )
    .await;
    ok(serde_json::json!({ "kicked": true }))
}

#[utoipa::path(get, path = "/api/v1/admin/plans", tag = "admin", responses((status = 200, description = "套餐列表，Plan[]")))]
pub async fn list_plans(
    State(state): State<Arc<AppState>>,
    _admin: AdminAuth,
) -> ApiResult<Vec<Plan>> {
    let plans = state.store.list_plans().await.map_err(ApiError::from)?;
    ok(plans)
}

#[utoipa::path(post, path = "/api/v1/admin/plans", tag = "admin", request_body = CreatePlanRequest, responses((status = 200, description = "新建套餐（Plan）")))]
pub async fn create_plan(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    headers: HeaderMap,
    Json(req): Json<CreatePlanRequest>,
) -> ApiResult<Plan> {
    require(&admin, &[AdminRole::SuperAdmin], "create plans")?;
    // M10：建前预查套餐名（store 层唯一约束兜底见 From<anyhow::Error>）。
    let plans = state.store.list_plans().await.map_err(ApiError::from)?;
    if plans.iter().any(|p| p.name == req.name) {
        return Err(ApiError::new(
            ErrorCode::Conflict,
            "plan name already exists",
        ));
    }
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

/// PATCH /api/v1/admin/plans/{id}（M19d）：改套餐（仅超管）。
#[utoipa::path(patch, path = "/api/v1/admin/plans/{id}", tag = "admin", request_body = UpdatePlanRequest, responses((status = 200, description = "更新套餐（Plan）")))]
pub async fn update_plan(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(req): Json<UpdatePlanRequest>,
) -> ApiResult<Plan> {
    require(&admin, &[AdminRole::SuperAdmin], "update plans")?;
    // 改名时与建套餐同款预查：重名 → 409（store 层唯一约束兜底）。
    if let Some(name) = &req.name {
        let plans = state.store.list_plans().await.map_err(ApiError::from)?;
        if plans.iter().any(|p| p.name == *name && p.id != id) {
            return Err(ApiError::new(
                ErrorCode::Conflict,
                "plan name already exists",
            ));
        }
    }
    let before = state.store.get_plan(id).await.map_err(ApiError::from)?;
    let plan = state
        .store
        .update_plan(id, &req)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::new(ErrorCode::NotFound, "plan not found"))?;
    audit(
        &state,
        &admin,
        "plan.update",
        "plan",
        &id.to_string(),
        before.map(|p| serde_json::to_value(p).unwrap_or_default()),
        Some(serde_json::to_value(&plan).unwrap_or_default()),
        ip_of(&headers).as_deref(),
    )
    .await;
    ok(plan)
}

/// DELETE /api/v1/admin/plans/{id}（M19d）：删套餐（仅超管）；仍有用户
/// 引用该套餐时拒绝（409）并明示人数。
#[utoipa::path(delete, path = "/api/v1/admin/plans/{id}", tag = "admin", responses((status = 200, description = "ok")))]
pub async fn delete_plan(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> ApiResult<serde_json::Value> {
    require(&admin, &[AdminRole::SuperAdmin], "delete plans")?;
    let before = state.store.get_plan(id).await.map_err(ApiError::from)?;
    let n_users = state
        .store
        .count_users_with_plan(id)
        .await
        .map_err(ApiError::from)?;
    if n_users > 0 {
        // N1：被引用文案走 FTL（`errors.plan-in-use`，$n 为引用人数），
        // 不再拼英文；code 仍为 40900，message 由序列化层按 locale 渲染。
        return Err(ApiError::with_key(
            ErrorCode::Conflict,
            "errors.plan-in-use",
            format!("plan is in use by {n_users} user(s); reassign them first"),
            vec![("n".to_string(), serde_json::json!(n_users))],
        ));
    }
    let deleted = state.store.delete_plan(id).await.map_err(ApiError::from)?;
    if !deleted {
        return Err(ApiError::new(ErrorCode::NotFound, "plan not found"));
    }
    audit(
        &state,
        &admin,
        "plan.delete",
        "plan",
        &id.to_string(),
        before.map(|p| serde_json::to_value(p).unwrap_or_default()),
        None,
        ip_of(&headers).as_deref(),
    )
    .await;
    ok(serde_json::json!({ "deleted": true }))
}

#[utoipa::path(get, path = "/api/v1/admin/admins", tag = "admin", responses((status = 200, description = "管理员列表（超管），AdminUser[]")))]
pub async fn list_admins(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
) -> ApiResult<Vec<AdminUser>> {
    require(&admin, &[AdminRole::SuperAdmin], "list admins")?;
    let admins = state.store.list_admins().await.map_err(ApiError::from)?;
    ok(admins)
}

#[utoipa::path(post, path = "/api/v1/admin/admins", tag = "admin", request_body = CreateAdminRequest, responses((status = 200, description = "新建管理员（超管），CreateAdminResponse")))]
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
    // M10：建前预查用户名（store 层唯一约束兜底见 From<anyhow::Error>）。
    if state
        .store
        .find_admin_by_username(&req.username)
        .await
        .map_err(ApiError::from)?
        .is_some()
    {
        return Err(ApiError::new(
            ErrorCode::Conflict,
            "username already exists",
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
#[utoipa::path(patch, path = "/api/v1/admin/admins/{id}", tag = "admin", request_body = UpdateAdminRequest, responses((status = 200, description = "更新管理员（AdminUser）")))]
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
#[utoipa::path(delete, path = "/api/v1/admin/admins/{id}", tag = "admin", responses((status = 200, description = "ok")))]
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
#[utoipa::path(post, path = "/api/v1/admin/admins/{id}/reset-totp", tag = "admin", responses((status = 200, description = "ok")))]
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

#[derive(Debug, serde::Deserialize, utoipa::IntoParams)]
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

#[utoipa::path(get, path = "/api/v1/admin/audit", tag = "admin", params(AuditQuery), responses((status = 200, description = "审计检索（超管），Paged<AuditLog>")))]
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
#[utoipa::path(get, path = "/api/v1/admin/audit/export", tag = "admin", params(AuditQuery), responses((status = 200, description = "审计导出（CSV 文件）")))]
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

#[utoipa::path(get, path = "/api/v1/admin/settings", tag = "admin", responses((status = 200, description = "系统设置（超管），SystemSetting[]")))]
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

#[utoipa::path(patch, path = "/api/v1/admin/settings", tag = "admin", request_body = UpdateSettingsRequest, responses((status = 200, description = "更新系统设置（超管，仅 §3.2 的键），SystemSetting[]")))]
pub async fn update_settings(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    headers: HeaderMap,
    Json(req): Json<UpdateSettingsRequest>,
) -> ApiResult<Vec<SystemSetting>> {
    require(&admin, &[AdminRole::SuperAdmin], "update settings")?;
    // m-openapi：typed DTO 只含已知键 + flatten 的未知键；先把 DTO 还原
    // 成 JSON 对象，再走与原来完全相同的逐键校验/写库流程（未知键仍 422）。
    let body = serde_json::to_value(&req)
        .map_err(|_| ApiError::new(ErrorCode::ValidationFailed, "invalid settings body"))?;
    let obj = body
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
        Some(body),
        ip_of(&headers).as_deref(),
    )
    .await;
    let settings = state.store.list_settings().await.map_err(ApiError::from)?;
    ok(settings)
}

// ---------------------------------------------------------------------------
// m22：文案覆盖（§12.8 `i18n_override`）
// ---------------------------------------------------------------------------

/// 校验覆盖的 locale（须为支持语言）与 key（点分 FTL key 形）。
fn validate_override(locale: &str, key: &str, value: &str) -> Result<String, ApiError> {
    let locale = iptv_i18n::normalize_locale(locale).unwrap_or_else(|| locale.to_string());
    let supported = iptv_i18n::I18n::supported_locales();
    if !supported.contains(&locale.as_str()) && locale != "en-XA" {
        return Err(ApiError::new(
            ErrorCode::ValidationFailed,
            format!("unsupported locale: {locale}"),
        ));
    }
    if key.is_empty()
        || !key.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        || !key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
    {
        return Err(ApiError::new(
            ErrorCode::ValidationFailed,
            format!("invalid i18n key: {key}"),
        ));
    }
    if value.is_empty() || value.chars().count() > 4096 {
        return Err(ApiError::new(
            ErrorCode::ValidationFailed,
            "override value must be non-empty and at most 4096 chars",
        ));
    }
    Ok(locale)
}

/// 读库刷新本进程覆盖表（写覆盖后调用；失败只记日志，不让写操作失败，
/// 下次启动仍会从库重装）。
async fn refresh_overrides(state: &Arc<AppState>) {
    match state.store.i18n_overrides_map().await {
        Ok(map) => iptv_i18n::I18n::set_overrides(map),
        Err(e) => tracing::warn!("刷新 i18n 覆盖失败：{e}"),
    }
}

/// m22：文案覆盖列表（任意管理员可读，与 `list_settings` 同权限）。
#[utoipa::path(get, path = "/api/v1/admin/i18n-overrides", tag = "admin", responses((status = 200, description = "文案覆盖列表，I18nOverride[]")))]
pub async fn list_i18n_overrides(
    State(state): State<Arc<AppState>>,
    _admin: AdminAuth,
) -> ApiResult<Vec<iptv_dto::I18nOverride>> {
    let list = state
        .store
        .list_i18n_overrides()
        .await
        .map_err(ApiError::from)?;
    ok(list)
}

/// m22：新增/修改单条覆盖（仅超管；空串不许覆盖，用 DELETE 删覆盖）。
#[utoipa::path(put, path = "/api/v1/admin/i18n-overrides", tag = "admin", request_body = iptv_dto::UpsertI18nOverrideRequest, responses((status = 200, description = "新增/改写文案覆盖，I18nOverride[]")))]
pub async fn upsert_i18n_override(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    headers: HeaderMap,
    Json(req): Json<iptv_dto::UpsertI18nOverrideRequest>,
) -> ApiResult<Vec<iptv_dto::I18nOverride>> {
    require(&admin, &[AdminRole::SuperAdmin], "update i18n overrides")?;
    let locale = validate_override(&req.locale, &req.key, &req.value)?;
    state
        .store
        .upsert_i18n_override(&locale, &req.key, &req.value)
        .await
        .map_err(ApiError::from)?;
    refresh_overrides(&state).await;
    audit(
        &state,
        &admin,
        "i18n_override.upsert",
        "i18n_override",
        &format!("{locale}:{}", req.key),
        None,
        Some(serde_json::json!({"value": req.value})),
        ip_of(&headers).as_deref(),
    )
    .await;
    let list = state
        .store
        .list_i18n_overrides()
        .await
        .map_err(ApiError::from)?;
    ok(list)
}

#[derive(Debug, serde::Deserialize, utoipa::IntoParams)]
pub struct I18nOverrideQuery {
    pub locale: String,
    pub key: String,
}

/// m22：删除单条覆盖（仅超管；不存在也返回当前列表，不报 404，
/// 与“覆盖不存在即无覆盖”语义一致）。
#[utoipa::path(delete, path = "/api/v1/admin/i18n-overrides", tag = "admin", params(I18nOverrideQuery), responses((status = 200, description = "删除文案覆盖，I18nOverride[]")))]
pub async fn delete_i18n_override(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    headers: HeaderMap,
    Query(q): Query<I18nOverrideQuery>,
) -> ApiResult<Vec<iptv_dto::I18nOverride>> {
    require(&admin, &[AdminRole::SuperAdmin], "delete i18n overrides")?;
    let locale = iptv_i18n::normalize_locale(&q.locale).unwrap_or(q.locale.clone());
    state
        .store
        .delete_i18n_override(&locale, &q.key)
        .await
        .map_err(ApiError::from)?;
    refresh_overrides(&state).await;
    audit(
        &state,
        &admin,
        "i18n_override.delete",
        "i18n_override",
        &format!("{locale}:{}", q.key),
        None,
        None,
        ip_of(&headers).as_deref(),
    )
    .await;
    let list = state
        .store
        .list_i18n_overrides()
        .await
        .map_err(ApiError::from)?;
    ok(list)
}
