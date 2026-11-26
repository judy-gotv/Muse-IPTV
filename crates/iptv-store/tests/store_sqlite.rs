//! iptv-store SQLite 集成测试：建表、用户/Token、upsert 三态、播放会话过期、
//! 审计/设置、门户会话/设备、统计/EPG/地址、Token 频道授权。
//! 用临时文件库（`sqlite::memory:` 在连接池下每个连接是独立的库，不可用）。
// 作者: Passwd Word

use chrono::{Duration, Utc};
use iptv_core::default_settings;
use iptv_dto::*;
use iptv_m3u::NormalizedChannel;
use iptv_store::{EpgProgramInput, NewAuditLog, NewPlaybackEvent, Store};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    store: Store,
    path: std::path::PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

async fn setup() -> Fixture {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let path =
        std::env::temp_dir().join(format!("iptv-store-test-{}-{}.db", std::process::id(), n));
    let url = format!("sqlite://{}?mode=rwc", path.display());
    let store = Store::connect(&url).await.expect("connect sqlite");
    assert_eq!(store.dialect(), iptv_store::Dialect::Sqlite);
    store.migrate().await.expect("migrate");
    Fixture { store, path }
}

fn user_req(name: &str) -> CreateUserRequest {
    CreateUserRequest {
        display_name: name.to_string(),
        email: Some(format!("{name}@example.com")),
        phone: None,
        plan_id: None,
        locale: Some("zh-Hant-TW".to_string()),
        remark: None,
        expires_at: None,
    }
}

fn token_req(user_id: i64, groups: &[&str]) -> CreateTokenRequest {
    CreateTokenRequest {
        user_id,
        name: "客厅电视".to_string(),
        device_type: Some("android_tv".to_string()),
        max_concurrent_sessions: 2,
        max_bound_devices: 3,
        expires_at: Some(Utc::now() + Duration::days(365)),
        locale: Some("zh-Hant-TW".to_string()),
        allowed_groups: groups.iter().map(|s| s.to_string()).collect(),
        denied_channels: vec![],
        features: TokenFeatures {
            catchup: true,
            recording: false,
            max_quality: "1080p".to_string(),
        },
    }
}

fn channel_req(name: &str, group: &str, url: &str) -> CreateChannelRequest {
    CreateChannelRequest {
        tvg_id: Some(format!("{name}.cn")),
        name: name.to_string(),
        display_name: None,
        logo_url: None,
        group_title: Some(group.to_string()),
        language: None,
        country: None,
        play_url: url.to_string(),
        backup_url: None,
        protocol: None,
        sort_weight: None,
        visibility: None,
    }
}

#[tokio::test]
async fn migrate_is_idempotent() {
    // B1 回归：API 与 Worker 启动都调 migrate()，同一库上重复执行必须成功。
    let fx = setup().await;
    fx.store.migrate().await.expect("second migrate");
    fx.store.migrate().await.expect("third migrate");
}

#[tokio::test]
async fn user_and_token_roundtrip() {
    let fx = setup().await;
    let s = &fx.store;
    let user = s.create_user(&user_req("alice")).await.unwrap();
    assert_eq!(user.display_name, "alice");
    assert_eq!(user.status, UserStatus::Active);
    assert_eq!(s.get_user(user.id).await.unwrap().unwrap().id, user.id);

    let hash = [7u8; 32];
    let token = s
        .create_token(user.id, "tk_live_", &hash, &token_req(user.id, &["央视"]))
        .await
        .unwrap();
    assert_eq!(token.token_prefix, "tk_live_");
    assert_eq!(token.max_concurrent_sessions, 2);
    assert!(token.is_usable(Utc::now()));
    assert_eq!(token.days_remaining(Utc::now()), 364);

    let found = s.find_token_by_hash(&hash).await.unwrap().unwrap();
    assert_eq!(found.id, token.id);
    assert!(s.find_token_by_hash(&[8u8; 32]).await.unwrap().is_none());

    let page = s
        .list_tokens(
            Some(user.id),
            Some(TokenStatus::Active),
            &PageQuery::default(),
        )
        .await
        .unwrap();
    assert_eq!(page.total, 1);
    let updated = s
        .update_token(
            token.id,
            &UpdateTokenRequest {
                name: None,
                device_type: None,
                max_concurrent_sessions: Some(3),
                max_bound_devices: None,
                status: None,
                expires_at: None,
                locale: None,
                allowed_groups: None,
                denied_channels: None,
                features: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.max_concurrent_sessions, 3);

    let users = s
        .list_users(&PageQuery::default(), Some(UserStatus::Active))
        .await
        .unwrap();
    assert_eq!(users.total, 1);
    let upd_user = s
        .update_user(
            user.id,
            &UpdateUserRequest {
                display_name: Some("alice2".to_string()),
                email: None,
                phone: None,
                plan_id: None,
                status: None,
                locale: None,
                remark: None,
                expires_at: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(upd_user.display_name, "alice2");
}

#[tokio::test]
async fn upsert_channels_add_update_missing() {
    let fx = setup().await;
    let s = &fx.store;
    let src = s
        .create_source(&CreateSourceRequest {
            name: "测试源".to_string(),
            url: Some("https://example.com/list.m3u".to_string()),
            sync_interval_sec: Some(100), // 小于最小值 300，应被钳制
            http_headers: None,
            epg_url: None,
            on_missing: None,
            auto_enable_new: None,
            enabled: None,
        })
        .await
        .unwrap();
    assert_eq!(src.sync_interval_sec, 300);
    assert_eq!(src.on_missing, OnMissing::Disable);

    let ch = |tvg: &str, name: &str, url: &str| NormalizedChannel {
        tvg_id: Some(tvg.to_string()),
        name: name.to_string(),
        logo: None,
        group: Some("央视".to_string()),
        url: url.to_string(),
        catchup: None,
        catchup_source: None,
    };
    let out = s
        .upsert_channels(
            src.id,
            vec![
                ch("cctv1.cn", "CCTV-1", "https://x.example/cctv1.m3u8"),
                ch("cctv5.cn", "CCTV-5", "https://x.example/cctv5.m3u8"),
            ],
        )
        .await
        .unwrap();
    assert_eq!((out.added, out.updated, out.removed), (2, 0, 0));

    // 元数据变更（改名）+ URL 变更（改地址）各一条。
    let out = s
        .upsert_channels(
            src.id,
            vec![
                ch("cctv1.cn", "CCTV-1", "https://x.example/cctv1-new.m3u8"),
                ch("cctv5.cn", "CCTV-5 体育", "https://x.example/cctv5.m3u8"),
            ],
        )
        .await
        .unwrap();
    assert_eq!((out.added, out.updated, out.removed), (0, 2, 0));
    assert_eq!(out.url_changed.len(), 1);
    assert_eq!(out.metadata_changed.len(), 1);
    // 无变化时再次同步应全部为 0。
    let out = s
        .upsert_channels(
            src.id,
            vec![
                ch("cctv1.cn", "CCTV-1", "https://x.example/cctv1-new.m3u8"),
                ch("cctv5.cn", "CCTV-5 体育", "https://x.example/cctv5.m3u8"),
            ],
        )
        .await
        .unwrap();
    assert_eq!((out.added, out.updated, out.removed), (0, 0, 0));

    // 消失一条：默认 on_missing=disable，应被停用而非删除。
    let out = s
        .upsert_channels(
            src.id,
            vec![ch("cctv1.cn", "CCTV-1", "https://x.example/cctv1-new.m3u8")],
        )
        .await
        .unwrap();
    assert_eq!((out.added, out.updated, out.removed), (0, 0, 1));
    let cctv5 = out
        .missing
        .iter()
        .find(|c| c.tvg_id.as_deref() == Some("cctv5.cn"))
        .unwrap();
    assert_eq!(
        s.get_channel(cctv5.id).await.unwrap().unwrap().status,
        ChannelStatus::Disabled
    );

    let diff = s.get_sync_diff(src.id).await.unwrap();
    assert_eq!(diff.missing.len(), 1);

    // 同步日志：创建 -> 完成。
    let log_id = s.create_sync_log(src.id).await.unwrap();
    s.finish_sync_log(log_id, "ok", 2, 2, 1, None)
        .await
        .unwrap();
    let logs = s
        .list_sync_logs(src.id, &PageQuery::default())
        .await
        .unwrap();
    assert_eq!(logs.total, 1);
    assert_eq!(logs.items[0].status, "ok");
}

#[tokio::test]
async fn upsert_matches_manual_channel_by_tvg_id() {
    // 回归：PG 契约 contract.rs:124——手动建的频道（source_id 为 NULL）
    // 再被订阅同步同名时应算 1 次元数据变更，而非新增撞 uq_channel_tvg。
    let fx = setup().await;
    let s = &fx.store;
    let src = s
        .create_source(&CreateSourceRequest {
            name: "回归源".to_string(),
            url: Some("https://example.com/list.m3u".to_string()),
            sync_interval_sec: None,
            http_headers: None,
            epg_url: None,
            on_missing: None,
            auto_enable_new: Some(true),
            enabled: None,
        })
        .await
        .unwrap();
    let manual = s
        .create_channel(&channel_req(
            "CCTV-1",
            "央视",
            "https://x.example/cctv1.m3u8",
        ))
        .await
        .unwrap();
    assert!(manual.source_id.is_none());
    let out = s
        .upsert_channels(
            src.id,
            vec![NormalizedChannel {
                tvg_id: Some("CCTV-1.cn".to_string()),
                name: "CCTV-1 综合".to_string(),
                logo: None,
                group: Some("央视".to_string()),
                url: "https://x.example/cctv1.m3u8".to_string(),
                catchup: None,
                catchup_source: None,
            }],
        )
        .await
        .unwrap();
    assert_eq!((out.added, out.updated, out.removed), (0, 1, 0));
    assert_eq!(out.metadata_changed.len(), 1);
    assert_eq!(out.metadata_changed[0].id, manual.id);
    // 同 URL：不应误判 URL 变更、重复插入主地址。
    let eps = s.list_endpoints(manual.id).await.unwrap();
    assert_eq!(eps.len(), 1);
}

#[tokio::test]
async fn playback_session_lifecycle_and_expiry() {
    let fx = setup().await;
    let s = &fx.store;
    let user = s.create_user(&user_req("bob")).await.unwrap();
    let token = s
        .create_token(
            user.id,
            "tk_live_",
            &[1u8; 32],
            &token_req(user.id, &["体育"]),
        )
        .await
        .unwrap();
    let ch = s
        .create_channel(&channel_req(
            "CCTV-5",
            "体育",
            "https://x.example/cctv5.m3u8",
        ))
        .await
        .unwrap();

    let ps = s
        .create_playback_session(
            token.id,
            ch.id,
            "dev-1",
            "sess-uid-1",
            Some("1080p"),
            Some("192.168.1.2"),
        )
        .await
        .unwrap();
    assert_eq!(ps.status, "active");
    assert!(s.heartbeat_session("sess-uid-1", Utc::now()).await.unwrap());
    assert!(s
        .get_playback_session("sess-uid-1")
        .await
        .unwrap()
        .is_some());
    assert_eq!(
        s.active_sessions_for_token(token.id).await.unwrap().len(),
        1
    );
    assert_eq!(
        s.list_active_playback_sessions(&PageQuery::default())
            .await
            .unwrap()
            .total,
        1
    );

    s.record_event(&NewPlaybackEvent {
        session_uid: "sess-uid-1".to_string(),
        token_id: token.id,
        channel_id: ch.id,
        event_type: "start".to_string(),
        detail: serde_json::json!({"quality": "1080p"}),
    })
    .await
    .unwrap();
    assert_eq!(
        s.playback_history(token.id, &PageQuery::default())
            .await
            .unwrap()
            .total,
        1
    );

    // 90s 未心跳即过期（§6.3）：cutoff 在未来，心跳时间在 cutoff 之前。
    let expired = s
        .expire_stale_sessions(Utc::now() + Duration::seconds(3600))
        .await
        .unwrap();
    assert_eq!(expired, 1);
    assert_eq!(
        s.get_playback_session("sess-uid-1")
            .await
            .unwrap()
            .unwrap()
            .status,
        "expired"
    );
    assert_eq!(
        s.list_active_playback_sessions(&PageQuery::default())
            .await
            .unwrap()
            .total,
        0
    );
    assert!(!s.heartbeat_session("sess-uid-1", Utc::now()).await.unwrap());

    // 正常结束一条新会话。
    let _ps2 = s
        .create_playback_session(token.id, ch.id, "dev-1", "sess-uid-2", None, None)
        .await
        .unwrap();
    assert!(s
        .end_session("sess-uid-2", Utc::now(), "ended")
        .await
        .unwrap());
}

#[tokio::test]
async fn audit_and_settings() {
    let fx = setup().await;
    let s = &fx.store;
    // V2 种子：default_settings 的每个键都已存在且值一致。
    let seeded = s.list_settings().await.unwrap();
    for (k, v) in default_settings() {
        let found = seeded.iter().find(|st| st.key == k);
        assert!(found.is_some(), "缺少种子设置 {k}");
        assert_eq!(found.unwrap().value, v, "种子设置 {k} 值不一致");
    }
    s.set_setting("cdn_domain", &serde_json::json!("https://cdn.example.com"))
        .await
        .unwrap();
    assert_eq!(
        s.get_setting("cdn_domain").await.unwrap().unwrap(),
        serde_json::json!("https://cdn.example.com")
    );
    assert!(s.get_setting("不存在的键").await.unwrap().is_none());

    let id = s
        .write_audit(&NewAuditLog {
            actor: "admin".to_string(),
            actor_role: Some("super_admin".to_string()),
            action: "token.create".to_string(),
            object_type: "user_token".to_string(),
            object_id: "8821".to_string(),
            before_json: None,
            after_json: Some(serde_json::json!({"name": "客厅电视"})),
            ip_address: Some("127.0.0.1".to_string()),
        })
        .await
        .unwrap();
    assert!(id > 0);
    let page = s
        .list_audit(
            Some("admin"),
            Some("token.create"),
            None,
            None,
            &PageQuery::default(),
        )
        .await
        .unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(
        page.items[0].after_json.as_ref().unwrap()["name"],
        "客厅电视"
    );
    // 审计保留 180 天：删掉 cutoff 之前的记录。
    let deleted = s
        .delete_old_audit(Utc::now() + Duration::days(1))
        .await
        .unwrap();
    assert_eq!(deleted, 1);
    assert_eq!(
        s.list_audit(None, None, None, None, &PageQuery::default())
            .await
            .unwrap()
            .total,
        0
    );
}

#[tokio::test]
async fn portal_session_and_device_binding() {
    let fx = setup().await;
    let s = &fx.store;
    let user = s.create_user(&user_req("carol")).await.unwrap();
    let token = s
        .create_token(
            user.id,
            "tk_live_",
            &[2u8; 32],
            &token_req(user.id, &["央视"]),
        )
        .await
        .unwrap();
    let exp = Utc::now() + Duration::days(7);
    let ps = s
        .create_portal_session(
            token.id,
            "dev-tv",
            &[9u8; 32],
            Some("10.0.0.1"),
            Some("TV/1.0"),
            exp,
        )
        .await
        .unwrap();
    assert_eq!(
        s.find_portal_session_by_hash(&[9u8; 32])
            .await
            .unwrap()
            .unwrap()
            .id,
        ps.id
    );
    assert_eq!(s.bound_device_count(token.id).await.unwrap(), 1);
    // 同一设备重复登录不重复计数；新设备 +1。
    s.create_portal_session(token.id, "dev-tv", &[10u8; 32], None, None, exp)
        .await
        .unwrap();
    assert_eq!(s.bound_device_count(token.id).await.unwrap(), 1);
    s.create_portal_session(token.id, "dev-phone", &[11u8; 32], None, None, exp)
        .await
        .unwrap();
    assert_eq!(s.bound_device_count(token.id).await.unwrap(), 2);

    assert_eq!(s.list_portal_sessions(token.id).await.unwrap().len(), 3);
    s.touch_portal_session(ps.id, Utc::now(), Utc::now() + Duration::days(7))
        .await
        .unwrap();
    assert!(s.revoke_portal_session(ps.id).await.unwrap());
    assert!(!s.revoke_portal_session(ps.id).await.unwrap()); // 已吊销，再吊销返回 false
    assert_eq!(s.list_portal_sessions(token.id).await.unwrap().len(), 2);
    let n = s.revoke_all_portal_sessions(token.id, None).await.unwrap();
    assert_eq!(n, 2);
    assert!(s.list_portal_sessions(token.id).await.unwrap().is_empty());
}

#[tokio::test]
async fn stats_epg_and_endpoints() {
    let fx = setup().await;
    let s = &fx.store;
    // V2 种子分组与套餐。
    let groups = s.list_groups().await.unwrap();
    assert_eq!(groups.len(), 6);
    assert!(groups.iter().any(|g| g.name == "央视" && g.is_default));
    let plans = s.list_plans().await.unwrap();
    assert!(plans.iter().any(|p| p.name == "家庭版"));
    assert!(s.get_plan(plans[0].id).await.unwrap().is_some());

    let user = s.create_user(&user_req("dave")).await.unwrap();
    let _token = s
        .create_token(
            user.id,
            "tk_live_",
            &[3u8; 32],
            &token_req(user.id, &["央视"]),
        )
        .await
        .unwrap();
    let mut req = channel_req("CCTV-1", "央视", "https://x.example/cctv1.m3u8");
    req.backup_url = Some("https://backup.example/cctv1.m3u8".to_string());
    let ch = s.create_channel(&req).await.unwrap();
    // create_channel 同步写 channel_endpoint：主 + 备。
    let eps = s.list_endpoints(ch.id).await.unwrap();
    assert_eq!(eps.len(), 2);
    assert!(eps[0].is_primary);
    assert_eq!(eps[0].protocol, "hls");
    assert!(!eps[1].is_primary);
    assert_eq!(eps[1].priority, 1);

    // EPG：源、写入、当前节目、时段查询。
    let epg_src = s
        .create_epg_source("测试EPG", "https://example.com/epg.xml")
        .await
        .unwrap();
    assert_eq!(s.list_epg_sources().await.unwrap().len(), 1);
    let _ = epg_src;
    let now = Utc::now();
    let n = s
        .upsert_epg_programs(vec![
            EpgProgramInput {
                channel_id: ch.id,
                tvg_id: Some("cctv1.cn".to_string()),
                title: "新闻联播".to_string(),
                description: None,
                category: Some("新闻".to_string()),
                start_at: now - Duration::minutes(10),
                stop_at: now + Duration::minutes(20),
            },
            EpgProgramInput {
                channel_id: ch.id,
                tvg_id: Some("cctv1.cn".to_string()),
                title: "天气预报".to_string(),
                description: None,
                category: None,
                start_at: now + Duration::minutes(20),
                stop_at: now + Duration::minutes(30),
            },
        ])
        .await
        .unwrap();
    assert_eq!(n, 2);
    assert_eq!(
        s.current_program(ch.id, now).await.unwrap().unwrap().title,
        "新闻联播"
    );
    assert_eq!(
        s.programs_for_channel(ch.id, now - Duration::hours(1), now + Duration::hours(1))
            .await
            .unwrap()
            .len(),
        2
    );
    // 同一 (channel, start_at) 重复写入去重后仍为 2 条。
    let n = s
        .upsert_epg_programs(vec![EpgProgramInput {
            channel_id: ch.id,
            tvg_id: Some("cctv1.cn".to_string()),
            title: "新闻联播（重播）".to_string(),
            description: None,
            category: None,
            start_at: now - Duration::minutes(10),
            stop_at: now + Duration::minutes(20),
        }])
        .await
        .unwrap();
    assert_eq!(n, 1);
    assert_eq!(
        s.programs_for_channel(ch.id, now - Duration::hours(1), now + Duration::hours(1))
            .await
            .unwrap()
            .len(),
        2
    );

    // 管理员：创建、按用户名查（返回哈希与 TOTP 密钥）、列表、设 TOTP。
    let admin = s
        .create_admin("root", "argon2:hash", AdminRole::SuperAdmin, true)
        .await
        .unwrap();
    assert!(admin.totp_required);
    let (found, hash, secret) = s.find_admin_by_username("root").await.unwrap().unwrap();
    assert_eq!(found.id, admin.id);
    assert_eq!(hash, "argon2:hash");
    assert!(secret.is_none());
    s.set_admin_totp(admin.id, "JBSWY3DPEHPK3PXP")
        .await
        .unwrap();
    assert!(s
        .find_admin_by_username("root")
        .await
        .unwrap()
        .unwrap()
        .2
        .is_some());
    assert_eq!(s.list_admins().await.unwrap().len(), 1);

    // 统计：真实聚合。
    let stats = s.stats_overview().await.unwrap();
    assert_eq!(stats.total_channels, 1);
    assert_eq!(stats.available_channels, 1);
    assert_eq!(stats.total_tokens, 1);
    assert_eq!(stats.active_tokens, 1);
    assert_eq!(stats.active_sessions, 0);
    assert!(stats.alerts.is_empty());
}

#[tokio::test]
async fn channels_for_token_authorization() {
    let fx = setup().await;
    let s = &fx.store;
    let user = s.create_user(&user_req("eve")).await.unwrap();
    let token = s
        .create_token(
            user.id,
            "tk_live_",
            &[4u8; 32],
            &token_req(user.id, &["体育"]),
        )
        .await
        .unwrap();
    let sports = s
        .create_channel(&channel_req(
            "CCTV-5",
            "体育",
            "https://x.example/cctv5.m3u8",
        ))
        .await
        .unwrap();
    let _news = s
        .create_channel(&channel_req(
            "CCTV-1",
            "央视",
            "https://x.example/cctv1.m3u8",
        ))
        .await
        .unwrap();

    let page = s
        .list_channels_for_token(&token, &PageQuery::default(), None)
        .await
        .unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.items[0].id, sports.id);

    // 分组过滤 + 黑名单。
    let page = s
        .list_channels_for_token(&token, &PageQuery::default(), Some("央视"))
        .await
        .unwrap();
    assert_eq!(page.total, 0);
    let token2 = s
        .update_token(
            token.id,
            &UpdateTokenRequest {
                name: None,
                device_type: None,
                max_concurrent_sessions: None,
                max_bound_devices: None,
                status: None,
                expires_at: None,
                locale: None,
                allowed_groups: None,
                denied_channels: Some(vec![sports.id]),
                features: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        s.list_channels_for_token(&token2, &PageQuery::default(), None)
            .await
            .unwrap()
            .total,
        0
    );

    // 空 allowed_groups = 全部组（与 authorize_channel 一致）：两个频道都可见。
    let token3 = s
        .create_token(user.id, "tk_live_", &[5u8; 32], &token_req(user.id, &[]))
        .await
        .unwrap();
    assert_eq!(
        s.list_channels_for_token(&token3, &PageQuery::default(), None)
            .await
            .unwrap()
            .total,
        2
    );

    // 频道删除：端点一并删除。
    assert!(s.delete_channel(sports.id).await.unwrap());
    assert!(s.list_endpoints(sports.id).await.unwrap().is_empty());
    assert!(!s.delete_channel(sports.id).await.unwrap());
}

#[tokio::test]
async fn v5_migration_updates_existing_default_locale() {
    // V5：线上旧库的 default_locale 是 V2 种子写死的 zh-Hant-TW；
    // migrate 重跑必须把它改成 zh-Hans-CN，且反复跑结果相同（幂等）。
    // 注意：migrate() 直接跑 SQL，不清 settings 缓存，故这里用
    // list_settings()（直读表）断言，不用带缓存的 get_setting()。
    async fn db_value(s: &Store, key: &str) -> serde_json::Value {
        s.list_settings()
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.key == key)
            .unwrap()
            .value
    }
    let f = setup().await;
    let s = &f.store;
    assert_eq!(
        db_value(s, "default_locale").await,
        serde_json::json!("zh-Hans-CN"),
        "新库种子应为简体中文"
    );
    // 模拟线上旧库：改回旧值，再跑一次 migrate。
    s.set_setting("default_locale", &serde_json::json!("zh-Hant-TW"))
        .await
        .unwrap();
    assert_eq!(
        db_value(s, "default_locale").await,
        serde_json::json!("zh-Hant-TW")
    );
    s.migrate().await.expect("re-migrate");
    assert_eq!(
        db_value(s, "default_locale").await,
        serde_json::json!("zh-Hans-CN"),
        "V5 应把旧值改成简体中文"
    );
    // 再跑一次：幂等。
    s.migrate().await.expect("re-migrate again");
    assert_eq!(
        db_value(s, "default_locale").await,
        serde_json::json!("zh-Hans-CN")
    );
}
