//! M10 回归：四处唯一约束第二次创建 → 识别为唯一冲突（API 层据此
//! 映射为 40900，而非 500）。store 层无建前预查，这里走的是 DB 唯一
//! 约束兜底；handler 层的预查由 iptv-api 的集成测试覆盖。
//! 用临时文件库（`sqlite::memory:` 在连接池下每个连接是独立的库，不可用）。
// 作者: Passwd Word

use iptv_dto::*;
use iptv_store::Store;
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
    let path = std::env::temp_dir().join(format!(
        "iptv-store-m10-test-{}-{}.db",
        std::process::id(),
        n
    ));
    let url = format!("sqlite://{}?mode=rwc", path.display());
    let store = Store::connect(&url).await.expect("connect sqlite");
    assert_eq!(store.dialect(), iptv_store::Dialect::Sqlite);
    store.migrate().await.expect("migrate");
    Fixture { store, path }
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
async fn duplicate_creates_are_unique_violations() {
    let fx = setup().await;
    let s = &fx.store;

    // 建管理员重名。
    s.create_admin("dup-admin", "argon2:hash", AdminRole::SuperAdmin, false)
        .await
        .unwrap();
    let err = s
        .create_admin("dup-admin", "argon2:hash", AdminRole::Operator, false)
        .await
        .unwrap_err();
    assert!(
        iptv_store::is_unique_violation_error(&err),
        "建管理员重名应识别为唯一冲突，实际：{err}"
    );

    // 建频道同 tvg_id。
    s.create_channel(&channel_req(
        "DupChan",
        "央视",
        "https://x.example/dup.m3u8",
    ))
    .await
    .unwrap();
    let err = s
        .create_channel(&channel_req(
            "DupChan",
            "央视",
            "https://x.example/dup2.m3u8",
        ))
        .await
        .unwrap_err();
    assert!(
        iptv_store::is_unique_violation_error(&err),
        "建频道同 tvg_id 应识别为唯一冲突，实际：{err}"
    );

    // 建套餐同名。
    let plan_req = CreatePlanRequest {
        name: "dup-plan".to_string(),
        max_concurrent_sessions: 2,
        max_bound_devices: 2,
        channel_groups: vec![],
        duration_days: 30,
        price_cents: 0,
        currency: None,
    };
    s.create_plan(&plan_req).await.unwrap();
    let err = s.create_plan(&plan_req).await.unwrap_err();
    assert!(
        iptv_store::is_unique_violation_error(&err),
        "建套餐同名应识别为唯一冲突，实际：{err}"
    );

    // 建分组同名。
    let group_req = CreateGroupRequest {
        name: "dup-group".to_string(),
        sort_weight: None,
        icon: None,
        is_default: None,
    };
    s.create_group(&group_req).await.unwrap();
    let err = s.create_group(&group_req).await.unwrap_err();
    assert!(
        iptv_store::is_unique_violation_error(&err),
        "建分组同名应识别为唯一冲突，实际：{err}"
    );

    // 对照：普通错误（不存在的行）不是唯一冲突。
    let err = s
        .get_channel(999_999)
        .await
        .unwrap()
        .ok_or_else(|| anyhow::anyhow!("not found"))
        .unwrap_err();
    assert!(!iptv_store::is_unique_violation_error(&err));
}
