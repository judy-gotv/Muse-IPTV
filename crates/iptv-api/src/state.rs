//! 应用状态与配置（开发文档 §3、§10、§13）。
//!
//! 配置全部走环境变量（§3.2 的系统设置是运行期 DB 配置，两者分层）：
//! - `DATABASE_URL`：默认 `sqlite://iptv.db`（单机演示）；生产用
//!   `postgres://…` / `mysql://…`，只改配置不改代码（§10）。
//! - `REDIS_URL`：可选；不配则并发走内存兜底（§6.3 同语义）。
//! - `PLAY_SECRET` / `ADMIN_SECRET`：未配则启动时随机生成（仅当次运行有效，
//!   生产必须固定，否则重启后已签发的播放地址与管理员会话全部失效）。
//! - `PORTAL_BASE_URL`：生成 Token 登录链接用。
//! - `BIND_ADDR`：默认 `0.0.0.0:8080`。
// 作者: Passwd Word

use iptv_concurrency::ConcurrencyLimiter;
use iptv_i18n::I18n;
use iptv_store::Store;
use rand::RngCore;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub redis_url: Option<String>,
    pub play_secret: String,
    pub admin_secret: String,
    pub portal_base_url: String,
    pub bind_addr: String,
}

impl Config {
    pub fn from_env() -> Self {
        let mut secret = [0u8; 32];
        let play_secret = std::env::var("PLAY_SECRET").unwrap_or_else(|_| {
            rand::thread_rng().fill_bytes(&mut secret);
            tracing::warn!("PLAY_SECRET 未配置，已随机生成（重启后失效，生产请固定）");
            hex::encode(secret)
        });
        let admin_secret = std::env::var("ADMIN_SECRET").unwrap_or_else(|_| {
            rand::thread_rng().fill_bytes(&mut secret);
            tracing::warn!("ADMIN_SECRET 未配置，已随机生成（重启后失效，生产请固定）");
            hex::encode(secret)
        });
        Self {
            database_url: std::env::var("DATABASE_URL")
                .unwrap_or_else(|_| "sqlite://iptv.db".to_string()),
            redis_url: std::env::var("REDIS_URL").ok(),
            play_secret,
            admin_secret,
            portal_base_url: std::env::var("PORTAL_BASE_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:8081".to_string()),
            bind_addr: std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string()),
        }
    }
}

/// Prometheus 风格计数器（§14.4 可观测性；`/metrics` 输出）。
#[derive(Default)]
pub struct Metrics {
    pub http_requests: AtomicU64,
    pub playback_starts: AtomicU64,
    pub concurrent_limit_hits: AtomicU64,
    pub ticket_exchanges: AtomicU64,
    pub sync_triggers: AtomicU64,
}

pub struct AppState {
    pub store: Arc<Store>,
    pub limiter: Arc<ConcurrencyLimiter>,
    pub i18n: Arc<I18n>,
    pub config: Arc<Config>,
    pub metrics: Arc<Metrics>,
    pub tickets: Arc<crate::tickets::TicketStore>,
    pub login_limiter: Arc<iptv_auth::RateLimiter>,
}

impl AppState {
    pub async fn new(config: Config) -> anyhow::Result<Self> {
        let store = Arc::new(Store::connect(&config.database_url).await?);
        store.migrate().await?;
        // 种子默认系统设置（不存在才写，不覆盖已有配置）。
        for (k, v) in iptv_core::default_settings() {
            if store.get_setting(&k).await?.is_none() {
                store.set_setting(&k, &v).await?;
            }
        }
        let limiter = Arc::new(ConcurrencyLimiter::new(config.redis_url.as_deref()));
        Ok(Self {
            store,
            limiter,
            i18n: Arc::new(I18n::new()),
            config: Arc::new(config),
            metrics: Arc::new(Metrics::default()),
            tickets: Arc::new(crate::tickets::TicketStore::new()),
            // §13：登录口限流 5 次/分钟/IP
            login_limiter: Arc::new(iptv_auth::RateLimiter::new(5, 60)),
        })
    }
}
