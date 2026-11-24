//! 管理员、审计与仪表盘统计（开发文档 §3.2 / §5.1 / §5.3 / §13）。
// 作者: Passwd Word

use super::*;

pub(crate) fn row_to_admin(row: &AnyRow) -> anyhow::Result<AdminUser> {
    Ok(AdminUser {
        id: r_i64(row, "id")?,
        username: r_string(row, "username")?,
        role: match r_string(row, "role")?.as_str() {
            "super_admin" => AdminRole::SuperAdmin,
            "operator" => AdminRole::Operator,
            "support" => AdminRole::Support,
            _ => AdminRole::Readonly,
        },
        totp_enabled: r_bool(row, "totp_enabled")?,
        totp_required: r_bool(row, "totp_required")?,
        created_at: r_ts(row, "created_at")?,
    })
}

pub(crate) fn admin_role_str(role: &AdminRole) -> &'static str {
    match role {
        AdminRole::SuperAdmin => "super_admin",
        AdminRole::Operator => "operator",
        AdminRole::Support => "support",
        AdminRole::Readonly => "readonly",
    }
}

pub(crate) fn row_to_audit(row: &AnyRow) -> anyhow::Result<AuditLog> {
    Ok(AuditLog {
        id: r_i64(row, "id")?,
        actor: r_string(row, "actor")?,
        actor_role: r_opt_string(row, "actor_role")?,
        action: r_string(row, "action")?,
        object_type: r_string(row, "object_type")?,
        object_id: r_string(row, "object_id")?,
        before_json: r_opt_json(row, "before_json"),
        after_json: r_opt_json(row, "after_json"),
        ip_address: r_opt_string(row, "ip_address")?,
        created_at: r_ts(row, "created_at")?,
    })
}

const ADMIN_COLS: &str = "id, username, role, totp_enabled, totp_required, created_at";
const AUDIT_COLS: &str = "id, actor, actor_role, action, object_type, object_id, before_json, after_json, ip_address, created_at";

impl Store {
    pub async fn create_admin(
        &self,
        username: &str,
        password_hash: &str,
        role: AdminRole,
        totp_required: bool,
    ) -> anyhow::Result<AdminUser> {
        let now = Utc::now();
        let id = insert_id!(
            self,
            "INSERT INTO admin_user (username, password_hash, role, totp_secret, totp_enabled, totp_required, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            username,
            password_hash,
            admin_role_str(&role),
            None::<String>,
            0i64,
            i64::from(totp_required),
            epoch(now),
            epoch(now)
        )?;
        let row = sqlx::query(&format!("SELECT {ADMIN_COLS} FROM admin_user WHERE id = ?"))
            .bind(id)
            .fetch_one(&self.pool)
            .await?;
        row_to_admin(&row)
    }

    /// 返回 (管理员, 密码哈希, TOTP 密钥)：登录校验需要哈希与密钥，
    /// 它们不进 `AdminUser` DTO。
    pub async fn find_admin_by_username(
        &self,
        username: &str,
    ) -> anyhow::Result<Option<(AdminUser, String, Option<String>)>> {
        let row = sqlx::query(&format!(
            "SELECT {ADMIN_COLS}, password_hash, totp_secret FROM admin_user WHERE username = ?"
        ))
        .bind(username)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some(r) => {
                let admin = row_to_admin(&r)?;
                let hash: String = r.try_get("password_hash")?;
                let secret: Option<String> = r.try_get("totp_secret")?;
                Ok(Some((admin, hash, secret)))
            }
            None => Ok(None),
        }
    }

    pub async fn list_admins(&self) -> anyhow::Result<Vec<AdminUser>> {
        let rows = sqlx::query(&format!(
            "SELECT {ADMIN_COLS} FROM admin_user ORDER BY id ASC"
        ))
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            out.push(row_to_admin(r)?);
        }
        Ok(out)
    }

    pub async fn set_admin_totp(&self, id: i64, secret: &str) -> anyhow::Result<()> {
        sqlx::query("UPDATE admin_user SET totp_secret = ?, totp_enabled = 1 WHERE id = ?")
            .bind(secret)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn write_audit(&self, log: &NewAuditLog) -> anyhow::Result<i64> {
        let now = Utc::now();
        insert_id!(
            self,
            "INSERT INTO audit_log (actor, actor_role, action, object_type, object_id, before_json, after_json, ip_address, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &log.actor,
            log.actor_role.clone(),
            &log.action,
            &log.object_type,
            &log.object_id,
            log.before_json.as_ref().map(json_text),
            log.after_json.as_ref().map(json_text),
            log.ip_address.clone(),
            epoch(now)
        )
    }

    pub async fn list_audit(
        &self,
        actor: Option<&str>,
        action: Option<&str>,
        from: Option<DateTime<Utc>>,
        to: Option<DateTime<Utc>>,
        q: &PageQuery,
    ) -> anyhow::Result<Paged<AuditLog>> {
        use sqlx::QueryBuilder;
        let (limit, offset) = page_bounds(q);
        let mut count_qb =
            QueryBuilder::<sqlx::Any>::new("SELECT COUNT(*) FROM audit_log WHERE 1 = 1");
        let mut list_qb = QueryBuilder::<sqlx::Any>::new(format!(
            "SELECT {AUDIT_COLS} FROM audit_log WHERE 1 = 1"
        ));
        if let Some(a) = actor {
            count_qb.push(" AND actor = ").push_bind(a);
            list_qb.push(" AND actor = ").push_bind(a);
        }
        if let Some(a) = action {
            count_qb.push(" AND action = ").push_bind(a);
            list_qb.push(" AND action = ").push_bind(a);
        }
        if let Some(f) = from {
            count_qb.push(" AND created_at >= ").push_bind(epoch(f));
            list_qb.push(" AND created_at >= ").push_bind(epoch(f));
        }
        if let Some(t) = to {
            count_qb.push(" AND created_at <= ").push_bind(epoch(t));
            list_qb.push(" AND created_at <= ").push_bind(epoch(t));
        }
        let total: i64 = count_qb.build_query_scalar().fetch_one(&self.pool).await?;
        list_qb
            .push(" ORDER BY created_at DESC, id DESC LIMIT ")
            .push_bind(limit)
            .push(" OFFSET ")
            .push_bind(offset);
        let rows = list_qb.build().fetch_all(&self.pool).await?;
        let mut items = Vec::with_capacity(rows.len());
        for r in &rows {
            items.push(row_to_audit(r)?);
        }
        Ok(Paged {
            items,
            total: total as u64,
            page: q.page.max(1),
            page_size: q.page_size.clamp(1, 100),
        })
    }

    /// 审计保留 180 天（§13）：删掉 `before` 之前的审计，返回删除条数。
    pub async fn delete_old_audit(&self, before: DateTime<Utc>) -> anyhow::Result<u64> {
        let n = sqlx::query("DELETE FROM audit_log WHERE created_at < ?")
            .bind(epoch(before))
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(n)
    }

    /// 仪表盘聚合（§5.1）：全部取自真实查询，不返回写死数字。
    pub async fn stats_overview(&self) -> anyhow::Result<StatsOverview> {
        let now = Utc::now();
        let now_e = epoch(now);
        let online_users: i64 = sqlx::query_scalar(
            "SELECT COUNT(DISTINCT token_id) FROM portal_session WHERE revoked_at IS NULL AND expires_at > ?",
        )
        .bind(now_e)
        .fetch_one(&self.pool)
        .await?;
        let active_sessions: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM playback_session WHERE status = 'active'")
                .fetch_one(&self.pool)
                .await?;
        let total_channels: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM channel")
            .fetch_one(&self.pool)
            .await?;
        let available_channels: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM channel WHERE status = 'active' AND (last_available IS NULL OR last_available = 1)",
        )
        .fetch_one(&self.pool)
        .await?;
        let total_tokens: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user_token")
            .fetch_one(&self.pool)
            .await?;
        let active_tokens: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM user_token WHERE status = 'active' AND (expires_at IS NULL OR expires_at > ?)",
        )
        .bind(now_e)
        .fetch_one(&self.pool)
        .await?;
        let last_sync: Option<(Option<i64>, Option<String>)> =
            sqlx::query_as::<_, (Option<i64>, Option<String>)>(
                "SELECT finished_at, status FROM sync_log ORDER BY id DESC LIMIT 1",
            )
            .fetch_optional(&self.pool)
            .await?;
        let (last_sync_at, last_sync_status) = match last_sync {
            Some((ts, st)) => (ts.map(from_epoch), st),
            None => (None, None),
        };
        // 告警：同步失败的订阅源（真实数据）。
        let failed: Vec<String> =
            sqlx::query_scalar("SELECT name FROM playlist_source WHERE last_status = 'failed'")
                .fetch_all(&self.pool)
                .await?;
        let alerts: Vec<String> = failed
            .iter()
            .map(|n| format!("订阅源 {n} 同步失败"))
            .collect();
        Ok(StatsOverview {
            online_users,
            active_sessions,
            total_channels,
            available_channels,
            total_tokens,
            active_tokens,
            last_sync_at,
            last_sync_status,
            alerts,
        })
    }
}
