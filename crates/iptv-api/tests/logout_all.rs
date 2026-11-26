//! M14 回归：POST /api/v1/auth/logout-all 一次吊销该 Token 的全部会话，
//! 含当前会话；写一条 `auth.logout_all` 审计。直接调 handler（不走 HTTP），
//! DB 用临时文件 SQLite 库。
// 作者: Passwd Word

use axum::extract::State;
use axum::http::HeaderMap;
use chrono::{Duration, Utc};
use iptv_api::handlers::auth;
use iptv_api::{AppState, Config, PortalAuth};
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

async fn audit_total(state: &Arc<AppState>, action: &str) -> u64 {
    state
        .store
        .list_audit(None, Some(action), None, None, &PageQuery::default())
        .await
        .unwrap()
        .total
}

#[tokio::test]
async fn logout_all_revokes_every_session_including_current() {
    let fx = setup().await;
    let state = fx.state.clone();
    let user = state
        .store
        .create_user(&CreateUserRequest {
            display_name: "multi-device".to_string(),
            email: None,
            phone: None,
            plan_id: None,
            locale: Some("zh-Hans-CN".to_string()),
            remark: None,
            expires_at: None,
        })
        .await
        .unwrap();
    let token = state
        .store
        .create_token(
            user.id,
            "tk_live_",
            &[7u8; 32],
            &CreateTokenRequest {
                user_id: user.id,
                name: "客厅电视".to_string(),
                device_type: Some("android_tv".to_string()),
                max_concurrent_sessions: 5,
                max_bound_devices: 5,
                expires_at: Some(Utc::now() + Duration::days(365)),
                locale: Some("zh-Hans-CN".to_string()),
                allowed_groups: vec![],
                denied_channels: vec![],
                features: TokenFeatures::default(),
            },
        )
        .await
        .unwrap();
    // 三台设备各一个会话（模拟多设备登录）。
    let exp = Utc::now() + Duration::days(7);
    let mut sessions = vec![];
    for (i, dev) in ["dev-tv", "dev-phone", "dev-pad"].iter().enumerate() {
        let ps = state
            .store
            .create_portal_session(token.id, dev, &[(10 + i) as u8; 32], None, None, exp)
            .await
            .unwrap();
        sessions.push(ps);
    }
    assert_eq!(
        state
            .store
            .list_portal_sessions(token.id)
            .await
            .unwrap()
            .len(),
        3
    );
    // 以其中一个会话（当前设备）的身份调 logout-all。
    let current = &sessions[1];
    let auth = PortalAuth {
        token_id: token.id,
        user_id: user.id,
        session_id: current.id,
        device_id: "dev-phone".to_string(),
        locale: "zh-Hans-CN".to_string(),
    };
    let resp = auth::logout_all(State(state.clone()), auth, HeaderMap::new())
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    // 全部会话失效，含当前会话（旧行为传 Some(session_id) 会漏掉当前会话）。
    assert!(state
        .store
        .list_portal_sessions(token.id)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(audit_total(&state, "auth.logout_all").await, 1);
}
