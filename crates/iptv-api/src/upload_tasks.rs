//! M16：异步上传任务注册表（§7.1 异步解析）。
//!
//! 任务只放进程内内存（`Arc<Mutex<HashMap<…>>>`，挂在 `AppState` 上）：
//! 实现简单、无需迁移；代价是任务重启即失、丢了不可恢复——与 M15 修前
//! 的票据内存实现同款取舍，单实例部署可接受；多实例下进度查询须落在
//! 同一实例上（网关不做粘性路由时可能 404，属已知限制）。
// 作者: Passwd Word

use iptv_dto::{UploadTaskProgress, UploadTaskStatus};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// 任务注册表：task_id → 进度快照。
pub type UploadTasks = Arc<Mutex<HashMap<String, UploadTaskProgress>>>;

pub fn new_upload_tasks() -> UploadTasks {
    Arc::new(Mutex::new(HashMap::new()))
}

pub fn register(tasks: &UploadTasks, progress: UploadTaskProgress) {
    tasks
        .lock()
        .expect("upload_tasks lock poisoned")
        .insert(progress.task_id.clone(), progress);
}

/// 对任务做一次原地更新；任务不存在时返回 false。
pub fn update<F: FnOnce(&mut UploadTaskProgress)>(tasks: &UploadTasks, id: &str, f: F) -> bool {
    let mut map = tasks.lock().expect("upload_tasks lock poisoned");
    match map.get_mut(id) {
        Some(p) => {
            f(p);
            true
        }
        None => false,
    }
}

pub fn get(tasks: &UploadTasks, id: &str) -> Option<UploadTaskProgress> {
    tasks
        .lock()
        .expect("upload_tasks lock poisoned")
        .get(id)
        .cloned()
}

/// 新建一个 queued 任务并注册，返回其进度快照。
pub fn create_queued(
    tasks: &UploadTasks,
    task_id: String,
    filename: String,
    source_id: Option<i64>,
) -> UploadTaskProgress {
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
    register(tasks, progress.clone());
    progress
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queued() -> (UploadTasks, UploadTaskProgress) {
        let tasks = new_upload_tasks();
        let p = create_queued(&tasks, "task-1".to_string(), "a.m3u".to_string(), None);
        (tasks, p)
    }

    #[test]
    fn state_flow_queued_running_done() {
        let (tasks, p) = queued();
        assert_eq!(p.status, UploadTaskStatus::Queued);
        assert!(update(&tasks, "task-1", |p| {
            p.status = UploadTaskStatus::Running;
            p.total_lines = 2000;
            p.channels_parsed = 900;
        }));
        let p = get(&tasks, "task-1").unwrap();
        assert_eq!(p.status, UploadTaskStatus::Running);
        assert_eq!((p.total_lines, p.channels_parsed), (2000, 900));
        assert!(update(&tasks, "task-1", |p| {
            p.status = UploadTaskStatus::Done;
            p.total_lines = 4200;
            p.channels_parsed = 2000;
            p.channels_imported = 1998;
            p.error_count = 2;
        }));
        let p = get(&tasks, "task-1").unwrap();
        assert_eq!(p.status, UploadTaskStatus::Done);
        assert_eq!(p.channels_imported, 1998);
        assert_eq!(p.error_count, 2);
        assert!(p.error.is_none());
    }

    #[test]
    fn state_flow_queued_running_failed_keeps_error() {
        let (tasks, _) = queued();
        assert!(update(&tasks, "task-1", |p| {
            p.status = UploadTaskStatus::Running;
            p.total_lines = 500;
        }));
        assert!(update(&tasks, "task-1", |p| {
            p.status = UploadTaskStatus::Failed;
            p.error = Some("source not found".to_string());
        }));
        let p = get(&tasks, "task-1").unwrap();
        assert_eq!(p.status, UploadTaskStatus::Failed);
        assert_eq!(p.error.as_deref(), Some("source not found"));
        assert_eq!(p.total_lines, 500);
    }

    #[test]
    fn progress_numbers_only_move_forward() {
        // M16：模拟后台任务的更新序列，进度数字单调不减。
        let (tasks, _) = queued();
        let mut last = (0usize, 0usize, 0usize);
        for (lines, parsed, errors) in [
            (2000, 900, 0),
            (4000, 1800, 1),
            (4200, 2000, 2),
            (4200, 2000, 2),
        ] {
            update(&tasks, "task-1", |p| {
                p.status = UploadTaskStatus::Running;
                p.total_lines = lines;
                p.channels_parsed = parsed;
                p.error_count = errors;
            });
            let p = get(&tasks, "task-1").unwrap();
            let cur = (p.total_lines, p.channels_parsed, p.error_count);
            assert!(cur.0 >= last.0 && cur.1 >= last.1 && cur.2 >= last.2);
            last = cur;
        }
    }

    #[test]
    fn unknown_task_returns_none_and_update_false() {
        let tasks = new_upload_tasks();
        assert!(get(&tasks, "nope").is_none());
        assert!(!update(&tasks, "nope", |p| p.total_lines = 1));
    }
}
