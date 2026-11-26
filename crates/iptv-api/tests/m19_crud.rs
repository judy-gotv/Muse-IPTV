//! M19a–f 回归：用户/Token/订阅源/套餐/分组/EPG 源各建→改→删全流程，
//! 含删除语义（删用户禁用 Token+吊销会话；删 Token 吊销会话；
//! 删被引用的套餐 409；删分组清 channel_group_map；删 EPG 源清
//! epg_channel_map；删订阅源清 sync_log）与审计记录。
//! 直接调 handler（不走 HTTP），DB 用临时文件 SQLite 库。
// 作者: Passwd Word

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::Json;
use chrono::{Duration, Utc};
use iptv_api::handlers::admin;
use iptv_api::{AdminAuth, AppState, Config};
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

fn super_admin() -> AdminAuth {
    AdminAuth {
        admin_id: 1,
        username: "root".to_string(),
        role: AdminRole::SuperAdmin,
    }
}

/// handler 返回 `Json<Envelope<T>>`，解出 data。
fn data<T>(r: Json<Envelope<T>>) -> T {
    r.0.data.unwrap()
}

async fn audit_total(state: &Arc<AppState>, action: &str) -> u64 {
    state
        .store
        .list_audit(None, Some(action), None, None, &PageQuery::default())
        .await
        .unwrap()
        .total
}

fn user_req(name: &str) -> CreateUserRequest {
    CreateUserRequest {
        display_name: name.to_string(),
        email: None,
        phone: None,
        plan_id: None,
        locale: Some("zh-Hans-CN".to_string()),
        remark: None,
        expires_at: None,
    }
}

fn token_req(user_id: i64) -> CreateTokenRequest {
    CreateTokenRequest {
        user_id,
        name: "客厅电视".to_string(),
        device_type: Some("android_tv".to_string()),
        max_concurrent_sessions: 2,
        max_bound_devices: 3,
        expires_at: Some(Utc::now() + Duration::days(365)),
        locale: Some("zh-Hans-CN".to_string()),
        allowed_groups: vec![],
        denied_channels: vec![],
        features: TokenFeatures::default(),
    }
}

#[tokio::test]
async fn m19a_delete_user_disables_tokens_and_revokes_sessions() {
    let fx = setup().await;
    let state = fx.state.clone();
    let user = data(
        admin::create_user(
            State(state.clone()),
            super_admin(),
            HeaderMap::new(),
            Json(user_req("m19a-user")),
        )
        .await
        .unwrap(),
    );
    // 改备注（UI 编辑表单的字段）。
    let user = data(
        admin::update_user(
            State(state.clone()),
            super_admin(),
            HeaderMap::new(),
            Path(user.id),
            Json(UpdateUserRequest {
                display_name: None,
                email: None,
                phone: None,
                plan_id: None,
                status: None,
                locale: None,
                theme: None,
                remark: Some("改过备注".to_string()),
                expires_at: None,
            }),
        )
        .await
        .unwrap(),
    );
    assert_eq!(user.remark.as_deref(), Some("改过备注"));
    let token = state
        .store
        .create_token(user.id, "tk_live_", &[7u8; 32], &token_req(user.id))
        .await
        .unwrap();
    state
        .store
        .create_portal_session(
            token.id,
            "dev-tv",
            &[8u8; 32],
            None,
            None,
            Utc::now() + Duration::days(7),
        )
        .await
        .unwrap();
    assert_eq!(
        state
            .store
            .list_portal_sessions(token.id)
            .await
            .unwrap()
            .len(),
        1
    );
    let del = data(
        admin::delete_user(
            State(state.clone()),
            super_admin(),
            HeaderMap::new(),
            Path(user.id),
        )
        .await
        .unwrap(),
    );
    assert_eq!(del["deleted"], true);
    assert!(state.store.get_user(user.id).await.unwrap().is_none());
    // Token 被禁用，会话被吊销。
    let tok = state.store.get_token(token.id).await.unwrap().unwrap();
    assert_eq!(tok.status, TokenStatus::Disabled);
    assert!(state
        .store
        .list_portal_sessions(token.id)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(audit_total(&state, "user.delete").await, 1);
    // 再删一次 → 404。
    assert!(admin::delete_user(
        State(state.clone()),
        super_admin(),
        HeaderMap::new(),
        Path(user.id)
    )
    .await
    .is_err());
}

#[tokio::test]
async fn m19b_token_update_then_delete_revokes_sessions() {
    let fx = setup().await;
    let state = fx.state.clone();
    let user = state
        .store
        .create_user(&user_req("m19b-user"))
        .await
        .unwrap();
    let token = state
        .store
        .create_token(user.id, "tk_live_", &[7u8; 32], &token_req(user.id))
        .await
        .unwrap();
    // 改有效期与并发数（UI 编辑表单的字段）。
    let new_exp = Utc::now() + Duration::days(30);
    let token = data(
        admin::update_token(
            State(state.clone()),
            super_admin(),
            HeaderMap::new(),
            Path(token.id),
            Json(UpdateTokenRequest {
                name: None,
                device_type: None,
                max_concurrent_sessions: Some(9),
                max_bound_devices: None,
                status: None,
                expires_at: Some(new_exp),
                locale: None,
                allowed_groups: None,
                denied_channels: None,
                features: None,
            }),
        )
        .await
        .unwrap(),
    );
    assert_eq!(token.max_concurrent_sessions, 9);
    // 库里 expires_at 只存整秒，与传入值按秒比较。
    assert_eq!(
        token.expires_at.map(|t| t.timestamp()),
        Some(new_exp.timestamp())
    );
    state
        .store
        .create_portal_session(
            token.id,
            "dev-tv",
            &[8u8; 32],
            None,
            None,
            Utc::now() + Duration::days(7),
        )
        .await
        .unwrap();
    let del = data(
        admin::delete_token(
            State(state.clone()),
            super_admin(),
            HeaderMap::new(),
            Path(token.id),
        )
        .await
        .unwrap(),
    );
    assert_eq!(del["deleted"], true);
    assert!(state.store.get_token(token.id).await.unwrap().is_none());
    assert!(state
        .store
        .list_portal_sessions(token.id)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(audit_total(&state, "token.delete").await, 1);
}

#[tokio::test]
async fn m19c_source_update_then_delete_cleans_sync_logs() {
    let fx = setup().await;
    let state = fx.state.clone();
    let source = data(
        admin::create_source(
            State(state.clone()),
            super_admin(),
            HeaderMap::new(),
            Json(CreateSourceRequest {
                name: "m19c-src".to_string(),
                url: Some("https://example.com/a.m3u".to_string()),
                sync_interval_sec: Some(3600),
                http_headers: None,
                epg_url: None,
                on_missing: None,
                auto_enable_new: None,
                enabled: None,
            }),
        )
        .await
        .unwrap(),
    );
    let source = data(
        admin::update_source(
            State(state.clone()),
            super_admin(),
            HeaderMap::new(),
            Path(source.id),
            Json(UpdateSourceRequest {
                name: None,
                url: Some("https://example.com/b.m3u".to_string()),
                sync_interval_sec: Some(7200),
                http_headers: None,
                epg_url: Some("https://example.com/epg.xml".to_string()),
                on_missing: Some(OnMissing::Delete),
                auto_enable_new: None,
                enabled: None,
            }),
        )
        .await
        .unwrap(),
    );
    assert_eq!(source.url.as_deref(), Some("https://example.com/b.m3u"));
    assert_eq!(source.sync_interval_sec, 7200);
    assert_eq!(source.on_missing, OnMissing::Delete);
    // 留一条同步日志，删源后应一并清理。
    state.store.create_sync_log(source.id).await.unwrap();
    assert_eq!(
        state
            .store
            .list_sync_logs(source.id, &PageQuery::default())
            .await
            .unwrap()
            .total,
        1
    );
    let del = data(
        admin::delete_source(
            State(state.clone()),
            super_admin(),
            HeaderMap::new(),
            Path(source.id),
        )
        .await
        .unwrap(),
    );
    assert_eq!(del["deleted"], true);
    assert!(state.store.get_source(source.id).await.unwrap().is_none());
    assert_eq!(
        state
            .store
            .list_sync_logs(source.id, &PageQuery::default())
            .await
            .unwrap()
            .total,
        0
    );
    assert_eq!(audit_total(&state, "source.delete").await, 1);
}

#[tokio::test]
async fn m19d_plan_in_use_conflicts_then_deletes_after_user_gone() {
    let fx = setup().await;
    let state = fx.state.clone();
    let plan = data(
        admin::create_plan(
            State(state.clone()),
            super_admin(),
            HeaderMap::new(),
            Json(CreatePlanRequest {
                name: "m19d-plan".to_string(),
                max_concurrent_sessions: 2,
                max_bound_devices: 3,
                channel_groups: vec![],
                duration_days: 30,
                price_cents: 9900,
                currency: Some("CNY".to_string()),
            }),
        )
        .await
        .unwrap(),
    );
    let plan = data(
        admin::update_plan(
            State(state.clone()),
            super_admin(),
            HeaderMap::new(),
            Path(plan.id),
            Json(UpdatePlanRequest {
                name: None,
                max_concurrent_sessions: None,
                max_bound_devices: None,
                channel_groups: None,
                duration_days: None,
                price_cents: Some(19900),
                currency: None,
            }),
        )
        .await
        .unwrap(),
    );
    assert_eq!(plan.price_cents, 19900);
    let mut req = user_req("m19d-user");
    req.plan_id = Some(plan.id);
    let user = state.store.create_user(&req).await.unwrap();
    // 仍有用户引用 → 409。
    let err = admin::delete_plan(
        State(state.clone()),
        super_admin(),
        HeaderMap::new(),
        Path(plan.id),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);
    assert_eq!(err.code.http_status(), 409);
    // 用户删掉后即可删套餐。
    let _ = admin::delete_user(
        State(state.clone()),
        super_admin(),
        HeaderMap::new(),
        Path(user.id),
    )
    .await
    .unwrap();
    let del = data(
        admin::delete_plan(
            State(state.clone()),
            super_admin(),
            HeaderMap::new(),
            Path(plan.id),
        )
        .await
        .unwrap(),
    );
    assert_eq!(del["deleted"], true);
    assert!(state.store.get_plan(plan.id).await.unwrap().is_none());
    assert_eq!(audit_total(&state, "plan.delete").await, 1);
}

#[tokio::test]
async fn m19e_group_update_then_delete_cleans_map() {
    let fx = setup().await;
    let state = fx.state.clone();
    let group = data(
        admin::create_group(
            State(state.clone()),
            super_admin(),
            HeaderMap::new(),
            Json(CreateGroupRequest {
                name: "m19e-group".to_string(),
                sort_weight: Some(1),
                icon: None,
                is_default: None,
            }),
        )
        .await
        .unwrap(),
    );
    let group = data(
        admin::update_group(
            State(state.clone()),
            super_admin(),
            HeaderMap::new(),
            Path(group.id),
            Json(UpdateGroupRequest {
                name: Some("m19e-group-renamed".to_string()),
                sort_weight: Some(5),
                icon: None,
                is_default: None,
            }),
        )
        .await
        .unwrap(),
    );
    assert_eq!(group.name, "m19e-group-renamed");
    assert_eq!(group.sort_weight, 5);
    // 模拟一条频道-分组映射（同步/导入时写入），删分组后应清理。
    sqlx::query("INSERT INTO channel_group_map (channel_id, group_id) VALUES (?, ?)")
        .bind(999001i64)
        .bind(group.id)
        .execute(state.store.pool())
        .await
        .unwrap();
    let del = data(
        admin::delete_group(
            State(state.clone()),
            super_admin(),
            HeaderMap::new(),
            Path(group.id),
        )
        .await
        .unwrap(),
    );
    assert_eq!(del["deleted"], true);
    assert!(state.store.get_group(group.id).await.unwrap().is_none());
    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM channel_group_map WHERE group_id = ?")
        .bind(group.id)
        .fetch_one(state.store.pool())
        .await
        .unwrap();
    assert_eq!(left, 0);
    assert_eq!(audit_total(&state, "group.delete").await, 1);
}

#[tokio::test]
async fn m19f_epg_source_update_then_delete_cleans_map() {
    let fx = setup().await;
    let state = fx.state.clone();
    let source = data(
        admin::create_epg_source(
            State(state.clone()),
            super_admin(),
            HeaderMap::new(),
            Json(serde_json::json!({"name": "m19f-epg", "url": "https://example.com/epg.xml"})),
        )
        .await
        .unwrap(),
    );
    let source = data(
        admin::update_epg_source(
            State(state.clone()),
            super_admin(),
            HeaderMap::new(),
            Path(source.id),
            Json(UpdateEpgSourceRequest {
                name: None,
                url: Some("https://example.com/epg2.xml".to_string()),
            }),
        )
        .await
        .unwrap(),
    );
    assert_eq!(source.url, "https://example.com/epg2.xml");
    // 模拟一条节目映射（EPG 同步时写入），删源后应清理。
    sqlx::query("INSERT INTO epg_channel_map (epg_source_id, tvg_id) VALUES (?, ?)")
        .bind(source.id)
        .bind("CCTV1")
        .execute(state.store.pool())
        .await
        .unwrap();
    let del = data(
        admin::delete_epg_source(
            State(state.clone()),
            super_admin(),
            HeaderMap::new(),
            Path(source.id),
        )
        .await
        .unwrap(),
    );
    assert_eq!(del["deleted"], true);
    assert!(state
        .store
        .get_epg_source(source.id)
        .await
        .unwrap()
        .is_none());
    let left: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM epg_channel_map WHERE epg_source_id = ?")
            .bind(source.id)
            .fetch_one(state.store.pool())
            .await
            .unwrap();
    assert_eq!(left, 0);
    assert_eq!(audit_total(&state, "epg_source.delete").await, 1);
}
