//! 用户（`app_user`，开发文档 §3.2 / §9）。
// 作者: Passwd Word

use super::*;

pub(crate) fn row_to_user(row: &AnyRow) -> anyhow::Result<AppUser> {
    Ok(AppUser {
        id: r_i64(row, "id")?,
        display_name: r_string(row, "display_name")?,
        email: r_opt_string(row, "email")?,
        phone: r_opt_string(row, "phone")?,
        plan_id: r_opt_i64(row, "plan_id")?,
        status: match r_string(row, "status")?.as_str() {
            "suspended" => UserStatus::Suspended,
            "expired" => UserStatus::Expired,
            _ => UserStatus::Active,
        },
        locale: r_string(row, "locale")?,
        remark: r_opt_string(row, "remark")?,
        expires_at: r_opt_ts(row, "expires_at")?,
        created_at: r_ts(row, "created_at")?,
        updated_at: r_ts(row, "updated_at")?,
    })
}

pub(crate) fn status_str(s: &UserStatus) -> &'static str {
    match s {
        UserStatus::Active => "active",
        UserStatus::Suspended => "suspended",
        UserStatus::Expired => "expired",
    }
}

const USER_COLS: &str = "id, display_name, email, phone, plan_id, status, locale, remark, expires_at, created_at, updated_at";

impl Store {
    pub async fn create_user(&self, req: &CreateUserRequest) -> anyhow::Result<AppUser> {
        let now = Utc::now();
        let id = insert_id!(
            self,
            "INSERT INTO app_user (display_name, email, phone, plan_id, status, locale, remark, expires_at, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &req.display_name,
            req.email.clone(),
            req.phone.clone(),
            req.plan_id,
            "active",
            req.locale.clone().unwrap_or_else(|| "zh-Hant-TW".to_string()),
            req.remark.clone(),
            req.expires_at.map(epoch),
            epoch(now),
            epoch(now)
        )?;
        self.get_user(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("新建用户后回查失败 id={id}"))
    }

    pub async fn get_user(&self, id: i64) -> anyhow::Result<Option<AppUser>> {
        let row = sqlx::query(&ph(
            &format!("SELECT {USER_COLS} FROM app_user WHERE id = ?"),
            self.dialect,
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| row_to_user(&r)).transpose()
    }

    pub async fn list_users(
        &self,
        q: &PageQuery,
        status: Option<UserStatus>,
    ) -> anyhow::Result<Paged<AppUser>> {
        let (limit, offset) = page_bounds(q);
        let pat = like_pattern(&q.q);
        // 不用 sqlx::QueryBuilder：Any 的 push_bind 在 PG 下仍写 `?`
        // （见 lib.rs `ph`），这里手拼字符串 + `ph()` + 按顺序 `.bind()`。
        let mut count_sql = String::from("SELECT COUNT(*) FROM app_user WHERE 1 = 1");
        let mut list_sql = format!("SELECT {USER_COLS} FROM app_user WHERE 1 = 1");
        if status.is_some() {
            count_sql.push_str(" AND status = ?");
            list_sql.push_str(" AND status = ?");
        }
        if pat.is_some() {
            count_sql.push_str(" AND (display_name LIKE ? OR email LIKE ?)");
            list_sql.push_str(" AND (display_name LIKE ? OR email LIKE ?)");
        }
        let count_sql = ph(&count_sql, self.dialect);
        let mut cq = sqlx::query_scalar::<_, i64>(&count_sql);
        if let Some(s) = &status {
            cq = cq.bind(status_str(s));
        }
        if let Some(p) = &pat {
            cq = cq.bind(p).bind(p);
        }
        let total: i64 = cq.fetch_one(&self.pool).await?;
        list_sql.push_str(" ORDER BY id DESC LIMIT ? OFFSET ?");
        let list_sql = ph(&list_sql, self.dialect);
        let mut lq = sqlx::query(&list_sql);
        if let Some(s) = &status {
            lq = lq.bind(status_str(s));
        }
        if let Some(p) = &pat {
            lq = lq.bind(p).bind(p);
        }
        let rows = lq.bind(limit).bind(offset).fetch_all(&self.pool).await?;
        let mut items = Vec::with_capacity(rows.len());
        for r in &rows {
            items.push(row_to_user(r)?);
        }
        Ok(Paged {
            items,
            total: total as u64,
            page: q.page.max(1),
            page_size: q.page_size.clamp(1, 100),
        })
    }

    /// 只更新请求里提供的字段（其余字段保持原值，先读后写，避免动态 SET 拼接）。
    pub async fn update_user(
        &self,
        id: i64,
        req: &UpdateUserRequest,
    ) -> anyhow::Result<Option<AppUser>> {
        let Some(cur) = self.get_user(id).await? else {
            return Ok(None);
        };
        let now = Utc::now();
        let display_name = req.display_name.clone().unwrap_or(cur.display_name);
        let email = req.email.clone().or(cur.email);
        let phone = req.phone.clone().or(cur.phone);
        let plan_id = req.plan_id.or(cur.plan_id);
        let status = req.status.clone().unwrap_or(cur.status);
        let locale = req.locale.clone().unwrap_or(cur.locale);
        let remark = req.remark.clone().or(cur.remark);
        let expires_at = req.expires_at.or(cur.expires_at);
        sqlx::query(
            &ph("UPDATE app_user SET display_name = ?, email = ?, phone = ?, plan_id = ?, status = ?, locale = ?, remark = ?, expires_at = ?, updated_at = ? WHERE id = ?", self.dialect),
        )
        .bind(display_name)
        .bind(email)
        .bind(phone)
        .bind(plan_id)
        .bind(status_str(&status))
        .bind(locale)
        .bind(remark)
        .bind(expires_at.map(epoch))
        .bind(epoch(now))
        .bind(id)
        .execute(&self.pool)
        .await?;
        self.get_user(id).await
    }
}
