//! Store 契约测试（开发文档 §16 数据库验收）：同一套 Store 方法在
//! PostgreSQL / MySQL 上跑通。由 CI 的 contract-postgres / contract-mysql
//! job 触发（DATABASE_URL 由 job 注入）；本地跑：
//!   DATABASE_URL=postgres://iptv:iptv@127.0.0.1:5432/iptv cargo test -p iptv-store --test contract
//!
//! 单个测试内顺序执行（多测试并行会共享同一库，互相干扰）。
// 作者: Passwd Word
use chrono::{Duration, Utc};
use iptv_dto::{
    CreateChannelRequest, CreateSourceRequest, CreateTokenRequest, CreateUserRequest, OnMissing,
    PageQuery, TokenFeatures,
};
use iptv_store::Store;

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
        tvg_id: Some(name.to_string()),
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
async fn contract_core() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("跳过 contract 测试：未设置 DATABASE_URL（CI 的 contract job 会注入）");
        return;
    };
    let store = Store::connect(&url).await.expect("connect");
    store.migrate().await.expect("migrate");

    // 用户 + Token 往返。
    let user = store.create_user(&user_req("contract")).await.unwrap();
    let token = store
        .create_token(user.id, "tk_live_", &[7u8; 32], &token_req(user.id, &[]))
        .await
        .unwrap();
    assert_eq!(
        store.get_token(token.id).await.unwrap().unwrap().id,
        token.id
    );
    assert!(store
        .find_token_by_hash(&[7u8; 32])
        .await
        .unwrap()
        .is_some());

    // 订阅源 + 频道 upsert：新增 → 元数据变更（同 URL 改名）。
    let source = store
        .create_source(&CreateSourceRequest {
            name: "contract".to_string(),
            url: Some("https://x.example/all.m3u".to_string()),
            sync_interval_sec: None,
            http_headers: None,
            epg_url: None,
            on_missing: Some(OnMissing::Disable),
            auto_enable_new: Some(true),
            enabled: Some(true),
        })
        .await
        .unwrap();
    let c1 = store
        .create_channel(&channel_req(
            "CCTV-1",
            "央视",
            "https://x.example/cctv1.m3u8",
        ))
        .await
        .unwrap();
    let outcome = store
        .upsert_channels(
            source.id,
            vec![iptv_m3u::NormalizedChannel {
                tvg_id: Some("CCTV-1".into()),
                name: "CCTV-1 综合".into(),
                logo: None,
                group: Some("央视".into()),
                url: "https://x.example/cctv1.m3u8".into(),
                catchup: None,
                catchup_source: None,
            }],
        )
        .await
        .unwrap();
    assert_eq!(
        (outcome.added, outcome.updated, outcome.removed),
        (0, 1, 0),
        "同 URL 改名应算 1 次元数据变更"
    );

    // 空 allowed_groups = 全部组（与 authorize_channel 一致）。
    let page = store
        .list_channels_for_token(&token, &PageQuery::default(), None)
        .await
        .unwrap();
    assert!(
        page.total >= 1,
        "空分组 Token 应看到频道，实际 {}",
        page.total
    );
    assert!(page.items.iter().any(|c| c.id == c1.id));

    // 播放会话生命周期。
    let s = store
        .create_playback_session(
            token.id,
            c1.id,
            "dev-1",
            "ps_contract_1",
            Some("1080p"),
            None,
        )
        .await
        .unwrap();
    assert!(store
        .get_playback_session(&s.session_uid)
        .await
        .unwrap()
        .is_some());
    assert!(store
        .heartbeat_session(&s.session_uid, Utc::now())
        .await
        .unwrap());
    assert!(store
        .end_session(&s.session_uid, Utc::now(), "ended")
        .await
        .unwrap());
    assert!(store
        .get_playback_session(&s.session_uid)
        .await
        .unwrap()
        .unwrap()
        .ended_at
        .is_some());

    // 审计 + 设置往返。
    store
        .write_audit(&iptv_store::NewAuditLog {
            actor: "contract".into(),
            actor_role: None,
            action: "contract.check".into(),
            object_type: "channel".into(),
            object_id: c1.id.to_string(),
            before_json: None,
            after_json: None,
            ip_address: None,
        })
        .await
        .unwrap();
    assert!(!store
        .list_audit(None, None, None, None, &PageQuery::default())
        .await
        .unwrap()
        .items
        .is_empty());
    assert_eq!(
        store.get_setting("default_locale").await.unwrap().unwrap(),
        serde_json::json!("zh-Hant-TW")
    );
}
