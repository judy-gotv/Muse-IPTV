-- V8: (m-uptask + N5) 上传任务落库：`upload_task` 表。
-- 进度注册表不再放进程内内存：任务行随解析进度做行级 UPDATE，重启后
-- GET /api/v1/admin/upload-tasks/{id} 仍能查到；完成（done/failed）24 小时
-- 后过期（`updated_at` 为最后一次进度时间），由 API 进程后台任务定期删；
-- 另有 1000 行封顶（先删最旧的已完成任务，见 Store::purge_expired_upload_tasks）。
-- 时间一律 epoch 秒 BIGINT；状态存小写文本（queued/running/done/failed）。
-- MySQL：`task_id` 用 VARCHAR(64) 作主键（TEXT 列不能直接作主键）。
CREATE TABLE IF NOT EXISTS upload_task (
    task_id VARCHAR(64) NOT NULL PRIMARY KEY,
    status TEXT NOT NULL,
    filename TEXT NOT NULL,
    source_id BIGINT,
    total_lines BIGINT NOT NULL DEFAULT 0,
    channels_parsed BIGINT NOT NULL DEFAULT 0,
    channels_imported BIGINT NOT NULL DEFAULT 0,
    error_count BIGINT NOT NULL DEFAULT 0,
    error TEXT,
    updated_at BIGINT NOT NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE INDEX idx_upload_task_updated ON upload_task(updated_at);
CREATE INDEX idx_upload_task_status_updated ON upload_task(status, updated_at);
