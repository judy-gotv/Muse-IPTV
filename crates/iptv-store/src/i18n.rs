//! 文案覆盖（`i18n_override`，开发文档 §12.8 / m22）。
//!
//! 后台改个别文案不必发版：覆盖只作用于指定 key 与 locale，
//! `I18n::t` 先查覆盖再查 FTL。`key` 列在 MySQL 是保留字，
//! SQL 里按方言引用（MySQL 反引号，其余双引号）。
// 作者: Passwd Word

use super::*;

/// `key` 列的方言引用形式。
fn key_col(dialect: Dialect) -> &'static str {
    match dialect {
        Dialect::MySql => "`key`",
        _ => "\"key\"",
    }
}

fn row_to_override(r: &AnyRow) -> anyhow::Result<iptv_dto::I18nOverride> {
    Ok(iptv_dto::I18nOverride {
        id: r_i64(r, "id")?,
        locale: r_string(r, "locale")?,
        // 列名本身就是 `key`（引用形式只出现在 SQL 文本里，见 key_col）。
        key: r_string(r, "key")?,
        value: r_string(r, "value")?,
        updated_at: r_ts(r, "updated_at")?,
    })
}

impl Store {
    /// 全表读出（API 启动装入进程内覆盖表、后台列表页用）。
    pub async fn list_i18n_overrides(&self) -> anyhow::Result<Vec<iptv_dto::I18nOverride>> {
        let kc = key_col(self.dialect);
        let rows = sqlx::query(&format!(
            "SELECT id, locale, {kc}, value, updated_at FROM i18n_override ORDER BY locale ASC, {kc} ASC"
        ))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_to_override).collect()
    }

    /// 读库装入 `I18n` 进程内覆盖表用的 Map（key 为 `(locale, key)`）。
    pub async fn i18n_overrides_map(&self) -> anyhow::Result<HashMap<(String, String), String>> {
        let mut map = HashMap::new();
        for o in self.list_i18n_overrides().await? {
            map.insert((o.locale, o.key), o.value);
        }
        Ok(map)
    }

    /// 单条读取（不存在返回 None）。
    pub async fn get_i18n_override(
        &self,
        locale: &str,
        key: &str,
    ) -> anyhow::Result<Option<iptv_dto::I18nOverride>> {
        let kc = key_col(self.dialect);
        let row = sqlx::query(&format!(
            "SELECT id, locale, {kc}, value, updated_at FROM i18n_override WHERE locale = ? AND {kc} = ?"
        ))
        .bind(locale)
        .bind(key)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| row_to_override(&r)).transpose()
    }

    /// 先 UPDATE，命中 0 行再 INSERT（与 `set_setting` 同款三方言写法，
    /// §10.2 坑 #5；`value` 为空串时调用方应先调 `delete_i18n_override`）。
    pub async fn upsert_i18n_override(
        &self,
        locale: &str,
        key: &str,
        value: &str,
    ) -> anyhow::Result<()> {
        let kc = key_col(self.dialect);
        let now = epoch(Utc::now());
        let n = sqlx::query(&ph(
            &format!(
                "UPDATE i18n_override SET value = ?, updated_at = ? WHERE locale = ? AND {kc} = ?"
            ),
            self.dialect,
        ))
        .bind(value)
        .bind(now)
        .bind(locale)
        .bind(key)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if n == 0 {
            sqlx::query(&ph(
                &format!(
                    "INSERT INTO i18n_override (locale, {kc}, value, updated_at) VALUES (?, ?, ?, ?)"
                ),
                self.dialect,
            ))
            .bind(locale)
            .bind(key)
            .bind(value)
            .bind(now)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    /// 删除单条覆盖（不存在返回 Ok(false)）。
    pub async fn delete_i18n_override(&self, locale: &str, key: &str) -> anyhow::Result<bool> {
        let kc = key_col(self.dialect);
        let n = sqlx::query(&ph(
            &format!("DELETE FROM i18n_override WHERE locale = ? AND {kc} = ?"),
            self.dialect,
        ))
        .bind(locale)
        .bind(key)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n > 0)
    }
}
