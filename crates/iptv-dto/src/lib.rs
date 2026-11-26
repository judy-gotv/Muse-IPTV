//! 请求/响应结构体，前后端共享（开发文档 §1.1、§2、§4.5、§5.2）。
//!
//! 约定：
//! - 所有 API 响应包在 `Envelope<T>` 里：`code == 0` 为成功，`message` 按
//!   `Accept-Language` 本地化（§12.5），`code` 永远不变。
//! - 时间一律 RFC 3339 UTC（`DateTime<Utc>`），存库时转 epoch 秒（§10 方言可移植）。
//! - ID 为数据库自增 `i64`（§6.2 / §9 的 BIGSERIAL / AUTO_INCREMENT / AUTOINCREMENT）。
// 作者: Passwd Word

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// §12.5 / §6.4：统一响应包与错误码
// ---------------------------------------------------------------------------

/// 成功 / 失败统一包。`code == 0` 成功；其余见 [`ErrorCode`]。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope<T> {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
}

impl<T> Envelope<T> {
    pub fn ok(data: T) -> Self {
        Self {
            code: 0,
            message: "ok".to_string(),
            data: Some(data),
        }
    }
    pub fn err(code: ErrorCode, message: impl Into<String>) -> Envelope<()> {
        Envelope {
            code: code as i32,
            message: message.into(),
            data: None,
        }
    }
}

/// §6.4 播放判定错误码 + 通用码。数值是稳定契约，前端只按 `code` 分支。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(i32)]
pub enum ErrorCode {
    Ok = 0,
    BadRequest = 40000,
    ValidationFailed = 40001,
    Unauthorized = 40100,
    /// §6.4：会话过期，需重新登录
    SessionExpired = 40101,
    /// 管理员登录失败 / TOTP 失败
    AdminAuthFailed = 40102,
    Forbidden = 40300,
    /// §6.4：无该频道权限
    ChannelForbidden = 40301,
    /// §6.4：Token 已停用或过期
    TokenDisabledOrExpired = 40302,
    NotFound = 40401,
    /// M10：唯一约束冲突（重名/已存在），如建管理员/频道/套餐/分组重名。
    Conflict = 40900,
    /// §6.4：并发已达上限
    ConcurrentLimitReached = 42901,
    /// §6.4：绑定设备数已达上限
    DeviceLimitReached = 42902,
    /// 通用限流
    RateLimited = 42903,
    /// §6.4：频道源不可用
    SourceUnavailable = 50301,
    Internal = 50000,
}

impl ErrorCode {
    /// i18n key（§12.3）：`errors.e<code>`，如 `errors.e42901`
    /// （Fluent 标识符须以字母开头，数字前加 `e`）。
    pub fn i18n_key(self) -> String {
        format!("errors.e{}", self as i32)
    }
    pub fn http_status(self) -> u16 {
        match self {
            ErrorCode::Ok => 200,
            ErrorCode::BadRequest | ErrorCode::ValidationFailed => 400,
            ErrorCode::Unauthorized | ErrorCode::SessionExpired | ErrorCode::AdminAuthFailed => 401,
            ErrorCode::Forbidden
            | ErrorCode::ChannelForbidden
            | ErrorCode::TokenDisabledOrExpired => 403,
            ErrorCode::NotFound => 404,
            ErrorCode::Conflict => 409,
            ErrorCode::ConcurrentLimitReached
            | ErrorCode::DeviceLimitReached
            | ErrorCode::RateLimited => 429,
            ErrorCode::SourceUnavailable => 503,
            ErrorCode::Internal => 500,
        }
    }
}

/// 分页请求（§4.5 / §5.2 各列表接口通用）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PageQuery {
    #[serde(default = "default_page")]
    pub page: u32,
    #[serde(default = "default_page_size")]
    pub page_size: u32,
    #[serde(default)]
    pub q: Option<String>,
}
fn default_page() -> u32 {
    1
}
fn default_page_size() -> u32 {
    20
}

/// 分页响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Paged<T> {
    pub items: Vec<T>,
    pub total: u64,
    pub page: u32,
    pub page_size: u32,
}

// ---------------------------------------------------------------------------
// §9 / §3.2：用户、套餐、Token
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserStatus {
    Active,
    Suspended,
    Expired,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppUser {
    pub id: i64,
    pub display_name: String,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub plan_id: Option<i64>,
    pub status: UserStatus,
    pub locale: String,
    pub theme: String,
    pub remark: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateUserRequest {
    pub display_name: String,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub plan_id: Option<i64>,
    pub locale: Option<String>,
    pub remark: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateUserRequest {
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub plan_id: Option<i64>,
    pub status: Option<UserStatus>,
    pub locale: Option<String>,
    pub theme: Option<String>,
    pub remark: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// 套餐（§3.2 / §5.1 plans）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub id: i64,
    pub name: String,
    pub max_concurrent_sessions: i32,
    pub max_bound_devices: i32,
    pub channel_groups: Vec<String>,
    pub duration_days: i32,
    pub price_cents: i64,
    pub currency: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreatePlanRequest {
    pub name: String,
    pub max_concurrent_sessions: i32,
    pub max_bound_devices: i32,
    pub channel_groups: Vec<String>,
    pub duration_days: i32,
    pub price_cents: i64,
    pub currency: Option<String>,
}

/// M19d：套餐 PATCH——只改提供的字段。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdatePlanRequest {
    pub name: Option<String>,
    pub max_concurrent_sessions: Option<i32>,
    pub max_bound_devices: Option<i32>,
    pub channel_groups: Option<Vec<String>>,
    pub duration_days: Option<i32>,
    pub price_cents: Option<i64>,
    pub currency: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenStatus {
    Active,
    Disabled,
    Expired,
}

/// Token 能力开关（§3.2：回看/录制权限、清晰度上限）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenFeatures {
    #[serde(default)]
    pub catchup: bool,
    #[serde(default)]
    pub recording: bool,
    #[serde(default = "default_max_quality")]
    pub max_quality: String,
}
fn default_max_quality() -> String {
    "1080p".to_string()
}

/// Token 主数据。明文 Token 永不入库，只存 SHA-256（§4.3 / §13）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserToken {
    pub id: i64,
    pub user_id: i64,
    /// 明文前 8 位，用于索引与展示（§6.2 `token_prefix`）。
    pub token_prefix: String,
    pub name: String,
    pub device_type: Option<String>,
    pub locale: String,
    pub max_concurrent_sessions: i32,
    pub max_bound_devices: i32,
    pub status: TokenStatus,
    pub expires_at: Option<DateTime<Utc>>,
    pub allowed_groups: Vec<String>,
    pub denied_channels: Vec<i64>,
    pub features: TokenFeatures,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl UserToken {
    /// 剩余天数；已过期为负数（§4.5 示例 `days_remaining`）。
    pub fn days_remaining(&self, now: DateTime<Utc>) -> i64 {
        match self.expires_at {
            Some(exp) => (exp - now).num_days(),
            None => i64::MAX,
        }
    }
    pub fn is_usable(&self, now: DateTime<Utc>) -> bool {
        self.status == TokenStatus::Active && self.expires_at.map(|e| e > now).unwrap_or(true)
    }
}

/// §5.2 创建 Token 请求示例（文档原字段逐一对应）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTokenRequest {
    pub user_id: i64,
    pub name: String,
    pub device_type: Option<String>,
    pub max_concurrent_sessions: i32,
    pub max_bound_devices: i32,
    pub expires_at: Option<DateTime<Utc>>,
    pub locale: Option<String>,
    pub allowed_groups: Vec<String>,
    #[serde(default)]
    pub denied_channels: Vec<i64>,
    #[serde(default)]
    pub features: TokenFeatures,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateTokenRequest {
    pub name: Option<String>,
    pub device_type: Option<String>,
    pub max_concurrent_sessions: Option<i32>,
    pub max_bound_devices: Option<i32>,
    pub status: Option<TokenStatus>,
    pub expires_at: Option<DateTime<Utc>>,
    pub locale: Option<String>,
    pub allowed_groups: Option<Vec<String>>,
    pub denied_channels: Option<Vec<i64>>,
    pub features: Option<TokenFeatures>,
}

/// 创建 Token 响应：明文 Token 只返回一次（§5.2）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTokenResponse {
    pub id: i64,
    /// 仅显示一次，调用方必须当场保存。
    pub token: String,
    pub portal_url: String,
    /// 登录票据二维码 PNG（data URI，§5.2 `qr_png`）。
    pub qr_png: String,
}

// ---------------------------------------------------------------------------
// §9：频道、分组、订阅源、EPG
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelStatus {
    Active,
    Disabled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Channel {
    pub id: i64,
    pub tvg_id: Option<String>,
    pub name: String,
    pub display_name: Option<String>,
    pub logo_url: Option<String>,
    pub group_title: Option<String>,
    pub language: Option<String>,
    pub country: Option<String>,
    pub source_id: Option<i64>,
    pub content_hash: Option<String>,
    pub status: ChannelStatus,
    pub visibility: String,
    pub sort_weight: i32,
    pub last_checked_at: Option<DateTime<Utc>>,
    pub last_available: Option<bool>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateChannelRequest {
    pub tvg_id: Option<String>,
    pub name: String,
    pub display_name: Option<String>,
    pub logo_url: Option<String>,
    pub group_title: Option<String>,
    pub language: Option<String>,
    pub country: Option<String>,
    pub play_url: String,
    pub backup_url: Option<String>,
    pub protocol: Option<String>,
    pub sort_weight: Option<i32>,
    pub visibility: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateChannelRequest {
    pub tvg_id: Option<String>,
    pub name: Option<String>,
    pub display_name: Option<String>,
    pub logo_url: Option<String>,
    pub group_title: Option<String>,
    pub status: Option<ChannelStatus>,
    pub visibility: Option<String>,
    pub sort_weight: Option<i32>,
}

/// 播放地址（主/备/多协议，§9 `channel_endpoint`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelEndpoint {
    pub id: i64,
    pub channel_id: i64,
    pub url: String,
    pub protocol: String,
    pub quality: Option<String>,
    pub is_primary: bool,
    pub priority: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelGroup {
    pub id: i64,
    pub name: String,
    pub sort_weight: i32,
    pub icon: Option<String>,
    pub is_default: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateGroupRequest {
    pub name: String,
    pub sort_weight: Option<i32>,
    pub icon: Option<String>,
    pub is_default: Option<bool>,
}

/// M19e：分组 PATCH——只改提供的字段。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateGroupRequest {
    pub name: Option<String>,
    pub sort_weight: Option<i32>,
    pub icon: Option<String>,
    pub is_default: Option<bool>,
}

/// 订阅源（§7.4 全部配置项）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnMissing {
    Disable,
    Delete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaylistSource {
    pub id: i64,
    pub name: String,
    pub url: Option<String>,
    pub sync_interval_sec: i64,
    pub http_headers: serde_json::Value,
    pub epg_url: Option<String>,
    pub on_missing: OnMissing,
    pub auto_enable_new: bool,
    pub enabled: bool,
    pub last_sync_at: Option<DateTime<Utc>>,
    pub last_status: Option<String>,
    pub last_error: Option<String>,
    pub next_sync_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSourceRequest {
    pub name: String,
    pub url: Option<String>,
    pub sync_interval_sec: Option<i64>,
    pub http_headers: Option<serde_json::Value>,
    pub epg_url: Option<String>,
    pub on_missing: Option<OnMissing>,
    pub auto_enable_new: Option<bool>,
    pub enabled: Option<bool>,
}

/// M19c：订阅源 PATCH——只改提供的字段（其余保持原值，先读后写）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateSourceRequest {
    pub name: Option<String>,
    pub url: Option<String>,
    pub sync_interval_sec: Option<i64>,
    pub http_headers: Option<serde_json::Value>,
    pub epg_url: Option<String>,
    pub on_missing: Option<OnMissing>,
    pub auto_enable_new: Option<bool>,
    pub enabled: Option<bool>,
}

/// 同步差异（§7.3：新增 / 元数据变更 / URL 变更 / 消失）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncDiff {
    pub added: Vec<Channel>,
    pub metadata_changed: Vec<Channel>,
    pub url_changed: Vec<Channel>,
    pub missing: Vec<Channel>,
}

/// 同步日志（§9 `sync_log`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncLog {
    pub id: i64,
    pub source_id: i64,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub status: String,
    pub added: i64,
    pub updated: i64,
    pub removed: i64,
    pub error: Option<String>,
}

/// EPG 节目（§9 `epg_program`，XMLTV）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EpgProgram {
    pub id: i64,
    pub channel_id: i64,
    pub tvg_id: Option<String>,
    pub title: String,
    pub description: Option<String>,
    pub category: Option<String>,
    pub start_at: DateTime<Utc>,
    pub stop_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EpgSource {
    pub id: i64,
    pub name: String,
    pub url: String,
    pub last_sync_at: Option<DateTime<Utc>>,
    pub last_status: Option<String>,
}

/// M19f：EPG 源 PATCH——只改提供的字段。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateEpgSourceRequest {
    pub name: Option<String>,
    pub url: Option<String>,
}

// ---------------------------------------------------------------------------
// §4：门户会话、设备、播放
// ---------------------------------------------------------------------------

/// 门户会话（§9 `portal_session`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortalSession {
    pub id: i64,
    pub token_id: i64,
    pub device_id: String,
    pub ip_address: Option<String>,
    pub user_agent: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

/// 在线设备行（§4.1：IP 脱敏后展示）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceView {
    pub session_id: i64,
    pub device_id: String,
    /// 已脱敏，如 `192.168.***.**`。
    pub ip_masked: String,
    pub user_agent: Option<String>,
    pub last_heartbeat_at: DateTime<Utc>,
    pub is_current: bool,
}

/// §4.5 `GET /api/v1/me` 响应示例（文档原字段）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeResponse {
    pub user: MeUser,
    pub token: MeToken,
    pub quota: MeQuota,
    pub plan: Option<MePlan>,
    pub allowed_groups: Vec<String>,
    pub features: TokenFeatures,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeUser {
    pub id: i64,
    pub display_name: String,
    pub locale: String,
    pub theme: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeToken {
    pub id: i64,
    pub name: String,
    pub device_type: Option<String>,
    pub status: TokenStatus,
    pub expires_at: Option<DateTime<Utc>>,
    pub days_remaining: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeQuota {
    pub max_concurrent_sessions: i32,
    pub active_sessions: i32,
    pub max_bound_devices: i32,
    pub bound_devices: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MePlan {
    pub name: String,
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TicketExchangeRequest {
    pub ticket: String,
    pub device_fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenLoginRequest {
    pub token: String,
    pub device_fingerprint: String,
    #[serde(default)]
    pub remember_device: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeSettingsPatch {
    pub display_name: Option<String>,
    pub locale: Option<String>,
    pub theme: Option<String>,
}

/// 播放会话（§6.2 `playback_session`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybackSession {
    pub id: i64,
    pub session_uid: String,
    pub token_id: i64,
    pub channel_id: i64,
    pub device_id: String,
    pub ip_address: Option<String>,
    pub quality: Option<String>,
    pub started_at: DateTime<Utc>,
    pub last_heartbeat_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybackStartRequest {
    pub channel_id: i64,
    pub quality: Option<String>,
    pub device_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybackStartResponse {
    pub session_uid: String,
    /// 短时效 HMAC 播放地址（§6.4 / §8，默认 300s，上游真实地址不下发）。
    pub play_url: String,
    pub expires_in_sec: i64,
    /// 客户端心跳间隔（§6.3，读系统设置 `heartbeat_interval_sec`，默认 30s）。
    pub heartbeat_interval_sec: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybackHeartbeatRequest {
    pub session_uid: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybackStopRequest {
    pub session_uid: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybackEvent {
    pub id: i64,
    pub session_uid: String,
    pub token_id: i64,
    pub channel_id: i64,
    pub event_type: String,
    pub detail: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// §5：管理后台 — 管理员、RBAC、审计、系统设置、仪表盘
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminRole {
    SuperAdmin,
    Operator,
    Support,
    Readonly,
}

impl AdminRole {
    /// §5.3 权限矩阵（服务端校验，前端仅做展示差异）。
    pub fn can_write_users(&self) -> bool {
        matches!(self, AdminRole::SuperAdmin | AdminRole::Operator)
    }
    pub fn can_write_tokens(&self) -> bool {
        matches!(self, AdminRole::SuperAdmin | AdminRole::Operator)
    }
    pub fn can_write_channels(&self) -> bool {
        matches!(self, AdminRole::SuperAdmin | AdminRole::Operator)
    }
    pub fn can_write_sources(&self) -> bool {
        matches!(self, AdminRole::SuperAdmin)
    }
    pub fn can_read_sources(&self) -> bool {
        matches!(
            self,
            AdminRole::SuperAdmin | AdminRole::Operator | AdminRole::Readonly
        )
    }
    pub fn can_write_settings(&self) -> bool {
        matches!(self, AdminRole::SuperAdmin)
    }
    pub fn can_read_audit(&self) -> bool {
        matches!(self, AdminRole::SuperAdmin)
    }
    pub fn can_kick_session(&self) -> bool {
        matches!(
            self,
            AdminRole::SuperAdmin | AdminRole::Operator | AdminRole::Support
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminUser {
    pub id: i64,
    pub username: String,
    pub role: AdminRole,
    pub totp_enabled: bool,
    pub totp_required: bool,
    /// 是否已配给 TOTP 密钥（secret 非空）。未配给 + totp_required 时，
    /// 登录放行密码（视为配置未完成），由超管在后台配给，不得锁死账号。
    pub totp_provisioned: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminLoginRequest {
    pub username: String,
    pub password: String,
    pub totp_code: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateAdminRequest {
    pub username: String,
    pub password: String,
    pub role: AdminRole,
    pub totp_required: bool,
}

/// 建管理员的返回：totp_required 时服务端当场生成密钥，密钥/URI/二维码
/// 仅返回一次（调用方必须立即展示并让用户扫码配置，之后不再可取）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateAdminResponse {
    pub admin: AdminUser,
    pub totp_secret: Option<String>,
    pub totp_uri: Option<String>,
    pub qr_png: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateAdminRequest {
    pub role: Option<AdminRole>,
    pub totp_required: Option<bool>,
}

/// 审计日志（§3.2：操作者、对象、变更前后快照，保留 180 天 §13）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditLog {
    pub id: i64,
    pub actor: String,
    pub actor_role: Option<String>,
    pub action: String,
    pub object_type: String,
    pub object_id: String,
    pub before_json: Option<serde_json::Value>,
    pub after_json: Option<serde_json::Value>,
    pub ip_address: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// 系统设置（§3.2 全部键，见 `system_setting` 表与默认配置）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemSetting {
    pub key: String,
    pub value: serde_json::Value,
    pub updated_at: DateTime<Utc>,
}

/// §5.2 `GET /api/v1/admin/stats/overview`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatsOverview {
    pub online_users: i64,
    pub active_sessions: i64,
    pub total_channels: i64,
    pub available_channels: i64,
    pub total_tokens: i64,
    pub active_tokens: i64,
    pub last_sync_at: Option<DateTime<Utc>>,
    pub last_sync_status: Option<String>,
    pub alerts: Vec<String>,
}

/// 健康检查。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
    pub database: String,
    pub redis: String,
}

/// M3U 上传解析报告（§7.2：逐条报错并汇总，异常行不得整批失败）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct M3uImportReport {
    pub total_lines: usize,
    pub channels_parsed: usize,
    pub channels_imported: usize,
    pub errors: Vec<M3uLineError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct M3uLineError {
    pub line: usize,
    pub message: String,
}
