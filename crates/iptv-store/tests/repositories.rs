//! m20：Repository trait 层测试（§10.4）。
//!
//! - 编译期断言：`Store` 满足全部 8 个 trait（任一方法签名漂移即编译失败）。
//! - `dyn` 路径：`Arc<dyn TicketRepository>` / `Arc<dyn EpgRepository>` /
//!   `Arc<dyn TokenRepository>` 等 trait 对象真实可用（签发/消费票据、
//!   EPG 源 CRUD、用户+Token 建查）。
//! - 泛型路径：以 trait 为约束的调用方（worker 的 `probe_sample` /
//!   `import_epg_source` 同款写法）经 `SourceRepository` +
//!   `ChannelRepository` 完成一次订阅源建源 → upsert → 列表。
//!
//! 临时文件库（`sqlite::memory:` 在连接池下每个连接是独立的库，不可用）。
// 作者: Passwd Word

use chrono::{Duration, Utc};
use iptv_dto::*;
use iptv_m3u::NormalizedChannel;
use iptv_store::{
    ChannelRepository, EpgProgramInput, EpgRepository, GroupRepository, PlanRepository,
    SourceRepository, Store, TicketRepository, TokenRepository, UserRepository,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    store: Arc<Store>,
    path: std::path::PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

async fn setup() -> Fixture {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let path = std::env::temp_dir().join(format!("iptv-repo-test-{}-{}.db", std::process::id(), n));
    let url = format!("sqlite://{}?mode=rwc", path.display());
    let store = Arc::new(Store::connect(&url).await.expect("connect sqlite"));
    store.migrate().await.expect("migrate");
    Fixture { store, path }
}

/// 编译期断言：`Store` 满足全部 8 个 Repository trait。
/// 任一 trait 方法签名与 `Store` 的 inherent 方法漂移，`impl` 缺失即编译失败。
fn assert_store_is_repository<S>()
where
    S: TokenRepository
        + ChannelRepository
        + UserRepository
        + SourceRepository
        + PlanRepository
        + GroupRepository
        + EpgRepository
        + TicketRepository,
{
}

const _: fn() = assert_store_is_repository::<Store>;

fn page() -> PageQuery {
    PageQuery {
        page: 1,
        page_size: 20,
        q: None,
    }
}

/// 以 trait 为约束的调用方（与 worker 的 `probe_sample` /
/// `import_epg_source` 同款泛型写法）：只依赖 trait，不触具体 `Store`。
async fn sync_one_channel_via_traits<S, C>(sources: &S, channels: &C) -> (i64, i64)
where
    S: SourceRepository,
    C: ChannelRepository,
{
    let src = sources
        .create_source(&CreateSourceRequest {
            name: "trait 源".to_string(),
            url: Some("https://example.com/list.m3u".to_string()),
            sync_interval_sec: None,
            http_headers: None,
            epg_url: None,
            on_missing: None,
            auto_enable_new: None,
            enabled: None,
        })
        .await
        .unwrap();
    let outcome = channels
        .upsert_channels(
            src.id,
            vec![NormalizedChannel {
                tvg_id: Some("cctv1.cn".to_string()),
                name: "CCTV-1".to_string(),
                logo: None,
                group: Some("央视".to_string()),
                url: "https://x.example/cctv1.m3u8".to_string(),
                catchup: None,
                catchup_source: None,
            }],
        )
        .await
        .unwrap();
    assert_eq!(outcome.added, 1);
    let listed = channels.list_channels(&page(), None, None).await.unwrap();
    assert_eq!(listed.total, 1);
    (src.id, listed.items[0].id)
}

#[tokio::test]
async fn generic_source_and_channel_repository_sync() {
    let fx = setup().await;
    // 泛型约束路径：传入 `&Store`，函数体内只见得到 trait 方法。
    let (_src_id, ch_id) = sync_one_channel_via_traits(&*fx.store, &*fx.store).await;
    assert!(ch_id > 0);
    // 同一条路径也可走 trait 对象（`&dyn` 同样满足约束）。
    let channels: &dyn ChannelRepository = &*fx.store;
    let got = channels.get_channel(ch_id).await.unwrap();
    assert_eq!(got.unwrap().name, "CCTV-1");
}

#[tokio::test]
async fn dyn_ticket_repository_issue_consume() {
    let fx = setup().await;
    // `Arc<Store>` 隐式转 `Arc<dyn TicketRepository>`（§10.4 的注入形态）。
    let tickets: Arc<dyn TicketRepository> = fx.store.clone();
    let now = Utc::now();
    tickets
        .issue_login_ticket("abc123hash", 42, Some("10.0."), now + Duration::seconds(60))
        .await
        .unwrap();
    let got = tickets
        .consume_login_ticket("abc123hash", Utc::now())
        .await
        .unwrap();
    let got = got.expect("first consume works via dyn trait");
    assert_eq!(got.token_id, 42);
    assert_eq!(got.ip_prefix.as_deref(), Some("10.0."));
    // 第二次消费必须为空（原子消费，单次有效）。
    assert!(
        tickets
            .consume_login_ticket("abc123hash", Utc::now())
            .await
            .unwrap()
            .is_none(),
        "second consume must fail"
    );
    // 过期票据清理走同一 trait。
    tickets
        .issue_login_ticket("expired1", 43, None, now - Duration::seconds(1))
        .await
        .unwrap();
    assert_eq!(tickets.purge_expired_tickets(Utc::now()).await.unwrap(), 1);
}

#[tokio::test]
async fn dyn_user_token_plan_group_crud() {
    let fx = setup().await;
    let users: Arc<dyn UserRepository> = fx.store.clone();
    let tokens: Arc<dyn TokenRepository> = fx.store.clone();
    let plans: Arc<dyn PlanRepository> = fx.store.clone();
    let groups: Arc<dyn GroupRepository> = fx.store.clone();

    let plan = plans
        .create_plan(&CreatePlanRequest {
            name: "trait 套餐".to_string(),
            max_concurrent_sessions: 2,
            max_bound_devices: 3,
            channel_groups: vec!["央视".to_string()],
            duration_days: 30,
            price_cents: 0,
            currency: None,
        })
        .await
        .unwrap();
    let user = users
        .create_user(&CreateUserRequest {
            display_name: "Trait 用户".to_string(),
            email: None,
            phone: None,
            plan_id: Some(plan.id),
            locale: None,
            remark: None,
            expires_at: None,
        })
        .await
        .unwrap();
    assert_eq!(plans.count_users_with_plan(plan.id).await.unwrap(), 1);

    let token = tokens
        .create_token(
            user.id,
            "tk_",
            &[7u8; 32],
            &CreateTokenRequest {
                user_id: user.id,
                name: "trait token".to_string(),
                device_type: None,
                max_concurrent_sessions: 2,
                max_bound_devices: 1,
                expires_at: None,
                locale: None,
                allowed_groups: vec![],
                denied_channels: vec![],
                features: TokenFeatures::default(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        tokens
            .find_token_by_hash(&[7u8; 32])
            .await
            .unwrap()
            .unwrap()
            .id,
        token.id
    );
    assert_eq!(
        tokens
            .list_tokens(Some(user.id), None, &page())
            .await
            .unwrap()
            .total,
        1
    );

    let group = groups
        .create_group(&CreateGroupRequest {
            name: "trait 分组".to_string(),
            sort_weight: None,
            icon: None,
            is_default: None,
        })
        .await
        .unwrap();
    // 种子数据已有默认分组，只断言新建分组在列表中。
    assert!(groups
        .list_groups()
        .await
        .unwrap()
        .iter()
        .any(|g| g.id == group.id));
    assert!(groups.delete_group(group.id).await.unwrap());
    assert!(tokens.delete_token(token.id).await.unwrap());
    assert!(users.delete_user(user.id).await.unwrap());
    assert!(plans.delete_plan(plan.id).await.unwrap());
}

#[tokio::test]
async fn dyn_epg_repository_roundtrip() {
    let fx = setup().await;
    let (_src_id, ch_id) = sync_one_channel_via_traits(&*fx.store, &*fx.store).await;
    let epg: Arc<dyn EpgRepository> = fx.store.clone();

    let src = epg
        .create_epg_source("trait epg", "https://example.com/epg.xml")
        .await
        .unwrap();
    assert_eq!(epg.list_epg_sources().await.unwrap().len(), 1);
    let now = Utc::now();
    let n = epg
        .upsert_epg_programs(vec![EpgProgramInput {
            channel_id: ch_id,
            tvg_id: Some("cctv1.cn".to_string()),
            title: "新闻联播".to_string(),
            description: None,
            category: None,
            start_at: now,
            stop_at: now + Duration::seconds(1800),
        }])
        .await
        .unwrap();
    assert_eq!(n, 1);
    let programs = epg
        .programs_for_channel(
            ch_id,
            now - Duration::seconds(60),
            now + Duration::seconds(3600),
        )
        .await
        .unwrap();
    assert_eq!(programs.len(), 1);
    assert_eq!(programs[0].title, "新闻联播");
    let cur = epg
        .current_program(ch_id, now + Duration::seconds(60))
        .await
        .unwrap();
    assert!(cur.is_some());
    assert!(epg.delete_epg_source(src.id).await.unwrap());
}
