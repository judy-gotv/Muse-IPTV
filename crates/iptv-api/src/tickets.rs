//! 一次性登录票据（开发文档 §4.2 方式 A / §4.3）。
//!
//! - 60 秒有效、单次消费、可绑 IP 段（§13）。
//! - 消费必须是原子的：内存实现用一把锁内“查 + 删”；有 Redis 时用
//!   `GETDEL`（原子取删）。票据只存 SHA-256，不存明文。
// 作者: Passwd Word

use iptv_auth::sha256_hex;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct TicketData {
    pub token_id: i64,
    pub ip_prefix: Option<String>,
}

struct Entry {
    data: TicketData,
    expires: Instant,
}

pub struct TicketStore {
    inner: Mutex<HashMap<String, Entry>>,
}

impl Default for TicketStore {
    fn default() -> Self {
        Self::new()
    }
}

impl TicketStore {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// 签发票据，返回明文票据（只展示一次）。
    pub fn issue(&self, token_id: i64, ip_prefix: Option<String>, ttl: Duration) -> String {
        let ticket = iptv_core::generate_ticket();
        let hash = sha256_hex(ticket.as_bytes());
        self.inner.lock().unwrap().insert(
            hash,
            Entry {
                data: TicketData {
                    token_id,
                    ip_prefix,
                },
                expires: Instant::now() + ttl,
            },
        );
        ticket
    }

    /// 原子消费：未找到 / 已过期返回 None；成功返回票据数据（且只返回一次）。
    pub fn consume(&self, ticket: &str) -> Option<TicketData> {
        let hash = sha256_hex(ticket.as_bytes());
        let mut map = self.inner.lock().unwrap();
        let entry = map.remove(&hash)?;
        if entry.expires < Instant::now() {
            return None;
        }
        Some(entry.data)
    }

    /// 清理过期票据（worker / 定时调用，非必需——过期票据消费时即失效）。
    pub fn purge_expired(&self) {
        let mut map = self.inner.lock().unwrap();
        let now = Instant::now();
        map.retain(|_, e| e.expires >= now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticket_single_use() {
        let store = TicketStore::new();
        let t = store.issue(42, None, Duration::from_secs(60));
        let d = store.consume(&t).expect("first consume works");
        assert_eq!(d.token_id, 42);
        assert!(store.consume(&t).is_none(), "second consume must fail");
        assert!(store.consume("nope").is_none());
    }

    #[test]
    fn ticket_expires() {
        let store = TicketStore::new();
        let t = store.issue(1, None, Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(5));
        assert!(store.consume(&t).is_none());
    }
}
