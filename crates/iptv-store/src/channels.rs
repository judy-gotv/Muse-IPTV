//! 频道（`channel` / `channel_endpoint`，开发文档 §7 / §9）。
//! `create_channel` 同步写 `channel_endpoint`（主/备地址，补充契约）。
// 作者: Passwd Word

use super::*;
use iptv_core::{content_hash, normalize_play_url};
use iptv_m3u::NormalizedChannel;
use std::collections::{HashMap, HashSet};

/// M-D：sqlx 0.8 不公开 `sqlx::any::AnyConnection`（只在 `pub(crate)
/// reexports` 里），用 `Database::Connection` 关联类型指代同一类型：
/// 事务连接与池连接都解引用到它，`upsert_channels_exec` 与两个 `_tx`
/// 辅助函数以它为执行器类型。
type AnyConn = <sqlx::Any as sqlx::Database>::Connection;

pub(crate) fn row_to_channel(row: &AnyRow) -> anyhow::Result<Channel> {
    Ok(Channel {
        id: r_i64(row, "id")?,
        tvg_id: r_opt_string(row, "tvg_id")?,
        name: r_string(row, "name")?,
        display_name: r_opt_string(row, "display_name")?,
        logo_url: r_opt_string(row, "logo_url")?,
        group_title: r_opt_string(row, "group_title")?,
        language: r_opt_string(row, "language")?,
        country: r_opt_string(row, "country")?,
        source_id: r_opt_i64(row, "source_id")?,
        content_hash: r_opt_string(row, "content_hash")?,
        status: match r_string(row, "status")?.as_str() {
            "disabled" => ChannelStatus::Disabled,
            _ => ChannelStatus::Active,
        },
        visibility: r_string(row, "visibility")?,
        sort_weight: r_i64(row, "sort_weight")? as i32,
        last_checked_at: r_opt_ts(row, "last_checked_at")?,
        last_available: r_opt_bool(row, "last_available")?,
        created_at: r_ts(row, "created_at")?,
        updated_at: r_ts(row, "updated_at")?,
    })
}

pub(crate) fn row_to_endpoint(row: &AnyRow) -> anyhow::Result<ChannelEndpoint> {
    Ok(ChannelEndpoint {
        id: r_i64(row, "id")?,
        channel_id: r_i64(row, "channel_id")?,
        url: r_string(row, "url")?,
        protocol: r_string(row, "protocol")?,
        quality: r_opt_string(row, "quality")?,
        is_primary: r_bool(row, "is_primary")?,
        priority: r_i64(row, "priority")? as i32,
    })
}

pub(crate) fn channel_status_str(s: &ChannelStatus) -> &'static str {
    match s {
        ChannelStatus::Active => "active",
        ChannelStatus::Disabled => "disabled",
    }
}

const CHANNEL_COLS: &str = "id, tvg_id, name, display_name, logo_url, group_title, language, country, source_id, content_hash, status, visibility, sort_weight, last_checked_at, last_available, created_at, updated_at";
const ENDPOINT_COLS: &str = "id, channel_id, url, protocol, quality, is_primary, priority";

/// v0.1.6 WAL 大库修复：`upsert_channels` 的单批次大小。整批一个事务
/// 在大库下持写锁数十秒，并发写等满 busy_timeout 后 500；改成每批
/// 一个短事务，批次间隙释放写锁（见 `upsert_channels` 文档）。
const UPSERT_BATCH_SIZE: usize = 1000;

/// v0.1.6 WAL 大库修复：`sync_diff` 持久化时每列表最多保留的条数。
/// 未截断时 54 万台的 `missing` 写成 214MB 单行 JSON，一条 UPDATE
/// 持写锁 72 秒，并发写等满 busy_timeout 后 500；截断后整行约 1.6MB，
/// 写锁毫秒级。真实总数记在 `SyncDiff` 的 `*_total` 字段里。
const SYNC_DIFF_LIST_CAP: usize = 1000;

/// `upsert_channels` 的快照读结果：在事务外一次读出（WAL 下读不阻塞
/// 写），各批次共享；批次写入时随新行补登 `by_key` / `endpoints`。
struct UpsertSnapshot {
    existing: Vec<Channel>,
    by_key: HashMap<String, Channel>,
    endpoints: HashMap<i64, Vec<ChannelEndpoint>>,
}

/// 按 tvg_id（优先）或规范化主地址给频道定去重键（M16：原为闭包、
/// 借住 `endpoints` 导致循环内无法补登新插入行的端点，改成独立函数，
/// 供 `load_upsert_snapshot` 与 `apply_missing` 共用）。
fn key_of_existing(ch: &Channel, endpoints: &HashMap<i64, Vec<ChannelEndpoint>>) -> String {
    if let Some(t) = &ch.tvg_id {
        if !t.trim().is_empty() {
            return format!("tvg:{t}");
        }
    }
    let eps = endpoints.get(&ch.id);
    let primary = eps.and_then(|v| v.iter().find(|e| e.is_primary).or_else(|| v.first()));
    format!(
        "url:{}",
        primary
            .map(|e| normalize_play_url(&e.url))
            .unwrap_or_default()
    )
}

/// 协议判定（补充契约）：`.m3u8`→hls、`.mpd`→dash、`rtmp://`→rtmp、
/// `rtsp://`→rtsp，其余→hls；`req.protocol` 有值时优先。
pub(crate) fn protocol_for(url: &str, hint: Option<&str>) -> String {
    if let Some(h) = hint {
        let h = h.trim().to_lowercase();
        if !h.is_empty() {
            return h;
        }
    }
    let lower = url.to_lowercase();
    let base = lower.split(['?', '#']).next().unwrap_or(&lower);
    if base.ends_with(".m3u8") {
        "hls".to_string()
    } else if base.ends_with(".mpd") {
        "dash".to_string()
    } else if lower.starts_with("rtmp://") {
        "rtmp".to_string()
    } else if lower.starts_with("rtsp://") {
        "rtsp".to_string()
    } else {
        "hls".to_string()
    }
}

/// M-D（v0.1.6）：事务内取新行 id（`insert_id!` 的事务版）。
/// PG 用 `INSERT ... RETURNING id`；MySQL/SQLite 在同一事务连接上执行
/// INSERT 后取 `LAST_INSERT_ID()` / `last_insert_rowid()`（连接级函数，
/// 换连接会取错 id，因此这里直接在 `tx` 上执行，不经 `pool.acquire()`）。
macro_rules! insert_id_tx {
    ($tx:expr, $dialect:expr, $sql:literal, $($arg:expr),* $(,)?) => {{
        match $dialect {
            Dialect::Postgres => {
                sqlx::query_scalar::<_, i64>(&ph(
                    concat!($sql, " RETURNING id"),
                    $dialect,
                ))
                    $(.bind($arg))*
                    .fetch_one(&mut *$tx)
                    .await
                    .map_err(anyhow::Error::from)
            }
            _ => {
                sqlx::query($sql)
                    $(.bind($arg))*
                    .execute(&mut *$tx)
                    .await?;
                match $dialect {
                    Dialect::MySql => sqlx::query_scalar("SELECT LAST_INSERT_ID()")
                        .fetch_one(&mut *$tx)
                        .await
                        .map_err(anyhow::Error::from),
                    _ => sqlx::query_scalar("SELECT last_insert_rowid()")
                        .fetch_one(&mut *$tx)
                        .await
                        .map_err(anyhow::Error::from),
                }
            }
        }
    }};
}

/// M-D：`insert_endpoint` 的事务版（`upsert_channels` 内用；`create_channel`
/// 仍走池版 `Store::insert_endpoint`）。
async fn insert_endpoint_tx(
    conn: &mut AnyConn,
    dialect: Dialect,
    channel_id: i64,
    url: &str,
    protocol: &str,
    is_primary: bool,
    priority: i32,
) -> anyhow::Result<i64> {
    insert_id_tx!(
        conn,
        dialect,
        "INSERT INTO channel_endpoint (channel_id, url, protocol, quality, is_primary, priority) VALUES (?, ?, ?, ?, ?, ?)",
        channel_id,
        url,
        protocol,
        None::<String>,
        i64::from(is_primary),
        i64::from(priority)
    )
}

/// M-D：把一次同步的差异写进 V9 `sync_diff` 表（整行 JSON）。
/// 调用方须在短事务内调本函数（`save_sync_diff` 开事务）：差异与频道行
/// 不同提交——差异写失败不回滚已入库频道（见 `upsert_channels` ④）；
/// 先 UPDATE、影响 0 行再 INSERT，三方言通用，避开 PG 的 `ON CONFLICT`
/// 与 MySQL 的 `ON DUPLICATE KEY UPDATE` 两种方言写法。
async fn save_sync_diff_tx(
    conn: &mut AnyConn,
    dialect: Dialect,
    source_id: i64,
    diff: &SyncDiff,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    let json = serde_json::to_string(diff)?;
    let n = sqlx::query(&ph(
        "UPDATE sync_diff SET diff_json = ?, updated_at = ? WHERE source_id = ?",
        dialect,
    ))
    .bind(&json)
    .bind(epoch(now))
    .bind(source_id)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if n == 0 {
        sqlx::query(&ph(
            "INSERT INTO sync_diff (source_id, diff_json, updated_at) VALUES (?, ?, ?)",
            dialect,
        ))
        .bind(source_id)
        .bind(&json)
        .bind(epoch(now))
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

impl Store {
    async fn insert_endpoint(
        &self,
        channel_id: i64,
        url: &str,
        protocol: &str,
        is_primary: bool,
        priority: i32,
    ) -> anyhow::Result<i64> {
        insert_id!(
            self,
            "INSERT INTO channel_endpoint (channel_id, url, protocol, quality, is_primary, priority) VALUES (?, ?, ?, ?, ?, ?)",
            channel_id,
            url,
            protocol,
            None::<String>,
            i64::from(is_primary),
            i64::from(priority)
        )
    }

    pub async fn create_channel(&self, req: &CreateChannelRequest) -> anyhow::Result<Channel> {
        let now = Utc::now();
        let visibility = req
            .visibility
            .clone()
            .unwrap_or_else(|| "public".to_string());
        let id = insert_id!(
            self,
            "INSERT INTO channel (tvg_id, name, display_name, logo_url, group_title, language, country, source_id, content_hash, status, visibility, sort_weight, last_checked_at, last_available, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            req.tvg_id.clone(),
            &req.name,
            req.display_name.clone(),
            req.logo_url.clone(),
            req.group_title.clone(),
            req.language.clone(),
            req.country.clone(),
            None::<i64>,
            None::<String>,
            "active",
            visibility,
            i64::from(req.sort_weight.unwrap_or(0)),
            None::<i64>,
            None::<i64>,
            epoch(now),
            epoch(now)
        )?;
        // 同步写播放地址：主地址 + 备用地址（补充契约）。
        let proto = protocol_for(&req.play_url, req.protocol.as_deref());
        self.insert_endpoint(id, &req.play_url, &proto, true, 0)
            .await?;
        if let Some(backup) = &req.backup_url {
            if !backup.trim().is_empty() && backup != &req.play_url {
                let bproto = protocol_for(backup, req.protocol.as_deref());
                self.insert_endpoint(id, backup, &bproto, false, 1).await?;
            }
        }
        self.get_channel(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("新建频道后回查失败 id={id}"))
    }

    pub async fn get_channel(&self, id: i64) -> anyhow::Result<Option<Channel>> {
        let row = sqlx::query(&ph(
            &format!("SELECT {CHANNEL_COLS} FROM channel WHERE id = ?"),
            self.dialect,
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| row_to_channel(&r)).transpose()
    }

    pub async fn list_channels(
        &self,
        q: &PageQuery,
        group: Option<&str>,
        status: Option<ChannelStatus>,
    ) -> anyhow::Result<Paged<Channel>> {
        let (limit, offset) = page_bounds(q);
        let pat = like_pattern(&q.q);
        // 不用 sqlx::QueryBuilder：Any 的 push_bind 在 PG 下仍写 `?`
        // （见 lib.rs `ph`），这里手拼字符串 + `ph()` + 按顺序 `.bind()`。
        let mut count_sql = String::from("SELECT COUNT(*) FROM channel WHERE 1 = 1");
        let mut list_sql = format!("SELECT {CHANNEL_COLS} FROM channel WHERE 1 = 1");
        if group.is_some() {
            count_sql.push_str(" AND group_title = ?");
            list_sql.push_str(" AND group_title = ?");
        }
        if status.is_some() {
            count_sql.push_str(" AND status = ?");
            list_sql.push_str(" AND status = ?");
        }
        if pat.is_some() {
            count_sql.push_str(" AND (name LIKE ? OR tvg_id LIKE ?)");
            list_sql.push_str(" AND (name LIKE ? OR tvg_id LIKE ?)");
        }
        let count_sql = ph(&count_sql, self.dialect);
        let mut cq = sqlx::query_scalar::<_, i64>(&count_sql);
        if let Some(g) = group {
            cq = cq.bind(g);
        }
        if let Some(s) = &status {
            cq = cq.bind(channel_status_str(s));
        }
        if let Some(p) = &pat {
            cq = cq.bind(p).bind(p);
        }
        let total: i64 = cq.fetch_one(&self.pool).await?;
        list_sql.push_str(" ORDER BY sort_weight DESC, id ASC LIMIT ? OFFSET ?");
        let list_sql = ph(&list_sql, self.dialect);
        let mut lq = sqlx::query(&list_sql);
        if let Some(g) = group {
            lq = lq.bind(g);
        }
        if let Some(s) = &status {
            lq = lq.bind(channel_status_str(s));
        }
        if let Some(p) = &pat {
            lq = lq.bind(p).bind(p);
        }
        let rows = lq.bind(limit).bind(offset).fetch_all(&self.pool).await?;
        let mut items = Vec::with_capacity(rows.len());
        for r in &rows {
            items.push(row_to_channel(r)?);
        }
        Ok(Paged {
            items,
            total: total as u64,
            page: q.page.max(1),
            page_size: q.page_size.clamp(1, 100),
        })
    }

    /// 只改频道台标地址（m9：转存成功后把 `logo_url` 从上游直链换成
    /// 对象存储地址；转存失败不调这里，直链原样保留）。
    pub async fn update_channel_logo_url(&self, id: i64, logo_url: &str) -> anyhow::Result<()> {
        sqlx::query(&ph(
            "UPDATE channel SET logo_url = ?, updated_at = ? WHERE id = ?",
            self.dialect,
        ))
        .bind(logo_url)
        .bind(epoch(Utc::now()))
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 只改提供的字段（M-D，v0.1.6：PATCH 是读-改-写，旧的全行 UPDATE 会把
    /// 并发 upsert 刚写入的其他列用读到的旧值盖回去，造成丢失更新；现只
    /// SET 请求里 `Some` 的列，`sort_weight`/`display_name` 这类 upsert 不碰
    /// 的列在导入期间 PATCH 后仍在。未提供任何字段时不写库，直接回查。
    pub async fn update_channel(
        &self,
        id: i64,
        req: &UpdateChannelRequest,
    ) -> anyhow::Result<Option<Channel>> {
        if self.get_channel(id).await?.is_none() {
            return Ok(None);
        };
        let now = Utc::now();
        // SET 片段与绑定值顺序一致：先各提供字段，最后 `updated_at`，再 `id`。
        let mut sets: Vec<&str> = Vec::new();
        if req.tvg_id.is_some() {
            sets.push("tvg_id = ?");
        }
        if req.name.is_some() {
            sets.push("name = ?");
        }
        if req.display_name.is_some() {
            sets.push("display_name = ?");
        }
        if req.logo_url.is_some() {
            sets.push("logo_url = ?");
        }
        if req.group_title.is_some() {
            sets.push("group_title = ?");
        }
        if req.status.is_some() {
            sets.push("status = ?");
        }
        if req.visibility.is_some() {
            sets.push("visibility = ?");
        }
        if req.sort_weight.is_some() {
            sets.push("sort_weight = ?");
        }
        if sets.is_empty() {
            return self.get_channel(id).await;
        }
        sets.push("updated_at = ?");
        let sql = format!("UPDATE channel SET {} WHERE id = ?", sets.join(", "));
        let sql = ph(&sql, self.dialect);
        let mut q = sqlx::query(&sql);
        if let Some(v) = &req.tvg_id {
            q = q.bind(v.clone());
        }
        if let Some(v) = &req.name {
            q = q.bind(v.clone());
        }
        if let Some(v) = &req.display_name {
            q = q.bind(v.clone());
        }
        if let Some(v) = &req.logo_url {
            q = q.bind(v.clone());
        }
        if let Some(v) = &req.group_title {
            q = q.bind(v.clone());
        }
        if let Some(v) = &req.status {
            q = q.bind(channel_status_str(v));
        }
        if let Some(v) = &req.visibility {
            q = q.bind(v.clone());
        }
        if let Some(v) = req.sort_weight {
            q = q.bind(i64::from(v));
        }
        q.bind(epoch(now)).bind(id).execute(&self.pool).await?;
        self.get_channel(id).await
    }

    pub async fn delete_channel(&self, id: i64) -> anyhow::Result<bool> {
        sqlx::query(&ph(
            "DELETE FROM channel_endpoint WHERE channel_id = ?",
            self.dialect,
        ))
        .bind(id)
        .execute(&self.pool)
        .await?;
        let n = sqlx::query(&ph("DELETE FROM channel WHERE id = ?", self.dialect))
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(n > 0)
    }

    /// 健康探测写回（§7.3 第 7 步）：记录探测时间与是否可用。
    /// 后台频道列表与仪表盘据此展示可用性（`last_available IS NULL` 为未探测）。
    pub async fn record_channel_probe(
        &self,
        channel_id: i64,
        available: bool,
    ) -> anyhow::Result<()> {
        sqlx::query(&ph(
            "UPDATE channel SET last_checked_at = ?, last_available = ?, updated_at = ? WHERE id = ?",
            self.dialect,
        ))
        .bind(epoch(Utc::now()))
        .bind(i64::from(available))
        .bind(epoch(Utc::now()))
        .bind(channel_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// EPG 匹配用键（M7）：一次取回全部频道的 (id, tvg_id, name, sort_weight)，
    /// 按 sort_weight DESC、id ASC 排（与 `list_channels` 的默认排序一致），
    /// 调用方在内存里匹配，避免每个节目一次查库的 N+1。
    pub async fn list_channel_match_keys(
        &self,
    ) -> anyhow::Result<Vec<(i64, Option<String>, String, i64)>> {
        let rows = sqlx::query(
            "SELECT id, tvg_id, name, sort_weight FROM channel ORDER BY sort_weight DESC, id ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            out.push((
                r_i64(r, "id")?,
                r_opt_string(r, "tvg_id")?,
                r_string(r, "name")?,
                r_i64(r, "sort_weight")?,
            ));
        }
        Ok(out)
    }

    pub async fn list_endpoints(&self, channel_id: i64) -> anyhow::Result<Vec<ChannelEndpoint>> {
        let rows = sqlx::query(&ph(&format!(
            "SELECT {ENDPOINT_COLS} FROM channel_endpoint WHERE channel_id = ? ORDER BY priority ASC, id ASC"
        ), self.dialect))
        .bind(channel_id)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            out.push(row_to_endpoint(r)?);
        }
        Ok(out)
    }

    /// 门户频道列表：仅 `active` 且 `public` 的频道；按 Token 的
    /// `allowed_groups` 过滤分组，排除 `denied_channels`。
    /// `allowed_groups` 为空 = 全部组放行（与 iptv-core::authorize_channel
    /// 一致）。
    pub async fn list_channels_for_token(
        &self,
        token: &UserToken,
        q: &PageQuery,
        group: Option<&str>,
    ) -> anyhow::Result<Paged<Channel>> {
        let (limit, offset) = page_bounds(q);
        let pat = like_pattern(&q.q);
        // 不用 sqlx::QueryBuilder：Any 的 push_bind 在 PG 下仍写 `?`
        // （见 lib.rs `ph`），这里手拼字符串 + `ph()` + 按顺序 `.bind()`。
        // count 与 list 两条 SQL 的条件顺序一致，绑定顺序也一致。
        let mut count_sql = String::from(
            "SELECT COUNT(*) FROM channel WHERE status = 'active' AND visibility = 'public'",
        );
        let mut list_sql = format!(
            "SELECT {CHANNEL_COLS} FROM channel WHERE status = 'active' AND visibility = 'public'"
        );
        // 空 allowed_groups = 全部组（与 iptv-core::authorize_channel 一致；
        // 之前这里空列表直接返回 0 条，与播放鉴权放行矛盾）。
        if !token.allowed_groups.is_empty() {
            push_in_list(
                &mut count_sql,
                " AND group_title IN (",
                token.allowed_groups.len(),
            );
            push_in_list(
                &mut list_sql,
                " AND group_title IN (",
                token.allowed_groups.len(),
            );
        }
        if !token.denied_channels.is_empty() {
            push_in_list(
                &mut count_sql,
                " AND id NOT IN (",
                token.denied_channels.len(),
            );
            push_in_list(
                &mut list_sql,
                " AND id NOT IN (",
                token.denied_channels.len(),
            );
        }
        if group.is_some() {
            count_sql.push_str(" AND group_title = ?");
            list_sql.push_str(" AND group_title = ?");
        }
        if pat.is_some() {
            // 搜索同时匹配原始名与译名（§12.7：channel_i18n 有译名时也命中）。
            count_sql.push_str(
                " AND (name LIKE ? OR tvg_id LIKE ? OR id IN (SELECT channel_id FROM channel_i18n WHERE name LIKE ?))",
            );
            list_sql.push_str(
                " AND (name LIKE ? OR tvg_id LIKE ? OR id IN (SELECT channel_id FROM channel_i18n WHERE name LIKE ?))",
            );
        }
        let count_sql = ph(&count_sql, self.dialect);
        let mut cq = sqlx::query_scalar::<_, i64>(&count_sql);
        for g in &token.allowed_groups {
            cq = cq.bind(g);
        }
        for id in &token.denied_channels {
            cq = cq.bind(*id);
        }
        if let Some(g) = group {
            cq = cq.bind(g);
        }
        if let Some(p) = &pat {
            cq = cq.bind(p).bind(p).bind(p);
        }
        let total: i64 = cq.fetch_one(&self.pool).await?;
        list_sql.push_str(" ORDER BY sort_weight DESC, id ASC LIMIT ? OFFSET ?");
        let list_sql = ph(&list_sql, self.dialect);
        let mut lq = sqlx::query(&list_sql);
        for g in &token.allowed_groups {
            lq = lq.bind(g);
        }
        for id in &token.denied_channels {
            lq = lq.bind(*id);
        }
        if let Some(g) = group {
            lq = lq.bind(g);
        }
        if let Some(p) = &pat {
            lq = lq.bind(p).bind(p).bind(p);
        }
        let rows = lq.bind(limit).bind(offset).fetch_all(&self.pool).await?;
        let mut items = Vec::with_capacity(rows.len());
        for r in &rows {
            items.push(row_to_channel(r)?);
        }
        Ok(Paged {
            items,
            total: total as u64,
            page: q.page.max(1),
            page_size: q.page_size.clamp(1, 100),
        })
    }

    /// 同步 upsert（§7.3）：tvg_id 优先匹配，无则按规范化 URL 匹配；
    /// `content_hash` 比对分新增 / 元数据变更 / URL 变更；订阅里消失的
    /// 按源的 `on_missing`（disable 默认 / delete）处理；
    /// `auto_enable_new=false` 时新增频道为 disabled。
    ///
    /// v0.1.6 WAL 大库修复（M-B/M-D）：分批提交，不再整批一个事务。
    /// 流程：① 快照读在事务外（`load_upsert_snapshot`，WAL 下读不阻塞
    /// 写）；② 每 1000 台一个短事务（SQLite `BEGIN IMMEDIATE`，见
    /// `apply_upsert_batch` 内联注释：deferred 事务先读后写，在 WAL 下会取读快照，并发
    /// 写在快照后提交时首次写入即报 `SQLITE_BUSY_SNAPSHOT`（code 517），
    /// 而 busy_timeout 对快照错误无效），批次间隙释放写锁，让并发 PATCH
    /// 进库；③ 缺失处理按 500 id 一块、每块一个短事务；④ 差异最后用
    /// 独立短事务写 V9 `sync_diff` 表。
    /// 语义变化（断点续跑，不再整批原子）：中途失败时已提交批次保留，
    /// 错误里注明已入库数（added/updated），`sync_diff` 不写、保持上次
    /// 成功值；重跑即续跑（已入库行走更新分支，结果幂等）。单批次内仍
    /// 是原子的：同批失败整批回滚。
    /// 调用方须先持认领（`claim_source_sync`，上传走 `run_upload_task`、
    /// 同步走 Worker `sync_source`），本函数本身不认领。
    pub async fn upsert_channels(
        &self,
        source_id: i64,
        items: Vec<NormalizedChannel>,
    ) -> anyhow::Result<SyncOutcome> {
        let source = self
            .get_source(source_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("订阅源不存在 id={source_id}"))?;
        let now = Utc::now();
        // ① 快照读在事务外：`get_source` 与快照读都不占写锁（只做存在性
        // 校验与读 `auto_enable_new` / `on_missing`，读到旧值不影响
        // 正确性；并发 PATCH 的字段保护见 `apply_upsert_batch`）。
        let mut conn = self.pool.acquire().await?;
        let mut snap = self
            .load_upsert_snapshot(&source, &items, &mut conn)
            .await?;
        drop(conn);
        let mut outcome = SyncOutcome::default();
        let mut seen_keys: HashSet<String> = HashSet::with_capacity(items.len());
        let mut committed_items = 0usize;
        // ② 每 1000 台一个短事务；中途失败即停，已提交批次保留，错误里
        // 注明已入库数（调用方任务终态如实报错，可断点续跑）。
        for (bi, chunk) in items.chunks(UPSERT_BATCH_SIZE).enumerate() {
            let n = chunk.len();
            let r = self
                .apply_upsert_batch(&source, chunk, now, &mut snap, &mut seen_keys, &mut outcome)
                .await;
            match r {
                Ok(()) => committed_items += n,
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "upsert failed at batch {bi} ({} items committed in earlier batches: added={} updated={}): {e}",
                        committed_items, outcome.added, outcome.updated
                    ));
                }
            }
        }
        // ③ 缺失处理（每 500 id 一块、每块一个短事务）；失败同样保留已
        // 提交内容，错误里注明已入库数。
        if let Err(e) = self
            .apply_missing(&source, &snap, &seen_keys, now, &mut outcome)
            .await
        {
            return Err(anyhow::anyhow!(
                "upsert failed while applying missing-channel handling (items committed: added={} updated={}): {e}",
                outcome.added, outcome.updated
            ));
        }
        // ④ 差异用独立短事务写；写失败只记 error 日志，不回滚已入库频道，
        // `sync_diff` 保持上次成功值（与失败导入不覆盖差异的旧保证一致）。
        if let Err(e) = self.save_sync_diff(&source, &outcome, now).await {
            tracing::error!(
                "save sync_diff failed source={source_id} (channels already committed, diff stays at last success): {e:#}"
            );
        }
        Ok(outcome)
    }

    /// 差异写 V9 `sync_diff` 表（独立短事务，见 `upsert_channels` ④）。
    /// 四个列表截断到 `SYNC_DIFF_LIST_CAP` 条再写（见常量注释），真实
    /// 总数记在 `*_total` 里；内存里的 `outcome` 向量保持全量（Worker
    /// 台标转存等用它），只截断落库的 JSON。
    async fn save_sync_diff(
        &self,
        source: &PlaylistSource,
        outcome: &SyncOutcome,
        now: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        fn capped(v: Vec<Channel>) -> (Vec<Channel>, u64) {
            let total = v.len() as u64;
            let mut v = v;
            v.truncate(SYNC_DIFF_LIST_CAP);
            (v, total)
        }
        let (added, added_total) = capped(outcome.added_channels.clone());
        let (metadata_changed, metadata_changed_total) = capped(outcome.metadata_changed.clone());
        let (url_changed, url_changed_total) = capped(outcome.url_changed.clone());
        let (missing, missing_total) = capped(outcome.missing.clone());
        let diff = SyncDiff {
            added,
            metadata_changed,
            url_changed,
            missing,
            added_total,
            metadata_changed_total,
            url_changed_total,
            missing_total,
        };
        let source_id = source.id;
        let dialect = self.dialect;
        if dialect == Dialect::Sqlite {
            let mut conn = self.pool.acquire().await?;
            sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
            let r = save_sync_diff_tx(&mut conn, dialect, source_id, &diff, now).await;
            match r {
                Ok(()) => {
                    sqlx::query("COMMIT").execute(&mut *conn).await?;
                    Ok(())
                }
                Err(e) => {
                    let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
                    Err(e)
                }
            }
        } else {
            let mut tx = self.pool.begin().await?;
            let conn: &mut AnyConn = &mut tx;
            save_sync_diff_tx(conn, dialect, source_id, &diff, now).await?;
            tx.commit().await?;
            Ok(())
        }
    }
    /// `upsert_channels` 的快照读阶段：在事务外读（WAL 下读不阻塞
    /// 写），加载本源现有频道 + 全局 tvg 匹配 + 相关端点，建成 `by_key`
    /// 去重表。快照在批次开始前取定；并发 PATCH 在批次间隙进库，见
    /// `apply_upsert_batch` 的 status 处理（不写陈旧快照的 status）。
    async fn load_upsert_snapshot(
        &self,
        source: &PlaylistSource,
        items: &[NormalizedChannel],
        conn: &mut AnyConn,
    ) -> anyhow::Result<UpsertSnapshot> {
        let source_id = source.id;

        // 现有频道 + 各自主地址。
        let existing_rows = sqlx::query(&ph(
            &format!("SELECT {CHANNEL_COLS} FROM channel WHERE source_id = ? ORDER BY id ASC"),
            self.dialect,
        ))
        .bind(source_id)
        .fetch_all(&mut *conn)
        .await?;
        let mut existing = Vec::new();
        for r in &existing_rows {
            existing.push(row_to_channel(r)?);
        }
        // 全局 tvg_id 匹配：唯一索引 uq_channel_tvg 是全局的（不限 source_id），
        // 手动建的频道（source_id 为 NULL）或别源的同名频道也在此列。只按本源
        // 加载去重会误判新增、走纯 INSERT 撞唯一约束（PG contract.rs:124）。
        // 大文件导入：tvg_id 去重用 HashSet（Vec::contains 是 O(n^2)，82 万条
        // 会烧掉 10 分钟以上）；全局匹配的 IN 列表按 500 一批查，单条 SQL 的
        // 绑定变量数必须低于 SQLite 32766 / PG 65535 的上限（6 万条单批会直接
        // 报 too many SQL variables）。
        let mut tvg_ids: HashSet<&str> = HashSet::new();
        for item in items {
            if let Some(t) = item.tvg_id.as_deref().filter(|s| !s.trim().is_empty()) {
                tvg_ids.insert(t);
            }
        }
        let tvg_ids: Vec<&str> = tvg_ids.into_iter().collect();
        let mut global_matches: Vec<Channel> = Vec::new();
        for chunk in tvg_ids.chunks(500) {
            let mut sql = format!("SELECT {CHANNEL_COLS} FROM channel WHERE tvg_id IN (");
            push_in_list(&mut sql, "", chunk.len());
            let sql = ph(&sql, self.dialect);
            let mut q = sqlx::query(&sql);
            for t in chunk {
                q = q.bind(*t);
            }
            for r in q.fetch_all(&mut *conn).await? {
                global_matches.push(row_to_channel(&r)?);
            }
        }
        // 端点按本源频道 + 全局匹配频道的并集加载（UPDATE 分支要靠它判断
        // URL 是否变更；全局匹配的频道不在本源集合里，不补会误判 URL 变更
        // 并重复插入主地址）。
        // 端点只给真正需要的频道加载：无 tvg_id 的本源频道（URL 键需要主
        // 地址）+ 全局 tvg 匹配的频道（UPDATE 分支要判断 URL 是否变更）。
        // tvg 匹配的本源频道一定在 global_matches 里（tvg IN 查询覆盖全
        // 库），URL 匹配的只能是没有 tvg 的频道；其余本源频道的端点在本次
        // 导入里用不到。此前是全量加载：源里有 88 万台时光端点 IN 列表就
        // 要 1760 批查询，一个 4 行的小文件上传也会被拖到 120 秒以上超时。
        let mut ep_ids: HashSet<i64> = existing
            .iter()
            .filter(|c| c.tvg_id.as_deref().is_none_or(|t| t.trim().is_empty()))
            .map(|c| c.id)
            .collect();
        for ch in &global_matches {
            ep_ids.insert(ch.id);
        }
        let ep_ids: Vec<i64> = ep_ids.into_iter().collect();
        let mut endpoints: HashMap<i64, Vec<ChannelEndpoint>> = HashMap::new();
        // 同上：端点 IN 列表按 500 一批（重导入 82 万条时 ep_ids 同样超限）。
        for chunk in ep_ids.chunks(500) {
            // 不用 sqlx::QueryBuilder（见 lib.rs `ph`）：手拼 IN 列表。
            let mut sql =
                format!("SELECT {ENDPOINT_COLS} FROM channel_endpoint WHERE channel_id IN (");
            push_in_list(&mut sql, "", chunk.len());
            sql.push_str(" ORDER BY priority ASC");
            let sql = ph(&sql, self.dialect);
            let mut q = sqlx::query(&sql);
            for id in chunk {
                q = q.bind(*id);
            }
            for r in q.fetch_all(&mut *conn).await? {
                let e = row_to_endpoint(&r)?;
                endpoints.entry(e.channel_id).or_default().push(e);
            }
        }
        let mut by_key: HashMap<String, Channel> = HashMap::new();
        for ch in &existing {
            by_key.insert(key_of_existing(ch, &endpoints), ch.clone());
        }
        // 全局匹配补进同一张去重表（本源优先；键冲突即同一行，唯一索引保证）。
        // 注意：`existing` 仍只含本源频道——下面的“订阅里消失”逻辑只对本源
        // 频道生效，全局匹配的频道不会被误停用/误删。
        for ch in &global_matches {
            by_key
                .entry(key_of_existing(ch, &endpoints))
                .or_insert_with(|| ch.clone());
        }
        Ok(UpsertSnapshot {
            existing,
            by_key,
            endpoints,
        })
    }

    /// `upsert_channels` 的单批次写入：对 `chunk`（≤1000 台）逐行
    /// insert/update，本函数自己开一个短事务（SQLite `BEGIN IMMEDIATE`，
    /// PG/MySQL `pool.begin()`；见内联注释），提交后返回。`snap.by_key` /
    /// `snap.endpoints` 随写入同步更新（新行补登，见 M16），供后一批复用；
    /// `seen_keys` / `outcome` 跨批次累积。
    ///
    /// 并发 PATCH 保护：元数据 UPDATE 只写 M3U 来源列（name / logo_url /
    /// group_title / content_hash / updated_at），不写 `status`——快照里
    /// 的 status 可能已被并发 PATCH 改掉，用旧值回写会把 PATCH 盖回去。
    /// 唯一例外是重启用：快照为 disabled 且源 `auto_enable_new` 时，
    /// 用 `WHERE status = 'disabled'` 按行现值有条件重启用（PATCH 已改
    /// active 的行不受影响；PATCH 在快照后禁用的行因快照为 active 而不
    /// 触发重启用）。`display_name` / `sort_weight` / `visibility` 等
    /// PATCH 列 upsert 从不写。`updated` 里的 status 取快照值 + 重启用
    /// 结果（并发 PATCH 改过的 status 以库里为准，不以此处的快照值为准）。
    async fn apply_upsert_batch(
        &self,
        source: &PlaylistSource,
        chunk: &[NormalizedChannel],
        now: DateTime<Utc>,
        snap: &mut UpsertSnapshot,
        seen_keys: &mut HashSet<String>,
        outcome: &mut SyncOutcome,
    ) -> anyhow::Result<()> {
        if self.dialect == Dialect::Sqlite {
            // SQLite 必须 `BEGIN IMMEDIATE`：deferred 事务先读后写，在 WAL
            // 下会取读快照，并发 PATCH 在快照后提交时，首次写入立即报
            // `SQLITE_BUSY_SNAPSHOT`（code 517），`busy_timeout` 对快照
            // 错误无效——实测并发测试 0.33 秒即失败。IMMEDIATE 开局即持
            // RESERVED 写锁，不取读快照；并发写方等 busy_timeout（M-B 15 秒）
            // 后串行进事务。手动 BEGIN/COMMIT/ROLLBACK 在同一池连接上做；
            // 任何 `?` 返回都走 ROLLBACK 分支，不留半开事务。
            let mut conn = self.pool.acquire().await?;
            sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
            let r = self
                .apply_upsert_batch_tx(source, chunk, now, snap, seen_keys, outcome, &mut conn)
                .await;
            match r {
                Ok(()) => {
                    sqlx::query("COMMIT").execute(&mut *conn).await?;
                    Ok(())
                }
                Err(e) => {
                    let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
                    Err(e)
                }
            }
        } else {
            // PG/MySQL 无快照升级问题：MVCC/行锁下 deferred 事务可直接写。
            let mut tx = self.pool.begin().await?;
            let conn: &mut AnyConn = &mut tx;
            self.apply_upsert_batch_tx(source, chunk, now, snap, seen_keys, outcome, conn)
                .await?;
            tx.commit().await?;
            Ok(())
        }
    }

    /// `apply_upsert_batch` 的事务内本体：全部读写走 `conn`（已在短事务
    /// 内），任何 `?` 返回即由调用方回滚本批。
    /// 8 参数是分批 worker 的固有输入（源/批次/快照/去重视集/结果/连接），
    /// 包成结构体反而让三处调用点更难读，故放行。
    #[allow(clippy::too_many_arguments)]
    async fn apply_upsert_batch_tx(
        &self,
        source: &PlaylistSource,
        chunk: &[NormalizedChannel],
        now: DateTime<Utc>,
        snap: &mut UpsertSnapshot,
        seen_keys: &mut HashSet<String>,
        outcome: &mut SyncOutcome,
        conn: &mut AnyConn,
    ) -> anyhow::Result<()> {
        let source_id = source.id;
        let by_key = &mut snap.by_key;
        let endpoints = &mut snap.endpoints;
        for item in chunk {
            let tvg = item.tvg_id.clone().filter(|s| !s.trim().is_empty());
            let group = item.group.clone().unwrap_or_default();
            let key = match &tvg {
                Some(t) => format!("tvg:{t}"),
                None => format!("url:{}", normalize_play_url(&item.url)),
            };
            seen_keys.insert(key.clone());
            let hash = content_hash(tvg.as_deref().unwrap_or(""), &item.url, &item.name, &group);
            match by_key.get(&key) {
                Some(cur) => {
                    if cur.content_hash.as_deref() == Some(hash.as_str()) {
                        // m9：content_hash 不含 logo（normalize::content_hash
                        // 只取 tvg_id/url/name/group），故仅台标变化时哈希
                        // 不变；单独比对 logo，变了就只更新 logo_url 并记
                        // 入 metadata_changed，让 worker 的 mirror_logos
                        // 接手转存；logo 没变才真正跳过。
                        if cur.logo_url != item.logo {
                            sqlx::query(&ph(
                                "UPDATE channel SET logo_url = ?, updated_at = ? WHERE id = ?",
                                self.dialect,
                            ))
                            .bind(item.logo.clone())
                            .bind(epoch(now))
                            .bind(cur.id)
                            .execute(&mut *conn)
                            .await?;
                            let mut updated = cur.clone();
                            updated.logo_url = item.logo.clone();
                            updated.updated_at = now;
                            outcome.metadata_changed.push(updated);
                            outcome.updated += 1;
                        }
                        continue; // 其余字段无变化
                    }
                    let old_url = endpoints
                        .get(&cur.id)
                        .and_then(|v| v.iter().find(|e| e.is_primary).or_else(|| v.first()))
                        .map(|e| normalize_play_url(&e.url));
                    let url_changed =
                        old_url.as_deref() != Some(normalize_play_url(&item.url).as_str());
                    sqlx::query(
                        &ph("UPDATE channel SET name = ?, logo_url = ?, group_title = ?, content_hash = ?, updated_at = ? WHERE id = ?", self.dialect),
                    )
                    .bind(&item.name)
                    .bind(item.logo.clone())
                    .bind(item.group.clone())
                    .bind(&hash)
                    .bind(epoch(now))
                    .bind(cur.id)
                    .execute(&mut *conn)
                    .await?;
                    // 重启用：只对快照里就是 disabled 的行、且源允许自动启用时，
                    // 按行现值有条件写回 active（`WHERE status = 'disabled'`）。
                    // 快照后 PATCH 改过的 status 不会被旧值盖回（见本函数文档）。
                    let mut reenabled = false;
                    if cur.status == ChannelStatus::Disabled && source.auto_enable_new {
                        let n = sqlx::query(&ph(
                            "UPDATE channel SET status = 'active', updated_at = ? WHERE id = ? AND status = 'disabled'",
                            self.dialect,
                        ))
                        .bind(epoch(now))
                        .bind(cur.id)
                        .execute(&mut *conn)
                        .await?
                        .rows_affected();
                        reenabled = n > 0;
                    }
                    if url_changed {
                        let proto = protocol_for(&item.url, None);
                        if let Some(ep) = endpoints
                            .get(&cur.id)
                            .and_then(|v| v.iter().find(|e| e.is_primary))
                        {
                            sqlx::query(&ph(
                                "UPDATE channel_endpoint SET url = ?, protocol = ? WHERE id = ?",
                                self.dialect,
                            ))
                            .bind(&item.url)
                            .bind(proto)
                            .bind(ep.id)
                            .execute(&mut *conn)
                            .await?;
                        } else {
                            insert_endpoint_tx(
                                &mut *conn,
                                self.dialect,
                                cur.id,
                                &item.url,
                                &proto,
                                true,
                                0,
                            )
                            .await?;
                        }
                    }
                    // 直接由 cur + 本次写入值构造，不回查（刚 UPDATE 的行各字段
                    // 都已知；逐行 get_channel 是大文件导入里每行多一次查询）。
                    let mut updated = cur.clone();
                    updated.name = item.name.clone();
                    updated.logo_url = item.logo.clone();
                    updated.group_title = item.group.clone();
                    updated.content_hash = Some(hash.clone());
                    updated.status = if reenabled {
                        ChannelStatus::Active
                    } else {
                        cur.status.clone()
                    };
                    updated.updated_at = now;
                    if url_changed {
                        outcome.url_changed.push(updated);
                    } else {
                        outcome.metadata_changed.push(updated);
                    }
                    outcome.updated += 1;
                }
                None => {
                    let status = if source.auto_enable_new {
                        "active"
                    } else {
                        "disabled"
                    };
                    let id = insert_id_tx!(
                        conn,
                        self.dialect,
                        "INSERT INTO channel (tvg_id, name, display_name, logo_url, group_title, language, country, source_id, content_hash, status, visibility, sort_weight, last_checked_at, last_available, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                        tvg.clone(),
                        &item.name,
                        None::<String>,
                        item.logo.clone(),
                        item.group.clone(),
                        None::<String>,
                        None::<String>,
                        source_id,
                        &hash,
                        status,
                        "public",
                        0i64,
                        None::<i64>,
                        None::<i64>,
                        epoch(now),
                        epoch(now)
                    )?;
                    let proto = protocol_for(&item.url, None);
                    let ep_id = insert_endpoint_tx(
                        &mut *conn,
                        self.dialect,
                        id,
                        &item.url,
                        &proto,
                        true,
                        0,
                    )
                    .await?;
                    let ch = Channel {
                        id,
                        tvg_id: tvg,
                        name: item.name.clone(),
                        display_name: None,
                        logo_url: item.logo.clone(),
                        group_title: item.group.clone(),
                        language: None,
                        country: None,
                        source_id: Some(source_id),
                        content_hash: Some(hash),
                        status: if status == "active" {
                            ChannelStatus::Active
                        } else {
                            ChannelStatus::Disabled
                        },
                        visibility: "public".to_string(),
                        sort_weight: 0,
                        last_checked_at: None,
                        last_available: None,
                        created_at: now,
                        updated_at: now,
                    };
                    // M16：新插入行立即补登 `by_key` 与 `endpoints`——同一批里
                    // 再出现同 tvg_id（或同 URL）时走更新分支（后者胜出），
                    // 不再重走 INSERT 撞 `uq_channel_tvg`、整批失败（§7.2
                    // 容错：重复不得整批失败）。不补登时第二条同 tvg_id
                    // 仍命中 None 分支 INSERT，报 UNIQUE constraint failed。
                    by_key.insert(key.clone(), ch.clone());
                    endpoints.entry(id).or_default().push(ChannelEndpoint {
                        id: ep_id,
                        channel_id: id,
                        url: item.url.clone(),
                        protocol: proto,
                        quality: None,
                        is_primary: true,
                        priority: 0,
                    });
                    outcome.added_channels.push(ch);
                    outcome.added += 1;
                }
            }
        }

        Ok(())
    }

    /// `upsert_channels` 的缺失处理：订阅里消失的频道按源 `on_missing`
    ///（disable 默认 / delete）处理。先收齐 id 再分批 bulk 写，每 500
    /// id 一块、每块一个短事务，块间释放写锁；逐行写在
    /// 源里有几十万台时会把小文件上传也拖垮（实测 88 万台消失时每行
    /// 约 4 秒）。`missing` / `removed` 记入 `outcome`，供 `sync_diff`
    /// 与调用方计数；某块失败时已提交的块保留，错误向上传播（见
    /// `upsert_channels` ③）。
    async fn apply_missing(
        &self,
        source: &PlaylistSource,
        snap: &UpsertSnapshot,
        seen_keys: &HashSet<String>,
        now: DateTime<Utc>,
        outcome: &mut SyncOutcome,
    ) -> anyhow::Result<()> {
        let mut missing_ids: Vec<i64> = Vec::new();
        for cur in &snap.existing {
            let key = key_of_existing(cur, &snap.endpoints);
            if seen_keys.contains(&key) {
                continue;
            }
            missing_ids.push(cur.id);
            outcome.missing.push(cur.clone());
            outcome.removed += 1;
        }
        if missing_ids.is_empty() {
            return Ok(());
        }
        // 注意：OnMissing 未派生 Copy，用 == 比较，不移出字段。
        let delete = source.on_missing == iptv_dto::OnMissing::Delete;
        let dialect = self.dialect;
        for chunk in missing_ids.chunks(500) {
            let ids: Vec<i64> = chunk.to_vec();
            // 每块一个短事务（SQLite BEGIN IMMEDIATE / 其余 pool.begin），
            // 块间释放写锁；端点删 + 频道删/禁在同一事务内。
            if dialect == Dialect::Sqlite {
                let mut conn = self.pool.acquire().await?;
                sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
                let r = self
                    .apply_missing_chunk_tx(&ids, delete, now, &mut conn)
                    .await;
                match r {
                    Ok(()) => {
                        sqlx::query("COMMIT").execute(&mut *conn).await?;
                    }
                    Err(e) => {
                        let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
                        return Err(e);
                    }
                }
            } else {
                let mut tx = self.pool.begin().await?;
                let conn: &mut AnyConn = &mut tx;
                self.apply_missing_chunk_tx(&ids, delete, now, conn).await?;
                tx.commit().await?;
            }
        }
        Ok(())
    }

    /// `apply_missing` 的单块写入（事务内本体）：端点删 + 频道删/禁在
    /// 同一事务内，任何 `?` 返回即由调用方回滚本块。
    async fn apply_missing_chunk_tx(
        &self,
        ids: &[i64],
        delete: bool,
        now: DateTime<Utc>,
        conn: &mut AnyConn,
    ) -> anyhow::Result<()> {
        if delete {
            let mut sql = String::from("DELETE FROM channel_endpoint WHERE channel_id IN (");
            push_in_list(&mut sql, "", ids.len());
            let sql = ph(&sql, self.dialect);
            let mut q = sqlx::query(&sql);
            for id in ids {
                q = q.bind(*id);
            }
            q.execute(&mut *conn).await?;
            let mut sql = String::from("DELETE FROM channel WHERE id IN (");
            push_in_list(&mut sql, "", ids.len());
            let sql = ph(&sql, self.dialect);
            let mut q = sqlx::query(&sql);
            for id in ids {
                q = q.bind(*id);
            }
            q.execute(&mut *conn).await?;
        } else {
            let mut sql = String::from(
                "UPDATE channel SET status = 'disabled', updated_at = ? WHERE id IN (",
            );
            push_in_list(&mut sql, "", ids.len());
            sql.push_str(" AND status = 'active'");
            let sql = ph(&sql, self.dialect);
            let mut q = sqlx::query(&sql).bind(epoch(now));
            for id in ids {
                q = q.bind(*id);
            }
            q.execute(&mut *conn).await?;
        }
        Ok(())
    }
    /// M-D：差异读 V9 `sync_diff` 表（不再读进程内缓存，重启/多实例下仍
    /// 可查）；从未同步过的源返回空差异。
    pub async fn get_sync_diff(&self, source_id: i64) -> anyhow::Result<SyncDiff> {
        let row = sqlx::query(&ph(
            "SELECT diff_json FROM sync_diff WHERE source_id = ?",
            self.dialect,
        ))
        .bind(source_id)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(SyncDiff {
                added: vec![],
                metadata_changed: vec![],
                url_changed: vec![],
                missing: vec![],
                ..Default::default()
            });
        };
        let json = r_string(&row, "diff_json")?;
        let mut diff: SyncDiff = serde_json::from_str(&json)?;
        // 旧行 JSON 无 `*_total` 字段（反序列化为 0）时用列表长度回退；
        // 新行被截断时 total 已是真实总数，不动它。
        if diff.added_total == 0 {
            diff.added_total = diff.added.len() as u64;
        }
        if diff.metadata_changed_total == 0 {
            diff.metadata_changed_total = diff.metadata_changed.len() as u64;
        }
        if diff.url_changed_total == 0 {
            diff.url_changed_total = diff.url_changed.len() as u64;
        }
        if diff.missing_total == 0 {
            diff.missing_total = diff.missing.len() as u64;
        }
        Ok(diff)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        store: Store,
        path: std::path::PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
            let _ = std::fs::remove_file(format!("{}-wal", self.path.display()));
            let _ = std::fs::remove_file(format!("{}-shm", self.path.display()));
        }
    }

    async fn setup() -> Fixture {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("iptv-store-md-{}-{}.db", std::process::id(), n));
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let store = Store::connect(&url).await.expect("connect sqlite");
        store.migrate().await.expect("migrate");
        Fixture { store, path }
    }

    async fn make_source(s: &Store) -> PlaylistSource {
        s.create_source(&CreateSourceRequest {
            name: "m-d 源".to_string(),
            url: Some("https://example.com/list.m3u".to_string()),
            sync_interval_sec: None,
            http_headers: None,
            epg_url: None,
            on_missing: None,
            auto_enable_new: Some(true),
            enabled: None,
        })
        .await
        .unwrap()
    }

    fn ch(tvg: &str, name: &str, url: &str) -> NormalizedChannel {
        NormalizedChannel {
            tvg_id: Some(tvg.to_string()),
            name: name.to_string(),
            logo: None,
            group: Some("央视".to_string()),
            url: url.to_string(),
            catchup: None,
            catchup_source: None,
        }
    }

    fn patch_req(sort_weight: i32, display_name: &str) -> UpdateChannelRequest {
        UpdateChannelRequest {
            tvg_id: None,
            name: None,
            display_name: Some(display_name.to_string()),
            logo_url: None,
            group_title: None,
            status: None,
            visibility: None,
            sort_weight: Some(sort_weight),
        }
    }

    #[tokio::test]
    async fn upsert_is_atomic_on_failure() {
        // M-D / v0.1.6 分批提交：单批次内仍是原子的——中途注入失败（触发器
        // 让名为 `__boom__` 的行 INSERT 报错），同批先插入的好行不在库里，
        // 原有行不受影响，`sync_diff` 仍是上一次成功的差异（不被失败的导入
        // 覆盖）。跨批次的已提交保留见 `upsert_batches_commit_before_failure`。
        let fx = setup().await;
        let s = &fx.store;
        let src = make_source(s).await;
        let out = s
            .upsert_channels(
                src.id,
                vec![ch("cctv1.cn", "CCTV-1", "https://x.example/cctv1.m3u8")],
            )
            .await
            .unwrap();
        assert_eq!(out.added, 1);
        let orig_id = out.added_channels[0].id;

        sqlx::query(
            "CREATE TRIGGER upsert_atomic_fail BEFORE INSERT ON channel \
             WHEN NEW.name = '__boom__' \
             BEGIN SELECT RAISE(ABORT, 'injected failure'); END",
        )
        .execute(s.pool())
        .await
        .unwrap();
        let err = s
            .upsert_channels(
                src.id,
                vec![
                    ch("cctv9.cn", "CCTV-9", "https://x.example/cctv9.m3u8"),
                    ch("boom.cn", "__boom__", "https://x.example/boom.m3u8"),
                ],
            )
            .await
            .expect_err("boom 行必须让整批失败");
        assert!(err.to_string().contains("injected failure"));

        // 半导入检查：同批的好行未入库，原有行完好。
        assert!(s.get_channel(orig_id).await.unwrap().is_some());
        assert_eq!(
            s.get_channel(orig_id).await.unwrap().unwrap().name,
            "CCTV-1"
        );
        let page = s
            .list_channels(&PageQuery::default(), None, None)
            .await
            .unwrap();
        assert_eq!(page.total, 1, "失败的导入不得留半导入频道");
        // 差异仍是上一次成功的（added 1 条），不是失败导入的。
        let diff = s.get_sync_diff(src.id).await.unwrap();
        assert_eq!(diff.added.len(), 1);
        assert_eq!(diff.added[0].tvg_id.as_deref(), Some("cctv1.cn"));
    }

    #[tokio::test]
    async fn upsert_batches_commit_before_failure() {
        // v0.1.6 WAL 大库修复：分批提交——第 2 批失败时第 1 批（1000 台）已
        // 提交保留，错误里注明已入库数；`sync_diff` 仍是上一次成功的差异。
        let fx = setup().await;
        let s = &fx.store;
        let src = make_source(s).await;
        sqlx::query(
            "CREATE TRIGGER upsert_batch_fail BEFORE INSERT ON channel \
             WHEN NEW.name = '__boom__' \
             BEGIN SELECT RAISE(ABORT, 'injected failure'); END",
        )
        .execute(s.pool())
        .await
        .unwrap();
        let mut items: Vec<NormalizedChannel> = (0..1000)
            .map(|i| {
                ch(
                    &format!("t{i}.cn"),
                    &format!("CH{i}"),
                    &format!("https://x.example/t{i}.m3u8"),
                )
            })
            .collect();
        items.push(ch("boom.cn", "__boom__", "https://x.example/boom.m3u8"));
        let err = s
            .upsert_channels(src.id, items)
            .await
            .expect_err("第 2 批必须失败");
        let msg = err.to_string();
        assert!(msg.contains("injected failure"), "{msg}");
        assert!(msg.contains("1000 items committed"), "{msg}");
        assert!(msg.contains("added=1000"), "{msg}");
        let page = s
            .list_channels(&PageQuery::default(), None, None)
            .await
            .unwrap();
        assert_eq!(page.total, 1000, "第 1 批已提交保留");
        let diff = s.get_sync_diff(src.id).await.unwrap();
        assert_eq!(diff.added.len(), 0, "失败的导入不写差异");
    }

    #[tokio::test]
    async fn sync_diff_lists_are_capped_but_totals_kept() {
        // v0.1.6 WAL 大库修复：差异列表落库时截断到 1000 条（否则 54 万台
        // 的 missing 写成 214MB 单行 JSON，一条 UPDATE 持写锁 72 秒），
        // 真实总数记在 `*_total` 里，读回仍是全量计数。
        let fx = setup().await;
        let s = &fx.store;
        let src = make_source(s).await;
        let items: Vec<NormalizedChannel> = (0..1005)
            .map(|i| {
                ch(
                    &format!("t{i}.cn"),
                    &format!("CH{i}"),
                    &format!("https://x.example/t{i}.m3u8"),
                )
            })
            .collect();
        let out = s.upsert_channels(src.id, items).await.unwrap();
        assert_eq!(out.added, 1005);
        let diff = s.get_sync_diff(src.id).await.unwrap();
        assert_eq!(diff.added.len(), 1000, "落库列表截断到 1000 条");
        assert_eq!(diff.added_total, 1005, "真实总数保留");
        assert!(diff.missing.is_empty());
        assert_eq!(diff.missing_total, 0);
    }

    #[tokio::test]
    async fn sync_diff_totals_fall_back_to_len_for_legacy_rows() {
        // 旧行 JSON 无 `*_total` 字段：读回时用列表长度回退，计数不丢。
        let fx = setup().await;
        let s = &fx.store;
        let src = make_source(s).await;
        s.upsert_channels(
            src.id,
            vec![
                ch("cctv1.cn", "CCTV-1", "https://x.example/cctv1.m3u8"),
                ch("cctv2.cn", "CCTV-2", "https://x.example/cctv2.m3u8"),
            ],
        )
        .await
        .unwrap();
        let mut v = serde_json::to_value(s.get_sync_diff(src.id).await.unwrap()).unwrap();
        let o = v.as_object_mut().unwrap();
        for k in [
            "added_total",
            "metadata_changed_total",
            "url_changed_total",
            "missing_total",
        ] {
            o.remove(k);
        }
        let legacy = serde_json::to_string(&v).unwrap();
        assert!(!legacy.contains("total"));
        sqlx::query("UPDATE sync_diff SET diff_json = ? WHERE source_id = ?")
            .bind(legacy)
            .bind(src.id)
            .execute(s.pool())
            .await
            .unwrap();
        let diff = s.get_sync_diff(src.id).await.unwrap();
        assert_eq!(diff.added.len(), 2);
        assert_eq!(diff.added_total, 2, "旧行读回时 total 用 len() 回退");
        assert_eq!(diff.missing_total, 0);
    }

    #[tokio::test]
    async fn upsert_reenables_disabled_channel_when_auto_enable_new() {
        // 快照为 disabled 且 auto_enable_new 时，改名导入应按行现值重启用。
        let fx = setup().await;
        let s = &fx.store;
        let src = make_source(s).await;
        let out = s
            .upsert_channels(
                src.id,
                vec![ch("cctv1.cn", "CCTV-1", "https://x.example/cctv1.m3u8")],
            )
            .await
            .unwrap();
        let id = out.added_channels[0].id;
        let req = UpdateChannelRequest {
            tvg_id: None,
            name: None,
            display_name: None,
            logo_url: None,
            group_title: None,
            status: Some(ChannelStatus::Disabled),
            visibility: None,
            sort_weight: None,
        };
        s.update_channel(id, &req).await.unwrap();
        assert_eq!(
            s.get_channel(id).await.unwrap().unwrap().status,
            ChannelStatus::Disabled
        );
        let out = s
            .upsert_channels(
                src.id,
                vec![ch("cctv1.cn", "CCTV-1-HD", "https://x.example/cctv1.m3u8")],
            )
            .await
            .unwrap();
        assert_eq!(out.updated, 1);
        assert_eq!(
            s.get_channel(id).await.unwrap().unwrap().status,
            ChannelStatus::Active
        );
    }

    #[tokio::test]
    async fn upsert_does_not_clobber_concurrent_channel_patch() {
        // v0.1.6 WAL 大库修复：导入期间并发 PATCH 的字段在导入完成后仍在。
        // 目标行在最后一批（改名触发 UPDATE 分支）；观察者等第一批提交后
        // 才 PATCH 一次（此时快照已取、目标批尚未写）；旧写法会用快照的
        // active 把 PATCH 的 disabled 盖回。
        let fx = setup().await;
        let s = &fx.store;
        let src = make_source(s).await;
        let prefill: Vec<NormalizedChannel> = (0..2500)
            .map(|i| {
                ch(
                    &format!("tvg{i}.cn"),
                    &format!("CH{i}"),
                    &format!("https://x.example/t{i}.m3u8"),
                )
            })
            .collect();
        let out = s.upsert_channels(src.id, prefill).await.unwrap();
        assert_eq!(out.added, 2500);
        let first_id = out.added_channels[0].id;
        let target_id = out.added_channels[2499].id;
        let items: Vec<NormalizedChannel> = (0..2500)
            .map(|i| {
                ch(
                    &format!("tvg{i}.cn"),
                    &format!("CH{i}-v2"),
                    &format!("https://x.example/t{i}.m3u8"),
                )
            })
            .collect();
        let (upsert_r, _) = tokio::join!(s.upsert_channels(src.id, items), async {
            loop {
                if let Ok(Some(first)) = s.get_channel(first_id).await {
                    if first.name == "CH0-v2" {
                        break;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            let req = UpdateChannelRequest {
                tvg_id: None,
                name: None,
                display_name: Some("PATCHED".to_string()),
                logo_url: None,
                group_title: None,
                status: Some(ChannelStatus::Disabled),
                visibility: None,
                sort_weight: None,
            };
            let _ = s.update_channel(target_id, &req).await;
        });
        let out = upsert_r.unwrap();
        assert_eq!(out.updated, 2500);
        let target = s.get_channel(target_id).await.unwrap().unwrap();
        assert_eq!(target.name, "CH2499-v2", "M3U 改名应落库");
        assert_eq!(
            target.status,
            ChannelStatus::Disabled,
            "并发 PATCH 的 status 不得被快照值盖回"
        );
        assert_eq!(
            target.display_name.as_deref(),
            Some("PATCHED"),
            "并发 PATCH 的 display_name 不得丢失"
        );
    }

    #[tokio::test]
    async fn concurrent_claim_second_is_rejected() {
        // M-D：上传与 Worker 同步共用 `claim_source_sync`——同一源并发认领
        // 恰好一方成功；`is_source_syncing` 在认领期间为真，释放（写回 ok）
        // 后为假，之后可再次认领。`trigger_sync` 与 `run_upload_task` 都以
        // 这两个原语为准，故本测试覆盖“第二次应被拒或串行化”。
        let fx = setup().await;
        let s = &fx.store;
        let src = make_source(s).await;
        let next = Utc::now() + chrono::Duration::seconds(src.sync_interval_sec);
        let (first, second) = tokio::join!(
            s.claim_source_sync(src.id, next),
            s.claim_source_sync(src.id, next),
        );
        let first = first.unwrap();
        let second = second.unwrap();
        assert!(
            first ^ second,
            "并发认领恰好一方成功 first={first} second={second}"
        );
        assert!(s.is_source_syncing(src.id).await.unwrap());
        // 串行再认领同样被拒。
        assert!(!s.claim_source_sync(src.id, next).await.unwrap());
        // 释放后恢复：可再次认领，且 `is_source_syncing` 为假。
        s.update_source_sync(src.id, next, "ok", None)
            .await
            .unwrap();
        assert!(!s.is_source_syncing(src.id).await.unwrap());
        assert!(s.claim_source_sync(src.id, next).await.unwrap());
        assert!(s.is_source_syncing(src.id).await.unwrap());
        // 不存在的源不在同步中（trigger_sync 先做存在性校验，这里只保
        // 证读端不报错）。
        assert!(!s.is_source_syncing(src.id + 9999).await.unwrap());
    }

    #[tokio::test]
    async fn patch_fields_survive_concurrent_upsert() {
        // M-D：upsert 期间 PATCH 的字段在导入完成后仍在。PATCH 只改
        // `sort_weight` / `display_name`（upsert 不写这两列），upsert 只改
        // `name` 等导入列；三轮并发跑下来两边的值都在——旧的全行 UPDATE
        // 写法会在 PATCH 读-改-写跨过 upsert 提交时把对方改动盖掉。
        let fx = setup().await;
        let s = &fx.store;
        let src = make_source(s).await;
        let out = s
            .upsert_channels(
                src.id,
                vec![ch("cctv1.cn", "CCTV-1", "https://x.example/cctv1.m3u8")],
            )
            .await
            .unwrap();
        let chan_id = out.added_channels[0].id;
        for round in 0..3i32 {
            let mut items: Vec<NormalizedChannel> = (0..3000)
                .map(|i| {
                    ch(
                        &format!("r{round}-ch{i}.cn"),
                        &format!("CH-{round}-{i}"),
                        &format!("https://x.example/r{round}-{i}.m3u8"),
                    )
                })
                .collect();
            items.push(ch(
                "cctv1.cn",
                &format!("CCTV-1 v{round}"),
                "https://x.example/cctv1.m3u8",
            ));
            let req = patch_req(100 + round, &format!("display-{round}"));
            let (up, patched) = tokio::join!(
                s.upsert_channels(src.id, items),
                s.update_channel(chan_id, &req),
            );
            let up = up.unwrap();
            assert_eq!(up.added, 3000);
            let patched = patched.unwrap().expect("PATCH 目标频道存在");
            assert_eq!(patched.sort_weight, 100 + round);
            let after = s.get_channel(chan_id).await.unwrap().unwrap();
            assert_eq!(
                after.sort_weight,
                100 + round,
                "第 {round} 轮 sort_weight 被盖掉"
            );
            assert_eq!(
                after.display_name.as_deref(),
                Some(format!("display-{round}").as_str()),
                "第 {round} 轮 display_name 被盖掉"
            );
            assert_eq!(
                after.name,
                format!("CCTV-1 v{round}"),
                "第 {round} 轮导入改名未生效"
            );
        }
    }

    #[tokio::test]
    async fn sync_diff_survives_reconnect() {
        // M-D：`last_diffs` 不再是进程内缓存——同一库文件上新开 Store
        //（模拟重启），`get_sync_diff` 仍能读到上次同步的差异。
        let fx = setup().await;
        let s = &fx.store;
        let src = make_source(s).await;
        s.upsert_channels(
            src.id,
            vec![
                ch("cctv1.cn", "CCTV-1", "https://x.example/cctv1.m3u8"),
                ch("cctv5.cn", "CCTV-5", "https://x.example/cctv5.m3u8"),
            ],
        )
        .await
        .unwrap();
        let url = format!("sqlite://{}?mode=rwc", fx.path.display());
        let store2 = Store::connect(&url).await.expect("reconnect sqlite");
        store2.migrate().await.expect("migrate again");
        let diff = store2.get_sync_diff(src.id).await.unwrap();
        assert_eq!(diff.added.len(), 2);
        assert!(diff.missing.is_empty());
        // 从未同步过的源返回空差异。
        let diff2 = store2.get_sync_diff(src.id + 9999).await.unwrap();
        assert!(diff2.added.is_empty() && diff2.missing.is_empty());
    }
}
