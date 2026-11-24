-- V1 全部 19+ 张表（开发文档 §9、§6.2；PostgreSQL 方言）。
-- 类型映射（§10.4 同一套 SQL 跑三套库 + CONTRACTS.md epoch 秒约定）：
--   时间 TIMESTAMPTZ -> BIGINT（epoch 秒）；JSONB -> TEXT（JSON 字符串）；
--   INET -> TEXT；布尔 -> INTEGER 0/1；主键 BIGSERIAL。
--   PG 保留文档 §6.2 的部分索引 idx_ps_active。

CREATE TABLE IF NOT EXISTS app_user (
    id BIGSERIAL PRIMARY KEY,
    display_name TEXT NOT NULL,
    email TEXT,
    phone TEXT,
    plan_id BIGINT,
    status TEXT NOT NULL DEFAULT 'active',
    locale TEXT NOT NULL DEFAULT 'zh-Hant-TW',
    remark TEXT,
    expires_at BIGINT,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
);

CREATE TABLE IF NOT EXISTS user_token (
    id BIGSERIAL PRIMARY KEY,
    user_id BIGINT NOT NULL,
    token_prefix VARCHAR(16) NOT NULL,
    token_hash BYTEA NOT NULL UNIQUE,
    name VARCHAR(100) NOT NULL,
    device_type VARCHAR(32),
    locale VARCHAR(16) NOT NULL DEFAULT 'zh-Hant-TW',
    max_concurrent_sessions INTEGER NOT NULL DEFAULT 1,
    max_bound_devices INTEGER NOT NULL DEFAULT 1,
    status VARCHAR(16) NOT NULL DEFAULT 'active',
    expires_at BIGINT,
    allowed_groups TEXT NOT NULL DEFAULT '[]',
    denied_channels TEXT NOT NULL DEFAULT '[]',
    features TEXT NOT NULL DEFAULT '{}',
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
);

CREATE INDEX idx_user_token_user ON user_token(user_id);

CREATE TABLE IF NOT EXISTS portal_session (
    id BIGSERIAL PRIMARY KEY,
    session_hash BYTEA NOT NULL UNIQUE,
    token_id BIGINT NOT NULL,
    device_id VARCHAR(64) NOT NULL,
    ip_address TEXT,
    user_agent TEXT,
    created_at BIGINT NOT NULL,
    last_seen_at BIGINT NOT NULL,
    expires_at BIGINT NOT NULL,
    revoked_at BIGINT
);

CREATE INDEX idx_portal_session_token ON portal_session(token_id);

CREATE TABLE IF NOT EXISTS device_binding (
    id BIGSERIAL PRIMARY KEY,
    token_id BIGINT NOT NULL,
    device_id VARCHAR(64) NOT NULL,
    first_seen_at BIGINT NOT NULL,
    last_seen_at BIGINT NOT NULL,
    UNIQUE (token_id, device_id)
);

CREATE TABLE IF NOT EXISTS playlist_source (
    id BIGSERIAL PRIMARY KEY,
    name VARCHAR(255) NOT NULL,
    url TEXT,
    sync_interval_sec BIGINT NOT NULL DEFAULT 3600,
    http_headers TEXT NOT NULL DEFAULT '{}',
    epg_url TEXT,
    on_missing VARCHAR(16) NOT NULL DEFAULT 'disable',
    auto_enable_new INTEGER NOT NULL DEFAULT 1,
    enabled INTEGER NOT NULL DEFAULT 1,
    last_sync_at BIGINT,
    last_status VARCHAR(32),
    last_error TEXT,
    next_sync_at BIGINT,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
);

CREATE TABLE IF NOT EXISTS channel (
    id BIGSERIAL PRIMARY KEY,
    tvg_id VARCHAR(128),
    name VARCHAR(255) NOT NULL,
    display_name VARCHAR(255),
    logo_url TEXT,
    group_title VARCHAR(128),
    language VARCHAR(32),
    country VARCHAR(32),
    source_id BIGINT,
    content_hash CHAR(64),
    status VARCHAR(16) NOT NULL DEFAULT 'active',
    visibility VARCHAR(16) NOT NULL DEFAULT 'public',
    sort_weight INTEGER NOT NULL DEFAULT 0,
    last_checked_at BIGINT,
    last_available INTEGER,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
);

CREATE INDEX idx_channel_source ON channel(source_id);
CREATE INDEX idx_channel_group ON channel(group_title);

CREATE TABLE IF NOT EXISTS channel_endpoint (
    id BIGSERIAL PRIMARY KEY,
    channel_id BIGINT NOT NULL,
    url TEXT NOT NULL,
    protocol VARCHAR(16) NOT NULL DEFAULT 'hls',
    quality VARCHAR(16),
    is_primary INTEGER NOT NULL DEFAULT 0,
    priority INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX idx_endpoint_channel ON channel_endpoint(channel_id);

CREATE TABLE IF NOT EXISTS channel_group (
    id BIGSERIAL PRIMARY KEY,
    name VARCHAR(128) NOT NULL UNIQUE,
    sort_weight INTEGER NOT NULL DEFAULT 0,
    icon TEXT,
    is_default INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS channel_group_map (
    id BIGSERIAL PRIMARY KEY,
    channel_id BIGINT NOT NULL,
    group_id BIGINT NOT NULL,
    UNIQUE (channel_id, group_id)
);

CREATE TABLE IF NOT EXISTS epg_source (
    id BIGSERIAL PRIMARY KEY,
    name VARCHAR(255) NOT NULL,
    url TEXT NOT NULL,
    last_sync_at BIGINT,
    last_status VARCHAR(32),
    created_at BIGINT NOT NULL
);

CREATE TABLE IF NOT EXISTS epg_channel_map (
    id BIGSERIAL PRIMARY KEY,
    epg_source_id BIGINT NOT NULL,
    tvg_id VARCHAR(128),
    channel_id BIGINT,
    UNIQUE (epg_source_id, tvg_id)
);

CREATE TABLE IF NOT EXISTS epg_program (
    id BIGSERIAL PRIMARY KEY,
    channel_id BIGINT NOT NULL,
    tvg_id VARCHAR(128),
    title TEXT NOT NULL,
    description TEXT,
    category VARCHAR(64),
    start_at BIGINT NOT NULL,
    stop_at BIGINT NOT NULL,
    created_at BIGINT NOT NULL,
    UNIQUE (channel_id, start_at)
);

CREATE INDEX idx_epg_program_channel ON epg_program(channel_id, start_at);

CREATE TABLE IF NOT EXISTS playback_session (
    id BIGSERIAL PRIMARY KEY,
    session_uid VARCHAR(40) NOT NULL UNIQUE,
    token_id BIGINT NOT NULL,
    channel_id BIGINT NOT NULL,
    device_id VARCHAR(64) NOT NULL,
    ip_address TEXT,
    user_agent TEXT,
    quality VARCHAR(16),
    started_at BIGINT NOT NULL,
    last_heartbeat_at BIGINT NOT NULL,
    ended_at BIGINT,
    status VARCHAR(16) NOT NULL DEFAULT 'active'
);

CREATE INDEX idx_ps_active ON playback_session(token_id) WHERE status = 'active';

CREATE TABLE IF NOT EXISTS playback_event (
    id BIGSERIAL PRIMARY KEY,
    session_uid VARCHAR(40) NOT NULL,
    token_id BIGINT NOT NULL,
    channel_id BIGINT NOT NULL,
    event_type VARCHAR(32) NOT NULL,
    detail TEXT NOT NULL DEFAULT '{}',
    created_at BIGINT NOT NULL
);

CREATE INDEX idx_playback_event_token ON playback_event(token_id, created_at);
CREATE INDEX idx_playback_event_session ON playback_event(session_uid);

CREATE TABLE IF NOT EXISTS sync_job (
    id BIGSERIAL PRIMARY KEY,
    source_id BIGINT NOT NULL,
    scheduled_at BIGINT NOT NULL,
    status VARCHAR(16) NOT NULL DEFAULT 'pending',
    claimed_by VARCHAR(64),
    created_at BIGINT NOT NULL
);

CREATE INDEX idx_sync_job_source ON sync_job(source_id, status);

CREATE TABLE IF NOT EXISTS sync_log (
    id BIGSERIAL PRIMARY KEY,
    source_id BIGINT NOT NULL,
    started_at BIGINT NOT NULL,
    finished_at BIGINT,
    status VARCHAR(32) NOT NULL DEFAULT 'running',
    added BIGINT NOT NULL DEFAULT 0,
    updated BIGINT NOT NULL DEFAULT 0,
    removed BIGINT NOT NULL DEFAULT 0,
    error TEXT
);

CREATE INDEX idx_sync_log_source ON sync_log(source_id, started_at);

CREATE TABLE IF NOT EXISTS plan (
    id BIGSERIAL PRIMARY KEY,
    name VARCHAR(128) NOT NULL UNIQUE,
    max_concurrent_sessions INTEGER NOT NULL DEFAULT 1,
    max_bound_devices INTEGER NOT NULL DEFAULT 1,
    channel_groups TEXT NOT NULL DEFAULT '[]',
    duration_days INTEGER NOT NULL DEFAULT 30,
    price_cents BIGINT NOT NULL DEFAULT 0,
    currency VARCHAR(8) NOT NULL DEFAULT 'CNY',
    created_at BIGINT NOT NULL
);

CREATE TABLE IF NOT EXISTS subscription (
    id BIGSERIAL PRIMARY KEY,
    user_id BIGINT NOT NULL,
    plan_id BIGINT NOT NULL,
    starts_at BIGINT NOT NULL,
    expires_at BIGINT,
    status VARCHAR(16) NOT NULL DEFAULT 'active',
    created_at BIGINT NOT NULL
);

CREATE INDEX idx_subscription_user ON subscription(user_id);

CREATE TABLE IF NOT EXISTS admin_user (
    id BIGSERIAL PRIMARY KEY,
    username VARCHAR(64) NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    role VARCHAR(16) NOT NULL DEFAULT 'readonly',
    totp_secret TEXT,
    totp_enabled INTEGER NOT NULL DEFAULT 0,
    totp_required INTEGER NOT NULL DEFAULT 0,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
);

CREATE TABLE IF NOT EXISTS admin_role (
    id BIGSERIAL PRIMARY KEY,
    name VARCHAR(32) NOT NULL UNIQUE,
    description TEXT
);

CREATE TABLE IF NOT EXISTS admin_permission (
    id BIGSERIAL PRIMARY KEY,
    role_id BIGINT NOT NULL,
    resource VARCHAR(64) NOT NULL,
    action VARCHAR(16) NOT NULL,
    UNIQUE (role_id, resource, action)
);

CREATE TABLE IF NOT EXISTS audit_log (
    id BIGSERIAL PRIMARY KEY,
    actor VARCHAR(128) NOT NULL,
    actor_role VARCHAR(32),
    action VARCHAR(64) NOT NULL,
    object_type VARCHAR(64) NOT NULL,
    object_id VARCHAR(64) NOT NULL,
    before_json TEXT,
    after_json TEXT,
    ip_address TEXT,
    created_at BIGINT NOT NULL
);

CREATE INDEX idx_audit_actor ON audit_log(actor, created_at);
CREATE INDEX idx_audit_action ON audit_log(action, created_at);

CREATE TABLE IF NOT EXISTS system_setting (
    setting_key VARCHAR(128) NOT NULL PRIMARY KEY,
    setting_value TEXT NOT NULL,
    updated_at BIGINT NOT NULL
);

CREATE TABLE IF NOT EXISTS i18n_override (
    id BIGSERIAL PRIMARY KEY,
    locale VARCHAR(16) NOT NULL,
    key VARCHAR(255) NOT NULL,
    value TEXT NOT NULL,
    updated_at BIGINT NOT NULL,
    UNIQUE (locale, key)
);

CREATE UNIQUE INDEX uq_channel_tvg ON channel(tvg_id) WHERE tvg_id IS NOT NULL;
