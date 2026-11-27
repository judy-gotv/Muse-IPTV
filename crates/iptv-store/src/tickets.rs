//! 一次性登录票据落库（M15 选 B，开发文档 §4.3）。
//!
//! - 只存票据的 SHA-256 哈希，不存明文；60 秒 TTL（`expires_at`，epoch 秒）。
//! - 原子消费：单条 `UPDATE ... SET used = 1 WHERE ticket_hash = ? AND
//!   used = 0 AND expires_at > now`，凭 affected rows 判定——1 行即本次
//!   消费成功，0 行即已用/过期/不存在；多实例并发、进程重启都只成功一次。
//! - 过期清理：消费时顺带删过期行（尽力而为）+ Worker 每 15 秒轮询清理。
// 作者: Passwd Word

use super::*;

/// 原子消费成功后返回的票据归属。
#[derive(Debug, Clone)]
pub struct ConsumedTicket {
    pub token_id: i64,
    pub ip_prefix: Option<String>,
}

impl Store {
    /// 签发票据行：`ticket_hash` 为票据明文的 SHA-256（hex）。
    pub async fn issue_login_ticket(
        &self,
        ticket_hash: &str,
        token_id: i64,
        ip_prefix: Option<&str>,
        expires_at: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        sqlx::query(&ph(
            "INSERT INTO login_ticket (ticket_hash, token_id, ip_prefix, created_at, expires_at, used) VALUES (?, ?, ?, ?, ?, 0)",
            self.dialect,
        ))
        .bind(ticket_hash)
        .bind(token_id)
        .bind(ip_prefix)
        .bind(epoch(Utc::now()))
        .bind(epoch(expires_at))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 原子消费：先 UPDATE 置已用（WHERE 含 used=0 与未过期），affected
    /// rows 为 1 才回查并返回票据数据；否则返回 `None`（已用/过期/不存在）。
    /// 无论成败都顺带删过期行，失败不影响本次兑换结果。
    pub async fn consume_login_ticket(
        &self,
        ticket_hash: &str,
        now: DateTime<Utc>,
    ) -> anyhow::Result<Option<ConsumedTicket>> {
        let n = sqlx::query(&ph(
            "UPDATE login_ticket SET used = 1 WHERE ticket_hash = ? AND used = 0 AND expires_at > ?",
            self.dialect,
        ))
        .bind(ticket_hash)
        .bind(epoch(now))
        .execute(&self.pool)
        .await?
        .rows_affected();
        // 消费时顺带清过期行（尽力而为；Worker 轮询是第二道清理）。
        let _ = self.purge_expired_tickets(now).await;
        if n == 0 {
            return Ok(None);
        }
        let row = sqlx::query(&ph(
            "SELECT token_id, ip_prefix FROM login_ticket WHERE ticket_hash = ?",
            self.dialect,
        ))
        .bind(ticket_hash)
        .fetch_one(&self.pool)
        .await?;
        Ok(Some(ConsumedTicket {
            token_id: r_i64(&row, "token_id")?,
            ip_prefix: r_opt_string(&row, "ip_prefix")?,
        }))
    }

    /// 删除已过期的票据行（无论是否已用），返回删除行数。
    pub async fn purge_expired_tickets(&self, now: DateTime<Utc>) -> anyhow::Result<u64> {
        let n = sqlx::query(&ph(
            "DELETE FROM login_ticket WHERE expires_at <= ?",
            self.dialect,
        ))
        .bind(epoch(now))
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n)
    }
}
