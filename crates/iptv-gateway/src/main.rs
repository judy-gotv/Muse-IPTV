//! 单端口网关（Muse-IPTV）。
//!
//! 对外只暴露一个端口（默认 `:8081`），按路径把请求分发给三个内部服务：
//! - `/admin` 与 `/admin/*` → 管理后台（后台路由本身就带 `/admin` 前缀，
//!   转发时**不剥离**，页面内所有链接与 POST 路径原样可用）
//! - `/api/*`、`/p/*`、`/healthz`、`/metrics` → API
//! - 其余（`/`、`/t/{ticket}`、`/me`、`/channels`、`/watch/*`、`/design.css`…）→ 门户
//!
//! 转发是流式的：请求/响应 body 直接透传不缓冲整包，`/p/*` 视频分片与
//! M3U multipart 上传都能过；`Range` / `Cookie` / `Set-Cookie` 原样透传；
//! `X-Forwarded-For` 写成直连客户端 IP（API 的 `client_ip` 取首位，
//! 限流与播放 IP 统计才能看到真实 IP）。注意：客户端自带的 XFF 不可信
//! （伪造首项可绕过按 IP 限流），网关默认**覆盖**为直连对端 IP；只有在
//! 网关前面还有一层自己可信的反代（nginx/Caddy 已写好 XFF）时，才设
//! `GATEWAY_TRUST_XFF=1` 改为追加。
//!
//! 环境变量：
//! - `GATEWAY_BIND_ADDR`（默认 `0.0.0.0:8081`）：对外监听地址
//! - `GATEWAY_TRUST_XFF`（默认 `0`）：为 `1` 时信任入站 XFF 并追加对端 IP
//! - `IPTV_API_BASE`（默认 `http://127.0.0.1:8080`）
//! - `IPTV_PORTAL_BASE`（默认 `http://127.0.0.1:18081`）
//! - `IPTV_ADMIN_BASE`（默认 `http://127.0.0.1:18082`）
// 作者: Passwd Word

use axum::{
    body::Body,
    extract::{ConnectInfo, Request, State},
    http::{header, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
    Router,
};
use std::{net::SocketAddr, sync::Arc};

/// 逐跳头：不透传，由 reqwest / axum 各自重新生成。
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Upstream {
    Api,
    Portal,
    Admin,
}

/// 按请求路径决定分流目标（纯函数，见文末测试）。
/// 注意边界：`/adminx`、`/apiary`、`/healthz2` 都不是后台/API 路径，
/// 必须落到门户；`/p`、`/api` 这类无尾斜杠的裸前缀也归 API。
fn route_for(path: &str) -> Upstream {
    if path == "/admin" || path.starts_with("/admin/") {
        Upstream::Admin
    } else if path == "/api"
        || path.starts_with("/api/")
        || path == "/p"
        || path.starts_with("/p/")
        || path == "/healthz"
        || path == "/metrics"
    {
        Upstream::Api
    } else {
        Upstream::Portal
    }
}

struct Gate {
    client: reqwest::Client,
    api: String,
    portal: String,
    admin: String,
    /// 是否信任入站 XFF（网关前有可信反代时才开，默认 false：直接覆盖）。
    trust_xff: bool,
}

impl Gate {
    fn base(&self, target: Upstream) -> &str {
        match target {
            Upstream::Api => &self.api,
            Upstream::Portal => &self.portal,
            Upstream::Admin => &self.admin,
        }
    }
}

fn is_hop_by_hop(name: &str) -> bool {
    HOP_BY_HOP.contains(&name)
}

/// 把一个上游响应原样流回客户端（只去掉逐跳头）。
fn upstream_response(res: reqwest::Response) -> Response {
    let status = res.status();
    let headers = res.headers().clone();
    let stream = res.bytes_stream();
    let mut out = Response::builder().status(status);
    for (name, value) in headers.iter() {
        if is_hop_by_hop(name.as_str()) {
            continue;
        }
        // 同名头（如 Set-Cookie）可能有多条，逐条追加。
        out = out.header(name, value);
    }
    match out.body(Body::from_stream(stream)) {
        Ok(r) => r,
        Err(_) => (StatusCode::BAD_GATEWAY, "gateway: failed to build response").into_response(),
    }
}

async fn proxy(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(gate): State<Arc<Gate>>,
    req: Request,
) -> Response {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let path = uri.path();
    let target = route_for(path);
    let mut url = format!("{}{}", gate.base(target), path);
    if let Some(q) = uri.query() {
        url.push('?');
        url.push_str(q);
    }

    let (parts, body) = req.into_parts();
    let mut out = match reqwest::Method::from_bytes(method.as_str().as_bytes()) {
        Ok(m) => gate.client.request(m, &url),
        Err(_) => {
            return (StatusCode::BAD_GATEWAY, "gateway: unsupported method").into_response();
        }
    };

    // 透传请求头：去掉 Host（reqwest 按上游 URL 自己写）与逐跳头；
    // Content-Length 去掉，流式 body 由 reqwest 用 chunked 发送。
    let incoming_xff = parts
        .headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    for (name, value) in parts.headers.iter() {
        let n = name.as_str();
        if n == "host" || n == "content-length" || n == "x-forwarded-for" || is_hop_by_hop(n) {
            continue;
        }
        out = out.header(name, value);
    }
    // X-Forwarded-For：默认直接写成直连对端 IP（客户端自带的 XFF 不可信，
    // 追加会让伪造的首项被 API 当作真实 IP，绕过按 IP 限流）；只有网关前面
    // 还有一层可信反代（GATEWAY_TRUST_XFF=1）时才追加到入站 XFF 之后。
    let xff = if gate.trust_xff {
        match incoming_xff {
            Some(prev) if !prev.trim().is_empty() => format!("{prev}, {}", peer.ip()),
            _ => peer.ip().to_string(),
        }
    } else {
        peer.ip().to_string()
    };
    if let Ok(v) = HeaderValue::from_str(&xff) {
        out = out.header("x-forwarded-for", v);
    }
    if parts.headers.get("x-real-ip").is_none() {
        if let Ok(v) = HeaderValue::from_str(&peer.ip().to_string()) {
            out = out.header("x-real-ip", v);
        }
    }
    if parts.headers.get("x-forwarded-proto").is_none() {
        if let Ok(v) = HeaderValue::from_str("http") {
            out = out.header("x-forwarded-proto", v);
        }
    }

    // 流式转发 body（不缓冲；空 body 的 GET/HEAD 同样走这条路）。
    let has_body = !matches!(method, Method::GET | Method::HEAD | Method::DELETE)
        || parts.headers.contains_key(header::CONTENT_LENGTH)
        || parts.headers.contains_key(header::TRANSFER_ENCODING);
    if has_body {
        out = out.body(reqwest::Body::wrap_stream(body.into_data_stream()));
    }

    match out.send().await {
        Ok(res) => upstream_response(res),
        Err(e) => {
            tracing::warn!("gateway: upstream {target:?} {method} {path} failed: {e}");
            (
                StatusCode::BAD_GATEWAY,
                format!("gateway: upstream unreachable ({target:?})"),
            )
                .into_response()
        }
    }
}

#[tokio::main]
async fn main() {
    // 先读工作目录的 .env（不存在则忽略；已有环境变量优先，不会被覆盖）。
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let bind_addr =
        std::env::var("GATEWAY_BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8081".to_string());
    let gate = Arc::new(Gate {
        client: reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            // 绝不能跟随重定向：登录/票据兑换都是 302/303，网关必须把
            // 重定向原样交回浏览器，跟随了会把 303 吃成 200 登录页、
            // 还会丢掉 303 上的 Set-Cookie，会话永远建不起来。
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_else(|_| reqwest::Client::new()),
        api: std::env::var("IPTV_API_BASE").unwrap_or_else(|_| "http://127.0.0.1:8080".to_string()),
        portal: std::env::var("IPTV_PORTAL_BASE")
            .unwrap_or_else(|_| "http://127.0.0.1:18081".to_string()),
        admin: std::env::var("IPTV_ADMIN_BASE")
            .unwrap_or_else(|_| "http://127.0.0.1:18082".to_string()),
        trust_xff: std::env::var("GATEWAY_TRUST_XFF")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false),
    });
    // 注意：`/{*rest}` 通配符不匹配裸根路径 `/`（axum 0.8 通配符要求
    // 至少一段），根路径必须单独注册，否则门户首页会落到 axum 的空 404。
    let app = Router::new()
        .route("/", any(proxy))
        .route("/{*rest}", any(proxy))
        .with_state(gate.clone());
    let listener = tokio::net::TcpListener::bind(&bind_addr).await.unwrap();
    tracing::info!(
        "gateway listening on {bind_addr} (api={} portal={} admin={})",
        gate.api,
        gate.portal,
        gate.admin
    );
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_admin_by_prefix() {
        assert_eq!(route_for("/admin"), Upstream::Admin);
        assert_eq!(route_for("/admin/"), Upstream::Admin);
        assert_eq!(route_for("/admin/login"), Upstream::Admin);
        assert_eq!(route_for("/admin/tokens/1/ticket"), Upstream::Admin);
    }

    #[test]
    fn routes_api_by_prefix() {
        assert_eq!(route_for("/api"), Upstream::Api);
        assert_eq!(route_for("/api/v1/openapi.json"), Upstream::Api);
        assert_eq!(route_for("/api/v1/admin/users"), Upstream::Api);
        assert_eq!(route_for("/p"), Upstream::Api);
        assert_eq!(route_for("/p/seg.ts"), Upstream::Api);
        assert_eq!(route_for("/healthz"), Upstream::Api);
        assert_eq!(route_for("/metrics"), Upstream::Api);
    }

    #[test]
    fn routes_portal_by_default_and_similar_names() {
        assert_eq!(route_for("/"), Upstream::Portal);
        assert_eq!(route_for("/t/abc123"), Upstream::Portal);
        assert_eq!(route_for("/me"), Upstream::Portal);
        assert_eq!(route_for("/channels"), Upstream::Portal);
        assert_eq!(route_for("/watch/1"), Upstream::Portal);
        assert_eq!(route_for("/design.css"), Upstream::Portal);
        // 形似但不是：前缀必须整段匹配。
        assert_eq!(route_for("/adminx"), Upstream::Portal);
        assert_eq!(route_for("/admin2/x"), Upstream::Portal);
        assert_eq!(route_for("/apiary"), Upstream::Portal);
        assert_eq!(route_for("/healthz2"), Upstream::Portal);
        assert_eq!(route_for("/p2/seg.ts"), Upstream::Portal);
    }
}
