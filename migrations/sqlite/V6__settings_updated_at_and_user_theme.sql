-- V6: (M13) 把从未修改过的设置行 updated_at=0 改写为迁移执行时间；
-- (m17) 给用户加主题偏好列（门户设置页主题选择持久化到用户行）。
-- UPDATE 本身幂等（反复跑结果相同）。
UPDATE system_setting SET updated_at = CAST(strftime('%s', 'now') AS INTEGER) WHERE updated_at = 0;
ALTER TABLE app_user ADD COLUMN theme TEXT NOT NULL DEFAULT 'system';
