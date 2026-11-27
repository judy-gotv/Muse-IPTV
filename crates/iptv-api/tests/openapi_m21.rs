//! m21 回归：OpenAPI 文档由 utoipa 从代码生成（不再是静态文件）。
//! 断言：版本号取 Cargo 包版本（0.1.5，不再是过期静态文件的 0.1.0）；
//! 核心路由全在文档里；参数无重复（显式 params + 宏自动探测不得叠加）。
//! 直接调 `ApiDoc::openapi()` 与走 HTTP 路由 `/api/v1/openapi.json` 双通道验证。
// 作者: Passwd Word

use axum::body::Body;
use axum::http::{Request, StatusCode};
use iptv_api::{ApiDoc, AppState, Config};
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tower::ServiceExt;
use utoipa::OpenApi as _;

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
    let path = std::env::temp_dir().join(format!("iptv-api-openapi-{}.db", n));
    let config = Config {
        database_url: format!("sqlite://{}?mode=rwc", path.display()),
        redis_url: None,
        play_secret: "m21testvalue0123456789abcdef".to_string(),
        admin_secret: "m21testvaluefedcba9876543210".to_string(),
        portal_base_url: "http://127.0.0.1:8081".to_string(),
        bind_addr: "127.0.0.1:0".to_string(),
        cors_origins: vec![],
    };
    let state = Arc::new(AppState::new(config).await.expect("AppState::new"));
    Fixture { state, path }
}

/// 核心路由：登录/管理员/用户/Token/频道/订阅源/EPG/套餐/分组/播放/设置/健康检查
/// + v0.1.5 新增（粘贴导入/EPG 映射/上传任务）。
const CORE_PATHS: &[&str] = &[
    "/api/v1/auth/ticket-exchange",
    "/api/v1/auth/token-login",
    "/api/v1/auth/logout",
    "/api/v1/auth/logout-all",
    "/api/v1/me",
    "/api/v1/me/usage",
    "/api/v1/me/devices",
    "/api/v1/me/devices/{session_id}",
    "/api/v1/me/channels",
    "/api/v1/me/history",
    "/api/v1/me/plan",
    "/api/v1/me/settings",
    "/api/v1/playback/start",
    "/api/v1/playback/heartbeat",
    "/api/v1/playback/stop",
    "/api/v1/channels",
    "/api/v1/epg/now",
    "/api/v1/epg/channels/{channel_id}/programs",
    "/api/v1/admin/auth/login",
    "/api/v1/admin/stats/overview",
    "/api/v1/admin/users",
    "/api/v1/admin/users/{id}",
    "/api/v1/admin/tokens",
    "/api/v1/admin/tokens/{id}",
    "/api/v1/admin/tokens/{id}/login-ticket",
    "/api/v1/admin/tokens/{id}/revoke-sessions",
    "/api/v1/admin/channels",
    "/api/v1/admin/channels/{id}",
    "/api/v1/admin/sources",
    "/api/v1/admin/sources/{id}",
    "/api/v1/admin/sources/upload",
    "/api/v1/admin/sources/paste",
    "/api/v1/admin/upload-tasks/{id}",
    "/api/v1/admin/sources/{id}/sync",
    "/api/v1/admin/sources/{id}/diff",
    "/api/v1/admin/sources/{id}/logs",
    "/api/v1/admin/groups",
    "/api/v1/admin/groups/{id}",
    "/api/v1/admin/epg/sources",
    "/api/v1/admin/epg/sources/{id}",
    "/api/v1/admin/epg/sources/{id}/maps",
    "/api/v1/admin/epg/maps",
    "/api/v1/admin/epg/maps/{id}",
    "/api/v1/admin/sessions",
    "/api/v1/admin/sessions/{id}",
    "/api/v1/admin/plans",
    "/api/v1/admin/plans/{id}",
    "/api/v1/admin/admins",
    "/api/v1/admin/admins/{id}",
    "/api/v1/admin/admins/{id}/reset-totp",
    "/api/v1/admin/audit",
    "/api/v1/admin/audit/export",
    "/api/v1/admin/settings",
    "/api/v1/admin/i18n-overrides",
    "/healthz",
    "/metrics",
    "/api/v1/openapi.json",
    "/p/{channel_id}.m3u8",
    "/p/seg",
];

fn doc_json() -> Value {
    let doc = ApiDoc::openapi();
    serde_json::to_value(&doc).expect("openapi serializes")
}

/// 对一份文档做结构断言（版本 + 核心路由 + 参数无重复 + 操作都有响应）。
fn assert_doc(doc: &Value) {
    assert_eq!(
        doc["info"]["version"].as_str().unwrap(),
        env!("CARGO_PKG_VERSION"),
        "openapi version must follow the Cargo package version"
    );
    assert_eq!(doc["info"]["version"].as_str().unwrap(), "0.1.5");
    let paths = doc["paths"].as_object().expect("paths object");
    for p in CORE_PATHS {
        assert!(paths.contains_key(*p), "openapi paths missing {p}");
    }
    // 操作总数：每个 #[utoipa::path] 标注一个操作；低于此数说明有标注丢失。
    let ops: usize = paths
        .values()
        .map(|v| v.as_object().map(|o| o.len()).unwrap_or(0))
        .sum();
    assert!(
        ops >= 80,
        "expected at least 80 documented operations, got {ops}"
    );
    // 每个操作至少一个响应，且同一操作内参数名不重复
    //（显式 params + 宏自动探测叠加时会在此现形）。
    for (path, item) in paths {
        for (method, op) in item.as_object().unwrap() {
            assert!(
                op.get("responses")
                    .is_some_and(|r| r.as_object().is_some_and(|o| !o.is_empty())),
                "{method} {path} has no responses"
            );
            if let Some(params) = op.get("parameters").and_then(|p| p.as_array()) {
                let mut names: Vec<&str> = params
                    .iter()
                    .filter_map(|p| p.get("name").and_then(|n| n.as_str()))
                    .collect();
                let before = names.len();
                names.sort_unstable();
                names.dedup();
                assert_eq!(
                    names.len(),
                    before,
                    "{method} {path} has duplicate parameters"
                );
            }
        }
    }
    // 抽查：路径参数 + 查询参数各就各位。
    let programs = &paths["/api/v1/epg/channels/{channel_id}/programs"]["get"]["parameters"];
    let names: Vec<&str> = programs
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"channel_id"),
        "epg programs missing path param: {names:?}"
    );
    assert!(
        names.contains(&"from") && names.contains(&"to"),
        "epg programs missing query params: {names:?}"
    );
    let my_channels = &paths["/api/v1/me/channels"]["get"]["parameters"];
    let names: Vec<&str> = my_channels
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    for q in ["page", "page_size", "q", "group"] {
        assert!(
            names.contains(&q),
            "me/channels missing query param {q}: {names:?}"
        );
    }
    // 抽查：请求体 schema 已生成并进 components。
    let create_user = &paths["/api/v1/admin/users"]["post"]["requestBody"]["content"]
        ["application/json"]["schema"]["$ref"];
    assert_eq!(
        create_user.as_str().unwrap(),
        "#/components/schemas/CreateUserRequest"
    );
    assert!(
        doc["components"]["schemas"]
            .get("CreateUserRequest")
            .is_some(),
        "components.schemas missing CreateUserRequest"
    );
}

#[test]
fn openapi_generated_version_and_paths() {
    assert_doc(&doc_json());
}

#[tokio::test]
async fn openapi_json_route_serves_generated_doc() {
    let fx = setup().await;
    let router = AppState::router(fx.state.clone());
    let resp = router
        .oneshot(
            Request::builder()
                .uri("/api/v1/openapi.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let doc: Value = serde_json::from_slice(&bytes).expect("openapi.json parses");
    assert_doc(&doc);
}
