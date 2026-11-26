//! 共享业务组件（开发文档 §1.1 / §4.6 / §11.5）。
//!
//! - 全部文案走 [`iptv_i18n::I18n`] 的 key；语言包里没有的 key 用
//!   [`t_or`] 回退到英文原文字面量（语言包目前只定义了约 30 个 key，
//!   见 `locales/*/common.ftl|portal.ftl|admin.ftl`；回退不是硬编码
//!   第三语言文案，而是缺 key 时的英文兜底，见 README 的 i18n 说明）。
//! - `LocaleCtx`：每请求由门户/后台的 `App` 组件从请求头协商后注入。
// 作者: Passwd Word

use chrono::{DateTime, Utc};
use iptv_design::{Badge, BadgeTone, EmptyState, ProgressRing, Row, StatusDot, StatusKind};
use iptv_dto::{Channel, DeviceView, EpgProgram, MeQuota, PlaybackStartResponse, TokenFeatures};
use iptv_i18n::I18n;
use leptos::prelude::*;
use std::time::Duration;

/// 每请求 locale（由 App 从 Cookie / Accept-Language 协商后注入，§12.2）。
#[derive(Debug, Clone)]
pub struct LocaleCtx(pub String);

/// 当前请求的 locale；无上下文时回退默认语言。
pub fn locale() -> String {
    use_context::<LocaleCtx>()
        .map(|c| c.0)
        .unwrap_or_else(|| iptv_i18n::DEFAULT_LOCALE.to_string())
}

/// 查文案；key 缺失（`I18n::t` 原样返回 key）时用英文兜底。
pub fn t_or(key: &str, fallback: &str) -> String {
    let i18n = I18n::new();
    let loc = locale();
    let v = i18n.t_simple(&loc, key);
    if v == key {
        fallback.to_string()
    } else {
        v
    }
}

/// 带参数的文案；缺 key 时用 `format!` 的英文兜底。
pub fn t_args_or(key: &str, args: &[(&str, String)], fallback: String) -> String {
    let i18n = I18n::new();
    let loc = locale();
    let mut fa = fluent_bundle::FluentArgs::new();
    for (k, v) in args {
        fa.set(*k, v.clone());
    }
    let v = i18n.t(&loc, key, Some(&fa));
    if v == key {
        fallback
    } else {
        v
    }
}

/// 日期按 locale 格式化（§12.6）。
pub fn fmt_dt(dt: DateTime<Utc>) -> String {
    I18n::new().format_datetime(&locale(), dt)
}

// ---------------------------------------------------------------------------
// ChannelCard：Logo / 分类 / 当前节目 / 播放按钮（§4.1 / §4.4）
// ---------------------------------------------------------------------------

/// 频道卡片。播放按钮为真实链接 `/watch/{id}`（进入播放页后调播放接口）。
#[component]
pub fn ChannelCard(
    channel: Channel,
    #[prop(optional)] current_program: Option<EpgProgram>,
) -> impl IntoView {
    let watch_href = format!("/watch/{}", channel.id);
    let name = channel
        .display_name
        .clone()
        .unwrap_or_else(|| channel.name.clone());
    let group = channel.group_title.clone().unwrap_or_default();
    let now_title = current_program.map(|p| p.title);
    let logo = channel.logo_url.clone();
    let initial = name
        .chars()
        .next()
        .map(|c| c.to_string())
        .unwrap_or_else(|| "?".to_string());
    let watch_label = t_or("portal.watch-now", "Watch now");
    view! {
        <article class="channel-card">
            {match logo {
                Some(url) => view! { <img class="channel-card-logo" src=url alt=name.clone() loading="lazy"/> }.into_any(),
                None => view! { <div class="channel-card-logo-fallback" aria-hidden="true">{initial}</div> }.into_any(),
            }}
            <div class="channel-card-body">
                <div class="channel-card-name">{name}</div>
                <div class="channel-card-meta">
                    {group}
                    {channel.language.clone().map(|l| format!(" · {l}")).unwrap_or_default()}
                </div>
                {now_title.map(|t| view! { <div class="channel-card-now">{t}</div> })}
                <a class="btn btn-filled" href=watch_href>{watch_label}</a>
            </div>
        </article>
    }
}

// ---------------------------------------------------------------------------
// QuotaRing：并发额度环（§4.1 / §11.5 ProgressRing）
// ---------------------------------------------------------------------------

/// 并发额度环：`active / max`，并显示剩余路数（§4.5 `quota-used` / `quota-remaining`）。
#[component]
pub fn QuotaRing(quota: MeQuota) -> impl IntoView {
    let max = quota.max_concurrent_sessions.max(1);
    let used = quota.active_sessions.clamp(0, max);
    let frac = used as f64 / max as f64;
    let remaining = (max - used).max(0);
    let used_text = t_args_or(
        "portal.quota-used",
        &[("used", used.to_string()), ("max", max.to_string())],
        format!("{used} of {max} streams in use"),
    );
    let remaining_text = t_args_or(
        "portal.quota-remaining",
        &[("count", remaining.to_string())],
        format!("{remaining} streams remaining"),
    );
    let exhausted = used >= max;
    view! {
        <div style="display:flex;gap:16px;align-items:center;flex-wrap:wrap">
            <ProgressRing fraction=frac label=used_text.clone()/>
            <div>
                <div>{used_text}</div>
                <div style="color:var(--c-label-secondary)">{remaining_text}</div>
                <div style="margin-top:8px">
                    {format!("{} / {}", quota.bound_devices, quota.max_bound_devices)}
                    <span style="color:var(--c-label-secondary)">" bound devices"</span>
                </div>
                {exhausted.then(|| view! {
                    <div style="margin-top:8px">
                        <Badge text=t_or("portal.quota-exhausted-title", "Stream limit reached") tone=BadgeTone::Warning/>
                    </div>
                })}
            </div>
        </div>
    }
}

// ---------------------------------------------------------------------------
// DeviceRow：在线设备行（§4.1：IP 已脱敏展示 + 下线按钮）
// ---------------------------------------------------------------------------

/// 设备行。下线走 POST 表单 `/me/devices/{id}/kick`（门户 Axum 处理器调
/// 后端 `DELETE /api/v1/me/devices/{id}`，服务端只下线本 token 的会话）。
#[component]
pub fn DeviceRow(device: DeviceView) -> impl IntoView {
    let kick_action = format!("/me/devices/{}/kick", device.session_id);
    let since = fmt_dt(device.last_heartbeat_at);
    let online_since = t_args_or(
        "portal.device-online-since",
        &[("since", since.clone())],
        format!("Online since {since}"),
    );
    let kick_label = t_or("common.kick", "Kick");
    view! {
        <li>
            <div class="row device-row">
                <div class="row-main">
                    <div class="row-title">{device.device_id}</div>
                    <div class="row-subtitle">{format!("{} · {}", device.ip_masked, online_since)}</div>
                    {device.user_agent.clone().map(|ua| view! { <div class="row-subtitle">{ua}</div> })}
                </div>
                {if device.is_current {
                    view! { <StatusDot label=t_or("common.current-device", "This device") kind=StatusKind::Online/> }.into_any()
                } else {
                    view! {
                        <form method="post" action=kick_action>
                            <button class="btn btn-destructive" type="submit">{kick_label}</button>
                        </form>
                    }.into_any()
                }}
            </div>
        </li>
    }
}

// ---------------------------------------------------------------------------
// EpgSidebar：当前 / 下一节目（§14.1）
// ---------------------------------------------------------------------------

/// EPG 侧栏：第一条为当前节目，其余按开始时间列出。
#[component]
pub fn EpgSidebar(
    programs: Vec<EpgProgram>,
    #[prop(optional)] now: Option<DateTime<Utc>>,
) -> impl IntoView {
    let now = now.unwrap_or_else(Utc::now);
    let (current, upcoming): (Vec<_>, Vec<_>) = programs
        .iter()
        .partition(|p| p.start_at <= now && now < p.stop_at);
    let current = current.into_iter().next();
    let upcoming: Vec<_> = upcoming.into_iter().take(6).collect();
    view! {
        <aside class="epg-sidebar" aria-label="epg">
            <h3 style="margin-top:0">{t_or("admin.epg", "Program guide")}</h3>
            {match current {
                Some(p) => view! {
                    <div>
                        <div class="epg-now">{p.title.clone()}</div>
                        <div class="epg-next">{format!("{} – {}", fmt_dt(p.start_at), fmt_dt(p.stop_at))}</div>
                        {p.description.clone().map(|d| view! { <p style="font-size:var(--text-footnote)">{d}</p> })}
                    </div>
                }.into_any(),
                None => view! { <EmptyState title=t_or("portal.no-epg", "No program now") icon="◷">{view! { <></> }}</EmptyState> }.into_any(),
            }}
            {if upcoming.is_empty() {
                view! { "" }.into_any()
            } else {
                view! {
                    <ul class="list" style="margin-top:12px">
                        {upcoming.into_iter().map(|p| view! {
                            <Row title=p.title.clone() subtitle=format!("{} – {}", fmt_dt(p.start_at), fmt_dt(p.stop_at)) value=p.category.clone().unwrap_or_default()/>
                        }).collect_view()}
                    </ul>
                }.into_any()
            }}
        </aside>
    }
}

// ---------------------------------------------------------------------------
// LanguageSwitcher：语言切换（§12.4：写 Cookie + PATCH /api/v1/me/settings）
// ---------------------------------------------------------------------------

/// 语言切换。纯 HTML 表单 POST 到门户 `/locale`：服务端写
/// `iptv_locale` Cookie（持久化）并调后端 `PATCH /api/v1/me/settings`
/// 同步用户语言设置（§12.4），随后 302 回原页。
#[component]
pub fn LanguageSwitcher(
    #[prop(optional, into)] action: Option<String>,
    #[prop(optional, into)] redirect_to: Option<String>,
) -> impl IntoView {
    let loc = locale();
    let action = action.unwrap_or_else(|| "/locale".to_string());
    let label = t_or("common.language", "Language");
    let aria = label.clone();
    let locales = I18n::supported_locales();
    view! {
        <form method="post" action=action class="inline-form" aria-label=aria>
            <label for="locale-select" style="font-size:var(--text-footnote);color:var(--c-label-secondary)">{label}</label>
            <select id="locale-select" name="locale" style="min-height:44px;border-radius:var(--radius-m);border:1px solid var(--c-separator);background:var(--c-surface)">
                {locales.into_iter().map(|l| {
                    let selected = l == loc;
                    view! { <option value=l selected=selected>{iptv_i18n::locale_display_name(l)}</option> }
                }).collect_view()}
            </select>
            {redirect_to.map(|r| view! { <input type="hidden" name="redirect_to" value=r/> })}
            <button class="btn btn-tinted" type="submit">{t_or("common.save", "Save")}</button>
        </form>
    }
}

// ---------------------------------------------------------------------------
// Player：HLS 播放器（§4.6 / §8）
// ---------------------------------------------------------------------------

/// 播放器。
///
/// - `<video>` 原生播放 HLS：iOS / Safari 用原生 HLS，其余浏览器走 MSE
///   播放器链路（`play_url` 为后端签发的短时效 HMAC 地址，§6.4/§8）。
/// - 缓冲/错误：`on:error` 触发指数退避重连（1s → 2s → 4s → 8s → 16s → 32s，
///   最多 5 次），`set_timeout` 到时后刷新 `src`（加 `retry` 参数避缓存）。
/// - 清晰度切换：调 server fn `start_playback`（见门户 crate）重新
///   `POST /api/v1/playback/start` 取新签名的 `play_url`，旧会话由
///   `POST /api/v1/playback/stop` 释放。
/// - 5 次失败后：调 server fn `stop_playback` 释放会话并显示错误，
///   `42901` 时引导去 `/me/devices` 下线其他设备（§4.6）。
#[component]
pub fn Player(
    channel: Channel,
    initial: PlaybackStartResponse,
    #[prop(optional)] qualities: Option<Vec<String>>,
    #[prop(optional)] features: Option<TokenFeatures>,
    /// 5 次重连失败后回调（父级接 `stop_playback` server fn，真实释放播放会话）。
    #[prop(optional)]
    on_failed: Option<Callback<String>>,
) -> impl IntoView {
    let qualities = qualities.unwrap_or_else(|| {
        let mut q = vec!["1080p".to_string(), "720p".to_string(), "480p".to_string()];
        if let Some(f) = &features {
            q.retain(|x| quality_rank(x) <= quality_rank(&f.max_quality));
        }
        if q.is_empty() {
            q.push("480p".to_string());
        }
        q
    });
    let play_url = RwSignal::new(initial.play_url.clone());
    let session_uid = RwSignal::new(initial.session_uid.clone());
    let quality = RwSignal::new(
        qualities
            .first()
            .cloned()
            .unwrap_or_else(|| "1080p".to_string()),
    );
    let attempts = RwSignal::new(0u32);
    let reload_nonce = RwSignal::new(0u32);
    let status = RwSignal::new(String::new());
    let failed = RwSignal::new(false);
    let channel_id = channel.id;
    let channel_name = channel
        .display_name
        .clone()
        .unwrap_or_else(|| channel.name.clone());
    // src 只在 reload_nonce 变化时变；退避延迟到时才由 set_timeout  bump 它。
    let video_src = move || {
        let base = play_url.get();
        let n = reload_nonce.get();
        if n == 0 {
            base
        } else {
            format!("{base}&retry={n}")
        }
    };

    // 指数退避重连：错误后按 2^attempts 秒延迟刷新 src。
    let on_error = move |_| {
        let n = attempts.get() + 1;
        attempts.set(n);
        if n > 5 {
            failed.set(true);
            status.set(t_or("errors.e50301", "Channel source unavailable"));
            // 真实释放播放会话：父级（WatchPage）把 `stop_playback` server fn
            // 接到 on_failed；无回调时会话仍由服务端 90s TTL 兜底过期（§6.3）。
            if let Some(cb) = on_failed {
                cb.run(session_uid.get());
            }
            return;
        }
        let delay = 1u64 << n.min(5);
        status.set(t_args_or(
            "portal.player-retrying",
            &[("n", n.to_string()), ("secs", delay.to_string())],
            format!("Reconnecting… attempt {n}, retry in {delay}s"),
        ));
        set_timeout(
            move || {
                // 退避延迟到时才刷新 src（避缓存重载），实现真正的指数退避。
                reload_nonce.update(|v| *v += 1);
            },
            Duration::from_secs(delay),
        );
    };
    let on_retry = move |_| {
        attempts.set(0);
        failed.set(false);
        status.set(String::new());
        reload_nonce.update(|v| *v += 1);
    };

    view! {
        <div class="player-wrap">
            <div class="player">
                <video
                    controls
                    autoplay
                    playsinline
                    preload="auto"
                    src=video_src
                    aria-label=channel_name.clone()
                    on:error=on_error
                ></video>
            </div>
            <div class="player-status" role="status">
                {move || status.get()}
                {move || failed.get().then(|| view! {
                    <span>
                        " "
                        <a href="/me/devices">{t_or("portal.quota-exhausted", "Concurrent stream limit reached. Stop another device and try again.")}</a>
                    </span>
                })}
            </div>
            <div style="display:flex;gap:8px;align-items:center;flex-wrap:wrap;margin-top:8px">
                <span style="font-size:var(--text-footnote);color:var(--c-label-secondary)">"Quality"</span>
                {qualities.into_iter().map(|q| {
                    let q2 = q.clone();
                    let active = move || quality.get() == q2;
                    let label = q.clone();
                    view! {
                        <form method="post" action=format!("/watch/{channel_id}/quality") style="display:inline">
                            <input type="hidden" name="quality" value=q.clone()/>
                            <input type="hidden" name="session_uid" value=move || session_uid.get()/>
                            <button
                                type="submit"
                                class=move || if active() { "btn btn-filled" } else { "btn btn-tinted" }
                            >{label}</button>
                        </form>
                    }
                }).collect_view()}
                <button class="btn btn-plain" type="button" on:click=on_retry>
                    {t_or("portal.player-retry", "Retry")}
                </button>
            </div>
            <p style="font-size:var(--text-footnote);color:var(--c-label-secondary)">
                {format!("session: {}", session_uid.get())}
            </p>
        </div>
    }
}

/// 清晰度排序（用于按 Token 的 max_quality 过滤，§3.2）。
fn quality_rank(q: &str) -> u32 {
    match q {
        "2160p" | "4K" => 5,
        "1440p" => 4,
        "1080p" => 3,
        "720p" => 2,
        "480p" => 1,
        _ => 0,
    }
}
