# 内部契约（开发文档 v1.2 落地）

> 各 crate 并行开发时以此文件为准，改动契约先改此文件再改代码。

## `iptv-store::Store`（`crates/iptv-store/src/lib.rs`）

```rust
pub enum Dialect { Postgres, MySql, Sqlite }
pub struct Store { /* pool: sqlx::AnyPool, dialect: Dialect */ }
impl Store {
    pub async fn connect(database_url: &str) -> anyhow::Result<Self>;
    pub async fn migrate(&self) -> anyhow::Result<()>; // 按 dialect 跑 migrations/{dialect}/ 内嵌 SQL
    pub fn dialect(&self) -> Dialect;
    pub fn pool(&self) -> &sqlx::AnyPool;
}
```

时间存库：epoch 秒（`i64`）；读出时 `DateTime::from_timestamp(secs, 0)` 转 UTC。
JSON 字段（features / allowed_groups / http_headers 等）存 TEXT。
占位符一律 `?`，执行前由 `iptv-store::ph()` 按方言改写（PG `$1..$n`；sqlx Any 不替我们改写，2026-11-25 CI 契约测试实测）。
新增行取 id：PG 用 `INSERT ... RETURNING id`，MySQL 用 `SELECT LAST_INSERT_ID()`，
SQLite 用 `SELECT last_insert_rowid()`（§10.2 坑 #5）。

### 方法清单（签名中的类型全部来自 `iptv_dto` / `iptv_m3u`）

```rust
// 用户
create_user(&CreateUserRequest) -> AppUser
get_user(id) -> Option<AppUser>
list_users(&PageQuery, Option<UserStatus>) -> Paged<AppUser>
update_user(id, &UpdateUserRequest) -> Option<AppUser>
// Token
create_token(user_id, prefix: &str, hash: &[u8; 32], &CreateTokenRequest) -> UserToken
find_token_by_hash(hash: &[u8; 32]) -> Option<UserToken>
get_token(id) -> Option<UserToken>
list_tokens(Option<user_id>, Option<TokenStatus>, &PageQuery) -> Paged<UserToken>
update_token(id, &UpdateTokenRequest) -> Option<UserToken>
// 频道
create_channel(&CreateChannelRequest) -> Channel
get_channel(id) -> Option<Channel>
list_channels(&PageQuery, Option<group>, Option<ChannelStatus>) -> Paged<Channel>
update_channel(id, &UpdateChannelRequest) -> Option<Channel>
delete_channel(id) -> bool
list_endpoints(channel_id) -> Vec<ChannelEndpoint>
upsert_channels(source_id, Vec<NormalizedChannel>) -> SyncOutcome
list_channels_for_token(&UserToken, &PageQuery, Option<group>) -> Paged<Channel>
// 订阅源 / 同步
create_source(&CreateSourceRequest) -> PlaylistSource
get_source(id) -> Option<PlaylistSource>
list_sources() -> Vec<PlaylistSource>
update_source_sync(id, next_sync_at: DateTime<Utc>, last_status: &str, last_error: Option<&str>)
create_sync_log(source_id) -> i64
finish_sync_log(id, status: &str, added: i64, updated: i64, removed: i64, error: Option<&str>)
list_sync_logs(source_id, &PageQuery) -> Paged<SyncLog>
get_sync_diff(source_id) -> SyncDiff  // 最近一次同步的差异（由 upsert_channels 的结果重建）
// 播放会话
create_playback_session(token_id, channel_id, device_id: &str, session_uid: &str, quality: Option<&str>, ip: Option<&str>) -> PlaybackSession
get_playback_session(session_uid: &str) -> Option<PlaybackSession>
list_active_playback_sessions(&PageQuery) -> Paged<PlaybackSession>
heartbeat_session(session_uid: &str, now: DateTime<Utc>) -> bool
end_session(session_uid: &str, now: DateTime<Utc>, status: &str) -> bool
active_sessions_for_token(token_id) -> Vec<PlaybackSession>
expire_stale_sessions(cutoff: DateTime<Utc>) -> u64
playback_history(token_id, &PageQuery) -> Paged<PlaybackEvent>
record_event(&NewPlaybackEvent)
// 门户会话 / 设备绑定
create_portal_session(token_id, device_id: &str, session_hash: &[u8; 32], ip: Option<&str>, ua: Option<&str>, expires_at: DateTime<Utc>) -> PortalSession
find_portal_session_by_hash(hash: &[u8; 32]) -> Option<PortalSession>
revoke_portal_session(id) -> bool
revoke_all_portal_sessions(token_id, except_id: Option<i64>) -> u64
list_portal_sessions(token_id) -> Vec<PortalSession>
touch_portal_session(id, now: DateTime<Utc>, expires_at: DateTime<Utc>)
bound_device_count(token_id) -> i64
// 套餐 / 分组
create_plan(&CreatePlanRequest) -> Plan
list_plans() -> Vec<Plan>
get_plan(id) -> Option<Plan>
create_group(&CreateGroupRequest) -> ChannelGroup
list_groups() -> Vec<ChannelGroup>
// EPG
create_epg_source(name: &str, url: &str) -> EpgSource
list_epg_sources() -> Vec<EpgSource>
upsert_epg_programs(Vec<EpgProgramInput>) -> u64
programs_for_channel(channel_id, from: DateTime<Utc>, to: DateTime<Utc>) -> Vec<EpgProgram>
current_program(channel_id, now: DateTime<Utc>) -> Option<EpgProgram>
// 管理员 / 审计 / 设置 / 统计
create_admin(username: &str, password_hash: &str, role: AdminRole, totp_required: bool) -> AdminUser
find_admin_by_username(username: &str) -> Option<(AdminUser, String, Option<String>)> // (admin, password_hash, totp_secret)
list_admins() -> Vec<AdminUser>
set_admin_totp(id, secret: &str)
write_audit(&NewAuditLog) -> i64
list_audit(Option<actor>, Option<action>, Option<from>, Option<to>, &PageQuery) -> Paged<AuditLog>
delete_old_audit(before: DateTime<Utc>) -> u64
get_setting(key: &str) -> Option<serde_json::Value>
set_setting(key: &str, value: &serde_json::Value)
list_settings() -> Vec<SystemSetting>
stats_overview() -> StatsOverview
```

```rust
pub struct SyncOutcome { pub added: i64, pub updated: i64, pub removed: i64,
    pub added_channels: Vec<Channel>, pub metadata_changed: Vec<Channel>,
    pub url_changed: Vec<Channel>, pub missing: Vec<Channel> }
pub struct NewAuditLog { pub actor: String, pub actor_role: Option<String>, pub action: String,
    pub object_type: String, pub object_id: String, pub before_json: Option<serde_json::Value>,
    pub after_json: Option<serde_json::Value>, pub ip_address: Option<String> }
pub struct NewPlaybackEvent { pub session_uid: String, pub token_id: i64, pub channel_id: i64,
    pub event_type: String, pub detail: serde_json::Value }
pub struct EpgProgramInput { pub channel_id: i64, pub tvg_id: Option<String>, pub title: String,
    pub description: Option<String>, pub category: Option<String>,
    pub start_at: DateTime<Utc>, pub stop_at: DateTime<Utc> }
```

`upsert_channels` 规则（§7.3）：按 `tvg_id`（无则按规范化 URL）匹配；
`content_hash = sha256(tvg_id|url|name|group)`（`iptv_core::content_hash`）比对，
分新增 / 元数据变更 / URL 变更；库里有、订阅里没有的按源的 `on_missing`
（disable 默认 / delete）处理；`auto_enable_new=false` 时新增频道为 disabled。
分批提交（v0.1.6 WAL 大库修复）：每 1000 台一个短事务，单批次内原子、
跨批次不保证原子——中途失败时已提交批次保留，错误里注明已入库数
（added/updated）；`sync_diff` 只在全部批次成功后由独立短事务写，
写失败只记 error 日志、不回滚已入库频道；导入期间并发 PATCH 的字段
（status/display_name 等 upsert 不写的列）不被后一批覆盖。`sync_diff`
四个列表落库时截断到 1000 条，真实总数记 `*_total`（未截断时 214MB
单行 JSON 一条 UPDATE 持写锁 72 秒，并发写等满 busy_timeout 后 500）。

## `iptv-auth` / `iptv-concurrency` / `iptv-m3u` / `iptv-epg` / `iptv-i18n`

已实现并有单测（见各 crate `src/lib.rs`）：

- `iptv_auth::{hash_password, verify_password, sha256_hex, mask_ip, Totp,
  generate_totp_secret, RateLimiter, now_unix}`
- `iptv_concurrency::{ConcurrencyLimiter, ACQUIRE_LUA, HEARTBEAT_TTL_SEC}`
  `acquire(token_id, session_uid, max) -> AcquireOutcome{acquired, current}`，
  `heartbeat / release / count`，`uses_redis()`。
- `iptv_m3u::{parse_m3u(&str) -> ParseOutcome, NormalizedChannel}`
- `iptv_epg::{parse_xmltv(&[u8]) -> Result<XmltvData, EpgError>, XmltvChannel, XmltvProgramme}`
- `iptv_i18n::{I18n::new(), negotiate(user, cookie, accept) -> String,
  t(locale, key, Option<&FluentArgs>) -> String, t_simple, error_message(locale, code),
  format_datetime, format_number, SUPPORTED_LOCALES, DEFAULT_LOCALE}`

## `iptv-api` 的 `AppState`（`crates/iptv-api/src/lib.rs` 导出）

```rust
pub struct Config {
    pub database_url: String,
    pub redis_url: Option<String>,
    pub play_secret: String,      // HMAC 密钥（环境变量 PLAY_SECRET，未配则启动时随机生成并打印警告）
    pub portal_base_url: String,  // 如 https://portal.example.com
    pub admin_secret: String,     // 管理员会话 HMAC 密钥
}
pub struct AppState {
    pub store: Arc<Store>,
    pub limiter: Arc<ConcurrencyLimiter>,
    pub i18n: Arc<I18n>,
    pub config: Arc<Config>,
}
impl AppState {
    pub async fn new(config: Config) -> anyhow::Result<Self>;
    pub fn router(state: Arc<Self>) -> axum::Router; // 全部路由见 §3.1/§4.5/§5.2
}
```

管理员会话：登录成功后下发 `iptv_admin_sid` Cookie，值为
`base64(admin_id|exp|hmac(admin_secret, admin_id|exp))`，服务端验签，
不另建表；TOTP 在登录时校验（`totp_required` 或已启用时必填）。
门户会话：`iptv_portal_sid` Cookie，值 256 bit 随机串，DB 只存 SHA-256（§4.3）。
