//! Repository trait 层（m20，开发文档 §10.4）。
//!
//! 业务层只依赖这些 trait（`Arc<dyn TokenRepository>` 式注入，见
//! `iptv-api::tickets::TicketStore`；或泛型约束 `R: ChannelRepository`，
//! 见 `iptv-worker` 的 `probe_sample` / `import_epg_source`），不再
//! 直接依赖具体 `Store`——`Store` 只是本 trait 族的一份实现
//! （`sqlx::Any` 单代码路径 + `ph()` 占位符改写，三方言共用）。
//!
//! 方法签名与现有 `Store` 方法逐一对应（§10.4 的 trait 草图只画了
//! `TokenRepo` / `ChannelRepo` 各两三个方法，且名字与现状略有出入：
//! 草图的 `find_by_hash` / `update_quota` / `list_by_user` /
//! `upsert_batch` / `list_for_token`，对应现状的 `find_token_by_hash` /
//! `update_token` / `list_tokens` / `upsert_channels` /
//! `list_channels_for_token`——语义相同，签名以现状为准，不为贴合草图
//! 改名造成无意义 churn）。
//!
//! 用 `async_trait` 而不用原生 `async fn in trait`：原生写法（Rust 1.75+
//! 稳定）在 trait 对象上不可用（返回的 `impl Future` 不是对象安全），
//! 而 §10.4 要求 `Arc<dyn TokenRepo>` 注入；`async_trait` 把方法改写为
//! 返回装箱 future，`dyn` 才成立。
//!
//! 关于 §10.4 的两处有意偏差（m20 选 B，见 `docs/ACCEPTANCE.md` §16
//! 数据库验收）——此处写明，免得后人误以为“漏做”：
//! - 方言差异**没有**拆成 `pg.rs` / `mysql.rs` / `sqlite.rs` 三个文件：
//!   单一 `Any` 实现 + `ph()` + 按方言分的迁移脚本已覆盖三方言，
//!   契约测试（`crates/iptv-store/tests/contract.rs`）同一批用例跑三套库。
//!   拆三份实现是三倍维护量，且与已验证的单代码路径直接冲突。
//! - 没有 `db-postgres` / `db-mysql` / `db-sqlite` Cargo feature：
//!   方言由运行时 `DATABASE_URL` 的 scheme 选择（`Store::connect`），
//!   同一个二进制可连三种库；feature 若只做“薄门控”而不真正开关驱动，
//!   是名不副实的摆设，若真开关 sqlx 驱动则会破坏“只改 URL 就换库”
//!   的单代码路径与三方言契约测试。故 feature 一节以文档修正为准，
//!   不在 `Cargo.toml` 里立三个空 feature。
// 作者: Passwd Word

use super::*;
use iptv_m3u::NormalizedChannel;

/// Token 聚合（`user_token`，§3.2 / §6.2）。
#[async_trait::async_trait]
pub trait TokenRepository: Send + Sync {
    async fn create_token(
        &self,
        user_id: i64,
        prefix: &str,
        hash: &[u8; 32],
        req: &CreateTokenRequest,
    ) -> anyhow::Result<UserToken>;
    async fn find_token_by_hash(&self, hash: &[u8; 32]) -> anyhow::Result<Option<UserToken>>;
    async fn get_token(&self, id: i64) -> anyhow::Result<Option<UserToken>>;
    async fn list_tokens(
        &self,
        user_id: Option<i64>,
        status: Option<TokenStatus>,
        q: &PageQuery,
    ) -> anyhow::Result<Paged<UserToken>>;
    async fn update_token(
        &self,
        id: i64,
        req: &UpdateTokenRequest,
    ) -> anyhow::Result<Option<UserToken>>;
    async fn delete_token(&self, id: i64) -> anyhow::Result<bool>;
}

/// 频道聚合（`channel` / `channel_endpoint`，§3.2 / §7.3）。
#[async_trait::async_trait]
pub trait ChannelRepository: Send + Sync {
    async fn create_channel(&self, req: &CreateChannelRequest) -> anyhow::Result<Channel>;
    async fn get_channel(&self, id: i64) -> anyhow::Result<Option<Channel>>;
    async fn list_channels(
        &self,
        q: &PageQuery,
        group: Option<&str>,
        status: Option<ChannelStatus>,
    ) -> anyhow::Result<Paged<Channel>>;
    async fn update_channel(
        &self,
        id: i64,
        req: &UpdateChannelRequest,
    ) -> anyhow::Result<Option<Channel>>;
    async fn update_channel_logo_url(&self, id: i64, logo_url: &str) -> anyhow::Result<()>;
    async fn delete_channel(&self, id: i64) -> anyhow::Result<bool>;
    async fn list_channels_for_token(
        &self,
        token: &UserToken,
        q: &PageQuery,
        group: Option<&str>,
    ) -> anyhow::Result<Paged<Channel>>;
    async fn upsert_channels(
        &self,
        source_id: i64,
        items: Vec<NormalizedChannel>,
    ) -> anyhow::Result<SyncOutcome>;
    async fn get_sync_diff(&self, source_id: i64) -> anyhow::Result<SyncDiff>;
    async fn list_endpoints(&self, channel_id: i64) -> anyhow::Result<Vec<ChannelEndpoint>>;
    async fn record_channel_probe(&self, channel_id: i64, available: bool) -> anyhow::Result<()>;
    async fn list_channel_match_keys(
        &self,
    ) -> anyhow::Result<Vec<(i64, Option<String>, String, i64)>>;
}

/// 用户聚合（`app_user`，§3.2 / §9）。
#[async_trait::async_trait]
pub trait UserRepository: Send + Sync {
    async fn create_user(&self, req: &CreateUserRequest) -> anyhow::Result<AppUser>;
    async fn get_user(&self, id: i64) -> anyhow::Result<Option<AppUser>>;
    async fn list_users(
        &self,
        q: &PageQuery,
        status: Option<UserStatus>,
    ) -> anyhow::Result<Paged<AppUser>>;
    async fn update_user(
        &self,
        id: i64,
        req: &UpdateUserRequest,
    ) -> anyhow::Result<Option<AppUser>>;
    async fn delete_user(&self, id: i64) -> anyhow::Result<bool>;
}

/// 订阅源聚合（`playlist_source` / `sync_log`，§7.3 / §7.4）。
#[async_trait::async_trait]
pub trait SourceRepository: Send + Sync {
    async fn create_source(&self, req: &CreateSourceRequest) -> anyhow::Result<PlaylistSource>;
    async fn get_source(&self, id: i64) -> anyhow::Result<Option<PlaylistSource>>;
    async fn list_sources(&self) -> anyhow::Result<Vec<PlaylistSource>>;
    async fn update_source(
        &self,
        id: i64,
        req: &UpdateSourceRequest,
    ) -> anyhow::Result<Option<PlaylistSource>>;
    async fn delete_source(&self, id: i64) -> anyhow::Result<bool>;
    async fn update_source_sync(
        &self,
        id: i64,
        next_sync_at: DateTime<Utc>,
        last_status: &str,
        last_error: Option<&str>,
    ) -> anyhow::Result<()>;
    async fn claim_source_sync(&self, id: i64, next_sync_at: DateTime<Utc>)
        -> anyhow::Result<bool>;
    async fn create_sync_log(&self, source_id: i64) -> anyhow::Result<i64>;
    async fn finish_sync_log(
        &self,
        id: i64,
        status: &str,
        added: i64,
        updated: i64,
        removed: i64,
        error: Option<&str>,
    ) -> anyhow::Result<()>;
    async fn consecutive_sync_failures(&self, source_id: i64) -> anyhow::Result<u32>;
    async fn list_sync_logs(&self, source_id: i64, q: &PageQuery)
        -> anyhow::Result<Paged<SyncLog>>;
}

/// 套餐聚合（`plan`，§3.2）。
#[async_trait::async_trait]
pub trait PlanRepository: Send + Sync {
    async fn create_plan(&self, req: &CreatePlanRequest) -> anyhow::Result<Plan>;
    async fn list_plans(&self) -> anyhow::Result<Vec<Plan>>;
    async fn get_plan(&self, id: i64) -> anyhow::Result<Option<Plan>>;
    async fn update_plan(&self, id: i64, req: &UpdatePlanRequest) -> anyhow::Result<Option<Plan>>;
    async fn delete_plan(&self, id: i64) -> anyhow::Result<bool>;
    async fn count_users_with_plan(&self, plan_id: i64) -> anyhow::Result<i64>;
}

/// 分组聚合（`channel_group`，§3.2）。
#[async_trait::async_trait]
pub trait GroupRepository: Send + Sync {
    async fn create_group(&self, req: &CreateGroupRequest) -> anyhow::Result<ChannelGroup>;
    async fn list_groups(&self) -> anyhow::Result<Vec<ChannelGroup>>;
    async fn get_group(&self, id: i64) -> anyhow::Result<Option<ChannelGroup>>;
    async fn update_group(
        &self,
        id: i64,
        req: &UpdateGroupRequest,
    ) -> anyhow::Result<Option<ChannelGroup>>;
    async fn delete_group(&self, id: i64) -> anyhow::Result<bool>;
}

/// EPG 聚合（`epg_source` / `epg_channel_map` / `epg_program`，§5.1）。
#[async_trait::async_trait]
pub trait EpgRepository: Send + Sync {
    async fn create_epg_source(&self, name: &str, url: &str) -> anyhow::Result<EpgSource>;
    async fn list_epg_sources(&self) -> anyhow::Result<Vec<EpgSource>>;
    async fn get_epg_source(&self, id: i64) -> anyhow::Result<Option<EpgSource>>;
    async fn update_epg_source(
        &self,
        id: i64,
        req: &UpdateEpgSourceRequest,
    ) -> anyhow::Result<Option<EpgSource>>;
    async fn delete_epg_source(&self, id: i64) -> anyhow::Result<bool>;
    async fn list_epg_channel_maps(
        &self,
        epg_source_id: Option<i64>,
        channel_id: Option<i64>,
    ) -> anyhow::Result<Vec<EpgChannelMap>>;
    async fn get_epg_channel_map(
        &self,
        epg_source_id: i64,
        tvg_id: &str,
    ) -> anyhow::Result<Option<EpgChannelMap>>;
    async fn upsert_epg_channel_map(
        &self,
        epg_source_id: i64,
        tvg_id: &str,
        channel_id: i64,
    ) -> anyhow::Result<EpgChannelMap>;
    async fn delete_epg_channel_map(&self, id: i64) -> anyhow::Result<bool>;
    async fn upsert_epg_programs(&self, items: Vec<EpgProgramInput>) -> anyhow::Result<u64>;
    async fn programs_for_channel(
        &self,
        channel_id: i64,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> anyhow::Result<Vec<EpgProgram>>;
    async fn current_program(
        &self,
        channel_id: i64,
        now: DateTime<Utc>,
    ) -> anyhow::Result<Option<EpgProgram>>;
}

/// 一次性登录票据聚合（`login_ticket`，§4.3；M15 选 B 落库）。
/// 不在 §10.4 点名之列，但 `iptv-api::tickets::TicketStore` 以
/// `Arc<dyn TicketRepository>` 注入，是 trait 层的一处代表性调用方。
#[async_trait::async_trait]
pub trait TicketRepository: Send + Sync {
    async fn issue_login_ticket(
        &self,
        ticket_hash: &str,
        token_id: i64,
        ip_prefix: Option<&str>,
        expires_at: DateTime<Utc>,
    ) -> anyhow::Result<()>;
    async fn consume_login_ticket(
        &self,
        ticket_hash: &str,
        now: DateTime<Utc>,
    ) -> anyhow::Result<Option<ConsumedTicket>>;
    async fn purge_expired_tickets(&self, now: DateTime<Utc>) -> anyhow::Result<u64>;
}

// ---------------------------------------------------------------------------
// `Store` 实现上述 trait：方法体只转发到同名的 inherent 方法。
// 方法调用解析优先 inherent 实现（`self.create_token(...)` 在此解析为
// `impl Store` 的方法，而非本 trait 方法），故无递归。
// ---------------------------------------------------------------------------

#[async_trait::async_trait]
impl TokenRepository for Store {
    async fn create_token(
        &self,
        user_id: i64,
        prefix: &str,
        hash: &[u8; 32],
        req: &CreateTokenRequest,
    ) -> anyhow::Result<UserToken> {
        self.create_token(user_id, prefix, hash, req).await
    }
    async fn find_token_by_hash(&self, hash: &[u8; 32]) -> anyhow::Result<Option<UserToken>> {
        self.find_token_by_hash(hash).await
    }
    async fn get_token(&self, id: i64) -> anyhow::Result<Option<UserToken>> {
        self.get_token(id).await
    }
    async fn list_tokens(
        &self,
        user_id: Option<i64>,
        status: Option<TokenStatus>,
        q: &PageQuery,
    ) -> anyhow::Result<Paged<UserToken>> {
        self.list_tokens(user_id, status, q).await
    }
    async fn update_token(
        &self,
        id: i64,
        req: &UpdateTokenRequest,
    ) -> anyhow::Result<Option<UserToken>> {
        self.update_token(id, req).await
    }
    async fn delete_token(&self, id: i64) -> anyhow::Result<bool> {
        self.delete_token(id).await
    }
}

#[async_trait::async_trait]
impl ChannelRepository for Store {
    async fn create_channel(&self, req: &CreateChannelRequest) -> anyhow::Result<Channel> {
        self.create_channel(req).await
    }
    async fn get_channel(&self, id: i64) -> anyhow::Result<Option<Channel>> {
        self.get_channel(id).await
    }
    async fn list_channels(
        &self,
        q: &PageQuery,
        group: Option<&str>,
        status: Option<ChannelStatus>,
    ) -> anyhow::Result<Paged<Channel>> {
        self.list_channels(q, group, status).await
    }
    async fn update_channel(
        &self,
        id: i64,
        req: &UpdateChannelRequest,
    ) -> anyhow::Result<Option<Channel>> {
        self.update_channel(id, req).await
    }
    async fn update_channel_logo_url(&self, id: i64, logo_url: &str) -> anyhow::Result<()> {
        self.update_channel_logo_url(id, logo_url).await
    }
    async fn delete_channel(&self, id: i64) -> anyhow::Result<bool> {
        self.delete_channel(id).await
    }
    async fn list_channels_for_token(
        &self,
        token: &UserToken,
        q: &PageQuery,
        group: Option<&str>,
    ) -> anyhow::Result<Paged<Channel>> {
        self.list_channels_for_token(token, q, group).await
    }
    async fn upsert_channels(
        &self,
        source_id: i64,
        items: Vec<NormalizedChannel>,
    ) -> anyhow::Result<SyncOutcome> {
        self.upsert_channels(source_id, items).await
    }
    async fn get_sync_diff(&self, source_id: i64) -> anyhow::Result<SyncDiff> {
        self.get_sync_diff(source_id).await
    }
    async fn list_endpoints(&self, channel_id: i64) -> anyhow::Result<Vec<ChannelEndpoint>> {
        self.list_endpoints(channel_id).await
    }
    async fn record_channel_probe(&self, channel_id: i64, available: bool) -> anyhow::Result<()> {
        self.record_channel_probe(channel_id, available).await
    }
    async fn list_channel_match_keys(
        &self,
    ) -> anyhow::Result<Vec<(i64, Option<String>, String, i64)>> {
        self.list_channel_match_keys().await
    }
}

#[async_trait::async_trait]
impl UserRepository for Store {
    async fn create_user(&self, req: &CreateUserRequest) -> anyhow::Result<AppUser> {
        self.create_user(req).await
    }
    async fn get_user(&self, id: i64) -> anyhow::Result<Option<AppUser>> {
        self.get_user(id).await
    }
    async fn list_users(
        &self,
        q: &PageQuery,
        status: Option<UserStatus>,
    ) -> anyhow::Result<Paged<AppUser>> {
        self.list_users(q, status).await
    }
    async fn update_user(
        &self,
        id: i64,
        req: &UpdateUserRequest,
    ) -> anyhow::Result<Option<AppUser>> {
        self.update_user(id, req).await
    }
    async fn delete_user(&self, id: i64) -> anyhow::Result<bool> {
        self.delete_user(id).await
    }
}

#[async_trait::async_trait]
impl SourceRepository for Store {
    async fn create_source(&self, req: &CreateSourceRequest) -> anyhow::Result<PlaylistSource> {
        self.create_source(req).await
    }
    async fn get_source(&self, id: i64) -> anyhow::Result<Option<PlaylistSource>> {
        self.get_source(id).await
    }
    async fn list_sources(&self) -> anyhow::Result<Vec<PlaylistSource>> {
        self.list_sources().await
    }
    async fn update_source(
        &self,
        id: i64,
        req: &UpdateSourceRequest,
    ) -> anyhow::Result<Option<PlaylistSource>> {
        self.update_source(id, req).await
    }
    async fn delete_source(&self, id: i64) -> anyhow::Result<bool> {
        self.delete_source(id).await
    }
    async fn update_source_sync(
        &self,
        id: i64,
        next_sync_at: DateTime<Utc>,
        last_status: &str,
        last_error: Option<&str>,
    ) -> anyhow::Result<()> {
        self.update_source_sync(id, next_sync_at, last_status, last_error)
            .await
    }
    async fn claim_source_sync(
        &self,
        id: i64,
        next_sync_at: DateTime<Utc>,
    ) -> anyhow::Result<bool> {
        self.claim_source_sync(id, next_sync_at).await
    }
    async fn create_sync_log(&self, source_id: i64) -> anyhow::Result<i64> {
        self.create_sync_log(source_id).await
    }
    async fn finish_sync_log(
        &self,
        id: i64,
        status: &str,
        added: i64,
        updated: i64,
        removed: i64,
        error: Option<&str>,
    ) -> anyhow::Result<()> {
        self.finish_sync_log(id, status, added, updated, removed, error)
            .await
    }
    async fn consecutive_sync_failures(&self, source_id: i64) -> anyhow::Result<u32> {
        self.consecutive_sync_failures(source_id).await
    }
    async fn list_sync_logs(
        &self,
        source_id: i64,
        q: &PageQuery,
    ) -> anyhow::Result<Paged<SyncLog>> {
        self.list_sync_logs(source_id, q).await
    }
}

#[async_trait::async_trait]
impl PlanRepository for Store {
    async fn create_plan(&self, req: &CreatePlanRequest) -> anyhow::Result<Plan> {
        self.create_plan(req).await
    }
    async fn list_plans(&self) -> anyhow::Result<Vec<Plan>> {
        self.list_plans().await
    }
    async fn get_plan(&self, id: i64) -> anyhow::Result<Option<Plan>> {
        self.get_plan(id).await
    }
    async fn update_plan(&self, id: i64, req: &UpdatePlanRequest) -> anyhow::Result<Option<Plan>> {
        self.update_plan(id, req).await
    }
    async fn delete_plan(&self, id: i64) -> anyhow::Result<bool> {
        self.delete_plan(id).await
    }
    async fn count_users_with_plan(&self, plan_id: i64) -> anyhow::Result<i64> {
        self.count_users_with_plan(plan_id).await
    }
}

#[async_trait::async_trait]
impl GroupRepository for Store {
    async fn create_group(&self, req: &CreateGroupRequest) -> anyhow::Result<ChannelGroup> {
        self.create_group(req).await
    }
    async fn list_groups(&self) -> anyhow::Result<Vec<ChannelGroup>> {
        self.list_groups().await
    }
    async fn get_group(&self, id: i64) -> anyhow::Result<Option<ChannelGroup>> {
        self.get_group(id).await
    }
    async fn update_group(
        &self,
        id: i64,
        req: &UpdateGroupRequest,
    ) -> anyhow::Result<Option<ChannelGroup>> {
        self.update_group(id, req).await
    }
    async fn delete_group(&self, id: i64) -> anyhow::Result<bool> {
        self.delete_group(id).await
    }
}

#[async_trait::async_trait]
impl EpgRepository for Store {
    async fn create_epg_source(&self, name: &str, url: &str) -> anyhow::Result<EpgSource> {
        self.create_epg_source(name, url).await
    }
    async fn list_epg_sources(&self) -> anyhow::Result<Vec<EpgSource>> {
        self.list_epg_sources().await
    }
    async fn get_epg_source(&self, id: i64) -> anyhow::Result<Option<EpgSource>> {
        self.get_epg_source(id).await
    }
    async fn update_epg_source(
        &self,
        id: i64,
        req: &UpdateEpgSourceRequest,
    ) -> anyhow::Result<Option<EpgSource>> {
        self.update_epg_source(id, req).await
    }
    async fn delete_epg_source(&self, id: i64) -> anyhow::Result<bool> {
        self.delete_epg_source(id).await
    }
    async fn list_epg_channel_maps(
        &self,
        epg_source_id: Option<i64>,
        channel_id: Option<i64>,
    ) -> anyhow::Result<Vec<EpgChannelMap>> {
        self.list_epg_channel_maps(epg_source_id, channel_id).await
    }
    async fn get_epg_channel_map(
        &self,
        epg_source_id: i64,
        tvg_id: &str,
    ) -> anyhow::Result<Option<EpgChannelMap>> {
        self.get_epg_channel_map(epg_source_id, tvg_id).await
    }
    async fn upsert_epg_channel_map(
        &self,
        epg_source_id: i64,
        tvg_id: &str,
        channel_id: i64,
    ) -> anyhow::Result<EpgChannelMap> {
        self.upsert_epg_channel_map(epg_source_id, tvg_id, channel_id)
            .await
    }
    async fn delete_epg_channel_map(&self, id: i64) -> anyhow::Result<bool> {
        self.delete_epg_channel_map(id).await
    }
    async fn upsert_epg_programs(&self, items: Vec<EpgProgramInput>) -> anyhow::Result<u64> {
        self.upsert_epg_programs(items).await
    }
    async fn programs_for_channel(
        &self,
        channel_id: i64,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> anyhow::Result<Vec<EpgProgram>> {
        self.programs_for_channel(channel_id, from, to).await
    }
    async fn current_program(
        &self,
        channel_id: i64,
        now: DateTime<Utc>,
    ) -> anyhow::Result<Option<EpgProgram>> {
        self.current_program(channel_id, now).await
    }
}

#[async_trait::async_trait]
impl TicketRepository for Store {
    async fn issue_login_ticket(
        &self,
        ticket_hash: &str,
        token_id: i64,
        ip_prefix: Option<&str>,
        expires_at: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        self.issue_login_ticket(ticket_hash, token_id, ip_prefix, expires_at)
            .await
    }
    async fn consume_login_ticket(
        &self,
        ticket_hash: &str,
        now: DateTime<Utc>,
    ) -> anyhow::Result<Option<ConsumedTicket>> {
        self.consume_login_ticket(ticket_hash, now).await
    }
    async fn purge_expired_tickets(&self, now: DateTime<Utc>) -> anyhow::Result<u64> {
        self.purge_expired_tickets(now).await
    }
}
