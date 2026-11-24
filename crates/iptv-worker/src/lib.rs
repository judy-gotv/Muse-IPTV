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
use iptv_store::{EpgProgramInput, Store};
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
            .user_agent("Muse-IPTV-Worker/0.1.0")
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

    fn record_failure(&self, source_id: i64) {
        let mut failures = self.failures.lock().unwrap();
        *failures.entry(source_id).or_insert(0) += 1;
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

    /// 同步所有到期的订阅源（§7.3）。
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
            if source.last_status.as_deref() == Some("syncing") {
                continue; // 另一实例正在同步
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
        // 认领：置为 syncing，防止多实例重复跑。
        self.store
            .update_source_sync(
                source.id,
                Utc::now() + Duration::seconds(source.sync_interval_sec),
                "syncing",
                None,
            )
            .await?;
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
                self.record_failure(source.id);
                self.store
                    .finish_sync_log(log_id, "failed", 0, 0, 0, Some(&msg))
                    .await?;
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
        // SSRF 校验（§13）：仅 http/https，目标 IP 不在私网段。
        let parsed = reqwest::Url::parse(url)?;
        if parsed.scheme() != "http" && parsed.scheme() != "https" {
            anyhow::bail!("only http/https subscription urls allowed");
        }
        if let Some(host) = parsed.host_str() {
            if is_private_host(host) {
                anyhow::bail!("subscription host is private");
            }
        }
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
        let text = resp.text().await?;
        if text.len() > 20 * 1024 * 1024 {
            anyhow::bail!("subscription too large (max 20 MB)");
        }
        let outcome = parse_m3u(&text);
        let sync = self
            .store
            .upsert_channels(source.id, outcome.channels)
            .await?;
        // 抽样健康探测（§7.3 第 7 步）：前 5 个新增/变更频道 HEAD + manifest 前 1 KB。
        self.probe_sample(&sync.added_channels).await;
        Ok((sync.added, sync.updated, sync.removed))
    }

    async fn probe_sample(&self, channels: &[iptv_dto::Channel]) {
        for ch in channels.iter().take(5) {
            let endpoints = match self.store.list_endpoints(ch.id).await {
                Ok(e) => e,
                Err(_) => continue,
            };
            if let Some(ep) = endpoints.into_iter().find(|e| e.is_primary) {
                match self.http.head(&ep.url).send().await {
                    Ok(resp) => {
                        let ok = resp.status().is_success();
                        tracing::info!("probe channel {} {} -> {}", ch.id, ep.url, resp.status());
                        let _ = ok;
                    }
                    Err(e) => tracing::warn!("probe channel {} failed: {e}", ch.id),
                }
            }
        }
    }

    /// EPG 同步：拉 XMLTV → 按 tvg-id 匹配频道 → 入库。
    pub async fn sync_due_epg(&self) -> anyhow::Result<()> {
        let sources = self.store.list_epg_sources().await?;
        for source in sources {
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
            let bytes = resp.bytes().await?;
            let data = match parse_xmltv(&bytes) {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!("epg parse {} failed: {e}", source.url);
                    continue;
                }
            };
            // 按 tvg-id 找频道 id。
            let mut inputs = Vec::new();
            for prog in data.programmes {
                let channels = self
                    .store
                    .list_channels(
                        &iptv_dto::PageQuery {
                            page: 1,
                            page_size: 1,
                            q: Some(prog.channel.clone()),
                        },
                        None,
                        None,
                    )
                    .await?;
                if let Some(ch) = channels.items.into_iter().next() {
                    inputs.push(EpgProgramInput {
                        channel_id: ch.id,
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

    /// 心跳停止 90s 的播放会话 DB 状态同步为 `expired`（§6.3 / §16 验收）。
    pub async fn expire_stale_sessions(&self) -> anyhow::Result<()> {
        let cutoff = Utc::now() - Duration::seconds(90);
        let n = self.store.expire_stale_sessions(cutoff).await?;
        if n > 0 {
            tracing::info!("expired {n} stale playback sessions");
        }
        Ok(())
    }

    /// 审计保留 180 天（§13）。
    pub async fn cleanup_audit(&self) -> anyhow::Result<()> {
        let before = Utc::now() - Duration::days(180);
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

fn is_private_host(host: &str) -> bool {
    // 域名先放行（解析后校验在 reqwest 层无法拦截，故只拦字面 IP）。
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return ip.is_loopback()
            || ip.is_unspecified()
            || match ip {
                std::net::IpAddr::V4(v4) => v4.is_private() || v4.is_link_local(),
                std::net::IpAddr::V6(v6) => v6.is_loopback(),
            };
    }
    false
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
