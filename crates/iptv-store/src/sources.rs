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

    /// M19c：只更新请求里提供的字段（先读后写）；同步周期与创建时一致，
    /// 最小 300 秒。
    pub async fn update_source(
        &self,
        id: i64,
        req: &UpdateSourceRequest,
    ) -> anyhow::Result<Option<PlaylistSource>> {
        let Some(cur) = self.get_source(id).await? else {
            return Ok(None);
        };
        let now = Utc::now();
        let name = req.name.clone().unwrap_or(cur.name);
        let url = req.url.clone().or(cur.url);
        let interval = req
            .sync_interval_sec
            .unwrap_or(cur.sync_interval_sec)
            .max(300);
        let http_headers = req.http_headers.clone().unwrap_or(cur.http_headers);
        let epg_url = req.epg_url.clone().or(cur.epg_url);
        let on_missing = req.on_missing.clone().unwrap_or(cur.on_missing);
        let auto_enable_new = req.auto_enable_new.unwrap_or(cur.auto_enable_new);
        let enabled = req.enabled.unwrap_or(cur.enabled);
        sqlx::query(
            &ph("UPDATE playlist_source SET name = ?, url = ?, sync_interval_sec = ?, http_headers = ?, epg_url = ?, on_missing = ?, auto_enable_new = ?, enabled = ?, updated_at = ? WHERE id = ?", self.dialect),
        )
        .bind(name)
        .bind(url)
        .bind(interval)
        .bind(json_text(&http_headers))
        .bind(epg_url)
        .bind(match on_missing {
            OnMissing::Disable => "disable",
            OnMissing::Delete => "delete",
        })
        .bind(i64::from(auto_enable_new))
        .bind(i64::from(enabled))
        .bind(epoch(now))
        .bind(id)
        .execute(&self.pool)
        .await?;
        self.get_source(id).await
    }

    /// M19c：删订阅源——先清其同步日志（已删源的日志无处展示），再删源行；
    /// 该源同步出的频道保留（内容独立存在，删源不等同删台）。
    pub async fn delete_source(&self, id: i64) -> anyhow::Result<bool> {
        sqlx::query(&ph(
            "DELETE FROM sync_log WHERE source_id = ?",
            self.dialect,
        ))
        .bind(id)
        .execute(&self.pool)
        .await?;
        let n = sqlx::query(&ph(
            "DELETE FROM playlist_source WHERE id = ?",
            self.dialect,
        ))
        .bind(id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n > 0)
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
    /// M-D（v0.1.6）：上传/同步/手动触发三路共用同一认领——Worker
    /// `sync_source` 与 API `run_upload_task`（upsert 前）都调本函数，
    /// 未认领到的一方直接失败/跳过，不再各跑各的 `upsert_channels`；
    /// `trigger_sync` 用 `is_source_syncing` 读同一状态，认领被占住时回 409。
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

    /// M-D（v0.1.6）：源是否正被认领同步中（上传/同步/手动触发三路共用的
    /// 认领状态读端）。与 `claim_source_sync` 同一条陈旧规则：`last_status`
    /// 为 `syncing` 且 `updated_at` 在 15 分钟内才算在同步中；超 15 分钟
    /// 未更新视为持有者已崩溃，认领可被他人接管，本函数也返回 false。
    /// `trigger_sync` 靠它在认领已被上传/Worker 占住时回 409；源不存在
    /// 返回 false（调用方先做存在性校验）。
    pub async fn is_source_syncing(&self, id: i64) -> anyhow::Result<bool> {
        let row = sqlx::query(&ph(
            "SELECT last_status, updated_at FROM playlist_source WHERE id = ?",
            self.dialect,
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(false);
        };
        let status = r_opt_string(&row, "last_status")?;
        if status.as_deref() != Some("syncing") {
            return Ok(false);
        }
        let updated_at = r_i64(&row, "updated_at")?;
        let stale_before = epoch(Utc::now() - Duration::seconds(15 * 60));
        Ok(updated_at >= stale_before)
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

    /// 最近连续失败次数（M18）：按 `sync_log` 从新到旧数 `failed` 行，
    /// 遇到 `ok` 即中断；`running` 行跳过不中断——认领失败会留下一条
    /// 永不完结的 `running` 行（`sync_source` 先建 log 后认领），
    /// 崩溃中途的 `running` 行也不应把已计失败清零。
    /// 成功后 `sync_log` 里最新行是 `ok`，故重启后恢复为 0，与
    /// `record_success` 清内存计数的语义一致。最多回溯 128 行。
    pub async fn consecutive_sync_failures(&self, source_id: i64) -> anyhow::Result<u32> {
        let rows = sqlx::query(&ph(
            "SELECT status FROM sync_log WHERE source_id = ? ORDER BY started_at DESC, id DESC LIMIT ?",
            self.dialect,
        ))
        .bind(source_id)
        .bind(128i64)
        .fetch_all(&self.pool)
        .await?;
        let mut n = 0u32;
        for r in &rows {
            match r_string(r, "status")?.as_str() {
                "failed" => n += 1,
                "ok" => break,
                _ => continue,
            }
        }
        Ok(n)
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
