//! 系统设置（`system_setting`，开发文档 §3.2）。
//! 缺失的键由调用方用 `iptv_core::default_settings()` 兜底；
//! 这里只做存取，不在这里硬编码默认值（避免两处默认值漂移）。
// 作者: Passwd Word

use super::*;

impl Store {
    /// 单键读取（M1：60 秒整表缓存；缓存失效或过期时一次读出整表填缓存）。
    /// 缺失的键返回 None，由调用方用 `iptv_core::default_setting()` 兜底。
    pub async fn get_setting(&self, key: &str) -> anyhow::Result<Option<serde_json::Value>> {
        if let Some(v) = self.get_setting_cached(key) {
            return Ok(Some(v));
        }
        // 缓存未命中/过期：整表读出并填缓存（一次查询供后续 60 秒内所有键用）。
        let all = self.list_settings().await?;
        let mut map = HashMap::with_capacity(all.len());
        for s in all {
            map.insert(s.key, s.value);
        }
        let value = map.get(key).cloned();
        if let Ok(mut cache) = self.settings_cache.write() {
            *cache = Some((std::time::Instant::now(), map));
        }
        Ok(value)
    }

    /// 缓存命中才返回（不触 DB）；过期缓存视为未命中。
    fn get_setting_cached(&self, key: &str) -> Option<serde_json::Value> {
        let cache = self.settings_cache.read().ok()?;
        let (at, map) = cache.as_ref()?;
        if at.elapsed() > std::time::Duration::from_secs(60) {
            return None;
        }
        map.get(key).cloned()
    }

    /// 整型设置：读不到或类型不对时用 `iptv_core::default_setting()` 兜底，
    /// 再没有则用 0（调用方应保证键在默认表里）。
    pub async fn get_setting_i64(&self, key: &str) -> i64 {
        if let Ok(Some(v)) = self.get_setting(key).await {
            if let Some(n) = v.as_i64() {
                return n;
            }
        }
        iptv_core::default_setting(key)
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
    }

    /// 字符串设置：读不到时用默认值兜底。
    pub async fn get_setting_string(&self, key: &str) -> String {
        if let Ok(Some(v)) = self.get_setting(key).await {
            if let Some(s) = v.as_str() {
                return s.to_string();
            }
        }
        iptv_core::default_setting(key)
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_default()
    }

    /// 字符串数组设置（如 `available_locales`）：读不到时用默认值兜底。
    pub async fn get_setting_string_list(&self, key: &str) -> Vec<String> {
        if let Ok(Some(v)) = self.get_setting(key).await {
            if let Some(arr) = v.as_array() {
                return arr
                    .iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect();
            }
        }
        iptv_core::default_setting(key)
            .and_then(|v| v.as_array().cloned())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default()
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
        // 写后清空本进程缓存，下次读重新加载（跨进程最多 stale 60 秒）。
        if let Ok(mut cache) = self.settings_cache.write() {
            *cache = None;
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
            let value = r_string(r, "setting_value")?;
            out.push(SystemSetting {
                key,
                value: serde_json::from_str(&value).unwrap_or(serde_json::Value::Null),
                updated_at: r_ts(r, "updated_at")?,
            });
        }
        Ok(out)
    }
}
