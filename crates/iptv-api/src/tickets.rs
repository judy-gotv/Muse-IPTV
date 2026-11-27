//! 一次性登录票据（开发文档 §4.2 方式 A / §4.3；M15 选 B：票据落库）。
//!
//! - 60 秒有效、单次消费、可绑 IP 段（§13）。
//! - 库是事实来源：只存 SHA-256 哈希（`login_ticket` 表，V7 迁移）；消费是
//!   库内单条原子 UPDATE（`used = 1 WHERE ticket_hash = ? AND used = 0
//!   AND expires_at > now`），凭 affected rows 判定——同一票据两次兑换
//!   只有一次成功，即使跨进程、跨实例、重启后也一样。
//! - 本模块不再持有任何内存票据状态（无缓存可绕过库的原子消费）。
//! - 不引入 Redis：当前部署没有 Redis，落库才能在现部署上生效。
// 作者: Passwd Word

use chrono::{Duration as ChronoDuration, Utc};
use iptv_auth::sha256_hex;
use iptv_store::TicketRepository;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct TicketData {
    pub token_id: i64,
    pub ip_prefix: Option<String>,
}

/// 票据签发与消费：全部走 `TicketRepository` trait（m20，§10.4）。
/// `TicketStore` 只依赖 trait（`Arc<dyn TicketRepository>` 注入），不依赖
/// 具体 `Store`——换存储实现只需在 `AppState::new` 换注入对象，此处不动。
/// 事实来源仍是 `login_ticket` 表（M15 选 B 落库），原子消费语义不变。
pub struct TicketStore {
    store: Arc<dyn TicketRepository>,
}

impl TicketStore {
    pub fn new(store: Arc<dyn TicketRepository>) -> Self {
        Self { store }
    }

    /// 签发票据，返回明文票据（只展示一次）；库里只存其 SHA-256 哈希。
    pub async fn issue(
        &self,
        token_id: i64,
        ip_prefix: Option<String>,
        ttl: Duration,
    ) -> anyhow::Result<String> {
        let ticket = iptv_core::generate_ticket();
        let hash = sha256_hex(ticket.as_bytes());
        let expires_at = Utc::now()
            + ChronoDuration::from_std(ttl).unwrap_or_else(|_| ChronoDuration::seconds(60));
        self.store
            .issue_login_ticket(&hash, token_id, ip_prefix.as_deref(), expires_at)
            .await?;
        Ok(ticket)
    }

    /// 原子消费：未找到 / 已用 / 已过期返回 None；成功返回票据数据（且只返回一次）。
    pub async fn consume(&self, ticket: &str) -> anyhow::Result<Option<TicketData>> {
        let hash = sha256_hex(ticket.as_bytes());
        let got = self.store.consume_login_ticket(&hash, Utc::now()).await?;
        Ok(got.map(|c| TicketData {
            token_id: c.token_id,
            ip_prefix: c.ip_prefix,
        }))
    }

    /// 清理过期票据行（Worker 每 15 秒轮询调用；消费时也顺带清理）。
    /// 返回删除的行数。
    pub async fn purge_expired(&self) -> anyhow::Result<u64> {
        self.store.purge_expired_tickets(Utc::now()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iptv_store::Store;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        store: Arc<Store>,
        path: std::path::PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    async fn setup() -> Fixture {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("iptv-ticket-test-{}-{}.db", std::process::id(), n));
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let store = Arc::new(Store::connect(&url).await.expect("connect sqlite"));
        store.migrate().await.expect("migrate");
        Fixture { store, path }
    }

    #[tokio::test]
    async fn ticket_single_use() {
        let fx = setup().await;
        let store = TicketStore::new(fx.store.clone());
        let t = store
            .issue(42, None, Duration::from_secs(60))
            .await
            .unwrap();
        let d = store
            .consume(&t)
            .await
            .unwrap()
            .expect("first consume works");
        assert_eq!(d.token_id, 42);
        assert!(
            store.consume(&t).await.unwrap().is_none(),
            "second consume must fail"
        );
        assert!(store.consume("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn ticket_expires() {
        let fx = setup().await;
        let store = TicketStore::new(fx.store.clone());
        let t = store
            .issue(1, None, Duration::from_millis(1))
            .await
            .unwrap();
        std::thread::sleep(Duration::from_millis(5));
        assert!(store.consume(&t).await.unwrap().is_none());
    }

    /// 重启模拟：票据签发后重开连接（新 Store + 新 TicketStore 实例），
    /// 未消费的票据仍可兑换一次；已消费的票据重开后仍不可用。
    #[tokio::test]
    async fn ticket_survives_restart_but_used_stays_used() {
        let fx = setup().await;
        let store = TicketStore::new(fx.store.clone());
        let fresh = store
            .issue(7, Some("10.0.".to_string()), Duration::from_secs(60))
            .await
            .unwrap();
        let used = store.issue(8, None, Duration::from_secs(60)).await.unwrap();
        assert!(store.consume(&used).await.unwrap().is_some());
        drop(store);
        // “重启”：同一库文件上新开连接、新跑迁移、新 TicketStore 实例。
        let url = format!("sqlite://{}?mode=rwc", fx.path.display());
        let store2 = Arc::new(Store::connect(&url).await.expect("reconnect sqlite"));
        store2.migrate().await.expect("migrate again");
        let reopened = TicketStore::new(store2);
        let d = reopened
            .consume(&fresh)
            .await
            .unwrap()
            .expect("unconsumed ticket survives restart");
        assert_eq!(d.token_id, 7);
        assert_eq!(d.ip_prefix.as_deref(), Some("10.0."));
        assert!(
            reopened.consume(&used).await.unwrap().is_none(),
            "consumed ticket stays consumed after restart"
        );
        assert!(
            reopened.consume(&fresh).await.unwrap().is_none(),
            "second consume after restart must fail"
        );
    }

    #[tokio::test]
    async fn ticket_purge_expired() {
        let fx = setup().await;
        let store = TicketStore::new(fx.store.clone());
        let t = store
            .issue(1, None, Duration::from_millis(1))
            .await
            .unwrap();
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(store.purge_expired().await.unwrap(), 1);
        assert!(store.consume(&t).await.unwrap().is_none());
        assert_eq!(store.purge_expired().await.unwrap(), 0);
    }
}
