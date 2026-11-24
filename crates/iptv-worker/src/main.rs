//! iptv-worker 二进制：订阅同步 / EPG / 探测 / 清理（开发文档 §3.1、§7.3）。
//!
//! 环境变量：DATABASE_URL、REDIS_URL（可选）。启动后每 15 秒扫一轮。
// 作者: Passwd Word

use iptv_store::Store;
use iptv_worker::Worker;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();
    let database_url =
        std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite://iptv.db".to_string());
    let store = Arc::new(Store::connect(&database_url).await?);
    store.migrate().await?;
    let worker = Worker::new(store);
    tracing::info!("iptv-worker {} started", iptv_core::VERSION);
    worker.run_forever().await
}
