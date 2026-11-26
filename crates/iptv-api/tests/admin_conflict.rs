//! M10/M20/m26 回归：四处唯一冲突第二次创建 → 40900（HTTP 409）；
//! 4 处补审计后审计表里有记录；客服看 Token 列表脱敏。
//! 直接调 handler（不走 HTTP），DB 用临时文件 SQLite 库。
// 作者: Passwd Word

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::Json;
use chrono::{Duration, Utc};
use iptv_api::handlers::admin;
use iptv_api::{AdminAuth, ApiError, AppState, Config};
use iptv_dto::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    state: Arc<AppState>,
    path: std::path::PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

async fn setup() -> Fixture {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let path = std::env::temp_dir().join(format!("iptv-api-test-{}-{}.db", std::process::id(), n));
    let config = Config {
        database_url: format!("sqlite://{}?mode=rwc", path.display()),
        redis_url: None,
        play_secret: "test-play-secret".to_string(),
        admin_secret: "test-admin-secret".to_string(),
        portal_base_url: "http://127.0.0.1:8081".to_string(),
        bind_addr: "127.0.0.1:0".to_string(),
    };
    let state = Arc::new(AppState::new(config).await.expect("AppState::new"));
    Fixture { state, path }
}

fn super_admin(id: i64) -> AdminAuth {
    AdminAuth {
        admin_id: id,
        username: "root".to_string(),
        role: AdminRole::SuperAdmin,
    }
}

fn role_auth(id: i64, role: AdminRole) -> AdminAuth {
    AdminAuth {
        admin_id: id,
        username: "viewer".to_string(),
        role,
    }
}

async fn seed_root(fx: &Fixture) -> i64 {
    fx.state
        .store
        .create_admin("root", "argon2:hash", AdminRole::SuperAdmin, false)
        .await
        .unwrap()
        .id
}

fn token_req(user_id: i64) -> CreateTokenRequest {
    CreateTokenRequest {
        user_id,
        name: "客厅电视".to_string(),
        device_type: Some("android_tv".to_string()),
        max_concurrent_sessions: 2,
        max_bound_devices: 3,
        expires_at: Some(Utc::now() + Duration::days(365)),
        locale: Some("zh-Hant-TW".to_string()),
        allowed_groups: vec![],
        denied_channels: vec![],
        features: TokenFeatures::default(),
    }
}

async fn audit_total(state: &Arc<AppState>, action: &str) -> u64 {
    state
        .store
        .list_audit(None, Some(action), None, None, &PageQuery::default())
        .await
        .unwrap()
        .total
}

#[tokio::test]
async fn duplicate_admin_channel_plan_group_return_409() {
    // M10：四处第二次同名创建一律 40900（HTTP 409），不再是 50000。
    let fx = setup().await;
    let state = fx.state.clone();
    let root_id = seed_root(&fx).await;

    // 建管理员。
    let req = CreateAdminRequest {
        username: "dup-admin".to_string(),
        password: "password123".to_string(),
        role: AdminRole::Operator,
        totp_required: false,
    };
    let _ = admin::create_admin(
        State(state.clone()),
        super_admin(root_id),
        HeaderMap::new(),
        Json(req.clone()),
    )
    .await
    .unwrap();
    let err = admin::create_admin(
        State(state.clone()),
        super_admin(root_id),
        HeaderMap::new(),
        Json(req),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);
    assert_eq!(err.code.http_status(), 409);

    // 建频道（同 tvg_id）。
    let ch_req = CreateChannelRequest {
        tvg_id: Some("dup-tvg-1".to_string()),
        name: "Dup".to_string(),
        display_name: None,
        logo_url: None,
        group_title: None,
        language: None,
        country: None,
        play_url: "https://x.example/dup.m3u8".to_string(),
        backup_url: None,
        protocol: None,
        sort_weight: None,
        visibility: None,
    };
    let _ = admin::create_channel(
        State(state.clone()),
        super_admin(root_id),
        HeaderMap::new(),
        Json(ch_req.clone()),
    )
    .await
    .unwrap();
    let err = admin::create_channel(
        State(state.clone()),
        super_admin(root_id),
        HeaderMap::new(),
        Json(ch_req),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);
    assert_eq!(err.code.http_status(), 409);

    // 建套餐（同名）。
    let plan_req = CreatePlanRequest {
        name: "dup-plan".to_string(),
        max_concurrent_sessions: 2,
        max_bound_devices: 2,
        channel_groups: vec![],
        duration_days: 30,
        price_cents: 0,
        currency: None,
    };
    let _ = admin::create_plan(
        State(state.clone()),
        super_admin(root_id),
        HeaderMap::new(),
        Json(plan_req.clone()),
    )
    .await
    .unwrap();
    let err = admin::create_plan(
        State(state.clone()),
        super_admin(root_id),
        HeaderMap::new(),
        Json(plan_req),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);

    // 建分组（同名）。
    let group_req = CreateGroupRequest {
        name: "dup-group".to_string(),
        sort_weight: None,
        icon: None,
        is_default: None,
    };
    let _ = admin::create_group(
        State(state.clone()),
        super_admin(root_id),
        HeaderMap::new(),
        Json(group_req.clone()),
    )
    .await
    .unwrap();
    let err = admin::create_group(
        State(state.clone()),
        super_admin(root_id),
        HeaderMap::new(),
        Json(group_req),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);

    // 40900 有本地化文案（六语 errors.ftl 均已补）。
    assert_eq!(
        state.i18n.error_message("zh-Hans-CN", 40900),
        "资源已存在，名称已被占用"
    );
    assert_eq!(
        state.i18n.error_message("en", 40900),
        "Resource already exists; the name is already taken"
    );
}

#[tokio::test]
async fn store_unique_violation_maps_to_conflict() {
    // M10 兜底：绕过 handler 预查、直接撞 DB 唯一约束的 anyhow 错误，
    // 经 From<anyhow::Error> 必须映射为 Conflict（409），而非 Internal。
    let fx = setup().await;
    let state = fx.state.clone();
    state
        .store
        .create_admin("dup-admin-2", "argon2:hash", AdminRole::Operator, false)
        .await
        .unwrap();
    let err = state
        .store
        .create_admin("dup-admin-2", "argon2:hash", AdminRole::Operator, false)
        .await
        .unwrap_err();
    assert!(iptv_store::is_unique_violation_error(&err));
    let api_err = ApiError::from(err);
    assert_eq!(api_err.code, ErrorCode::Conflict);
    assert_eq!(api_err.code.http_status(), 409);
}

#[tokio::test]
async fn four_missing_audits_are_written() {
    // M20：create_login_ticket / revoke_token_sessions /
    // create_epg_source / kick_session 四处调用后审计表里有记录。
    let fx = setup().await;
    let state = fx.state.clone();
    let root_id = seed_root(&fx).await;

    let user = state
        .store
        .create_user(&CreateUserRequest {
            display_name: "alice".to_string(),
            email: None,
            phone: None,
            plan_id: None,
            locale: None,
            remark: None,
            expires_at: None,
        })
        .await
        .unwrap();
    let token = state
        .store
        .create_token(user.id, "tk_live_", &[7u8; 32], &token_req(user.id))
        .await
        .unwrap();
    let ch = state
        .store
        .create_channel(&CreateChannelRequest {
            tvg_id: Some("cctv1.cn".to_string()),
            name: "CCTV-1".to_string(),
            display_name: None,
            logo_url: None,
            group_title: None,
            language: None,
            country: None,
            play_url: "https://x.example/cctv1.m3u8".to_string(),
            backup_url: None,
            protocol: None,
            sort_weight: None,
            visibility: None,
        })
        .await
        .unwrap();
    state
        .store
        .create_playback_session(token.id, ch.id, "dev-1", "sess-uid-1", None, None)
        .await
        .unwrap();

    let _ = admin::create_login_ticket(
        State(state.clone()),
        super_admin(root_id),
        HeaderMap::new(),
        axum::extract::Path(token.id),
    )
    .await
    .unwrap();
    assert_eq!(audit_total(&state, "token.login_ticket").await, 1);

    let _ = admin::revoke_token_sessions(
        State(state.clone()),
        super_admin(root_id),
        HeaderMap::new(),
        axum::extract::Path(token.id),
    )
    .await
    .unwrap();
    assert_eq!(audit_total(&state, "token.revoke_sessions").await, 1);

    let _ = admin::create_epg_source(
        State(state.clone()),
        super_admin(root_id),
        HeaderMap::new(),
        Json(serde_json::json!({"name": "测试EPG", "url": "https://example.com/epg.xml"})),
    )
    .await
    .unwrap();
    assert_eq!(audit_total(&state, "epg_source.create").await, 1);

    // 踢会话：先重建一条活跃播放会话（上一条已被 revoke 流程结束）。
    let ps = state
        .store
        .create_playback_session(token.id, ch.id, "dev-1", "sess-uid-2", None, None)
        .await
        .unwrap();
    let _ = admin::kick_session(
        State(state.clone()),
        super_admin(root_id),
        HeaderMap::new(),
        axum::extract::Path(ps.id),
    )
    .await
    .unwrap();
    assert_eq!(audit_total(&state, "session.kick").await, 1);
}

#[tokio::test]
async fn support_role_sees_masked_token_prefix() {
    // m26：客服（Support）看 Token 列表与只读角色同等脱敏。
    let fx = setup().await;
    let state = fx.state.clone();
    let root_id = seed_root(&fx).await;
    let user = state
        .store
        .create_user(&CreateUserRequest {
            display_name: "bob".to_string(),
            email: None,
            phone: None,
            plan_id: None,
            locale: None,
            remark: None,
            expires_at: None,
        })
        .await
        .unwrap();
    state
        .store
        .create_token(user.id, "tk_live_", &[9u8; 32], &token_req(user.id))
        .await
        .unwrap();

    let page = admin::list_tokens(
        State(state.clone()),
        role_auth(root_id, AdminRole::Support),
        Query(PageQuery::default()),
    )
    .await
    .unwrap();
    let items = &page.0.data.as_ref().unwrap().items;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].token_prefix, "tk_l****");

    let page = admin::list_tokens(
        State(state.clone()),
        role_auth(root_id, AdminRole::Readonly),
        Query(PageQuery::default()),
    )
    .await
    .unwrap();
    assert_eq!(
        page.0.data.as_ref().unwrap().items[0].token_prefix,
        "tk_l****"
    );

    // 对照：超管看完整前缀。
    let page = admin::list_tokens(
        State(state.clone()),
        super_admin(root_id),
        Query(PageQuery::default()),
    )
    .await
    .unwrap();
    assert_eq!(
        page.0.data.as_ref().unwrap().items[0].token_prefix,
        "tk_live_"
    );
}
