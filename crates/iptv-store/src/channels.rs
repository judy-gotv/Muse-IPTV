//! 频道（`channel` / `channel_endpoint`，开发文档 §7 / §9）。
//! `create_channel` 同步写 `channel_endpoint`（主/备地址，补充契约）。
// 作者: Passwd Word

use super::*;
use iptv_core::{content_hash, normalize_play_url};
use iptv_m3u::NormalizedChannel;
use std::collections::{HashMap, HashSet};

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

    pub async fn update_channel(
        &self,
        id: i64,
        req: &UpdateChannelRequest,
    ) -> anyhow::Result<Option<Channel>> {
        let Some(cur) = self.get_channel(id).await? else {
            return Ok(None);
        };
        let now = Utc::now();
        let tvg_id = req.tvg_id.clone().or(cur.tvg_id);
        let name = req.name.clone().unwrap_or(cur.name);
        let display_name = req.display_name.clone().or(cur.display_name);
        let logo_url = req.logo_url.clone().or(cur.logo_url);
        let group_title = req.group_title.clone().or(cur.group_title);
        let status = req.status.clone().unwrap_or(cur.status);
        let visibility = req.visibility.clone().unwrap_or(cur.visibility);
        let sort_weight = req.sort_weight.unwrap_or(cur.sort_weight);
        sqlx::query(
            &ph("UPDATE channel SET tvg_id = ?, name = ?, display_name = ?, logo_url = ?, group_title = ?, status = ?, visibility = ?, sort_weight = ?, updated_at = ? WHERE id = ?", self.dialect),
        )
        .bind(tvg_id)
        .bind(name)
        .bind(display_name)
        .bind(logo_url)
        .bind(group_title)
        .bind(channel_status_str(&status))
        .bind(visibility)
        .bind(i64::from(sort_weight))
        .bind(epoch(now))
        .bind(id)
        .execute(&self.pool)
        .await?;
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
        let mut outcome = SyncOutcome::default();

        // 现有频道 + 各自主地址。
        let existing_rows = sqlx::query(&ph(
            &format!("SELECT {CHANNEL_COLS} FROM channel WHERE source_id = ? ORDER BY id ASC"),
            self.dialect,
        ))
        .bind(source_id)
        .fetch_all(&self.pool)
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
        for item in &items {
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
            for r in q.fetch_all(&self.pool).await? {
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
            for r in q.fetch_all(&self.pool).await? {
                let e = row_to_endpoint(&r)?;
                endpoints.entry(e.channel_id).or_default().push(e);
            }
        }
        // M16：原为闭包、借住 `endpoints`，导致循环内无法再补登新插入行
        // 的端点（E0502）；改成独立函数、每次传 `endpoints` 引用。
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
        let mut seen_keys: HashSet<String> = HashSet::with_capacity(items.len());

        for item in &items {
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
                            .execute(&self.pool)
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
                    let status = if cur.status == ChannelStatus::Disabled && source.auto_enable_new
                    {
                        "active"
                    } else {
                        channel_status_str(&cur.status)
                    };
                    sqlx::query(
                        &ph("UPDATE channel SET name = ?, logo_url = ?, group_title = ?, content_hash = ?, status = ?, updated_at = ? WHERE id = ?", self.dialect),
                    )
                    .bind(&item.name)
                    .bind(item.logo.clone())
                    .bind(item.group.clone())
                    .bind(&hash)
                    .bind(status)
                    .bind(epoch(now))
                    .bind(cur.id)
                    .execute(&self.pool)
                    .await?;
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
                            .execute(&self.pool)
                            .await?;
                        } else {
                            self.insert_endpoint(cur.id, &item.url, &proto, true, 0)
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
                    updated.status = if status == "active" {
                        ChannelStatus::Active
                    } else {
                        ChannelStatus::Disabled
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
                    let id = insert_id!(
                        self,
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
                    let ep_id = self.insert_endpoint(id, &item.url, &proto, true, 0).await?;
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

        // 订阅里消失的频道：先收齐 id 再分批 bulk 写。逐行 DELETE/UPDATE
        // 在源里有几十万台时会把小文件上传也拖垮——每行一次写，还要与
        // worker 轮询抢 SQLite 写锁；实测 88 万台消失时每行约 4 秒、基本
        // 停滞。分批（500 个 id 一批）后同等规模约数十秒内完成。
        let mut missing_ids: Vec<i64> = Vec::new();
        for cur in &existing {
            let key = key_of_existing(cur, &endpoints);
            if seen_keys.contains(&key) {
                continue;
            }
            missing_ids.push(cur.id);
            outcome.missing.push(cur.clone());
            outcome.removed += 1;
        }
        if !missing_ids.is_empty() {
            // 注意：OnMissing 未派生 Copy，用 == 比较，不移出字段。
            if source.on_missing == iptv_dto::OnMissing::Delete {
                for chunk in missing_ids.chunks(500) {
                    let mut sql =
                        String::from("DELETE FROM channel_endpoint WHERE channel_id IN (");
                    push_in_list(&mut sql, "", chunk.len());
                    let sql = ph(&sql, self.dialect);
                    let mut q = sqlx::query(&sql);
                    for id in chunk {
                        q = q.bind(*id);
                    }
                    q.execute(&self.pool).await?;
                    let mut sql = String::from("DELETE FROM channel WHERE id IN (");
                    push_in_list(&mut sql, "", chunk.len());
                    let sql = ph(&sql, self.dialect);
                    let mut q = sqlx::query(&sql);
                    for id in chunk {
                        q = q.bind(*id);
                    }
                    q.execute(&self.pool).await?;
                }
            } else {
                for chunk in missing_ids.chunks(500) {
                    let mut sql = String::from(
                        "UPDATE channel SET status = 'disabled', updated_at = ? WHERE id IN (",
                    );
                    push_in_list(&mut sql, "", chunk.len());
                    sql.push_str(" AND status = 'active'");
                    let sql = ph(&sql, self.dialect);
                    let mut q = sqlx::query(&sql).bind(epoch(now));
                    for id in chunk {
                        q = q.bind(*id);
                    }
                    q.execute(&self.pool).await?;
                }
            }
        }

        // 缓存差异供 get_sync_diff 重建（CONTRACTS.md）。
        if let Ok(mut guard) = self.last_diffs.lock() {
            guard.insert(
                source_id,
                SyncDiff {
                    added: outcome.added_channels.clone(),
                    metadata_changed: outcome.metadata_changed.clone(),
                    url_changed: outcome.url_changed.clone(),
                    missing: outcome.missing.clone(),
                },
            );
        }
        Ok(outcome)
    }

    pub async fn get_sync_diff(&self, source_id: i64) -> anyhow::Result<SyncDiff> {
        if let Ok(guard) = self.last_diffs.lock() {
            if let Some(diff) = guard.get(&source_id) {
                return Ok(diff.clone());
            }
        }
        Ok(SyncDiff {
            added: vec![],
            metadata_changed: vec![],
            url_changed: vec![],
            missing: vec![],
        })
    }
}
