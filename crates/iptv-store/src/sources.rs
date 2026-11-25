//! 订阅源与同步日志（`playlist_source` / `sync_log`，开发文档 §7.3/§7.4/§9）。
// 作者: Passwd Word

use super::*;
use chrono::Duration;

pub(crate) fn row_to_source(row: &AnyRow) -> anyhow::Result<PlaylistSource> {
    Ok(PlaylistSource {
        id: r_i64(row, "id")?,
        name: r_string(row, "name")?,
        url: r_opt_string(row, "url")?,
        sync_interval_sec: r_i64(row, "sync_interval_sec")?,
        http_headers: r_json(row, "http_headers"),
        epg_url: r_opt_string(row, "epg_url")?,
        on_missing: match r_string(row, "on_missing")?.as_str() {
            "delete" => OnMissing::Delete,
            _ => OnMissing::Disable,
        },
        auto_enable_new: r_bool(row, "auto_enable_new")?,
        enabled: r_bool(row, "enabled")?,
        last_sync_at: r_opt_ts(row, "last_sync_at")?,
        last_status: r_opt_string(row, "last_status")?,
        last_error: r_opt_string(row, "last_error")?,
        next_sync_at: r_opt_ts(row, "next_sync_at")?,
        created_at: r_ts(row, "created_at")?,
    })
}

pub(crate) fn row_to_sync_log(row: &AnyRow) -> anyhow::Result<SyncLog> {
    Ok(SyncLog {
        id: r_i64(row, "id")?,
        source_id: r_i64(row, "source_id")?,
        started_at: r_ts(row, "started_at")?,
        finished_at: r_opt_ts(row, "finished_at")?,
        status: r_string(row, "status")?,
        added: r_i64(row, "added")?,
        updated: r_i64(row, "updated")?,
        removed: r_i64(row, "removed")?,
        error: r_opt_string(row, "error")?,
    })
}

const SOURCE_COLS: &str = "id, name, url, sync_interval_sec, http_headers, epg_url, on_missing, auto_enable_new, enabled, last_sync_at, last_status, last_error, next_sync_at, created_at";
const SYNC_LOG_COLS: &str =
    "id, source_id, started_at, finished_at, status, added, updated, removed, error";

impl Store {
    pub async fn create_source(&self, req: &CreateSourceRequest) -> anyhow::Result<PlaylistSource> {
        let now = Utc::now();
        // §7.4：同步周期最小 300 秒。
        let interval = req.sync_interval_sec.unwrap_or(3600).max(300);
        let id = insert_id!(
            self,
            "INSERT INTO playlist_source (name, url, sync_interval_sec, http_headers, epg_url, on_missing, auto_enable_new, enabled, last_sync_at, last_status, last_error, next_sync_at, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &req.name,
            req.url.clone(),
            interval,
            json_text(&req.http_headers.clone().unwrap_or(serde_json::json!({}))),
            req.epg_url.clone(),
            match req.on_missing.clone().unwrap_or(OnMissing::Disable) {
                OnMissing::Disable => "disable",
                OnMissing::Delete => "delete",
            },
            i64::from(req.auto_enable_new.unwrap_or(true)),
            i64::from(req.enabled.unwrap_or(true)),
            None::<i64>,
            None::<String>,
            None::<String>,
            None::<i64>,
            epoch(now),
            epoch(now)
        )?;
        self.get_source(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("新建订阅源后回查失败 id={id}"))
    }

    pub async fn get_source(&self, id: i64) -> anyhow::Result<Option<PlaylistSource>> {
        let row = sqlx::query(&ph(
            &format!("SELECT {SOURCE_COLS} FROM playlist_source WHERE id = ?"),
            self.dialect,
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| row_to_source(&r)).transpose()
    }

    pub async fn list_sources(&self) -> anyhow::Result<Vec<PlaylistSource>> {
        let rows = sqlx::query(&ph(
            &format!("SELECT {SOURCE_COLS} FROM playlist_source ORDER BY id ASC"),
            self.dialect,
        ))
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            out.push(row_to_source(r)?);
        }
        Ok(out)
    }

    pub async fn update_source_sync(
        &self,
        id: i64,
        next_sync_at: DateTime<Utc>,
        last_status: &str,
        last_error: Option<&str>,
    ) -> anyhow::Result<()> {
        let now = Utc::now();
        sqlx::query(
            &ph("UPDATE playlist_source SET last_sync_at = ?, next_sync_at = ?, last_status = ?, last_error = ?, updated_at = ? WHERE id = ?", self.dialect),
        )
        .bind(epoch(now))
        .bind(epoch(next_sync_at))
        .bind(last_status)
        .bind(last_error)
        .bind(epoch(now))
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 原子认领同步任务（M6）：只有未认领、或认领超过 15 分钟未更新
    /// （持有者已崩溃）的源才能被认领；返回 true 表示本次认领成功。
    /// 先查后置的旧写法会让多实例重复跑、崩溃后永久卡 syncing。
    pub async fn claim_source_sync(
        &self,
        id: i64,
        next_sync_at: DateTime<Utc>,
    ) -> anyhow::Result<bool> {
        let now = Utc::now();
        let stale_before = epoch(now - Duration::seconds(15 * 60));
        let n = sqlx::query(
            &ph("UPDATE playlist_source SET last_sync_at = ?, next_sync_at = ?, last_status = 'syncing', last_error = NULL, updated_at = ? WHERE id = ? AND (last_status IS NULL OR last_status <> 'syncing' OR updated_at < ?)", self.dialect),
        )
        .bind(epoch(now))
        .bind(epoch(next_sync_at))
        .bind(epoch(now))
        .bind(id)
        .bind(stale_before)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n > 0)
    }

    pub async fn create_sync_log(&self, source_id: i64) -> anyhow::Result<i64> {
        let now = Utc::now();
        insert_id!(
            self,
            "INSERT INTO sync_log (source_id, started_at, finished_at, status, added, updated, removed, error) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            source_id,
            epoch(now),
            None::<i64>,
            "running",
            0i64,
            0i64,
            0i64,
            None::<String>
        )
    }

    pub async fn finish_sync_log(
        &self,
        id: i64,
        status: &str,
        added: i64,
        updated: i64,
        removed: i64,
        error: Option<&str>,
    ) -> anyhow::Result<()> {
        let now = Utc::now();
        sqlx::query(
            &ph("UPDATE sync_log SET finished_at = ?, status = ?, added = ?, updated = ?, removed = ?, error = ? WHERE id = ?", self.dialect),
        )
        .bind(epoch(now))
        .bind(status)
        .bind(added)
        .bind(updated)
        .bind(removed)
        .bind(error)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_sync_logs(
        &self,
        source_id: i64,
        q: &PageQuery,
    ) -> anyhow::Result<Paged<SyncLog>> {
        let (limit, offset) = page_bounds(q);
        let total: i64 = sqlx::query_scalar(&ph(
            "SELECT COUNT(*) FROM sync_log WHERE source_id = ?",
            self.dialect,
        ))
        .bind(source_id)
        .fetch_one(&self.pool)
        .await?;
        let rows = sqlx::query(&ph(&format!(
            "SELECT {SYNC_LOG_COLS} FROM sync_log WHERE source_id = ? ORDER BY started_at DESC, id DESC LIMIT ? OFFSET ?"
        ), self.dialect))
        .bind(source_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        let mut items = Vec::with_capacity(rows.len());
        for r in &rows {
            items.push(row_to_sync_log(r)?);
        }
        Ok(Paged {
            items,
            total: total as u64,
            page: q.page.max(1),
            page_size: q.page_size.clamp(1, 100),
        })
    }
}
