//! 频道（`channel` / `channel_endpoint`，开发文档 §7 / §9）。
//! `create_channel` 同步写 `channel_endpoint`（主/备地址，补充契约）。
// 作者: Passwd Word

use super::*;
use iptv_core::{content_hash, normalize_play_url};
use iptv_m3u::NormalizedChannel;
use sqlx::QueryBuilder;
use std::collections::HashMap;

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
        let row = sqlx::query(&format!("SELECT {CHANNEL_COLS} FROM channel WHERE id = ?"))
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
        let mut count_qb =
            QueryBuilder::<sqlx::Any>::new("SELECT COUNT(*) FROM channel WHERE 1 = 1");
        let mut list_qb = QueryBuilder::<sqlx::Any>::new(format!(
            "SELECT {CHANNEL_COLS} FROM channel WHERE 1 = 1"
        ));
        if let Some(g) = group {
            count_qb.push(" AND group_title = ").push_bind(g);
            list_qb.push(" AND group_title = ").push_bind(g);
        }
        if let Some(s) = &status {
            let v = channel_status_str(s);
            count_qb.push(" AND status = ").push_bind(v);
            list_qb.push(" AND status = ").push_bind(v);
        }
        if let Some(p) = &pat {
            count_qb
                .push(" AND (name LIKE ")
                .push_bind(p)
                .push(" OR tvg_id LIKE ")
                .push_bind(p)
                .push(")");
            list_qb
                .push(" AND (name LIKE ")
                .push_bind(p)
                .push(" OR tvg_id LIKE ")
                .push_bind(p)
                .push(")");
        }
        let total: i64 = count_qb.build_query_scalar().fetch_one(&self.pool).await?;
        list_qb
            .push(" ORDER BY sort_weight DESC, id ASC LIMIT ")
            .push_bind(limit)
            .push(" OFFSET ")
            .push_bind(offset);
        let rows = list_qb.build().fetch_all(&self.pool).await?;
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
            "UPDATE channel SET tvg_id = ?, name = ?, display_name = ?, logo_url = ?, group_title = ?, status = ?, visibility = ?, sort_weight = ?, updated_at = ? WHERE id = ?",
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
        sqlx::query("DELETE FROM channel_endpoint WHERE channel_id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        let n = sqlx::query("DELETE FROM channel WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(n > 0)
    }

    pub async fn list_endpoints(&self, channel_id: i64) -> anyhow::Result<Vec<ChannelEndpoint>> {
        let rows = sqlx::query(&format!(
            "SELECT {ENDPOINT_COLS} FROM channel_endpoint WHERE channel_id = ? ORDER BY priority ASC, id ASC"
        ))
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
    /// `allowed_groups` 为空表示未授权任何分组，返回空页。
    pub async fn list_channels_for_token(
        &self,
        token: &UserToken,
        q: &PageQuery,
        group: Option<&str>,
    ) -> anyhow::Result<Paged<Channel>> {
        let (limit, offset) = page_bounds(q);
        let pat = like_pattern(&q.q);
        let mut count_qb = QueryBuilder::<sqlx::Any>::new(
            "SELECT COUNT(*) FROM channel WHERE status = 'active' AND visibility = 'public'",
        );
        let mut list_qb = QueryBuilder::<sqlx::Any>::new(format!(
            "SELECT {CHANNEL_COLS} FROM channel WHERE status = 'active' AND visibility = 'public'"
        ));
        // 空 allowed_groups = 全部组（与 iptv-core::authorize_channel 一致；
        // 之前这里空列表直接返回 0 条，与播放鉴权放行矛盾）。
        if !token.allowed_groups.is_empty() {
            count_qb.push(" AND group_title IN (");
            list_qb.push(" AND group_title IN (");
            {
                let mut csep = count_qb.separated(", ");
                let mut lsep = list_qb.separated(", ");
                for g in &token.allowed_groups {
                    csep.push_bind(g);
                    lsep.push_bind(g);
                }
            }
            count_qb.push(")");
            list_qb.push(")");
        }
        if !token.denied_channels.is_empty() {
            count_qb.push(" AND id NOT IN (");
            list_qb.push(" AND id NOT IN (");
            {
                let mut csep = count_qb.separated(", ");
                let mut lsep = list_qb.separated(", ");
                for id in &token.denied_channels {
                    csep.push_bind(*id);
                    lsep.push_bind(*id);
                }
            }
            count_qb.push(")");
            list_qb.push(")");
        }
        if let Some(g) = group {
            count_qb.push(" AND group_title = ").push_bind(g);
            list_qb.push(" AND group_title = ").push_bind(g);
        }
        if let Some(p) = &pat {
            // 搜索同时匹配原始名与译名（§12.7：channel_i18n 有译名时也命中）。
            count_qb
                .push(" AND (name LIKE ")
                .push_bind(p)
                .push(" OR tvg_id LIKE ")
                .push_bind(p)
                .push(" OR id IN (SELECT channel_id FROM channel_i18n WHERE name LIKE ")
                .push_bind(p)
                .push("))");
            list_qb
                .push(" AND (name LIKE ")
                .push_bind(p)
                .push(" OR tvg_id LIKE ")
                .push_bind(p)
                .push(" OR id IN (SELECT channel_id FROM channel_i18n WHERE name LIKE ")
                .push_bind(p)
                .push("))");
        }
        let total: i64 = count_qb.build_query_scalar().fetch_one(&self.pool).await?;
        list_qb
            .push(" ORDER BY sort_weight DESC, id ASC LIMIT ")
            .push_bind(limit)
            .push(" OFFSET ")
            .push_bind(offset);
        let rows = list_qb.build().fetch_all(&self.pool).await?;
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
        let existing_rows = sqlx::query(&format!(
            "SELECT {CHANNEL_COLS} FROM channel WHERE source_id = ? ORDER BY id ASC"
        ))
        .bind(source_id)
        .fetch_all(&self.pool)
        .await?;
        let mut existing = Vec::new();
        for r in &existing_rows {
            existing.push(row_to_channel(r)?);
        }
        let mut endpoints: HashMap<i64, Vec<ChannelEndpoint>> = HashMap::new();
        if !existing.is_empty() {
            let mut qb = QueryBuilder::<sqlx::Any>::new(format!(
                "SELECT {ENDPOINT_COLS} FROM channel_endpoint WHERE channel_id IN ("
            ));
            {
                let mut sep = qb.separated(", ");
                for ch in &existing {
                    sep.push_bind(ch.id);
                }
            }
            qb.push(") ORDER BY priority ASC");
            for r in qb.build().fetch_all(&self.pool).await? {
                let e = row_to_endpoint(&r)?;
                endpoints.entry(e.channel_id).or_default().push(e);
            }
        }
        let key_of_existing = |ch: &Channel| -> String {
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
        };
        let mut by_key: HashMap<String, Channel> = HashMap::new();
        for ch in &existing {
            by_key.insert(key_of_existing(ch), ch.clone());
        }
        let mut seen_keys: Vec<String> = Vec::with_capacity(items.len());

        for item in &items {
            let tvg = item.tvg_id.clone().filter(|s| !s.trim().is_empty());
            let group = item.group.clone().unwrap_or_default();
            let key = match &tvg {
                Some(t) => format!("tvg:{t}"),
                None => format!("url:{}", normalize_play_url(&item.url)),
            };
            seen_keys.push(key.clone());
            let hash = content_hash(tvg.as_deref().unwrap_or(""), &item.url, &item.name, &group);
            match by_key.get(&key) {
                Some(cur) => {
                    if cur.content_hash.as_deref() == Some(hash.as_str()) {
                        continue; // 无变化
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
                        "UPDATE channel SET name = ?, logo_url = ?, group_title = ?, content_hash = ?, status = ?, updated_at = ? WHERE id = ?",
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
                            sqlx::query(
                                "UPDATE channel_endpoint SET url = ?, protocol = ? WHERE id = ?",
                            )
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
                    let updated = self
                        .get_channel(cur.id)
                        .await?
                        .unwrap_or_else(|| cur.clone());
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
                    self.insert_endpoint(id, &item.url, &proto, true, 0).await?;
                    let ch = self.get_channel(id).await?.unwrap_or(Channel {
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
                    });
                    outcome.added_channels.push(ch);
                    outcome.added += 1;
                }
            }
        }

        // 订阅里消失的频道。
        for cur in &existing {
            let key = key_of_existing(cur);
            if seen_keys.contains(&key) {
                continue;
            }
            // 注意：OnMissing 未派生 Copy，循环里用 == 比较，不移出字段。
            if source.on_missing == iptv_dto::OnMissing::Delete {
                sqlx::query("DELETE FROM channel_endpoint WHERE channel_id = ?")
                    .bind(cur.id)
                    .execute(&self.pool)
                    .await?;
                sqlx::query("DELETE FROM channel WHERE id = ?")
                    .bind(cur.id)
                    .execute(&self.pool)
                    .await?;
            } else {
                sqlx::query("UPDATE channel SET status = 'disabled', updated_at = ? WHERE id = ? AND status = 'active'")
                    .bind(epoch(now))
                    .bind(cur.id)
                    .execute(&self.pool)
                    .await?;
            }
            outcome.missing.push(cur.clone());
            outcome.removed += 1;
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
