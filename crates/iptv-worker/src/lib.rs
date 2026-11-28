//! 后台任务 Worker（开发文档 §3.1、§7.3、§6.3）。
//!
//! 循环任务（m21：tokio-cron-scheduler 调度，`WORKER_CRON_SCHEDULE`
//! 每 15 秒一档，与原 15s interval 轮询同节奏；多实例安全靠 DB 认领：
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
use iptv_core::{pin_target, resolve_checked_ip};
use iptv_epg::parse_xmltv;
use iptv_m3u::parse_m3u;
use iptv_store::{ChannelRepository, EpgProgramInput, EpgRepository, NewAuditLog, Store};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration as StdDuration;
use tokio_cron_scheduler::{Job, JobScheduler};

/// m21：Worker 定时表达式（croner，秒在首位）：每 15 秒一档
/// （:00/:15/:30/:45），与原 `tokio::time::interval(15s)` 轮询同节奏。
pub const WORKER_CRON_SCHEDULE: &str = "*/15 * * * * *";

/// N6：连续跳过多少轮升级为 error 日志 + 计一次告警（见 `skip_alerts`）。
pub const SKIP_ALERT_THRESHOLD: u64 = 3;

#[derive(Clone)]
pub struct Worker {
    pub store: Arc<Store>,
    /// 连续失败次数（源 id → 次数），用于退避进档（§7.3 第 9 步）。
    /// 只是缓存：worker 重启后按 `sync_log` 恢复（M18，见 `load_failures`）。
    failures: Arc<std::sync::Mutex<std::collections::HashMap<i64, u32>>>,
    /// 上一轮 `run_once` 是否仍在跑：cron 档到期时若上一轮未完则跳过，
    /// 保持原 interval 循环“一次只跑一轮”的语义（重叠的清理/同步无意义）。
    round_running: Arc<AtomicBool>,
    /// 已完成轮数（调度单测的观测点；跳过不计）。
    rounds: Arc<AtomicU64>,
    /// N6：已跳过轮数（metrics 风格，与 `rounds` 同款 `AtomicU64`）。
    skipped_rounds: Arc<AtomicU64>,
    /// N6：当前连续跳过轮数（跑一轮即清零）。
    consecutive_skips: Arc<AtomicU64>,
    /// N6：连续跳过达到 `SKIP_ALERT_THRESHOLD` 而记 error 的次数。
    skip_alerts: Arc<AtomicU64>,
    /// N6 故障注入（仅测试）：下一轮 `run_once_inner` 开头强制 panic，
    /// 用于验证 panic 后标志复位、下一轮仍能跑。
    #[cfg(test)]
    panic_next_round: Arc<AtomicBool>,
}

/// N6：`round_running` 的 RAII 复位守卫：`run_once_inner` panic 时
/// 析构仍会把标志复位，下一轮 cron 档不再被误判为“上一轮仍在跑”。
struct RoundGuard {
    flag: Arc<AtomicBool>,
}

impl Drop for RoundGuard {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::SeqCst);
    }
}

impl Worker {
    pub fn new(store: Arc<Store>) -> Self {
        Self {
            store,
            failures: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            round_running: Arc::new(AtomicBool::new(false)),
            rounds: Arc::new(AtomicU64::new(0)),
            skipped_rounds: Arc::new(AtomicU64::new(0)),
            consecutive_skips: Arc::new(AtomicU64::new(0)),
            skip_alerts: Arc::new(AtomicU64::new(0)),
            #[cfg(test)]
            panic_next_round: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 已完成的 `run_once` 轮数。
    pub fn rounds_completed(&self) -> u64 {
        self.rounds.load(Ordering::Relaxed)
    }

    /// N6：已跳过的 `run_once` 轮数（上一轮仍在跑时 cron 档跳过）。
    pub fn rounds_skipped(&self) -> u64 {
        self.skipped_rounds.load(Ordering::Relaxed)
    }

    /// N6：当前连续跳过轮数（跑一轮即清零）。
    pub fn consecutive_skips(&self) -> u64 {
        self.consecutive_skips.load(Ordering::Relaxed)
    }

    /// N6：连续跳过达到 `SKIP_ALERT_THRESHOLD` 而记 error 的次数。
    pub fn skip_alerts(&self) -> u64 {
        self.skip_alerts.load(Ordering::Relaxed)
    }

    /// N6 故障注入（仅测试）：让下一轮 `run_once_inner` 开头 panic。
    #[cfg(test)]
    fn inject_panic_once(&self) {
        self.panic_next_round.store(true, Ordering::SeqCst);
    }

    /// 失败退避档位（§7.3）：1m → 5m → 30m → 2h（按连续失败次数进档）。
    async fn backoff_for(&self, source_id: i64) -> i64 {
        let n = self.load_failures(source_id).await;
        match n {
            0 => 60,
            1 => 300,
            2 => 1800,
            _ => 7200,
        }
    }

    /// 连续失败次数：内存有即用；没有（worker 刚重启）就按 `sync_log`
    /// 最近连续 failed 行恢复（M18），再写回内存当缓存。
    /// 封顶 6 次：满 6 次即停同步，重启后不应带着更大的旧数回来。
    async fn load_failures(&self, source_id: i64) -> u32 {
        {
            let failures = self.failures.lock().unwrap();
            if let Some(n) = failures.get(&source_id) {
                return *n;
            }
        }
        let n = self
            .store
            .consecutive_sync_failures(source_id)
            .await
            .unwrap_or(0)
            .min(6);
        self.failures.lock().unwrap().insert(source_id, n);
        n
    }

    async fn record_failure(&self, source_id: i64) -> u32 {
        // 先恢复（本次失败尚未写入 sync_log，库里数 +1 即本次累计）。
        let base = self.load_failures(source_id).await;
        let mut failures = self.failures.lock().unwrap();
        let n = failures.entry(source_id).or_insert(base);
        *n += 1;
        *n
    }

    fn record_success(&self, source_id: i64) {
        self.failures.lock().unwrap().remove(&source_id);
    }

    /// 带逐跳 SSRF 校验的请求（M17① + N2）：每跳先 `resolve_checked_ip`
    ///（解析一次、校验该组 IP 后取其中一个），再用 `pinned_client` 把
    /// 连接固定到该 IP（`ClientBuilder::resolve` 覆盖本次解析，reqwest
    /// 不再二次解析同一主机名，DNS 重绑定无窗口；TLS SNI/Host 仍用原
    /// 主机名）；3xx 带 Location 时解析相对地址后继续，最多跟 3 跳
    /// （与旧 `Policy::limited(3)` 同限）。无 Location 的 3xx 直接返
    /// 回响应（由调用方按状态码处理）。
    /// 客户端每跳新建：重定向目标 host 不同，固定值也不同；订阅/EPG
    /// 拉取本就是低频操作，复用连接池的收益可忽略。重定向仍由本函数
    /// 手动跟（Policy::none）：`Policy::limited(3)` 只限跳数，302 跳
    /// 到内网仍会被跟进（M3 残留），故不能再交给客户端自动跟。
    async fn fetch_checked(
        method: reqwest::Method,
        url: &str,
        headers: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> anyhow::Result<reqwest::Response> {
        let mut current = url.to_string();
        for _ in 0..=3 {
            let ip = resolve_checked_ip(&current).await?;
            let parsed = reqwest::Url::parse(&current)?;
            let client = pinned_client(&parsed, ip);
            let mut req = client.request(method.clone(), &current);
            if let Some(headers) = headers {
                for (k, v) in headers {
                    if let Some(v) = v.as_str() {
                        req = req.header(k.as_str(), v);
                    }
                }
            }
            let resp = req.send().await?;
            if resp.status().is_redirection() {
                let Some(loc) = resp
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string())
                else {
                    return Ok(resp);
                };
                let base = reqwest::Url::parse(&current)?;
                current = base.join(&loc).map(|u| u.to_string())?;
                continue;
            }
            return Ok(resp);
        }
        anyhow::bail!("too many redirects");
    }
}

/// N2：按校验过的 IP 建客户端（`ClientBuilder::resolve` 覆盖本次 DNS
/// 解析；字面 IP 主机无需固定，见 `pin_target`）。重定向目标 host
/// 不同故每跳新建，见 `Worker::fetch_checked`。
fn pinned_client(url: &reqwest::Url, ip: std::net::IpAddr) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .timeout(StdDuration::from_secs(30))
        .user_agent(format!("Muse-IPTV-Worker/{}", iptv_core::VERSION))
        // M-E：强制直连，禁用环境代理（reqwest 默认读 http_proxy 等；
        // 走 CONNECT 隧道时 DNS 由代理做，`resolve` 的固定值不生效，
        // N2 的 TOCTOU 保护会被静默绕过）。上游只能经代理到达时走
        // 应用自身的订阅代理（`Subscription.proxyId`），不依赖环境代理。
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none());
    if let Some((host, addr)) = pin_target(url, ip) {
        builder = builder.resolve(&host, addr);
    }
    builder.build().expect("http client")
}

impl Worker {
    /// 主循环：m21 改走 tokio-cron-scheduler（`WORKER_CRON_SCHEDULE`
    /// 每 15 秒一档）。启动时先立即跑一轮（原 interval 首 tick 立即触发），
    /// 随后调度器接管；`migrate` 与认领行为在 `main.rs` / `run_once` 里，
    /// 本函数不改。调度器在后台跑，本任务挂起直至进程退出。
    pub async fn run_forever(&self) -> anyhow::Result<()> {
        if let Err(e) = self.run_once().await {
            tracing::error!("worker round failed: {e:#}");
        }
        let sched = self.build_scheduler().await?;
        sched.start().await?;
        tracing::info!("worker scheduler started ({WORKER_CRON_SCHEDULE})");
        std::future::pending::<()>().await;
        #[allow(unreachable_code)]
        Ok(())
    }

    /// 按 `WORKER_CRON_SCHEDULE` 装配调度器（不启动）：每档触发一次
    /// `run_once`，失败只记日志，不停调度器。
    pub async fn build_scheduler(&self) -> anyhow::Result<JobScheduler> {
        self.build_scheduler_with(WORKER_CRON_SCHEDULE).await
    }

    /// 同 `build_scheduler`，表达式可换（单测用 1 秒档验证触发）。
    pub async fn build_scheduler_with(&self, schedule: &str) -> anyhow::Result<JobScheduler> {
        let sched = JobScheduler::new().await?;
        let worker = self.clone();
        let job = Job::new_async(schedule, move |_uuid, _lock| {
            let worker = worker.clone();
            Box::pin(async move {
                if let Err(e) = worker.run_once().await {
                    tracing::error!("worker round failed: {e:#}");
                }
            })
        })?;
        sched.add(job).await?;
        Ok(sched)
    }

    /// 一轮活：订阅/EPG 同步认领、过期会话清理、审计保留、过期票据
    /// 清理（原 15s interval 每轮做的事，一项不丢）。上一轮仍在跑时
    /// 本轮跳过（cron 档到期不排队）。
    ///
    /// N6：`round_running` 由 `RoundGuard` 在 `Drop` 时复位，且
    /// `run_once_inner` 包在 `catch_unwind` 里——轮内 panic 不再把
    /// 标志留在 `true` 上导致后续每轮都跳过；panic 转为 `Err` 返回，
    /// 由调用方（`run_forever` / cron 任务）记 error 日志，调度器继续跑。
    pub async fn run_once(&self) -> anyhow::Result<()> {
        use futures_util::FutureExt;
        if self.round_running.swap(true, Ordering::SeqCst) {
            let skipped = self.skipped_rounds.fetch_add(1, Ordering::Relaxed) + 1;
            let streak = self.consecutive_skips.fetch_add(1, Ordering::Relaxed) + 1;
            if streak >= SKIP_ALERT_THRESHOLD {
                // 每轮 streak 只在首次达到阈值时计一次告警；达到阈值后
                // 的每次跳过都记 error 日志，不再静默 debug。
                if streak == SKIP_ALERT_THRESHOLD {
                    self.skip_alerts.fetch_add(1, Ordering::Relaxed);
                }
                tracing::error!(
                    "worker round skipped {streak} times in a row (total skipped {skipped}): previous round still running"
                );
            } else {
                tracing::debug!("worker round skipped: previous round still running");
            }
            return Ok(());
        }
        let _guard = RoundGuard {
            flag: self.round_running.clone(),
        };
        let r = std::panic::AssertUnwindSafe(self.run_once_inner())
            .catch_unwind()
            .await;
        // 显式释放守卫：先复位标志，再计轮数 / 清零连续跳过。
        drop(_guard);
        self.consecutive_skips.store(0, Ordering::Relaxed);
        match r {
            Ok(Ok(())) => {
                self.rounds.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Ok(Err(e)) => Err(e),
            Err(payload) => {
                let msg = if let Some(s) = payload.downcast_ref::<&str>() {
                    (*s).to_string()
                } else if let Some(s) = payload.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "unknown panic".to_string()
                };
                tracing::error!("worker round panicked: {msg}");
                anyhow::bail!("worker round panicked: {msg}");
            }
        }
    }

    async fn run_once_inner(&self) -> anyhow::Result<()> {
        // N6 故障注入（仅测试，见 `inject_panic_once`）。
        #[cfg(test)]
        if self.panic_next_round.swap(false, Ordering::SeqCst) {
            panic!("injected worker round panic (N6 fault-injection test)");
        }
        self.sync_due_sources().await?;
        self.sync_due_epg().await?;
        self.expire_stale_sessions().await?;
        self.cleanup_audit().await?;
        self.purge_expired_tickets().await?;
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
                let failures = self.record_failure(source.id).await;
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
                let backoff = self.backoff_for(source.id).await;
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
        // SSRF 校验（§13）+ 逐跳重校验在 `fetch_checked` 内做（M17①）：
        // 仅 http/https，解析后目标 IP 不在私网段，重定向目标同款重验。
        let resp =
            Self::fetch_checked(reqwest::Method::GET, url, source.http_headers.as_object()).await?;
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
        // m9：新增/变更频道的台标转存对象存储（§7.2）。未配置存储时
        // `mirror_logos` 直接返回，直链行为不变；单张转存失败只记日志、
        // 保留直链，不影响同步结果。
        self.mirror_logos(&sync).await;
        // 抽样健康探测（§7.3 第 7 步）：前 5 个新增/变更频道 HEAD + manifest 前 1 KB。
        // m20：以 `ChannelRepository` trait 为依赖（泛型约束），不依赖具体 `Store`。
        probe_sample(&*self.store, &sync.added_channels).await;
        Ok((sync.added, sync.updated, sync.removed))
    }

    /// m9 台标转存：只处理本轮新增/变更且带 `logo_url` 的频道，去重后
    /// 最多 `LOGO_MIRROR_PER_SYNC` 张；转存成功才写回 `logo_url`。
    async fn mirror_logos(&self, sync: &iptv_store::SyncOutcome) {
        let Some(cfg) = iptv_core::S3Config::from_env() else {
            return;
        };
        let mut seen = std::collections::HashSet::new();
        let candidates: Vec<&iptv_dto::Channel> = sync
            .added_channels
            .iter()
            .chain(sync.metadata_changed.iter())
            .chain(sync.url_changed.iter())
            .filter(|ch| {
                ch.logo_url
                    .as_deref()
                    .is_some_and(|u| !u.trim().is_empty() && seen.insert(u.to_string()))
            })
            .take(iptv_core::logo_store::LOGO_MIRROR_PER_SYNC)
            .collect();
        if candidates.is_empty() {
            return;
        }
        for ch in candidates {
            let logo_url = ch.logo_url.clone().unwrap_or_default();
            match self.mirror_one_logo(&cfg, &logo_url).await {
                Some(new_url) => {
                    if let Err(e) = self.store.update_channel_logo_url(ch.id, &new_url).await {
                        tracing::warn!("update logo url channel {} failed: {e:#}", ch.id);
                    }
                }
                None => {
                    tracing::info!("logo kept direct for channel {}: {logo_url}", ch.id);
                }
            }
        }
    }

    /// 单张台标：下载走本 crate 的 `fetch_checked`（N2：解析一次、连接
    /// 固定到校验过的 IP，逐跳重验），上传走 core 的 `upload_logo`；
    /// 任一步失败返回 `None`（保留直链，不影响同步）。
    async fn mirror_one_logo(&self, cfg: &iptv_core::S3Config, logo_url: &str) -> Option<String> {
        let resp = match Self::fetch_checked(reqwest::Method::GET, logo_url, None).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("logo fetch {logo_url} failed: {e:#}");
                return None;
            }
        };
        if !resp.status().is_success() {
            tracing::warn!("logo fetch {logo_url} failed: {}", resp.status());
            return None;
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let bytes = match read_capped(resp, iptv_core::LOGO_MAX_BYTES).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("logo read {logo_url} failed: {e:#}");
                return None;
            }
        };
        match iptv_core::upload_logo(cfg, &bytes, content_type.as_deref(), logo_url).await {
            Ok(url) => Some(url),
            Err(e) => {
                tracing::warn!("logo upload {logo_url} failed: {e:#}");
                None
            }
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
            // m20：单源导入只依赖 `EpgRepository` trait（泛型约束），
            // 不依赖具体 `Store`；拉取/解析失败只跳过该源（函数内 Ok），
            // 入库层报错才向上传播（与原循环内 `?` 的语义一致）。
            import_epg_source(&*self.store, &source, &by_tvg_id, &match_keys).await?;
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

    /// 过期登录票据清理（M15：票据落库后过期行需定期清；消费时已顺带清，
    /// 这里是第二道清理，随每 15 秒 cron 档走，不另起进程）。
    pub async fn purge_expired_tickets(&self) -> anyhow::Result<()> {
        let n = self.store.purge_expired_tickets(Utc::now()).await?;
        if n > 0 {
            tracing::info!("cleaned {n} expired login tickets");
        }
        Ok(())
    }
}

/// 抽样健康探测（§7.3 第 7 步，m20）：前 5 个新增/变更频道 HEAD + 取
/// manifest 前 1 KB 校验可解析性；结果写回频道行（M8，`last_checked_at` /
/// `last_available`），不再只打日志丢弃。非 http(s) 端点（RTMP/RTSP）
/// 不在 HTTP 探测范围内，不写回（保持“未探测”）。
///
/// 存储访问只经 `ChannelRepository` trait（泛型约束），不依赖具体
/// `Store`——`sync_source_inner` 以 `&*self.store` 传入，trait 层真实
/// 接线而非摆设；换实现只需换传入对象。
async fn probe_sample<R: ChannelRepository>(repo: &R, channels: &[iptv_dto::Channel]) {
    for ch in channels.iter().take(5) {
        let endpoints = match repo.list_endpoints(ch.id).await {
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
        let available = match Worker::fetch_checked(reqwest::Method::HEAD, &ep.url, None).await {
            Ok(resp) if resp.status().is_success() => {
                // 再取 manifest 前 1 KB：HEAD 成功不代表清单可解析。
                fetch_manifest_head_ok(&ep.url).await
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
        if let Err(e) = repo.record_channel_probe(ch.id, available).await {
            tracing::warn!("record probe channel {} failed: {e:#}", ch.id);
        }
    }
}

/// 取上游前 1 KB：清单地址（.m3u8）须含 `#EXTM3U` 才算可解析；
/// 其他地址只要返回非空体即算可达。同样走 `fetch_checked`，
/// 重定向目标逐跳重校验（M17①）。
async fn fetch_manifest_head_ok(url: &str) -> bool {
    let Ok(resp) = Worker::fetch_checked(reqwest::Method::GET, url, None).await else {
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

/// 单个 EPG 源的拉取 → 匹配 → 入库（m20）：存储访问只经 `EpgRepository`
/// trait（泛型约束），频道匹配键由调用方一次查出传入（M7 内存匹配）。
/// 拉取/解析失败只跳过该源（返回 `Ok`）；映射查询与节目入库报错才向
/// 上传播（与原 `sync_due_epg` 循环体内 `?` 的语义一致）。
async fn import_epg_source<E: EpgRepository>(
    epg: &E,
    source: &iptv_dto::EpgSource,
    by_tvg_id: &std::collections::HashMap<String, i64>,
    match_keys: &[(i64, Option<String>, String, i64)],
) -> anyhow::Result<()> {
    // M3/M17①：EPG 拉取与订阅同款逐跳 SSRF 校验（此前完全无校验，
    // 之后只验首跳；现每跳重验，失败即跳过该源）。
    let resp = match Worker::fetch_checked(reqwest::Method::GET, &source.url, None).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("epg fetch {} blocked/failed: {e}", source.url);
            return Ok(());
        }
    };
    if !resp.status().is_success() {
        return Ok(());
    }
    // M4：边读边累计上限（20 MB），超限立即断流。
    let bytes = match read_capped(resp, FETCH_MAX_BYTES).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("epg fetch {} too large: {e:#}", source.url);
            return Ok(());
        }
    };
    let data = match parse_xmltv(&bytes) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("epg parse {} failed: {e}", source.url);
            return Ok(());
        }
    };
    // m19：先取本源已配的映射（`epg_channel_map`），匹配时优先用；
    // 拉取/解析失败的源直接跳过，不浪费这次查询。
    let mapped: std::collections::HashMap<String, i64> = epg
        .list_epg_channel_maps(Some(source.id), None)
        .await?
        .into_iter()
        .filter_map(|m| match (m.tvg_id, m.channel_id) {
            (Some(t), Some(c)) => Some((t, c)),
            _ => None,
        })
        .collect();
    let mut inputs = Vec::new();
    for prog in data.programmes {
        // m19：映射命中直接用；再精确 tvg-id；最后按旧 LIKE 语义
        //（name/tvg_id 含子串，sort_weight DESC、id ASC 的首个）
        // 在内存里扫一遍。
        let channel_id = resolve_epg_channel(&mapped, by_tvg_id, match_keys, &prog.channel);
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
        let n = epg.upsert_epg_programs(inputs).await?;
        tracing::info!("epg {} imported {} programmes", source.url, n);
    }
    Ok(())
}

/// m19：EPG 节目 → 频道的匹配优先级（纯函数，可单测）。
///
/// 1. `epg_channel_map` 映射命中直接用（管理员显式配置，哪怕该 EPG
///    频道的 tvg-id 与频道自身的 tvg-id 不同也以映射为准）；
/// 2. tvg-id 精确命中；3. 旧 LIKE 语义：name/tvg_id 含子串，
///    sort_weight DESC、id ASC 的首个。
pub fn resolve_epg_channel(
    mapped: &std::collections::HashMap<String, i64>,
    by_tvg_id: &std::collections::HashMap<String, i64>,
    match_keys: &[(i64, Option<String>, String, i64)],
    epg_channel: &str,
) -> Option<i64> {
    if let Some(id) = mapped.get(epg_channel) {
        return Some(*id);
    }
    if let Some(id) = by_tvg_id.get(epg_channel) {
        return Some(*id);
    }
    match_keys
        .iter()
        .find(|(_, tvg_id, name, _)| {
            tvg_id.as_deref().is_some_and(|t| t.contains(epg_channel)) || name.contains(epg_channel)
        })
        .map(|(id, _, _, _)| *id)
}

/// 失败退避档位表（§7.3 第 9 步）：连续失败 0/1/2/3+ 次 → 60/300/1800/7200 秒。
/// 实际进档由 `Worker::backoff_for` 按连续失败计数取档；计数以 `sync_log`
/// 为准持久化，内存只是缓存（M18）。
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// 起一个本地 HTTP stub：每个连接读到请求头结束（`\r\n\r\n`）后
    /// 回固定响应并关闭连接。返回监听地址。
    async fn spawn_stub(ip: std::net::IpAddr, response: String) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind((ip, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let response = response.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let mut req = Vec::new();
                    loop {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => {
                                req.extend_from_slice(&buf[..n]);
                                if req.windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                            }
                        }
                    }
                    let _ = sock.write_all(response.as_bytes()).await;
                });
            }
        });
        addr
    }

    fn http_ok(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
    }

    /// 进程环境变量（代理开关/NO_PROXY）是全局共享的：改动它们的测试
    /// 必须串行，否则一个测试恢复环境变量会关掉另一个正在跑的测试的
    /// 开关（M-E 回归测试引入第二个 `TestSwitchGuard` 使用方后，
    /// `fetch_checked_loopback_redirect_and_switch` 在 ① 即报
    /// "url host is private"，实测撞上）。持锁的测试全程独占环境变量。
    static ENV_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// 测试开关的 RAII 守卫：`IPTV_TEST_ALLOW_PRIVATE_UPSTREAMS` 是进
    /// 程环境变量，测试 panic 时也要恢复，避免污染同进程其他测试。
    struct TestSwitchGuard {
        prev: Option<String>,
    }

    impl TestSwitchGuard {
        fn set() -> Self {
            let prev = std::env::var("IPTV_TEST_ALLOW_PRIVATE_UPSTREAMS").ok();
            std::env::set_var("IPTV_TEST_ALLOW_PRIVATE_UPSTREAMS", "1");
            Self { prev }
        }
    }

    impl Drop for TestSwitchGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var("IPTV_TEST_ALLOW_PRIVATE_UPSTREAMS", v),
                None => std::env::remove_var("IPTV_TEST_ALLOW_PRIVATE_UPSTREAMS"),
            }
        }
    }

    /// 把域名追加进 `NO_PROXY`/`no_proxy` 的 RAII 守卫（只追加已有
    /// 值、不替换，drop 时恢复原值）。reqwest 在 `Client::build` 时
    /// 读代理环境变量，守卫须在建客户端之前生效。
    struct NoProxyGuard {
        prev_upper: Option<String>,
        prev_lower: Option<String>,
    }

    impl NoProxyGuard {
        fn append(domain: &str) -> Self {
            let prev_upper = std::env::var("NO_PROXY").ok();
            let prev_lower = std::env::var("no_proxy").ok();
            let add = |prev: &Option<String>| match prev {
                Some(v) if !v.is_empty() => format!("{v},{domain}"),
                _ => domain.to_string(),
            };
            std::env::set_var("NO_PROXY", add(&prev_upper));
            std::env::set_var("no_proxy", add(&prev_lower));
            Self {
                prev_upper,
                prev_lower,
            }
        }
    }

    impl Drop for NoProxyGuard {
        fn drop(&mut self) {
            match &self.prev_upper {
                Some(v) => std::env::set_var("NO_PROXY", v),
                None => std::env::remove_var("NO_PROXY"),
            }
            match &self.prev_lower {
                Some(v) => std::env::set_var("no_proxy", v),
                None => std::env::remove_var("no_proxy"),
            }
        }
    }

    /// N2 核心行为：固定后的客户端实际连向校验过的 IP。
    /// `pinned.test` 在 DNS 里不存在（沙箱 DNS 也解析不到它）：未经
    /// 固定的请求必然失败；固定到回环 stub 的 IP 后，请求必须打到
    /// 本地 stub 并返回其响应——证明 `ClientBuilder::resolve` 的覆盖
    /// 真的生效，而不是又走了一次 DNS。
    /// 注意：沙箱设了出口代理环境变量（`http_proxy` 等），reqwest
    /// 默认会走代理——`pinned.test` 不在 `NO_PROXY` 里时请求根本到
    /// 不了本地 stub。测试内把该域名追加进 `NO_PROXY`/`no_proxy`
    /// （只追加、不替换已有项，同进程并行的其他测试不受影响）。
    #[tokio::test]
    async fn pinned_client_connects_to_validated_ip() {
        let _env = ENV_TEST_LOCK.lock().await;
        let _no_proxy = NoProxyGuard::append("pinned.test");
        let loopback = std::net::IpAddr::from([127, 0, 0, 1]);
        let addr = spawn_stub(loopback, http_ok("pinned-ok")).await;
        let url = reqwest::Url::parse(&format!("http://pinned.test:{}/x", addr.port())).unwrap();
        let client = pinned_client(&url, addr.ip());
        let resp = client.get(url).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "pinned-ok");
    }

    /// M-E 回归测试用的最小 CONNECT 代理 stub：监听回环地址；收到
    /// `CONNECT host:port` 后记下目标、回 `200` 并把字节流中继到真实
    /// 目标（于是即使同进程并行测试的请求误走本代理也能正常完成，
    /// 只影响 CONNECT 记录的断言本身）。
    async fn spawn_connect_recorder() -> (std::net::SocketAddr, Arc<std::sync::Mutex<Vec<String>>>)
    {
        let listener = tokio::net::TcpListener::bind((std::net::IpAddr::from([127, 0, 0, 1]), 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let targets: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = targets.clone();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let seen = seen.clone();
                tokio::spawn(async move {
                    handle_proxy_conn(sock, seen).await;
                });
            }
        });
        (addr, targets)
    }

    async fn handle_proxy_conn(
        mut sock: tokio::net::TcpStream,
        seen: Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        let mut buf = [0u8; 4096];
        let mut req = Vec::new();
        loop {
            match sock.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    req.extend_from_slice(&buf[..n]);
                    if req.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
            }
        }
        let text = String::from_utf8_lossy(&req);
        let first = text.lines().next().unwrap_or("");
        let mut parts = first.split_whitespace();
        if parts.next() != Some("CONNECT") {
            // 非 CONNECT（绝对形式 GET 等）：直接关闭，调用方会报错。
            return;
        }
        let Some(target) = parts.next().map(|s| s.to_string()) else {
            return;
        };
        seen.lock().unwrap().push(target.clone());
        let Ok(mut upstream) = tokio::net::TcpStream::connect(target.as_str()).await else {
            let _ = sock
                .write_all(
                    b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
            return;
        };
        if sock
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .is_err()
        {
            return;
        }
        let _ = tokio::io::copy_bidirectional(&mut sock, &mut upstream).await;
    }

    /// M-E 回归测试用的代理环境守卫：把 `http_proxy`/`https_proxy`
    ///（大小写与 `ALL_PROXY`）指向记录 CONNECT 的 stub，并把 `NO_PROXY`
    /// 置空（沙箱默认 `NO_PROXY` 含 127.0.0.1，否则回环目标会绕过代理、
    /// 测不到 CONNECT）。drop 时恢复原值。
    struct HttpProxyGuard {
        prev: Vec<(&'static str, Option<String>)>,
    }

    impl HttpProxyGuard {
        fn set(proxy_addr: std::net::SocketAddr) -> Self {
            const KEYS: [&str; 8] = [
                "http_proxy",
                "https_proxy",
                "HTTP_PROXY",
                "HTTPS_PROXY",
                "ALL_PROXY",
                "all_proxy",
                "NO_PROXY",
                "no_proxy",
            ];
            let proxy_url = format!("http://127.0.0.1:{}/", proxy_addr.port());
            let mut prev = Vec::new();
            for k in KEYS {
                prev.push((k, std::env::var(k).ok()));
                let v = if k == "NO_PROXY" || k == "no_proxy" {
                    String::new()
                } else {
                    proxy_url.clone()
                };
                std::env::set_var(k, v);
            }
            Self { prev }
        }
    }

    impl Drop for HttpProxyGuard {
        fn drop(&mut self) {
            for (k, v) in &self.prev {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    /// M-E 回归：`http_proxy` 已设置时，pinned client 仍直连校验过的 IP，
    /// 不向代理发 `CONNECT <原主机名>`。用 `fetch_checked` 走订阅拉取
    /// 全程（解析+校验+固定+发送），目标为回环 stub；代理 stub 记录
    /// CONNECT 目标并中继——修复前会记录到 `CONNECT 127.0.0.1:<port>`，
    /// 修复后记录为空且响应仍为直连 stub 的内容。
    #[tokio::test]
    async fn pinned_client_ignores_http_proxy() {
        let _env = ENV_TEST_LOCK.lock().await;
        let _switch = TestSwitchGuard::set();
        let v4 = std::net::IpAddr::from([127, 0, 0, 1]);
        let stub = spawn_stub(v4, http_ok("fetch-ok")).await;
        let (proxy_addr, connects) = spawn_connect_recorder().await;
        let _proxy = HttpProxyGuard::set(proxy_addr);
        let url = format!("http://127.0.0.1:{}/x.m3u", stub.port());
        let resp = Worker::fetch_checked(reqwest::Method::GET, &url, None)
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "fetch-ok");
        // 直连断言：代理 stub 全程未收到发往本目标的 CONNECT。
        let target = format!("127.0.0.1:{}", stub.port());
        let seen = connects.lock().unwrap();
        assert!(
            !seen.iter().any(|t| t == &target),
            "pinned client must connect directly, but proxy saw CONNECT: {seen:?}"
        );
    }

    /// N2 接线：`fetch_checked` 走“解析+校验+固定”全程，且开关语义
    /// 不变。同一测试函数内顺序执行各阶段，避免多测试并行改进程环境
    /// 变量竞态：① 开关开时回环拉取成功；② IPv6 回环同样成功；
    /// ③ 302 重定向逐跳重新固定，终点响应可达；④ 开关关时回环被拒。
    #[tokio::test]
    async fn fetch_checked_loopback_redirect_and_switch() {
        let _env = ENV_TEST_LOCK.lock().await;
        let v4 = std::net::IpAddr::from([127, 0, 0, 1]);
        let v6 = std::net::IpAddr::from([0, 0, 0, 0, 0, 0, 0, 1]);
        let _guard = TestSwitchGuard::set();

        // ① 回环 v4 拉取成功。
        let addr = spawn_stub(v4, http_ok("fetch-ok")).await;
        let url = format!("http://127.0.0.1:{}/x.m3u", addr.port());
        let resp = Worker::fetch_checked(reqwest::Method::GET, &url, None)
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "fetch-ok");

        // ② 回环 v6（字面 IPv6）拉取成功。
        let addr6 = spawn_stub(v6, http_ok("fetch-ok-v6")).await;
        let url6 = format!("http://[::1]:{}/x.m3u", addr6.port());
        let resp = Worker::fetch_checked(reqwest::Method::GET, &url6, None)
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "fetch-ok-v6");

        // ③ 302 跳到另一回环端口：每跳重新解析+校验+固定，终点可达。
        let final_addr = spawn_stub(v4, http_ok("redirect-ok")).await;
        let redirect = format!(
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{}/final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            final_addr.port()
        );
        let redirect_addr = spawn_stub(v4, redirect).await;
        let url = format!("http://127.0.0.1:{}/start", redirect_addr.port());
        let resp = Worker::fetch_checked(reqwest::Method::GET, &url, None)
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "redirect-ok");

        // ④ 开关关时回环必须被拒（开关只放行 IP 段，关掉即恢复拦截）。
        drop(_guard);
        assert!(Worker::fetch_checked(reqwest::Method::GET, &url, None)
            .await
            .is_err());
    }

    /// m21：cron 表达式可解析、调度器装配后任务真的触发。
    /// 1 秒一档的计数任务在 2.5 秒内至少触发一次；`WORKER_CRON_SCHEDULE`
    ///（15 秒档）本身也要能建成 Job（表达式合法）。
    #[tokio::test]
    async fn cron_scheduler_fires_job() {
        use std::sync::atomic::AtomicUsize;
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let mut sched = JobScheduler::new().await.unwrap();
        let job = Job::new_async("*/1 * * * * *", move |_uuid, _lock| {
            let h = h.clone();
            Box::pin(async move {
                h.fetch_add(1, Ordering::Relaxed);
            })
        })
        .unwrap();
        sched.add(job).await.unwrap();
        sched.start().await.unwrap();
        tokio::time::sleep(StdDuration::from_millis(2500)).await;
        sched.shutdown().await.unwrap();
        assert!(
            hits.load(Ordering::Relaxed) >= 1,
            "1s cron job should have fired at least once in 2.5s"
        );
        // 生产表达式本身必须合法（只建 Job，不启动）。
        assert!(
            Job::new_async(WORKER_CRON_SCHEDULE, |_uuid, _lock| Box::pin(async move {})).is_ok()
        );
    }

    static WORKER_TEST_DB_COUNTER: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);

    /// m21：`build_scheduler_with` 装配出的调度器真的驱动 `run_once`。
    /// 空库（无源/无 EPG）上一轮是空操作但必须成功完成并计轮数。
    #[tokio::test]
    async fn scheduler_drives_run_once() {
        let n = WORKER_TEST_DB_COUNTER.fetch_add(1, Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("iptv-worker-test-{}-{n}.db", std::process::id()));
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let store = Arc::new(
            iptv_store::Store::connect(&url)
                .await
                .expect("Store::connect"),
        );
        store.migrate().await.expect("migrate");
        let worker = Worker::new(store);
        let mut sched = worker
            .build_scheduler_with("*/1 * * * * *")
            .await
            .expect("build_scheduler_with");
        sched.start().await.expect("scheduler start");
        tokio::time::sleep(StdDuration::from_millis(2500)).await;
        sched.shutdown().await.expect("scheduler shutdown");
        let rounds = worker.rounds_completed();
        let _ = std::fs::remove_file(&path);
        assert!(
            rounds >= 1,
            "scheduler should have driven run_once at least once in 2.5s, got {rounds}"
        );
    }

    /// 建一个空库 Worker（无源/无 EPG）：一轮是空操作但必须成功。
    /// 返回 worker 与库文件路径（调用方在测试末尾删文件）。
    async fn empty_worker() -> (Worker, std::path::PathBuf) {
        let n = WORKER_TEST_DB_COUNTER.fetch_add(1, Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("iptv-worker-test-{}-{n}.db", std::process::id()));
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let store = Arc::new(
            iptv_store::Store::connect(&url)
                .await
                .expect("Store::connect"),
        );
        store.migrate().await.expect("migrate");
        (Worker::new(store), path)
    }

    /// N6 故障注入：`run_once_inner` 强制 panic 后，`run_once` 返回 Err
    /// 而不是把 panic 抛给调用方；`round_running` 已复位，下一轮仍能
    /// 正常跑完并计轮数。
    #[tokio::test]
    async fn round_recovers_after_panic_in_run_once_inner() {
        let (worker, path) = empty_worker().await;
        worker.inject_panic_once();
        let r = worker.run_once().await;
        assert!(r.is_err(), "panicking round should report Err, got {r:?}");
        assert!(
            !worker.round_running.load(Ordering::SeqCst),
            "round_running must reset after panic"
        );
        assert_eq!(worker.rounds_completed(), 0);
        let r2 = worker.run_once().await;
        assert!(r2.is_ok(), "next round after panic must still run: {r2:?}");
        assert_eq!(worker.rounds_completed(), 1);
        assert_eq!(worker.consecutive_skips(), 0);
        let _ = std::fs::remove_file(&path);
    }

    /// N6：连续跳过计数与阈值告警。把 `round_running` 置 true 模拟上一轮
    /// 未完，连调 `SKIP_ALERT_THRESHOLD` 次 `run_once` 均跳过；达到阈值
    /// 计一次告警；放开标志后下一轮正常跑，连续跳过清零。
    #[tokio::test]
    async fn skipped_rounds_count_and_alert_threshold() {
        let (worker, path) = empty_worker().await;
        worker.round_running.store(true, Ordering::SeqCst);
        for _ in 0..SKIP_ALERT_THRESHOLD {
            assert!(worker.run_once().await.is_ok());
        }
        assert_eq!(worker.rounds_skipped(), SKIP_ALERT_THRESHOLD);
        assert_eq!(worker.consecutive_skips(), SKIP_ALERT_THRESHOLD);
        assert_eq!(worker.skip_alerts(), 1);
        worker.round_running.store(false, Ordering::SeqCst);
        assert!(worker.run_once().await.is_ok());
        assert_eq!(worker.rounds_completed(), 1);
        assert_eq!(worker.consecutive_skips(), 0);
        assert_eq!(worker.rounds_skipped(), SKIP_ALERT_THRESHOLD);
        let _ = std::fs::remove_file(&path);
    }

    /// m19：映射命中优先于 tvg-id 精确命中——EPG 频道 `epg.cctv5`
    /// 明明与频道 1 的 tvg-id `cctv5.cn` 不同，映射指到频道 2 就用 2；
    /// 无映射时仍走精确 tvg-id 与名称子串回落。
    #[test]
    fn resolve_epg_channel_prefers_map_over_tvg_id() {
        use std::collections::HashMap;
        let mapped: HashMap<String, i64> = [("epg.cctv5".to_string(), 2)].into_iter().collect();
        let by_tvg_id: HashMap<String, i64> = [("cctv5.cn".to_string(), 1)].into_iter().collect();
        let keys = vec![
            (1, Some("cctv5.cn".to_string()), "CCTV-5".to_string(), 10),
            (2, Some("cctv5b.cn".to_string()), "CCTV-5+".to_string(), 9),
        ];
        // 映射命中：即使 tvg-id 精确命中频道 1，也用映射的频道 2。
        assert_eq!(
            resolve_epg_channel(&mapped, &by_tvg_id, &keys, "epg.cctv5"),
            Some(2)
        );
        // 无映射：精确 tvg-id 命中频道 1。
        assert_eq!(
            resolve_epg_channel(&mapped, &by_tvg_id, &keys, "cctv5.cn"),
            Some(1)
        );
        // 无映射无精确命中：名称子串回落（"CCTV-5" 含 "CCTV-5"）。
        assert_eq!(
            resolve_epg_channel(&HashMap::new(), &by_tvg_id, &keys, "CCTV-5"),
            Some(1)
        );
        // 全无命中：None。
        assert_eq!(
            resolve_epg_channel(&HashMap::new(), &by_tvg_id, &keys, "nope"),
            None
        );
    }
}
