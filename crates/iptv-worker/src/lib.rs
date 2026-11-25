//! 后台任务 Worker（开发文档 §3.1、§7.3、§6.3）。
//!
//! 循环任务（tokio interval，多实例安全靠 DB 认领：
//! `next_sync_at <= now` 的源先被 `update_source_sync` 置为 syncing 状态，
//! 其他实例看到 status=syncing 且未超时则跳过）：
//! - 订阅同步：按 `next_sync_at` 取任务 → 拉取 → 解析 → 规范化 → 差异分类 →
//!   抽样健康探测 → 写 sync_log → 更新 next_sync_at；失败退避
//!   1m → 5m → 30m → 2h，上限 6 次，连续失败告警（§7.3）。
//! - EPG 同步：拉 XMLTV → 按 tvg-id 匹配频道 → 入库节目。
//! - 会话过期清理：心跳停止 90s 的播放会话 DB 状态同步为 `expired`（§6.3 / §16）。
//! - 审计清理：保留 180 天（§13）。
// 作者: Passwd Word

use chrono::{Duration, Utc};
use iptv_epg::parse_xmltv;
use iptv_m3u::parse_m3u;
use iptv_store::{EpgProgramInput, NewAuditLog, Store};
use std::sync::Arc;
use std::time::Duration as StdDuration;

pub struct Worker {
    pub store: Arc<Store>,
    pub http: reqwest::Client,
    /// 连续失败次数（源 id → 次数），用于退避进档（§7.3 第 9 步）。
    failures: std::sync::Mutex<std::collections::HashMap<i64, u32>>,
}

impl Worker {
    pub fn new(store: Arc<Store>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(StdDuration::from_secs(30))
            .user_agent(format!("Muse-IPTV-Worker/{}", iptv_core::VERSION))
            // M3：重定向最多跟 3 跳（默认跟 10 跳可被 302 拐到内网）。
            .redirect(reqwest::redirect::Policy::limited(3))
            .build()
            .expect("http client");
        Self {
            store,
            http,
            failures: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// 失败退避档位（§7.3）：1m → 5m → 30m → 2h（按连续失败次数进档）。
    fn backoff_for(&self, source_id: i64) -> i64 {
        let failures = self.failures.lock().unwrap();
        let n = failures.get(&source_id).copied().unwrap_or(0);
        match n {
            0 => 60,
            1 => 300,
            2 => 1800,
            _ => 7200,
        }
    }

    fn record_failure(&self, source_id: i64) -> u32 {
        let mut failures = self.failures.lock().unwrap();
        let n = failures.entry(source_id).or_insert(0);
        *n += 1;
        *n
    }

    fn record_success(&self, source_id: i64) {
        self.failures.lock().unwrap().remove(&source_id);
    }

    /// 主循环：每 15 秒扫一轮到期任务。
    pub async fn run_forever(&self) -> anyhow::Result<()> {
        let mut tick = tokio::time::interval(StdDuration::from_secs(15));
        loop {
            tick.tick().await;
            if let Err(e) = self.run_once().await {
                tracing::error!("worker round failed: {e:#}");
            }
        }
    }

    pub async fn run_once(&self) -> anyhow::Result<()> {
        self.sync_due_sources().await?;
        self.sync_due_epg().await?;
        self.expire_stale_sessions().await?;
        self.cleanup_audit().await?;
        Ok(())
    }

    /// 同步所有到期的订阅源（§7.3）。认领是原子的（`claim_source_sync`）：
    /// 另一实例正在同步的源认领失败即跳过，不再先查后置。
    pub async fn sync_due_sources(&self) -> anyhow::Result<()> {
        let sources = self.store.list_sources().await?;
        let now = Utc::now();
        for source in sources {
            if !source.enabled {
                continue;
            }
            let due = source.next_sync_at.map(|t| t <= now).unwrap_or(true);
            if !due {
                continue;
            }
            if let Err(e) = self.sync_source(&source).await {
                tracing::error!("sync source {} failed: {e:#}", source.id);
            }
        }
        Ok(())
    }

    /// 单源同步（§7.3 九步）。
    pub async fn sync_source(&self, source: &iptv_dto::PlaylistSource) -> anyhow::Result<()> {
        let Some(url) = source.url.clone() else {
            return Ok(()); // 本地上传源无 URL，不参与远程同步
        };
        let log_id = self.store.create_sync_log(source.id).await?;
        // 认领：原子置为 syncing（M6），认领失败说明另一实例正在同步。
        let claimed = self
            .store
            .claim_source_sync(
                source.id,
                Utc::now() + Duration::seconds(source.sync_interval_sec),
            )
            .await?;
        if !claimed {
            return Ok(());
        }
        let result = self.sync_source_inner(source, &url).await;
        match result {
            Ok((added, updated, removed)) => {
                self.record_success(source.id);
                self.store
                    .finish_sync_log(log_id, "ok", added, updated, removed, None)
                    .await?;
                self.store
                    .update_source_sync(
                        source.id,
                        Utc::now() + Duration::seconds(source.sync_interval_sec),
                        "ok",
                        None,
                    )
                    .await?;
            }
            Err(e) => {
                let msg = e.to_string();
                let failures = self.record_failure(source.id);
                self.store
                    .finish_sync_log(log_id, "failed", 0, 0, 0, Some(&msg))
                    .await?;
                if failures >= 6 {
                    // M9：连续失败满 6 次即停同步（§7.3 上限 6 次），源标记失败、
                    // 下次同步推迟 30 天（人工“立即同步”可恢复）；写审计留痕。
                    let capped_msg =
                        format!("连续失败 {failures} 次，已停止自动同步（上限 6 次）: {msg}");
                    self.store
                        .update_source_sync(
                            source.id,
                            Utc::now() + Duration::days(30),
                            "failed",
                            Some(&capped_msg),
                        )
                        .await?;
                    let _ = self
                        .store
                        .write_audit(&NewAuditLog {
                            actor: "system:worker".to_string(),
                            actor_role: None,
                            action: "source.sync_stopped".to_string(),
                            object_type: "source".to_string(),
                            object_id: source.id.to_string(),
                            before_json: None,
                            after_json: Some(serde_json::json!({
                                "failures": failures,
                                "error": msg,
                            })),
                            ip_address: None,
                        })
                        .await;
                    return Err(e);
                }
                // M1：连续失败达到 `alert_threshold_failed_syncs`（默认 3 次）
                // 即写一条告警审计（未到停同步的 6 次上限前先告警）。
                if failures as i64
                    == self
                        .store
                        .get_setting_i64("alert_threshold_failed_syncs")
                        .await
                {
                    let _ = self
                        .store
                        .write_audit(&NewAuditLog {
                            actor: "system:worker".to_string(),
                            actor_role: None,
                            action: "source.sync_alert".to_string(),
                            object_type: "source".to_string(),
                            object_id: source.id.to_string(),
                            before_json: None,
                            after_json: Some(serde_json::json!({
                                "failures": failures,
                                "error": msg,
                            })),
                            ip_address: None,
                        })
                        .await;
                }
                // 失败退避：1m → 5m → 30m → 2h（按连续失败次数取档）。
                let backoff = self.backoff_for(source.id);
                self.store
                    .update_source_sync(
                        source.id,
                        Utc::now() + Duration::seconds(backoff),
                        "failed",
                        Some(&msg),
                    )
                    .await?;
                return Err(e);
            }
        }
        Ok(())
    }

    async fn sync_source_inner(
        &self,
        source: &iptv_dto::PlaylistSource,
        url: &str,
    ) -> anyhow::Result<(i64, i64, i64)> {
        // SSRF 校验（§13）：仅 http/https，解析后目标 IP 不在私网段。
        check_url_allowed(url).await?;
        let mut req = self.http.get(url);
        if let Some(headers) = source.http_headers.as_object() {
            for (k, v) in headers {
                if let Some(v) = v.as_str() {
                    req = req.header(k.as_str(), v);
                }
            }
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("subscription fetch failed: {}", resp.status());
        }
        // M4：边读边累计上限（20 MB），超限立即断流，不先整包进内存再判。
        let bytes = read_capped(resp, FETCH_MAX_BYTES).await?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let outcome = parse_m3u(&text);
        let sync = self
            .store
            .upsert_channels(source.id, outcome.channels)
            .await?;
        // 抽样健康探测（§7.3 第 7 步）：前 5 个新增/变更频道 HEAD + manifest 前 1 KB。
        self.probe_sample(&sync.added_channels).await;
        Ok((sync.added, sync.updated, sync.removed))
    }

    /// 抽样健康探测（§7.3 第 7 步）：前 5 个新增/变更频道 HEAD + 取 manifest
    /// 前 1 KB 校验可解析性；结果写回频道行（M8，`last_checked_at` /
    /// `last_available`），不再只打日志丢弃。非 http(s) 端点（RTMP/RTSP）
    /// 不在 HTTP 探测范围内，不写回（保持“未探测”）。
    async fn probe_sample(&self, channels: &[iptv_dto::Channel]) {
        for ch in channels.iter().take(5) {
            let endpoints = match self.store.list_endpoints(ch.id).await {
                Ok(e) => e,
                Err(_) => continue,
            };
            let Some(ep) = endpoints.into_iter().find(|e| e.is_primary) else {
                continue;
            };
            let Ok(url) = reqwest::Url::parse(&ep.url) else {
                continue;
            };
            if url.scheme() != "http" && url.scheme() != "https" {
                continue;
            }
            let available = match self.http.head(&ep.url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    // 再取 manifest 前 1 KB：HEAD 成功不代表清单可解析。
                    self.fetch_manifest_head_ok(&ep.url).await
                }
                Ok(resp) => {
                    tracing::info!("probe channel {} {} -> {}", ch.id, ep.url, resp.status());
                    false
                }
                Err(e) => {
                    tracing::warn!("probe channel {} failed: {e}", ch.id);
                    false
                }
            };
            if let Err(e) = self.store.record_channel_probe(ch.id, available).await {
                tracing::warn!("record probe channel {} failed: {e:#}", ch.id);
            }
        }
    }

    /// 取上游前 1 KB：清单地址（.m3u8）须含 `#EXTM3U` 才算可解析；
    /// 其他地址只要返回非空体即算可达。
    async fn fetch_manifest_head_ok(&self, url: &str) -> bool {
        let Ok(resp) = self.http.get(url).send().await else {
            return false;
        };
        if !resp.status().is_success() {
            return false;
        }
        use futures_util::StreamExt;
        let mut first_kb = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(c) => {
                    let need = 1024usize.saturating_sub(first_kb.len());
                    first_kb.extend_from_slice(&c[..c.len().min(need)]);
                    if first_kb.len() >= 1024 {
                        break;
                    }
                }
                Err(_) => return false,
            }
        }
        let text = String::from_utf8_lossy(&first_kb);
        if url.contains(".m3u8") {
            text.contains("#EXTM3U")
        } else {
            !text.trim().is_empty()
        }
    }

    /// EPG 同步：拉 XMLTV → 按 tvg-id 匹配频道 → 入库。
    pub async fn sync_due_epg(&self) -> anyhow::Result<()> {
        // 先查源列表：没有 EPG 源时直接返回，不加载全部频道匹配键。
        // 否则每轮（15 秒）都要全表扫一次频道（82 万台时 4–8 秒），既浪费
        // 又长时间占着 SQLite 读锁、拖慢 API 的写入。
        let sources = self.store.list_epg_sources().await?;
        if sources.is_empty() {
            return Ok(());
        }
        // M7：开局一次查出全部频道匹配键，后面在内存里匹配，不再每节目查库。
        let match_keys = self.store.list_channel_match_keys().await?;
        let mut by_tvg_id: std::collections::HashMap<String, i64> =
            std::collections::HashMap::new();
        for (id, tvg_id, _name, _w) in &match_keys {
            if let Some(t) = tvg_id {
                by_tvg_id.entry(t.clone()).or_insert(*id);
            }
        }
        for source in sources {
            // M3：EPG 拉取与订阅同款 SSRF 校验（此前完全无校验）。
            if let Err(e) = check_url_allowed(&source.url).await {
                tracing::warn!("epg source {} blocked: {e}", source.url);
                continue;
            }
            let resp = match self.http.get(&source.url).send().await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("epg fetch {} failed: {e}", source.url);
                    continue;
                }
            };
            if !resp.status().is_success() {
                continue;
            }
            // M4：边读边累计上限（20 MB），超限立即断流。
            let bytes = match read_capped(resp, FETCH_MAX_BYTES).await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!("epg fetch {} too large: {e:#}", source.url);
                    continue;
                }
            };
            let data = match parse_xmltv(&bytes) {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!("epg parse {} failed: {e}", source.url);
                    continue;
                }
            };
            let mut inputs = Vec::new();
            for prog in data.programmes {
                // 先精确 tvg-id 命中；再按旧 LIKE 语义（name/tvg_id 含子串，
                // sort_weight DESC、id ASC 的首个）在内存里扫一遍。
                let channel_id = by_tvg_id.get(&prog.channel).copied().or_else(|| {
                    match_keys
                        .iter()
                        .find(|(_, tvg_id, name, _)| {
                            tvg_id.as_deref().is_some_and(|t| t.contains(&prog.channel))
                                || name.contains(&prog.channel)
                        })
                        .map(|(id, _, _, _)| *id)
                });
                if let Some(channel_id) = channel_id {
                    inputs.push(EpgProgramInput {
                        channel_id,
                        tvg_id: Some(prog.channel.clone()),
                        title: prog.title,
                        description: prog.desc,
                        category: prog.category,
                        start_at: prog.start,
                        stop_at: prog.stop,
                    });
                }
            }
            if !inputs.is_empty() {
                let n = self.store.upsert_epg_programs(inputs).await?;
                tracing::info!("epg {} imported {} programmes", source.url, n);
            }
        }
        Ok(())
    }

    /// 心跳停止 `session_timeout_sec` 的播放会话 DB 状态同步为 `expired`
    /// （§6.3 / §16 验收；默认 90s，读系统设置）。
    pub async fn expire_stale_sessions(&self) -> anyhow::Result<()> {
        let timeout = self.store.get_setting_i64("session_timeout_sec").await;
        let cutoff = Utc::now() - Duration::seconds(timeout.max(1));
        let n = self.store.expire_stale_sessions(cutoff).await?;
        if n > 0 {
            tracing::info!("expired {n} stale playback sessions");
        }
        Ok(())
    }

    /// 审计保留 `audit_retention_days`（§13；默认 180 天，读系统设置）。
    pub async fn cleanup_audit(&self) -> anyhow::Result<()> {
        let days = self.store.get_setting_i64("audit_retention_days").await;
        let before = Utc::now() - Duration::days(days.max(1));
        let n = self.store.delete_old_audit(before).await?;
        if n > 0 {
            tracing::info!("cleaned {n} old audit rows");
        }
        Ok(())
    }
}

/// 失败退避档位表（§7.3 第 9 步）：连续失败 0/1/2/3+ 次 → 60/300/1800/7200 秒。
/// 实际进档由 `Worker::backoff_for` 按内存中的连续失败计数取档。
pub const BACKOFF_STEPS_SEC: [i64; 4] = [60, 300, 1800, 7200];

/// 远程拉取的单响应上限（§7.1：订阅与 EPG 均为 20 MB；本地上传不限大小，
/// 不走这条路）。边读边累计，超限立即断流（M4）。
pub const FETCH_MAX_BYTES: usize = 20 * 1024 * 1024;

/// 流式读响应体，累计超 `limit` 立即报错中断，不继续消费上游。
async fn read_capped(resp: reqwest::Response, limit: usize) -> anyhow::Result<Vec<u8>> {
    use futures_util::StreamExt;
    let mut buf = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if buf.len() + chunk.len() > limit {
            anyhow::bail!("response too large (max {limit} bytes)");
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// 该 IP 是否禁止作为拉取目标（SSRF）：私网、回环、链路本地、未指定、
/// 组播、广播、文档/基准测试/保留段、CGNAT 共享段；v6 另加唯一本地与
/// 单播链路本地。
fn ip_is_blocked(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.is_documentation()
                // 以下三段在 1.89 仍不稳定（`ip` 特性），手写段判断：
                || (o[0] == 198 && (o[1] == 18 || o[1] == 19)) // 基准测试 198.18.0.0/15
                || o[0] >= 240 // 保留段 240.0.0.0/4
                || (o[0] == 100 && (64..=127).contains(&o[1])) // CGNAT 共享段 100.64.0.0/10
        }
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
                || v6.is_multicast()
        }
    }
}

fn is_private_host(host: &str) -> bool {
    // 域名先放行（解析后校验在 reqwest 层无法拦截，故只拦字面 IP）。
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return ip_is_blocked(ip);
    }
    false
}

/// SSRF 校验（M3）：仅 http/https；把 host 解析成 IP 后逐一校验，
/// 任一解析结果落在禁段即拒绝。注意这是请求前的尽力校验，解析与建连
/// 之间存在 TOCTOU 窗口（DNS 重绑定），不能当作绝对屏障。
async fn check_url_allowed(url: &str) -> anyhow::Result<()> {
    let parsed = reqwest::Url::parse(url)?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        anyhow::bail!("only http/https urls allowed");
    }
    let Some(host) = parsed.host_str() else {
        anyhow::bail!("url has no host");
    };
    if is_private_host(host) {
        anyhow::bail!("url host is private");
    }
    if host.parse::<std::net::IpAddr>().is_err() {
        let port = parsed.port_or_known_default().unwrap_or(443);
        let addrs = tokio::net::lookup_host((host, port)).await?;
        for addr in addrs {
            if ip_is_blocked(addr.ip()) {
                anyhow::bail!("url resolves to private address {addr}");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_host_detection() {
        assert!(is_private_host("127.0.0.1"));
        assert!(is_private_host("10.0.0.5"));
        assert!(is_private_host("192.168.1.1"));
        assert!(!is_private_host("8.8.8.8"));
        assert!(!is_private_host("example.com"));
    }
}
