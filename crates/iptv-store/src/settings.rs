//! 系统设置（`system_setting`，开发文档 §3.2）。
//! 缺失的键由调用方用 `iptv_core::default_settings()` 兜底；
//! 这里只做存取，不在这里硬编码默认值（避免两处默认值漂移）。
// 作者: Passwd Word

use super::*;

impl Store {
    pub async fn get_setting(&self, key: &str) -> anyhow::Result<Option<serde_json::Value>> {
        let row = sqlx::query(&ph(
            "SELECT setting_value FROM system_setting WHERE setting_key = ?",
            self.dialect,
        ))
        .bind(key)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some(r) => {
                let s: String = r.try_get("setting_value")?;
                Ok(serde_json::from_str(&s).ok())
            }
            None => Ok(None),
        }
    }

    /// 先 UPDATE，命中 0 行再 INSERT（三种方言都没有统一的 UPSERT 语法，
    /// §10.2 坑 #5 同源问题；单键更新竞态可接受，见 `set_setting` 注释）。
    pub async fn set_setting(&self, key: &str, value: &serde_json::Value) -> anyhow::Result<()> {
        let now = Utc::now();
        let n = sqlx::query(&ph(
            "UPDATE system_setting SET setting_value = ?, updated_at = ? WHERE setting_key = ?",
            self.dialect,
        ))
        .bind(json_text(value))
        .bind(epoch(now))
        .bind(key)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if n == 0 {
            sqlx::query(
                &ph("INSERT INTO system_setting (setting_key, setting_value, updated_at) VALUES (?, ?, ?)", self.dialect),
            )
            .bind(key)
            .bind(json_text(value))
            .bind(epoch(now))
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    pub async fn list_settings(&self) -> anyhow::Result<Vec<SystemSetting>> {
        let rows = sqlx::query(
            "SELECT setting_key, setting_value, updated_at FROM system_setting ORDER BY setting_key ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            let key: String = r.try_get("setting_key")?;
            let value: String = r.try_get("setting_value")?;
            out.push(SystemSetting {
                key,
                value: serde_json::from_str(&value).unwrap_or(serde_json::Value::Null),
                updated_at: r_ts(r, "updated_at")?,
            });
        }
        Ok(out)
    }
}
