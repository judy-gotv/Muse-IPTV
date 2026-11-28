//! M16 异步上传任务注册表（§7.1 异步解析），v0.1.6 m-uptask + N5 落库版。
//!
//! 事实来源是 `upload_task` 表（V8 迁移，`iptv-store::Store`）：
//! - 任务重启即失的问题已修：`create_queued` 写行，`get` 直接读库；
//!   同一库文件上新建 `UploadTaskRegistry`（模拟重启）仍能查到进度，
//!   多实例下网关无需粘性路由。`AppState` 上不再挂任何内存任务表。
//! - 进度更新走行级 UPDATE（`update_upload_task_progress` 只写三个计数，
//!   解析回调每 2000 行触发一次）；状态流转 queued → running →
//!   done/failed 与旧内存版一致；未知 id 仍返回 None（handler 侧 404）。
//! - N5 淘汰：完成（done/failed）24 小时后过期（`updated_at` 为最后一次
//!   进度时间；`get` 对已过期终态行直接返回 None，过期任务查 404，不必
//!   等后台清理跑完），另有 1000 行封顶（见 `Store::purge_expired_upload_tasks`）；
//!   API 进程每小时跑一次 `purge_expired`（`iptv-api` 的 main 里 spawn）。
// 作者: Passwd Word

use chrono::Utc;
use iptv_dto::{UploadTaskProgress, UploadTaskStatus};
use iptv_store::Store;
use std::sync::Arc;

pub use iptv_store::{UPLOAD_TASK_MAX_ROWS, UPLOAD_TASK_RETENTION_SECS};

/// 后台清理间隔（秒）：每小时删一次过期上传任务；`interval` 首 tick
/// 立即执行，故进程启动即先清一轮上次运行残留的过期行。
pub const UPLOAD_TASK_PURGE_INTERVAL_SECS: u64 = 3600;

/// DB 驱动的任务注册表：只持 `Arc<Store>`，自身无任何内存任务状态。
#[derive(Clone)]
pub struct UploadTaskRegistry {
    store: Arc<Store>,
}

impl UploadTaskRegistry {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    /// 新建一个 queued 任务行并返回其进度快照（POST /upload 立返 task_id）。
    pub async fn create_queued(
        &self,
        task_id: String,
        filename: String,
        source_id: Option<i64>,
    ) -> anyhow::Result<UploadTaskProgress> {
        let progress = UploadTaskProgress {
            task_id,
            status: UploadTaskStatus::Queued,
            filename,
            source_id,
            total_lines: 0,
            channels_parsed: 0,
            channels_imported: 0,
            error_count: 0,
            error: None,
        };
        self.store.create_upload_task(&progress, Utc::now()).await?;
        Ok(progress)
    }

    /// 读任务进度；未知 id 或已过期终态任务返回 `None`（handler 侧 404）。
    pub async fn get(&self, id: &str) -> anyhow::Result<Option<UploadTaskProgress>> {
        self.store.get_upload_task(id, Utc::now()).await
    }

    /// 对任务做一次读-改-写（全行 UPDATE）；任务不存在时返回 `Ok(false)`。
    pub async fn update<F: FnOnce(&mut UploadTaskProgress)>(
        &self,
        id: &str,
        f: F,
    ) -> anyhow::Result<bool> {
        let Some(mut p) = self.get(id).await? else {
            return Ok(false);
        };
        f(&mut p);
        self.store.update_upload_task(&p, Utc::now()).await
    }

    /// 解析进度回调用的行级 UPDATE（只写三个计数 + `updated_at`，不读行）。
    pub async fn update_progress(
        &self,
        id: &str,
        total_lines: usize,
        channels_parsed: usize,
        error_count: usize,
    ) -> anyhow::Result<bool> {
        self.store
            .update_upload_task_progress(
                id,
                total_lines as i64,
                channels_parsed as i64,
                error_count as i64,
                Utc::now(),
            )
            .await
    }

    /// `update_progress` 的阻塞版：只能在非 runtime 线程调用。解析回调跑
    /// 在 `spawn_blocking` 线程内，`Handle::block_on` 在此合法（阻塞线程
    /// 不在 async 任务上下文里）；在 async 任务内调用会 panic，勿用。
    /// 回调按顺序触发，`block_on` 保证每次写完才继续解析，进度数字单调。
    pub fn update_progress_blocking(
        &self,
        id: &str,
        total_lines: usize,
        channels_parsed: usize,
        error_count: usize,
    ) {
        let store = self.store.clone();
        let id = id.to_string();
        tokio::runtime::Handle::current().block_on(async move {
            let _ = store
                .update_upload_task_progress(
                    &id,
                    total_lines as i64,
                    channels_parsed as i64,
                    error_count as i64,
                    Utc::now(),
                )
                .await;
        });
    }

    /// 置任务为 failed 并记错误串；任务不存在返回 `Ok(false)`。
    pub async fn fail(&self, id: &str, msg: String) -> anyhow::Result<bool> {
        self.store
            .update_upload_task_status(id, UploadTaskStatus::Failed, Some(&msg), Utc::now())
            .await
    }

    /// N5 清理：删过期终态行 + 把总量压到 1000 行以内；返回删除行数。
    pub async fn purge_expired(&self) -> anyhow::Result<u64> {
        self.store.purge_expired_upload_tasks(Utc::now()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        store: Arc<Store>,
        path: std::path::PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    async fn setup() -> (Fixture, UploadTaskRegistry) {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "iptv-api-uploadreg-{}-{}.db",
            std::process::id(),
            n
        ));
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let store = Arc::new(Store::connect(&url).await.expect("connect sqlite"));
        store.migrate().await.expect("migrate");
        let registry = UploadTaskRegistry::new(store.clone());
        (Fixture { store, path }, registry)
    }

    #[tokio::test]
    async fn state_flow_queued_running_done() {
        let (_fx, reg) = setup().await;
        let p = reg
            .create_queued("task-1".to_string(), "a.m3u".to_string(), None)
            .await
            .unwrap();
        assert_eq!(p.status, UploadTaskStatus::Queued);
        assert!(reg
            .update("task-1", |p| {
                p.status = UploadTaskStatus::Running;
                p.total_lines = 2000;
                p.channels_parsed = 900;
            })
            .await
            .unwrap());
        let p = reg.get("task-1").await.unwrap().unwrap();
        assert_eq!(p.status, UploadTaskStatus::Running);
        assert_eq!((p.total_lines, p.channels_parsed), (2000, 900));
        assert!(reg
            .update("task-1", |p| {
                p.status = UploadTaskStatus::Done;
                p.total_lines = 4200;
                p.channels_parsed = 2000;
                p.channels_imported = 1998;
                p.error_count = 2;
            })
            .await
            .unwrap());
        let p = reg.get("task-1").await.unwrap().unwrap();
        assert_eq!(p.status, UploadTaskStatus::Done);
        assert_eq!(p.channels_imported, 1998);
        assert_eq!(p.error_count, 2);
        assert!(p.error.is_none());
    }

    #[tokio::test]
    async fn state_flow_queued_running_failed_keeps_error() {
        let (_fx, reg) = setup().await;
        reg.create_queued("task-1".to_string(), "a.m3u".to_string(), None)
            .await
            .unwrap();
        assert!(reg
            .update("task-1", |p| {
                p.status = UploadTaskStatus::Running;
                p.total_lines = 500;
            })
            .await
            .unwrap());
        assert!(reg
            .fail("task-1", "source not found".to_string())
            .await
            .unwrap());
        let p = reg.get("task-1").await.unwrap().unwrap();
        assert_eq!(p.status, UploadTaskStatus::Failed);
        assert_eq!(p.error.as_deref(), Some("source not found"));
        assert_eq!(p.total_lines, 500);
    }

    #[tokio::test]
    async fn progress_numbers_only_move_forward() {
        // M16：模拟后台任务的更新序列，进度数字单调不减。
        let (_fx, reg) = setup().await;
        reg.create_queued("task-1".to_string(), "a.m3u".to_string(), None)
            .await
            .unwrap();
        let mut last = (0usize, 0usize, 0usize);
        for (lines, parsed, errors) in [
            (2000, 900, 0),
            (4000, 1800, 1),
            (4200, 2000, 2),
            (4200, 2000, 2),
        ] {
            reg.update_progress("task-1", lines, parsed, errors)
                .await
                .unwrap();
            let p = reg.get("task-1").await.unwrap().unwrap();
            let cur = (p.total_lines, p.channels_parsed, p.error_count);
            assert!(cur.0 >= last.0 && cur.1 >= last.1 && cur.2 >= last.2);
            last = cur;
        }
    }

    #[tokio::test]
    async fn unknown_task_returns_none_and_update_false() {
        let (_fx, reg) = setup().await;
        assert!(reg.get("nope").await.unwrap().is_none());
        assert!(!reg.update("nope", |p| p.total_lines = 1).await.unwrap());
        assert!(!reg.fail("nope", "x".to_string()).await.unwrap());
    }

    /// m-uptask：模拟重启——同一库文件上新建 Store + 新 registry 实例，
    /// 任务进度仍能查到（旧内存版此处会 404）。
    #[tokio::test]
    async fn progress_survives_registry_rebuild() {
        let (fx, reg) = setup().await;
        reg.create_queued("task-1".to_string(), "a.m3u".to_string(), Some(7))
            .await
            .unwrap();
        reg.update("task-1", |p| {
            p.status = UploadTaskStatus::Running;
            p.total_lines = 2000;
            p.channels_parsed = 900;
        })
        .await
        .unwrap();
        drop(reg);
        let url = format!("sqlite://{}?mode=rwc", fx.path.display());
        let store2 = Arc::new(Store::connect(&url).await.expect("reconnect sqlite"));
        store2.migrate().await.expect("migrate again");
        let reopened = UploadTaskRegistry::new(store2);
        let p = reopened
            .get("task-1")
            .await
            .unwrap()
            .expect("task survives restart");
        assert_eq!(p.status, UploadTaskStatus::Running);
        assert_eq!((p.total_lines, p.channels_parsed), (2000, 900));
        assert_eq!(p.source_id, Some(7));
        // 重建后的实例也能继续更新同一任务。
        assert!(reopened
            .update("task-1", |p| {
                p.status = UploadTaskStatus::Done;
                p.channels_imported = 900;
            })
            .await
            .unwrap());
        assert_eq!(
            reopened.get("task-1").await.unwrap().unwrap().status,
            UploadTaskStatus::Done
        );
    }

    /// N5：连续建任务后 purge 把行数压到 1000 行封顶以内。
    #[tokio::test]
    async fn purge_caps_row_count() {
        let (fx, reg) = setup().await;
        for i in 0..(UPLOAD_TASK_MAX_ROWS + 5) {
            let id = format!("cap-{i:04}");
            reg.create_queued(id.clone(), "a.m3u".to_string(), None)
                .await
                .unwrap();
            reg.update(&id, |p| p.status = UploadTaskStatus::Done)
                .await
                .unwrap();
        }
        assert_eq!(
            fx.store.count_upload_tasks().await.unwrap(),
            UPLOAD_TASK_MAX_ROWS + 5
        );
        assert_eq!(reg.purge_expired().await.unwrap(), 5);
        assert_eq!(
            fx.store.count_upload_tasks().await.unwrap(),
            UPLOAD_TASK_MAX_ROWS
        );
    }
}
