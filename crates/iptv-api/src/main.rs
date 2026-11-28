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
    // 先读工作目录的 .env（不存在则忽略；已有环境变量优先，不会被覆盖）。
    dotenvy::dotenv().ok();
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
    // m22：启动时把 `i18n_override` 表装入进程内覆盖表（读库失败只记
    // 警告，不阻塞启动；无覆盖时 FTL 兜底）。
    match state.store.i18n_overrides_map().await {
        Ok(map) => {
            let n = map.len();
            iptv_i18n::I18n::set_overrides(map);
            tracing::info!("loaded {n} i18n overrides");
        }
        Err(e) => tracing::warn!("failed to load i18n overrides: {e}"),
    }
    // m-uptask + N5：后台定期清理过期上传任务（完成 24 小时后删 + 最多
    // 1000 行封顶）。`interval` 首 tick 立即执行，故启动即先清一轮上次
    // 运行残留的过期行；清理失败只记警告，不阻塞启动/服务。
    let purge_state = state.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(
            iptv_api::upload_tasks::UPLOAD_TASK_PURGE_INTERVAL_SECS,
        ));
        loop {
            ticker.tick().await;
            match purge_state.upload_tasks.purge_expired().await {
                Ok(n) if n > 0 => tracing::info!("purged {n} expired upload tasks"),
                Ok(_) => {}
                Err(e) => tracing::warn!("purge expired upload tasks failed: {e}"),
            }
        }
    });
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
    // 必须用 into_make_service_with_connect_info，否则登录 handler 的
    // ConnectInfo 拿不到对端 IP，client_ip 回落失效（M2）。
    axum::serve(
        listener,
        AppState::router(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;
    Ok(())
}
