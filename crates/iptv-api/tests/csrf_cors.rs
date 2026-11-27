//! m14 回归：CSRF 双提交中间件 + CORS 允许源列表。
//!
//! 不经过 DB/业务 handler：用最小路由 + 真实中间件/CORS 层跑 oneshot，
//! 只验防线本身（缺 token 拒、token 一致放行、token 不一致拒、
//! CORS 命中放行/未命中拒、默认同源行为）。
// 作者: Passwd Word

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::routing::{get, post};
use axum::Router;
use iptv_api::{build_cors_layer, csrf_middleware};
use tower::ServiceExt;

fn app(origins: Vec<String>) -> Router {
    Router::new()
        .route("/api/v1/ping", post(|| async { "ok" }))
        .route("/api/v1/ping", get(|| async { "ok" }))
        .layer(axum::middleware::from_fn(csrf_middleware))
        .layer(build_cors_layer(&origins))
}

fn req(method: Method, headers: &[(&str, &str)]) -> Request<Body> {
    let mut b = Request::builder().method(method).uri("/api/v1/ping");
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    b.body(Body::empty()).unwrap()
}

#[tokio::test]
async fn server_to_server_post_without_origin_passes() {
    let res = app(vec![]).oneshot(req(Method::POST, &[])).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn origin_mismatch_without_token_rejected() {
    let res = app(vec![])
        .oneshot(req(
            Method::POST,
            &[
                ("cookie", "iptv_portal_sid=abc"),
                ("origin", "https://evil.example.com"),
                ("host", "127.0.0.1:8081"),
            ],
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn double_submit_match_passes_even_with_mismatched_origin() {
    let res = app(vec![])
        .oneshot(req(
            Method::POST,
            &[
                ("cookie", "iptv_portal_sid=abc; iptv_csrf=tok123"),
                ("x-csrf-token", "tok123"),
                ("origin", "https://evil.example.com"),
            ],
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn double_submit_mismatch_rejected() {
    let res = app(vec![])
        .oneshot(req(
            Method::POST,
            &[
                ("cookie", "iptv_portal_sid=abc; iptv_csrf=tok123"),
                ("x-csrf-token", "nope"),
                ("origin", "http://127.0.0.1:8081"),
            ],
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn sec_fetch_site_cross_site_rejected_without_origin() {
    let res = app(vec![])
        .oneshot(req(
            Method::POST,
            &[
                ("cookie", "iptv_portal_sid=abc"),
                ("sec-fetch-site", "cross-site"),
            ],
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn cors_default_same_origin_only() {
    // 默认空列表：跨域预检收不到 Access-Control-Allow-Origin。
    let res = app(vec![])
        .oneshot(req(
            Method::OPTIONS,
            &[
                ("origin", "https://evil.example.com"),
                ("access-control-request-method", "POST"),
            ],
        ))
        .await
        .unwrap();
    assert!(res.headers().get("access-control-allow-origin").is_none());
}

#[tokio::test]
async fn cors_allowed_origin_gets_acao() {
    let res = app(vec!["https://app.example.com".to_string()])
        .oneshot(req(
            Method::OPTIONS,
            &[
                ("origin", "https://app.example.com"),
                ("access-control-request-method", "POST"),
            ],
        ))
        .await
        .unwrap();
    assert_eq!(
        res.headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("https://app.example.com")
    );
}

#[tokio::test]
async fn cors_unlisted_origin_gets_no_acao() {
    let res = app(vec!["https://app.example.com".to_string()])
        .oneshot(req(
            Method::OPTIONS,
            &[
                ("origin", "https://evil.example.com"),
                ("access-control-request-method", "POST"),
            ],
        ))
        .await
        .unwrap();
    assert!(res.headers().get("access-control-allow-origin").is_none());
}

#[tokio::test]
async fn cors_allow_does_not_imply_csrf_allow() {
    // CORS 配了源也不等于 CSRF 放行：写请求无 token 且 Origin 与 Host
    // 失配，仍 403。
    let res = app(vec!["https://app.example.com".to_string()])
        .oneshot(req(
            Method::POST,
            &[
                ("cookie", "iptv_portal_sid=abc"),
                ("origin", "https://app.example.com"),
                ("host", "127.0.0.1:8081"),
            ],
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}
