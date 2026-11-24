-- V2 种子数据（开发文档 §3.2 / §5.3）：默认系统设置、默认分组、默认套餐、角色与权限矩阵。
-- 系统设置与 iptv_core::default_settings() 逐项一致；价格为种子默认值，可在后台修改。

INSERT INTO system_setting (setting_key, setting_value, updated_at) VALUES ('default_max_concurrent_sessions', '1', 0) ON CONFLICT (setting_key) DO NOTHING;
INSERT INTO system_setting (setting_key, setting_value, updated_at) VALUES ('default_max_bound_devices', '1', 0) ON CONFLICT (setting_key) DO NOTHING;
INSERT INTO system_setting (setting_key, setting_value, updated_at) VALUES ('heartbeat_interval_sec', '30', 0) ON CONFLICT (setting_key) DO NOTHING;
INSERT INTO system_setting (setting_key, setting_value, updated_at) VALUES ('session_timeout_sec', '90', 0) ON CONFLICT (setting_key) DO NOTHING;
INSERT INTO system_setting (setting_key, setting_value, updated_at) VALUES ('single_ip_token_limit', '5', 0) ON CONFLICT (setting_key) DO NOTHING;
INSERT INTO system_setting (setting_key, setting_value, updated_at) VALUES ('play_url_ttl_sec', '300', 0) ON CONFLICT (setting_key) DO NOTHING;
INSERT INTO system_setting (setting_key, setting_value, updated_at) VALUES ('cdn_domain', '""', 0) ON CONFLICT (setting_key) DO NOTHING;
INSERT INTO system_setting (setting_key, setting_value, updated_at) VALUES ('alert_threshold_failed_syncs', '3', 0) ON CONFLICT (setting_key) DO NOTHING;
INSERT INTO system_setting (setting_key, setting_value, updated_at) VALUES ('default_locale', '"zh-Hant-TW"', 0) ON CONFLICT (setting_key) DO NOTHING;
INSERT INTO system_setting (setting_key, setting_value, updated_at) VALUES ('available_locales', '["zh-Hant-TW", "zh-Hans-CN", "en", "ja", "ko"]', 0) ON CONFLICT (setting_key) DO NOTHING;
INSERT INTO system_setting (setting_key, setting_value, updated_at) VALUES ('portal_session_ttl_days', '7', 0) ON CONFLICT (setting_key) DO NOTHING;
INSERT INTO system_setting (setting_key, setting_value, updated_at) VALUES ('remember_device_ttl_days', '90', 0) ON CONFLICT (setting_key) DO NOTHING;
INSERT INTO system_setting (setting_key, setting_value, updated_at) VALUES ('ticket_ttl_sec', '60', 0) ON CONFLICT (setting_key) DO NOTHING;
INSERT INTO system_setting (setting_key, setting_value, updated_at) VALUES ('audit_retention_days', '180', 0) ON CONFLICT (setting_key) DO NOTHING;
INSERT INTO system_setting (setting_key, setting_value, updated_at) VALUES ('max_upload_mb', '20', 0) ON CONFLICT (setting_key) DO NOTHING;

INSERT INTO channel_group (name, sort_weight, is_default) VALUES ('央视', 10, 1) ON CONFLICT (name) DO NOTHING;
INSERT INTO channel_group (name, sort_weight, is_default) VALUES ('卫视', 20, 0) ON CONFLICT (name) DO NOTHING;
INSERT INTO channel_group (name, sort_weight, is_default) VALUES ('体育', 30, 0) ON CONFLICT (name) DO NOTHING;
INSERT INTO channel_group (name, sort_weight, is_default) VALUES ('电影', 40, 0) ON CONFLICT (name) DO NOTHING;
INSERT INTO channel_group (name, sort_weight, is_default) VALUES ('少儿', 50, 0) ON CONFLICT (name) DO NOTHING;
INSERT INTO channel_group (name, sort_weight, is_default) VALUES ('港台', 60, 0) ON CONFLICT (name) DO NOTHING;

INSERT INTO plan (name, max_concurrent_sessions, max_bound_devices, channel_groups, duration_days, price_cents, currency, created_at) VALUES ('体验版', 1, 1, '[]', 7, 0, 'CNY', 0) ON CONFLICT (name) DO NOTHING;
INSERT INTO plan (name, max_concurrent_sessions, max_bound_devices, channel_groups, duration_days, price_cents, currency, created_at) VALUES ('家庭版', 2, 3, '["央视", "体育", "电影"]', 365, 9900, 'CNY', 0) ON CONFLICT (name) DO NOTHING;

INSERT INTO admin_role (id, name, description) VALUES (1, 'super_admin', '超级管理员') ON CONFLICT (id) DO NOTHING;
INSERT INTO admin_role (id, name, description) VALUES (2, 'operator', '运营') ON CONFLICT (id) DO NOTHING;
INSERT INTO admin_role (id, name, description) VALUES (3, 'support', '客服') ON CONFLICT (id) DO NOTHING;
INSERT INTO admin_role (id, name, description) VALUES (4, 'readonly', '只读') ON CONFLICT (id) DO NOTHING;

INSERT INTO admin_permission (role_id, resource, action) VALUES (1, 'users', 'read') ON CONFLICT (role_id, resource, action) DO NOTHING;
INSERT INTO admin_permission (role_id, resource, action) VALUES (1, 'users', 'write') ON CONFLICT (role_id, resource, action) DO NOTHING;
INSERT INTO admin_permission (role_id, resource, action) VALUES (1, 'tokens', 'read') ON CONFLICT (role_id, resource, action) DO NOTHING;
INSERT INTO admin_permission (role_id, resource, action) VALUES (1, 'tokens', 'write') ON CONFLICT (role_id, resource, action) DO NOTHING;
INSERT INTO admin_permission (role_id, resource, action) VALUES (1, 'channels', 'read') ON CONFLICT (role_id, resource, action) DO NOTHING;
INSERT INTO admin_permission (role_id, resource, action) VALUES (1, 'channels', 'write') ON CONFLICT (role_id, resource, action) DO NOTHING;
INSERT INTO admin_permission (role_id, resource, action) VALUES (1, 'sources', 'read') ON CONFLICT (role_id, resource, action) DO NOTHING;
INSERT INTO admin_permission (role_id, resource, action) VALUES (1, 'sources', 'write') ON CONFLICT (role_id, resource, action) DO NOTHING;
INSERT INTO admin_permission (role_id, resource, action) VALUES (1, 'settings', 'read') ON CONFLICT (role_id, resource, action) DO NOTHING;
INSERT INTO admin_permission (role_id, resource, action) VALUES (1, 'settings', 'write') ON CONFLICT (role_id, resource, action) DO NOTHING;
INSERT INTO admin_permission (role_id, resource, action) VALUES (1, 'audit', 'read') ON CONFLICT (role_id, resource, action) DO NOTHING;
INSERT INTO admin_permission (role_id, resource, action) VALUES (1, 'sessions', 'read') ON CONFLICT (role_id, resource, action) DO NOTHING;
INSERT INTO admin_permission (role_id, resource, action) VALUES (1, 'sessions', 'kick') ON CONFLICT (role_id, resource, action) DO NOTHING;
INSERT INTO admin_permission (role_id, resource, action) VALUES (2, 'users', 'read') ON CONFLICT (role_id, resource, action) DO NOTHING;
INSERT INTO admin_permission (role_id, resource, action) VALUES (2, 'users', 'write') ON CONFLICT (role_id, resource, action) DO NOTHING;
INSERT INTO admin_permission (role_id, resource, action) VALUES (2, 'tokens', 'read') ON CONFLICT (role_id, resource, action) DO NOTHING;
INSERT INTO admin_permission (role_id, resource, action) VALUES (2, 'tokens', 'write') ON CONFLICT (role_id, resource, action) DO NOTHING;
INSERT INTO admin_permission (role_id, resource, action) VALUES (2, 'channels', 'read') ON CONFLICT (role_id, resource, action) DO NOTHING;
INSERT INTO admin_permission (role_id, resource, action) VALUES (2, 'channels', 'write') ON CONFLICT (role_id, resource, action) DO NOTHING;
INSERT INTO admin_permission (role_id, resource, action) VALUES (2, 'sources', 'read') ON CONFLICT (role_id, resource, action) DO NOTHING;
INSERT INTO admin_permission (role_id, resource, action) VALUES (2, 'sessions', 'read') ON CONFLICT (role_id, resource, action) DO NOTHING;
INSERT INTO admin_permission (role_id, resource, action) VALUES (2, 'sessions', 'kick') ON CONFLICT (role_id, resource, action) DO NOTHING;
INSERT INTO admin_permission (role_id, resource, action) VALUES (3, 'users', 'read') ON CONFLICT (role_id, resource, action) DO NOTHING;
INSERT INTO admin_permission (role_id, resource, action) VALUES (3, 'tokens', 'read') ON CONFLICT (role_id, resource, action) DO NOTHING;
INSERT INTO admin_permission (role_id, resource, action) VALUES (3, 'channels', 'read') ON CONFLICT (role_id, resource, action) DO NOTHING;
INSERT INTO admin_permission (role_id, resource, action) VALUES (3, 'sessions', 'read') ON CONFLICT (role_id, resource, action) DO NOTHING;
INSERT INTO admin_permission (role_id, resource, action) VALUES (3, 'sessions', 'kick') ON CONFLICT (role_id, resource, action) DO NOTHING;
INSERT INTO admin_permission (role_id, resource, action) VALUES (4, 'users', 'read') ON CONFLICT (role_id, resource, action) DO NOTHING;
INSERT INTO admin_permission (role_id, resource, action) VALUES (4, 'tokens', 'read') ON CONFLICT (role_id, resource, action) DO NOTHING;
INSERT INTO admin_permission (role_id, resource, action) VALUES (4, 'channels', 'read') ON CONFLICT (role_id, resource, action) DO NOTHING;
INSERT INTO admin_permission (role_id, resource, action) VALUES (4, 'sources', 'read') ON CONFLICT (role_id, resource, action) DO NOTHING;
