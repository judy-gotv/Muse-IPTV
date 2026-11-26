//! 套餐与频道分组（`plan` / `channel_group`，开发文档 §3.2 / §5.1）。
// 作者: Passwd Word

use super::*;

pub(crate) fn row_to_plan(row: &AnyRow) -> anyhow::Result<Plan> {
    Ok(Plan {
        id: r_i64(row, "id")?,
        name: r_string(row, "name")?,
        max_concurrent_sessions: r_i64(row, "max_concurrent_sessions")? as i32,
        max_bound_devices: r_i64(row, "max_bound_devices")? as i32,
        channel_groups: str_list(&r_json(row, "channel_groups")),
        duration_days: r_i64(row, "duration_days")? as i32,
        price_cents: r_i64(row, "price_cents")?,
        currency: r_string(row, "currency")?,
        created_at: r_ts(row, "created_at")?,
    })
}

pub(crate) fn row_to_group(row: &AnyRow) -> anyhow::Result<ChannelGroup> {
    Ok(ChannelGroup {
        id: r_i64(row, "id")?,
        name: r_string(row, "name")?,
        sort_weight: r_i64(row, "sort_weight")? as i32,
        icon: r_opt_string(row, "icon")?,
        is_default: r_bool(row, "is_default")?,
    })
}

const PLAN_COLS: &str = "id, name, max_concurrent_sessions, max_bound_devices, channel_groups, duration_days, price_cents, currency, created_at";
const GROUP_COLS: &str = "id, name, sort_weight, icon, is_default";

impl Store {
    pub async fn create_plan(&self, req: &CreatePlanRequest) -> anyhow::Result<Plan> {
        let now = Utc::now();
        let id = insert_id!(
            self,
            "INSERT INTO plan (name, max_concurrent_sessions, max_bound_devices, channel_groups, duration_days, price_cents, currency, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            &req.name,
            i64::from(req.max_concurrent_sessions),
            i64::from(req.max_bound_devices),
            json_text(&serde_json::json!(req.channel_groups)),
            i64::from(req.duration_days),
            req.price_cents,
            req.currency.clone().unwrap_or_else(|| "CNY".to_string()),
            epoch(now)
        )?;
        self.get_plan(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("新建套餐后回查失败 id={id}"))
    }

    pub async fn list_plans(&self) -> anyhow::Result<Vec<Plan>> {
        let rows = sqlx::query(&ph(
            &format!("SELECT {PLAN_COLS} FROM plan ORDER BY id ASC"),
            self.dialect,
        ))
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            out.push(row_to_plan(r)?);
        }
        Ok(out)
    }

    pub async fn get_plan(&self, id: i64) -> anyhow::Result<Option<Plan>> {
        let row = sqlx::query(&ph(
            &format!("SELECT {PLAN_COLS} FROM plan WHERE id = ?"),
            self.dialect,
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| row_to_plan(&r)).transpose()
    }

    pub async fn create_group(&self, req: &CreateGroupRequest) -> anyhow::Result<ChannelGroup> {
        let id = insert_id!(
            self,
            "INSERT INTO channel_group (name, sort_weight, icon, is_default) VALUES (?, ?, ?, ?)",
            &req.name,
            i64::from(req.sort_weight.unwrap_or(0)),
            req.icon.clone(),
            i64::from(req.is_default.unwrap_or(false))
        )?;
        let row = sqlx::query(&ph(
            &format!("SELECT {GROUP_COLS} FROM channel_group WHERE id = ?"),
            self.dialect,
        ))
        .bind(id)
        .fetch_one(&self.pool)
        .await?;
        row_to_group(&row)
    }

    pub async fn list_groups(&self) -> anyhow::Result<Vec<ChannelGroup>> {
        let rows = sqlx::query(&ph(
            &format!("SELECT {GROUP_COLS} FROM channel_group ORDER BY sort_weight ASC, id ASC"),
            self.dialect,
        ))
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            out.push(row_to_group(r)?);
        }
        Ok(out)
    }

    pub async fn get_group(&self, id: i64) -> anyhow::Result<Option<ChannelGroup>> {
        let row = sqlx::query(&ph(
            &format!("SELECT {GROUP_COLS} FROM channel_group WHERE id = ?"),
            self.dialect,
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| row_to_group(&r)).transpose()
    }

    /// M19d：只更新请求里提供的字段（先读后写）。
    pub async fn update_plan(
        &self,
        id: i64,
        req: &UpdatePlanRequest,
    ) -> anyhow::Result<Option<Plan>> {
        let Some(cur) = self.get_plan(id).await? else {
            return Ok(None);
        };
        let name = req.name.clone().unwrap_or(cur.name);
        let max_sessions = req
            .max_concurrent_sessions
            .unwrap_or(cur.max_concurrent_sessions);
        let max_devices = req.max_bound_devices.unwrap_or(cur.max_bound_devices);
        let groups = req.channel_groups.clone().unwrap_or(cur.channel_groups);
        let duration = req.duration_days.unwrap_or(cur.duration_days);
        let price = req.price_cents.unwrap_or(cur.price_cents);
        let currency = req.currency.clone().unwrap_or(cur.currency);
        sqlx::query(
            &ph("UPDATE plan SET name = ?, max_concurrent_sessions = ?, max_bound_devices = ?, channel_groups = ?, duration_days = ?, price_cents = ?, currency = ? WHERE id = ?", self.dialect),
        )
        .bind(name)
        .bind(i64::from(max_sessions))
        .bind(i64::from(max_devices))
        .bind(json_text(&serde_json::json!(groups)))
        .bind(i64::from(duration))
        .bind(price)
        .bind(currency)
        .bind(id)
        .execute(&self.pool)
        .await?;
        self.get_plan(id).await
    }

    /// M19d：删套餐前由 handler 查引用；此处只删行。
    pub async fn delete_plan(&self, id: i64) -> anyhow::Result<bool> {
        let n = sqlx::query(&ph("DELETE FROM plan WHERE id = ?", self.dialect))
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(n > 0)
    }

    /// M19d：有多少用户正用该套餐（删前拒绝的依据）。
    pub async fn count_users_with_plan(&self, plan_id: i64) -> anyhow::Result<i64> {
        let n: i64 = sqlx::query_scalar(&ph(
            "SELECT COUNT(*) FROM app_user WHERE plan_id = ?",
            self.dialect,
        ))
        .bind(plan_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(n)
    }

    /// M19e：只更新请求里提供的字段（先读后写）。
    pub async fn update_group(
        &self,
        id: i64,
        req: &UpdateGroupRequest,
    ) -> anyhow::Result<Option<ChannelGroup>> {
        let Some(cur) = self.get_group(id).await? else {
            return Ok(None);
        };
        let name = req.name.clone().unwrap_or(cur.name);
        let sort_weight = req.sort_weight.unwrap_or(cur.sort_weight);
        let icon = req.icon.clone().or(cur.icon);
        let is_default = req.is_default.unwrap_or(cur.is_default);
        sqlx::query(
            &ph("UPDATE channel_group SET name = ?, sort_weight = ?, icon = ?, is_default = ? WHERE id = ?", self.dialect),
        )
        .bind(name)
        .bind(i64::from(sort_weight))
        .bind(icon)
        .bind(i64::from(is_default))
        .bind(id)
        .execute(&self.pool)
        .await?;
        self.get_group(id).await
    }

    /// M19e：删分组——先清 `channel_group_map` 里的映射（频道本身保留，
    /// 只是不再属于该分组），再删分组行。
    pub async fn delete_group(&self, id: i64) -> anyhow::Result<bool> {
        sqlx::query(&ph(
            "DELETE FROM channel_group_map WHERE group_id = ?",
            self.dialect,
        ))
        .bind(id)
        .execute(&self.pool)
        .await?;
        let n = sqlx::query(&ph("DELETE FROM channel_group WHERE id = ?", self.dialect))
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(n > 0)
    }
}
