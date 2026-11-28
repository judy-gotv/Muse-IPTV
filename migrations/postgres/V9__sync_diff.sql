-- V9: (M-D) 同步差异落库：`sync_diff` 表。
-- `get_sync_diff` 不再只读进程内缓存（重启/多实例下本就为空、不可靠）：
-- `upsert_channels` 在全部批次提交后由独立短事务写本行（v0.1.6 分批
-- 提交；失败的导入不留差异）。时间一律 epoch 秒 BIGINT；`diff_json`
-- 是整个 `SyncDiff` 的 JSON（added / metadata_changed / url_changed /
-- missing 四个频道数组 + 各自截断前的真实总数 `*_total`）；四个列表
-- 截断到 1000 条（v0.1.6：未截断时 214MB 单行 JSON 一条 UPDATE 持写锁
-- 72 秒，并发写等满 busy_timeout 后 500）。
CREATE TABLE IF NOT EXISTS sync_diff (
    source_id BIGINT NOT NULL PRIMARY KEY,
    diff_json TEXT NOT NULL,
    updated_at BIGINT NOT NULL
);
