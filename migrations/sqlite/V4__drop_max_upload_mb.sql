-- V4: 本地 M3U 上传不限大小（用户 2026-11-25 批准），删已废弃的 max_upload_mb 键。
DELETE FROM system_setting WHERE setting_key = 'max_upload_mb';
