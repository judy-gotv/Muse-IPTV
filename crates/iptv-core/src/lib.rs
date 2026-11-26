//! 领域模型 + 业务规则（开发文档 §1.1、§3、§6、§8）。
//!
//! 不依赖任何框架：纯函数 + 领域规则，可被 API / Worker / 前后端共享。
//! 数据库与网络只通过 `iptv-store` / `iptv-concurrency` 的 trait 注入。
// 作者: Passwd Word

pub mod error;
pub mod normalize;
pub mod play_url;
pub mod policy;
pub mod token;

pub use error::CoreError;
pub use iptv_dto::*;
pub use normalize::{content_hash, normalize_play_url, strip_tracking_params};
pub use play_url::{sign_play_url, verify_play_url};
pub use policy::{authorize_channel, PlaybackDecision};
pub use token::{
    generate_session_value, generate_ticket, generate_token, hash_token, token_prefix,
};

/// 应用版本号（与 workspace version 一致）。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 默认系统设置（§3.2 全部键）。`system_setting` 表缺失时用这些默认值。
/// 单个键的默认值（供 `get_setting_i64` 等兜底用，避免各处默认值漂移）。
pub fn default_setting(key: &str) -> Option<serde_json::Value> {
    default_settings()
        .into_iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
}

pub fn default_settings() -> Vec<(String, serde_json::Value)> {
    vec![
        (
            "default_max_concurrent_sessions".into(),
            serde_json::json!(1),
        ),
        ("default_max_bound_devices".into(), serde_json::json!(1)),
        ("heartbeat_interval_sec".into(), serde_json::json!(30)),
        ("session_timeout_sec".into(), serde_json::json!(90)),
        ("single_ip_token_limit".into(), serde_json::json!(5)),
        ("play_url_ttl_sec".into(), serde_json::json!(300)),
        ("cdn_domain".into(), serde_json::json!("")),
        ("alert_threshold_failed_syncs".into(), serde_json::json!(3)),
        ("default_locale".into(), serde_json::json!("zh-Hans-CN")),
        (
            "available_locales".into(),
            serde_json::json!(["zh-Hant-TW", "zh-Hans-CN", "en", "ja", "ko"]),
        ),
        ("portal_session_ttl_days".into(), serde_json::json!(7)),
        ("remember_device_ttl_days".into(), serde_json::json!(90)),
        ("ticket_ttl_sec".into(), serde_json::json!(60)),
        ("audit_retention_days".into(), serde_json::json!(180)),
        // `max_upload_mb` 已废弃：本地 M3U 上传不限大小（用户 2026-11-25
        // 批准，与 §7.1 的 20 MB 相悖），V4 迁移删已有行，这里不再列出。
    ]
}
