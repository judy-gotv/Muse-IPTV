//! Token（`user_token`，开发文档 §3.2 / §6.2）。
//! 明文 Token 永不入库，只存 SHA-256（`token_hash` BLOB / BYTEA / VARBINARY）。
// 作者: Passwd Word

use super::*;
use sqlx::QueryBuilder;

pub(crate) fn row_to_token(row: &AnyRow) -> anyhow::Result<UserToken> {
    Ok(UserToken {
        id: r_i64(row, "id")?,
        user_id: r_i64(row, "user_id")?,
        token_prefix: r_string(row, "token_prefix")?,
        name: r_string(row, "name")?,
        device_type: r_opt_string(row, "device_type")?,
        locale: r_string(row, "locale")?,
        max_concurrent_sessions: r_i64(row, "max_concurrent_sessions")? as i32,
        max_bound_devices: r_i64(row, "max_bound_devices")? as i32,
        status: match r_string(row, "status")?.as_str() {
            "disabled" => TokenStatus::Disabled,
            "expired" => TokenStatus::Expired,
            _ => TokenStatus::Active,
        },
        expires_at: r_opt_ts(row, "expires_at")?,
        allowed_groups: str_list(&r_json(row, "allowed_groups")),
        denied_channels: i64_list(&r_json(row, "denied_channels")),
        features: serde_json::from_value(r_json(row, "features")).unwrap_or_default(),
        created_at: r_ts(row, "created_at")?,
        updated_at: r_ts(row, "updated_at")?,
    })
}

pub(crate) fn token_status_str(s: &TokenStatus) -> &'static str {
    match s {
        TokenStatus::Active => "active",
        TokenStatus::Disabled => "disabled",
        TokenStatus::Expired => "expired",
    }
}

const TOKEN_COLS: &str = "id, user_id, token_prefix, name, device_type, locale, max_concurrent_sessions, max_bound_devices, status, expires_at, allowed_groups, denied_channels, features, created_at, updated_at";

impl Store {
    pub async fn create_token(
        &self,
        user_id: i64,
        prefix: &str,
        hash: &[u8; 32],
        req: &CreateTokenRequest,
    ) -> anyhow::Result<UserToken> {
        let now = Utc::now();
        let id = insert_id!(
            self,
            "INSERT INTO user_token (user_id, token_prefix, token_hash, name, device_type, locale, max_concurrent_sessions, max_bound_devices, status, expires_at, allowed_groups, denied_channels, features, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            user_id,
            prefix,
            hash.to_vec(),
            &req.name,
            req.device_type.clone(),
            req.locale.clone().unwrap_or_else(|| "zh-Hant-TW".to_string()),
            i64::from(req.max_concurrent_sessions),
            i64::from(req.max_bound_devices),
            "active",
            req.expires_at.map(epoch),
            json_text(&serde_json::json!(req.allowed_groups)),
            json_text(&serde_json::json!(req.denied_channels)),
            json_text(&serde_json::to_value(&req.features)?),
            epoch(now),
            epoch(now)
        )?;
        self.get_token(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("新建 Token 后回查失败 id={id}"))
    }

    pub async fn find_token_by_hash(&self, hash: &[u8; 32]) -> anyhow::Result<Option<UserToken>> {
        let row = sqlx::query(&format!(
            "SELECT {TOKEN_COLS} FROM user_token WHERE token_hash = ?"
        ))
        .bind(hash.to_vec())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| row_to_token(&r)).transpose()
    }

    pub async fn get_token(&self, id: i64) -> anyhow::Result<Option<UserToken>> {
        let row = sqlx::query(&format!("SELECT {TOKEN_COLS} FROM user_token WHERE id = ?"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|r| row_to_token(&r)).transpose()
    }

    pub async fn list_tokens(
        &self,
        user_id: Option<i64>,
        status: Option<TokenStatus>,
        q: &PageQuery,
    ) -> anyhow::Result<Paged<UserToken>> {
        let (limit, offset) = page_bounds(q);
        let pat = like_pattern(&q.q);
        let mut count_qb =
            QueryBuilder::<sqlx::Any>::new("SELECT COUNT(*) FROM user_token WHERE 1 = 1");
        let mut list_qb = QueryBuilder::<sqlx::Any>::new(format!(
            "SELECT {TOKEN_COLS} FROM user_token WHERE 1 = 1"
        ));
        if let Some(uid) = user_id {
            count_qb.push(" AND user_id = ").push_bind(uid);
            list_qb.push(" AND user_id = ").push_bind(uid);
        }
        if let Some(s) = &status {
            let v = token_status_str(s);
            count_qb.push(" AND status = ").push_bind(v);
            list_qb.push(" AND status = ").push_bind(v);
        }
        if let Some(p) = &pat {
            count_qb
                .push(" AND (name LIKE ")
                .push_bind(p)
                .push(" OR token_prefix LIKE ")
                .push_bind(p)
                .push(")");
            list_qb
                .push(" AND (name LIKE ")
                .push_bind(p)
                .push(" OR token_prefix LIKE ")
                .push_bind(p)
                .push(")");
        }
        let total: i64 = count_qb.build_query_scalar().fetch_one(&self.pool).await?;
        list_qb
            .push(" ORDER BY id DESC LIMIT ")
            .push_bind(limit)
            .push(" OFFSET ")
            .push_bind(offset);
        let rows = list_qb.build().fetch_all(&self.pool).await?;
        let mut items = Vec::with_capacity(rows.len());
        for r in &rows {
            items.push(row_to_token(r)?);
        }
        Ok(Paged {
            items,
            total: total as u64,
            page: q.page.max(1),
            page_size: q.page_size.clamp(1, 100),
        })
    }

    /// 只更新请求里提供的字段（先读后写）。
    pub async fn update_token(
        &self,
        id: i64,
        req: &UpdateTokenRequest,
    ) -> anyhow::Result<Option<UserToken>> {
        let Some(cur) = self.get_token(id).await? else {
            return Ok(None);
        };
        let now = Utc::now();
        let name = req.name.clone().unwrap_or(cur.name);
        let device_type = req.device_type.clone().or(cur.device_type);
        let max_sessions = req
            .max_concurrent_sessions
            .unwrap_or(cur.max_concurrent_sessions);
        let max_devices = req.max_bound_devices.unwrap_or(cur.max_bound_devices);
        let status = req.status.clone().unwrap_or(cur.status);
        let expires_at = req.expires_at.or(cur.expires_at);
        let locale = req.locale.clone().unwrap_or(cur.locale);
        let allowed_groups = req.allowed_groups.clone().unwrap_or(cur.allowed_groups);
        let denied_channels = req.denied_channels.clone().unwrap_or(cur.denied_channels);
        let features = req.features.clone().unwrap_or(cur.features);
        sqlx::query(
            "UPDATE user_token SET name = ?, device_type = ?, max_concurrent_sessions = ?, max_bound_devices = ?, status = ?, expires_at = ?, locale = ?, allowed_groups = ?, denied_channels = ?, features = ?, updated_at = ? WHERE id = ?",
        )
        .bind(name)
        .bind(device_type)
        .bind(i64::from(max_sessions))
        .bind(i64::from(max_devices))
        .bind(token_status_str(&status))
        .bind(expires_at.map(epoch))
        .bind(locale)
        .bind(json_text(&serde_json::json!(allowed_groups)))
        .bind(json_text(&serde_json::json!(denied_channels)))
        .bind(json_text(&serde_json::to_value(&features)?))
        .bind(epoch(now))
        .bind(id)
        .execute(&self.pool)
        .await?;
        self.get_token(id).await
    }
}
