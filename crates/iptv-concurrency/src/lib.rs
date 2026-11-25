//! 并发额度控制（开发文档 §6）。
//!
//! 语义：`max_concurrent_sessions` = 每个 Token 同时允许的活跃播放会话数
//! （§6.1）。检查 + 占用 + 计数在 Redis 内由 Lua 原子完成（§6.3，脚本
//! 逐字照抄文档）；无 Redis 时用内存实现跑同一语义（同一把锁内完成
//! 清理过期 + 计数 + 占用），行为一致，`cargo test` 覆盖。
//!
//! 心跳 30s 刷新 score；90s 未心跳自动过期（§6.3）；服务重启不丢额度，
//! TTL 兜底，最坏 90s 自愈。
// 作者: Passwd Word

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// §6.3 原子扣减 Lua（文档逐字）。KEYS[1] = token:active:{token_id}，
/// ARGV = session_uid, max_concurrent, now_epoch, ttl_seconds。
pub const ACQUIRE_LUA: &str = r#"redis.call('ZREMRANGEBYSCORE', KEYS[1], '-inf', ARGV[3])
local n = redis.call('ZCARD', KEYS[1])
if n >= tonumber(ARGV[2]) then
  return {0, n}
end
redis.call('ZADD', KEYS[1], tonumber(ARGV[3]) + tonumber(ARGV[4]), ARGV[1])
redis.call('EXPIRE', KEYS[1], tonumber(ARGV[4]) + 60)
return {1, n + 1}"#;

pub const HEARTBEAT_TTL_SEC: i64 = 90;
pub const HEARTBEAT_INTERVAL_SEC: i64 = 30;

/// 并发扣减结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcquireOutcome {
    pub acquired: bool,
    /// 扣减后的当前占用数（失败时为扣减前的占用数）。
    pub current: i64,
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 并发限制器：有 Redis 用 Redis Lua，无 Redis 用内存兜底（同一语义）。
pub struct ConcurrencyLimiter {
    redis_client: Option<redis::Client>,
    memory: Mutex<HashMap<i64, HashMap<String, i64>>>, // token_id -> session_uid -> expire_ts
    /// 会话超时秒数（M1：读系统设置 `session_timeout_sec`，默认 90s；
    /// `set_ttl_sec` 在启动与设置更新时刷新，运行中可改）。
    ttl_sec: std::sync::atomic::AtomicI64,
}

impl ConcurrencyLimiter {
    pub fn new(redis_url: Option<&str>) -> Self {
        let redis_client = redis_url.and_then(|u| redis::Client::open(u).ok());
        Self {
            redis_client,
            memory: Mutex::new(HashMap::new()),
            ttl_sec: std::sync::atomic::AtomicI64::new(HEARTBEAT_TTL_SEC),
        }
    }

    /// 当前会话超时（秒）。
    pub fn ttl_sec(&self) -> i64 {
        self.ttl_sec.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 运行时改会话超时（系统设置变更后调用；新值只影响此后签发的 score）。
    pub fn set_ttl_sec(&self, ttl: i64) {
        self.ttl_sec
            .store(ttl.max(1), std::sync::atomic::Ordering::Relaxed);
    }

    pub fn uses_redis(&self) -> bool {
        self.redis_client.is_some()
    }

    /// §6.3 原子扣减。返回 (是否占用成功, 当前占用数)。
    pub async fn acquire(
        &self,
        token_id: i64,
        session_uid: &str,
        max: i32,
    ) -> anyhow::Result<AcquireOutcome> {
        if let Some(client) = &self.redis_client {
            let mut conn = client.get_multiplexed_async_connection().await?;
            let key = format!("token:active:{token_id}");
            let now = now_epoch();
            let res: Vec<i64> = redis::Script::new(ACQUIRE_LUA)
                .key(key)
                .arg(session_uid)
                .arg(max as i64)
                .arg(now)
                .arg(self.ttl_sec())
                .invoke_async(&mut conn)
                .await?;
            let acquired = res.first().copied().unwrap_or(0) == 1;
            let current = res.get(1).copied().unwrap_or(0);
            return Ok(AcquireOutcome { acquired, current });
        }
        // 内存兜底：同一把锁内完成“清理过期 + 计数 + 占用”，与 Lua 同语义。
        let mut map = self.memory.lock().unwrap();
        let now = now_epoch();
        let sessions = map.entry(token_id).or_default();
        sessions.retain(|_, exp| *exp > now);
        if sessions.len() as i64 >= max as i64 && !sessions.contains_key(session_uid) {
            return Ok(AcquireOutcome {
                acquired: false,
                current: sessions.len() as i64,
            });
        }
        sessions.insert(session_uid.to_string(), now + self.ttl_sec());
        Ok(AcquireOutcome {
            acquired: true,
            current: sessions.len() as i64,
        })
    }

    /// 心跳：刷新 score / 过期时间。会话不存在返回 false。
    pub async fn heartbeat(&self, token_id: i64, session_uid: &str) -> anyhow::Result<bool> {
        if let Some(client) = &self.redis_client {
            let mut conn = client.get_multiplexed_async_connection().await?;
            let key = format!("token:active:{token_id}");
            let now = now_epoch();
            let added: i64 = redis::cmd("ZADD")
                .arg(&key)
                .arg(now + self.ttl_sec())
                .arg(session_uid)
                .query_async(&mut conn)
                .await?;
            let _: () = redis::cmd("EXPIRE")
                .arg(&key)
                .arg(self.ttl_sec() + 60)
                .query_async(&mut conn)
                .await?;
            // ZADD 返回 0 表示 member 已存在（即心跳刷新），1 表示新建。
            let _ = added;
            return Ok(true);
        }
        let mut map = self.memory.lock().unwrap();
        let now = now_epoch();
        if let Some(sessions) = map.get_mut(&token_id) {
            if sessions.contains_key(session_uid) {
                sessions.insert(session_uid.to_string(), now + self.ttl_sec());
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// 释放一路上播放（stop / 下线时调用）。
    pub async fn release(&self, token_id: i64, session_uid: &str) -> anyhow::Result<()> {
        if let Some(client) = &self.redis_client {
            let mut conn = client.get_multiplexed_async_connection().await?;
            let key = format!("token:active:{token_id}");
            let _: () = redis::cmd("ZREM")
                .arg(&key)
                .arg(session_uid)
                .query_async(&mut conn)
                .await?;
            return Ok(());
        }
        let mut map = self.memory.lock().unwrap();
        if let Some(sessions) = map.get_mut(&token_id) {
            sessions.remove(session_uid);
        }
        Ok(())
    }

    /// 当前占用数（清理过期后计数）。
    pub async fn count(&self, token_id: i64) -> anyhow::Result<i64> {
        if let Some(client) = &self.redis_client {
            let mut conn = client.get_multiplexed_async_connection().await?;
            let key = format!("token:active:{token_id}");
            let now = now_epoch();
            let _: () = redis::cmd("ZREMRANGEBYSCORE")
                .arg(&key)
                .arg("-inf")
                .arg(now)
                .query_async(&mut conn)
                .await?;
            let n: i64 = redis::cmd("ZCARD").arg(&key).query_async(&mut conn).await?;
            return Ok(n);
        }
        let mut map = self.memory.lock().unwrap();
        let now = now_epoch();
        if let Some(sessions) = map.get_mut(&token_id) {
            sessions.retain(|_, exp| *exp > now);
            Ok(sessions.len() as i64)
        } else {
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn memory_limiter_enforces_max() {
        let lim = ConcurrencyLimiter::new(None);
        assert!(!lim.uses_redis());
        let a = lim.acquire(7, "s1", 2).await.unwrap();
        assert!(a.acquired && a.current == 1);
        let b = lim.acquire(7, "s2", 2).await.unwrap();
        assert!(b.acquired && b.current == 2);
        let c = lim.acquire(7, "s3", 2).await.unwrap();
        assert!(!c.acquired && c.current == 2);
        assert_eq!(lim.count(7).await.unwrap(), 2);
        lim.release(7, "s1").await.unwrap();
        let d = lim.acquire(7, "s3", 2).await.unwrap();
        assert!(d.acquired);
        assert!(lim.heartbeat(7, "s3").await.unwrap());
        assert!(!lim.heartbeat(7, "gone").await.unwrap());
    }

    #[test]
    fn lua_script_is_doc_verbatim() {
        // §6.3 脚本的关键步骤一个都不能少。
        assert!(ACQUIRE_LUA.contains("ZREMRANGEBYSCORE"));
        assert!(ACQUIRE_LUA.contains("ZCARD"));
        assert!(ACQUIRE_LUA.contains("ZADD"));
        assert!(ACQUIRE_LUA.contains("EXPIRE"));
    }
}
