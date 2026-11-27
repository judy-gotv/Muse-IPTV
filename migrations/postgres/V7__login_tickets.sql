-- V7: (M15) 一次性登录票据落库：`login_ticket` 表。
-- 只存票据的 SHA-256 哈希（`ticket_hash`），不存明文；`used` 0/1（布尔存
-- INTEGER，三方言约定）；时间一律 epoch 秒 BIGINT。
-- 消费是原子的：单条 UPDATE ... SET used = 1 WHERE ticket_hash = ?
-- AND used = 0 AND expires_at > now，凭 affected rows 判定成功/已用/过期。
CREATE TABLE IF NOT EXISTS login_ticket (
    id BIGSERIAL PRIMARY KEY,
    ticket_hash TEXT NOT NULL UNIQUE,
    token_id BIGINT NOT NULL,
    ip_prefix TEXT,
    created_at BIGINT NOT NULL,
    expires_at BIGINT NOT NULL,
    used INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_login_ticket_expires ON login_ticket(expires_at);
