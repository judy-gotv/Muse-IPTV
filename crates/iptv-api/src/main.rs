//! iptv-api 二进制：Axum 后端服务（开发文档 §1、§3）。
//!
//! 环境变量见 `iptv_api::Config::from_env`。首次启动时若设置了
//! `IPTV_ADMIN_USER` / `IPTV_ADMIN_PASSWORD` 且库里尚无管理员，则创建
//! 超管账号（密码只从环境变量读，永不打日志）。
// 作者: Passwd Word

use iptv_api::{AppState, Config};
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();
    let config = Config::from_env();
    // SQLite 文件的父目录不存在时先建出来。
    if let Some(path) = config.database_url.strip_prefix("sqlite://") {
        if let Some(parent) = std::path::Path::new(path).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
    }
    let state = Arc::new(AppState::new(config.clone()).await?);
    // 首次启动建超管（仅当库里一个管理员都没有时）。
    if state.store.list_admins().await?.is_empty() {
        if let (Ok(user), Ok(pass)) = (
            std::env::var("IPTV_ADMIN_USER"),
            std::env::var("IPTV_ADMIN_PASSWORD"),
        ) {
            let hash = iptv_auth::hash_password(&pass)
                .map_err(|e| anyhow::anyhow!("hash admin password: {e}"))?;
            let admin = state
                .store
                .create_admin(&user, &hash, iptv_dto::AdminRole::SuperAdmin, false)
                .await?;
            tracing::info!(
                "created initial super admin id={} username={}",
                admin.id,
                admin.username
            );
        } else {
            tracing::warn!("no admin exists yet; set IPTV_ADMIN_USER / IPTV_ADMIN_PASSWORD and restart to create one");
        }
    }
    let listener = tokio::net::TcpListener::bind(&config.bind_addr).await?;
    tracing::info!(
        "iptv-api {} listening on {} (db: {})",
        iptv_core::VERSION,
        config.bind_addr,
        state.store.dialect()
    );
    axum::serve(listener, AppState::router(state)).await?;
    Ok(())
}
