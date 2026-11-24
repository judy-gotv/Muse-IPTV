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
        let rows = sqlx::query(&format!("SELECT {PLAN_COLS} FROM plan ORDER BY id ASC"))
            .fetch_all(&self.pool)
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            out.push(row_to_plan(r)?);
        }
        Ok(out)
    }

    pub async fn get_plan(&self, id: i64) -> anyhow::Result<Option<Plan>> {
        let row = sqlx::query(&format!("SELECT {PLAN_COLS} FROM plan WHERE id = ?"))
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
        let row = sqlx::query(&format!(
            "SELECT {GROUP_COLS} FROM channel_group WHERE id = ?"
        ))
        .bind(id)
        .fetch_one(&self.pool)
        .await?;
        row_to_group(&row)
    }

    pub async fn list_groups(&self) -> anyhow::Result<Vec<ChannelGroup>> {
        let rows = sqlx::query(&format!(
            "SELECT {GROUP_COLS} FROM channel_group ORDER BY sort_weight ASC, id ASC"
        ))
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            out.push(row_to_group(r)?);
        }
        Ok(out)
    }
}
