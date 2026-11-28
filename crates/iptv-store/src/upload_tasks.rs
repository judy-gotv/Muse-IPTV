//! 异步上传任务落库（m-uptask + N5，v0.1.6；开发文档 §7.1 异步解析）。
//!
//! 取代 `iptv-api::upload_tasks` 旧的进程内 `HashMap` 注册表（M15 修前的
//! 票据内存实现同款取舍）：`upload_task` 表（V8 迁移，三方言）是事实来源。
//! - 进度更新走行级 UPDATE（解析回调只写三个计数，不读行）；状态流转
//!   queued → running → done/failed 与旧内存版一致。
//! - 重启后任务可恢复查询：`get_upload_task` 直接读库；多实例下网关无需
//!   粘性路由。
//! - N5 淘汰（二选一取“两者都要”，原因写在下面）：
//!   1. 完成（done/failed）24 小时后过期：`updated_at` 是最后一次进度
//!      时间，`get` 对已过期终态行直接返回 None（过期任务查 404，不必等
//!      后台清理跑完）；`purge_expired_upload_tasks` 物理删行。
//!   2. 1000 行封顶：只靠 24 小时 TTL 仍可能在一天内被大量上传撑爆表
//!      （一次大上传潮可建上万任务），故 purge 时总量超 1000 行先删最旧
//!      的已完成任务；若仍超（全是 queued/running），再删最旧的 queued
//!      任务——running 任务永不因封顶被删，正在跑的上传不受影响。
// 作者: Passwd Word

use super::*;

/// 任务完成后保留多久（秒）：24 小时。
pub const UPLOAD_TASK_RETENTION_SECS: i64 = 24 * 3600;

/// `upload_task` 表最多保留多少行（封顶 LRU，见模块头注释）。
pub const UPLOAD_TASK_MAX_ROWS: i64 = 1000;

fn status_str(status: UploadTaskStatus) -> &'static str {
    match status {
        UploadTaskStatus::Queued => "queued",
        UploadTaskStatus::Running => "running",
        UploadTaskStatus::Done => "done",
        UploadTaskStatus::Failed => "failed",
    }
}

fn parse_status(s: &str) -> anyhow::Result<UploadTaskStatus> {
    match s {
        "queued" => Ok(UploadTaskStatus::Queued),
        "running" => Ok(UploadTaskStatus::Running),
        "done" => Ok(UploadTaskStatus::Done),
        "failed" => Ok(UploadTaskStatus::Failed),
        other => anyhow::bail!("未知的 upload_task 状态：{other}"),
    }
}

fn row_to_progress(row: &AnyRow) -> anyhow::Result<(UploadTaskProgress, i64)> {
    let progress = UploadTaskProgress {
        task_id: r_string(row, "task_id")?,
        status: parse_status(&r_string(row, "status")?)?,
        filename: r_string(row, "filename")?,
        source_id: r_opt_i64(row, "source_id")?,
        total_lines: r_i64(row, "total_lines")? as usize,
        channels_parsed: r_i64(row, "channels_parsed")? as usize,
        channels_imported: r_i64(row, "channels_imported")? as usize,
        error_count: r_i64(row, "error_count")? as usize,
        error: r_opt_string(row, "error")?,
    };
    let updated_at = r_i64(row, "updated_at")?;
    Ok((progress, updated_at))
}

const TASK_COLUMNS: &str = "task_id, status, filename, source_id, total_lines, \
    channels_parsed, channels_imported, error_count, error, updated_at";

impl Store {
    /// 新建一个 queued 任务行（POST /upload 立返 task_id 时调用）。
    pub async fn create_upload_task(
        &self,
        progress: &UploadTaskProgress,
        now: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        sqlx::query(&ph(
            "INSERT INTO upload_task (task_id, status, filename, source_id, total_lines, channels_parsed, channels_imported, error_count, error, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            self.dialect,
        ))
        .bind(&progress.task_id)
        .bind(status_str(progress.status))
        .bind(&progress.filename)
        .bind(progress.source_id)
        .bind(progress.total_lines as i64)
        .bind(progress.channels_parsed as i64)
        .bind(progress.channels_imported as i64)
        .bind(progress.error_count as i64)
        .bind(progress.error.as_deref())
        .bind(epoch(now))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 读任务进度。终态（done/failed）且 `updated_at` 超过保留期时返回
    /// `None`（过期任务查 404，即使后台清理还没跑到它）。
    pub async fn get_upload_task(
        &self,
        task_id: &str,
        now: DateTime<Utc>,
    ) -> anyhow::Result<Option<UploadTaskProgress>> {
        let sql = format!("SELECT {TASK_COLUMNS} FROM upload_task WHERE task_id = ?");
        let row = sqlx::query(&ph(&sql, self.dialect))
            .bind(task_id)
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let (progress, updated_at) = row_to_progress(&row)?;
        if matches!(
            progress.status,
            UploadTaskStatus::Done | UploadTaskStatus::Failed
        ) && updated_at <= epoch(now) - UPLOAD_TASK_RETENTION_SECS
        {
            return Ok(None);
        }
        Ok(Some(progress))
    }

    /// 全行 UPDATE（读-改-写：调用方先 `get` 再改字段）；任务不存在返回 false。
    pub async fn update_upload_task(
        &self,
        progress: &UploadTaskProgress,
        now: DateTime<Utc>,
    ) -> anyhow::Result<bool> {
        let n = sqlx::query(&ph(
            "UPDATE upload_task SET status = ?, filename = ?, source_id = ?, total_lines = ?, channels_parsed = ?, channels_imported = ?, error_count = ?, error = ?, updated_at = ? WHERE task_id = ?",
            self.dialect,
        ))
        .bind(status_str(progress.status))
        .bind(&progress.filename)
        .bind(progress.source_id)
        .bind(progress.total_lines as i64)
        .bind(progress.channels_parsed as i64)
        .bind(progress.channels_imported as i64)
        .bind(progress.error_count as i64)
        .bind(progress.error.as_deref())
        .bind(epoch(now))
        .bind(&progress.task_id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n > 0)
    }

    /// 解析进度回调用的行级 UPDATE：只写三个计数 + `updated_at`，不读行。
    /// 解析回调每 2000 行触发一次，单条 UPDATE 足够便宜；任务不存在返回 false。
    pub async fn update_upload_task_progress(
        &self,
        task_id: &str,
        total_lines: i64,
        channels_parsed: i64,
        error_count: i64,
        now: DateTime<Utc>,
    ) -> anyhow::Result<bool> {
        let n = sqlx::query(&ph(
            "UPDATE upload_task SET total_lines = ?, channels_parsed = ?, error_count = ?, updated_at = ? WHERE task_id = ?",
            self.dialect,
        ))
        .bind(total_lines)
        .bind(channels_parsed)
        .bind(error_count)
        .bind(epoch(now))
        .bind(task_id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n > 0)
    }

    /// 置任务状态（+ 可选错误串），行级 UPDATE；任务不存在返回 false。
    pub async fn update_upload_task_status(
        &self,
        task_id: &str,
        status: UploadTaskStatus,
        error: Option<&str>,
        now: DateTime<Utc>,
    ) -> anyhow::Result<bool> {
        let n = sqlx::query(&ph(
            "UPDATE upload_task SET status = ?, error = ?, updated_at = ? WHERE task_id = ?",
            self.dialect,
        ))
        .bind(status_str(status))
        .bind(error)
        .bind(epoch(now))
        .bind(task_id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n > 0)
    }

    /// 表里现有任务行数（测试与封顶逻辑用）。
    pub async fn count_upload_tasks(&self) -> anyhow::Result<i64> {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM upload_task")
            .fetch_one(&self.pool)
            .await?;
        Ok(n)
    }

    /// N5 清理：先删已过期终态行（完成 24 小时以上），再把总量压到
    /// `UPLOAD_TASK_MAX_ROWS` 行以内（先删最旧的已完成任务，不够再删最旧
    /// 的 queued 任务；running 任务永不因封顶被删）。返回删除的总行数。
    pub async fn purge_expired_upload_tasks(&self, now: DateTime<Utc>) -> anyhow::Result<u64> {
        let cutoff = epoch(now) - UPLOAD_TASK_RETENTION_SECS;
        let mut deleted = sqlx::query(&ph(
            "DELETE FROM upload_task WHERE status IN ('done', 'failed') AND updated_at <= ?",
            self.dialect,
        ))
        .bind(cutoff)
        .execute(&self.pool)
        .await?
        .rows_affected();
        // 封顶：总量超了先删最旧的已完成任务。
        let mut total = self.count_upload_tasks().await?;
        if total > UPLOAD_TASK_MAX_ROWS {
            let excess = total - UPLOAD_TASK_MAX_ROWS;
            let ids = self.oldest_terminal_task_ids(excess).await?;
            deleted += self.delete_upload_tasks_by_ids(&ids).await?;
            total = self.count_upload_tasks().await?;
        }
        // 仍超（全是 queued/running）：再删最旧的 queued 任务；running 不动。
        if total > UPLOAD_TASK_MAX_ROWS {
            let excess = total - UPLOAD_TASK_MAX_ROWS;
            let ids = self.oldest_queued_task_ids(excess).await?;
            deleted += self.delete_upload_tasks_by_ids(&ids).await?;
        }
        Ok(deleted)
    }

    async fn oldest_terminal_task_ids(&self, limit: i64) -> anyhow::Result<Vec<String>> {
        let rows = sqlx::query(&ph(
            "SELECT task_id FROM upload_task WHERE status IN ('done', 'failed') ORDER BY updated_at ASC, task_id ASC LIMIT ?",
            self.dialect,
        ))
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(|r| r_string(r, "task_id")).collect()
    }

    async fn oldest_queued_task_ids(&self, limit: i64) -> anyhow::Result<Vec<String>> {
        let rows = sqlx::query(&ph(
            "SELECT task_id FROM upload_task WHERE status = 'queued' ORDER BY updated_at ASC, task_id ASC LIMIT ?",
            self.dialect,
        ))
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(|r| r_string(r, "task_id")).collect()
    }

    async fn delete_upload_tasks_by_ids(&self, ids: &[String]) -> anyhow::Result<u64> {
        if ids.is_empty() {
            return Ok(0);
        }
        let mut sql = String::from("DELETE FROM upload_task WHERE task_id IN ");
        push_in_list(&mut sql, "(", ids.len());
        // `ph()` 返回的 Cow 须先绑到局部变量再取 `&str`：直接写
        // `sqlx::query(&ph(&sql, ...))` 时 Cow 是语句级临时值，`q` 借用它
        // 会跨语句存活，报 E0716（临时值被丢弃时仍被借用）。
        let query_sql = ph(&sql, self.dialect);
        let mut q = sqlx::query(&query_sql);
        for id in ids {
            q = q.bind(id);
        }
        Ok(q.execute(&self.pool).await?.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        store: Store,
        path: std::path::PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    async fn setup() -> Fixture {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("iptv-store-upload-{}-{}.db", std::process::id(), n));
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let store = Store::connect(&url).await.expect("connect sqlite");
        store.migrate().await.expect("migrate");
        Fixture { store, path }
    }

    fn queued(id: &str) -> UploadTaskProgress {
        UploadTaskProgress {
            task_id: id.to_string(),
            status: UploadTaskStatus::Queued,
            filename: "a.m3u".to_string(),
            source_id: None,
            total_lines: 0,
            channels_parsed: 0,
            channels_imported: 0,
            error_count: 0,
            error: None,
        }
    }

    #[tokio::test]
    async fn task_survives_reconnect() {
        // 模拟重启：同一库文件上新开 Store，任务进度仍可查。
        let fx = setup().await;
        let now = Utc::now();
        fx.store
            .create_upload_task(&queued("task-1"), now)
            .await
            .unwrap();
        fx.store
            .update_upload_task_status("task-1", UploadTaskStatus::Running, None, now)
            .await
            .unwrap();
        fx.store
            .update_upload_task_progress("task-1", 2000, 900, 1, now)
            .await
            .unwrap();
        // “重启”：同一库文件上新开连接、新跑迁移、新 Store 实例。
        // （旧 Store 实例留在世不影响结论：新实例直接读库里已落地的行。）
        let url = format!("sqlite://{}?mode=rwc", fx.path.display());
        let store2 = Store::connect(&url).await.expect("reconnect sqlite");
        store2.migrate().await.expect("migrate again");
        let p = store2
            .get_upload_task("task-1", Utc::now())
            .await
            .unwrap()
            .expect("task survives restart");
        assert_eq!(p.status, UploadTaskStatus::Running);
        assert_eq!(
            (p.total_lines, p.channels_parsed, p.error_count),
            (2000, 900, 1)
        );
        assert!(store2
            .get_upload_task("nope", Utc::now())
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn expired_terminal_task_reads_as_missing() {
        let fx = setup().await;
        let now = Utc::now();
        fx.store
            .create_upload_task(&queued("old-1"), now)
            .await
            .unwrap();
        fx.store
            .update_upload_task_status("old-1", UploadTaskStatus::Done, None, now)
            .await
            .unwrap();
        // 25 小时后查：终态已过期，返回 None（404）。
        let later = now + chrono::Duration::hours(25);
        assert!(fx
            .store
            .get_upload_task("old-1", later)
            .await
            .unwrap()
            .is_none());
        // 未过期时仍可查；running 任务即使旧也不过期。
        assert!(fx
            .store
            .get_upload_task("old-1", now)
            .await
            .unwrap()
            .is_some());
        fx.store
            .create_upload_task(&queued("run-1"), now)
            .await
            .unwrap();
        fx.store
            .update_upload_task_status("run-1", UploadTaskStatus::Running, None, now)
            .await
            .unwrap();
        assert!(fx
            .store
            .get_upload_task("run-1", later)
            .await
            .unwrap()
            .is_some());
        // purge 物理删掉过期行。
        assert_eq!(fx.store.purge_expired_upload_tasks(later).await.unwrap(), 1);
        assert_eq!(fx.store.count_upload_tasks().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn purge_enforces_row_cap() {
        let fx = setup().await;
        let now = Utc::now();
        for i in 0..(UPLOAD_TASK_MAX_ROWS + 5) {
            let id = format!("cap-{i:04}");
            fx.store
                .create_upload_task(&queued(&id), now)
                .await
                .unwrap();
            fx.store
                .update_upload_task_status(&id, UploadTaskStatus::Done, None, now)
                .await
                .unwrap();
        }
        assert_eq!(
            fx.store.count_upload_tasks().await.unwrap(),
            UPLOAD_TASK_MAX_ROWS + 5
        );
        assert_eq!(fx.store.purge_expired_upload_tasks(now).await.unwrap(), 5);
        assert_eq!(
            fx.store.count_upload_tasks().await.unwrap(),
            UPLOAD_TASK_MAX_ROWS
        );
    }
}
