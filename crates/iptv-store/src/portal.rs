//! 门户会话与设备绑定（`portal_session` / `device_binding`，开发文档 §4.3/§9）。
//! 会话 Cookie 值只存 SHA-256（`session_hash` BLOB / BYTEA / VARBINARY）。
// 作者: Passwd Word

use super::*;

pub(crate) fn row_to_portal_session(row: &AnyRow) -> anyhow::Result<PortalSession> {
    Ok(PortalSession {
        id: r_i64(row, "id")?,
        token_id: r_i64(row, "token_id")?,
        device_id: r_string(row, "device_id")?,
        ip_address: r_opt_string(row, "ip_address")?,
        user_agent: r_opt_string(row, "user_agent")?,
        created_at: r_ts(row, "created_at")?,
        last_seen_at: r_ts(row, "last_seen_at")?,
        expires_at: r_ts(row, "expires_at")?,
        revoked_at: r_opt_ts(row, "revoked_at")?,
    })
}

const PORTAL_COLS: &str = "id, token_id, device_id, ip_address, user_agent, created_at, last_seen_at, expires_at, revoked_at";

impl Store {
    pub async fn create_portal_session(
        &self,
        token_id: i64,
        device_id: &str,
        session_hash: &[u8; 32],
        ip: Option<&str>,
        ua: Option<&str>,
        expires_at: DateTime<Utc>,
    ) -> anyhow::Result<PortalSession> {
        let now = Utc::now();
        let id = insert_id!(
            self,
            "INSERT INTO portal_session (session_hash, token_id, device_id, ip_address, user_agent, created_at, last_seen_at, expires_at, revoked_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            session_hash.to_vec(),
            token_id,
            device_id,
            ip,
            ua,
            epoch(now),
            epoch(now),
            epoch(expires_at),
            None::<i64>
        )?;
        // 设备绑定：同一 (token, device) 只记一条，刷新 last_seen_at。
        let n = sqlx::query(&ph(
            "UPDATE device_binding SET last_seen_at = ? WHERE token_id = ? AND device_id = ?",
            self.dialect,
        ))
        .bind(epoch(now))
        .bind(token_id)
        .bind(device_id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if n == 0 {
            sqlx::query(
                &ph("INSERT INTO device_binding (token_id, device_id, first_seen_at, last_seen_at) VALUES (?, ?, ?, ?)", self.dialect),
            )
            .bind(token_id)
            .bind(device_id)
            .bind(epoch(now))
            .bind(epoch(now))
            .execute(&self.pool)
            .await?;
        }
        self.find_portal_session_by_hash(session_hash)
            .await?
            .ok_or_else(|| anyhow::anyhow!("新建门户会话后回查失败 id={id}"))
    }

    pub async fn find_portal_session_by_hash(
        &self,
        hash: &[u8; 32],
    ) -> anyhow::Result<Option<PortalSession>> {
        let row = sqlx::query(&ph(
            &format!("SELECT {PORTAL_COLS} FROM portal_session WHERE session_hash = ?"),
            self.dialect,
        ))
        .bind(hash.to_vec())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| row_to_portal_session(&r)).transpose()
    }

    pub async fn revoke_portal_session(&self, id: i64) -> anyhow::Result<bool> {
        let now = Utc::now();
        let n = sqlx::query(&ph(
            "UPDATE portal_session SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL",
            self.dialect,
        ))
        .bind(epoch(now))
        .bind(id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n > 0)
    }

    pub async fn revoke_all_portal_sessions(
        &self,
        token_id: i64,
        except_id: Option<i64>,
    ) -> anyhow::Result<u64> {
        let now = Utc::now();
        let n = match except_id {
            Some(except) => {
                sqlx::query(
                    &ph("UPDATE portal_session SET revoked_at = ? WHERE token_id = ? AND revoked_at IS NULL AND id <> ?", self.dialect),
                )
                .bind(epoch(now))
                .bind(token_id)
                .bind(except)
                .execute(&self.pool)
                .await?
                .rows_affected()
            }
            None => {
                sqlx::query(
                    &ph("UPDATE portal_session SET revoked_at = ? WHERE token_id = ? AND revoked_at IS NULL", self.dialect),
                )
                .bind(epoch(now))
                .bind(token_id)
                .execute(&self.pool)
                .await?
                .rows_affected()
            }
        };
        Ok(n)
    }

    pub async fn list_portal_sessions(&self, token_id: i64) -> anyhow::Result<Vec<PortalSession>> {
        let rows = sqlx::query(&ph(&format!(
            "SELECT {PORTAL_COLS} FROM portal_session WHERE token_id = ? AND revoked_at IS NULL ORDER BY last_seen_at DESC"
        ), self.dialect))
        .bind(token_id)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            out.push(row_to_portal_session(r)?);
        }
        Ok(out)
    }

    pub async fn touch_portal_session(
        &self,
        id: i64,
        now: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        sqlx::query(
            &ph("UPDATE portal_session SET last_seen_at = ?, expires_at = ? WHERE id = ? AND revoked_at IS NULL", self.dialect),
        )
        .bind(epoch(now))
        .bind(epoch(expires_at))
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn bound_device_count(&self, token_id: i64) -> anyhow::Result<i64> {
        Ok(sqlx::query_scalar(&ph(
            "SELECT COUNT(*) FROM device_binding WHERE token_id = ?",
            self.dialect,
        ))
        .bind(token_id)
        .fetch_one(&self.pool)
        .await?)
    }

    /// 该设备指纹是否已绑定到此 Token（§6.1：上限只计不同设备数，
    /// 已绑定设备重登录不占新名额——logout-all 后同一台设备重登不能被
    /// 42902 挡住，绑定本身是粘性的，吊销会话不解绑）。
    pub async fn is_device_bound(&self, token_id: i64, device_id: &str) -> anyhow::Result<bool> {
        let n: i64 = sqlx::query_scalar(&ph(
            "SELECT COUNT(*) FROM device_binding WHERE token_id = ? AND device_id = ?",
            self.dialect,
        ))
        .bind(token_id)
        .bind(device_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(n > 0)
    }
}
