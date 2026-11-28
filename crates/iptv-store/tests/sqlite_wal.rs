// M-B（v0.1.6）：SQLite 写锁竞争回归测试。
//
// 服务器实测：5000 台 upsert 期间并发 PATCH 28 次里 15 次报 500。
// 修法：`Store::connect` 的 SQLite 分支经 `after_connect` 钩子对池内
// 每个连接执行 `PRAGMA journal_mode=WAL / synchronous=NORMAL /
// busy_timeout = 15000`。本文件验证：
//   1. 全新库启动后各 PRAGMA 的读回值（journal_mode=wal、
//      busy_timeout=15000、synchronous=1/NORMAL），且池内第二条连接
//      同样生效（busy_timeout 是连接级，钩子必须对每条连接跑）
//   2. 缩小版服务器场景：大 upsert 与 28 路并发 PATCH + 读同时进行，
//      零 `database is locked` 失败（5000 台全量在单测里太慢，
//      降级为 8 路并发 upsert × 300 台 + 28 路 PATCH + 读轮询）。
// 作者: Passwd Word

use iptv_dto::*;
use iptv_m3u::NormalizedChannel;
use iptv_store::Store;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn fresh_url(tag: &str) -> (String, std::path::PathBuf) {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let path = std::env::temp_dir().join(format!(
        "iptv-store-wal-{tag}-{}-{n}.db",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("db-wal"));
    let _ = std::fs::remove_file(path.with_extension("db-shm"));
    (format!("sqlite://{}?mode=rwc", path.display()), path)
}

fn cleanup(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(path.with_extension("db-wal"));
    let _ = std::fs::remove_file(path.with_extension("db-shm"));
}

fn ch(i: usize, batch: usize) -> NormalizedChannel {
    NormalizedChannel {
        tvg_id: Some(format!("mb-{batch}-{i}.cn")),
        name: format!("MB 频道 {batch}-{i}"),
        logo: None,
        group: Some("测试".to_string()),
        url: format!("https://x.example/mb-{batch}-{i}.m3u8"),
        catchup: None,
        catchup_source: None,
    }
}

#[tokio::test]
async fn sqlite_pragmas_wal_and_busy_timeout() {
    let (url, path) = fresh_url("pragmas");
    let store = Store::connect(&url).await.expect("connect sqlite");
    store.migrate().await.expect("migrate");

    // PRAGMA 读回：journal_mode=wal、busy_timeout=15000、synchronous=1（NORMAL）。
    let journal: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(store.pool())
        .await
        .expect("PRAGMA journal_mode");
    assert_eq!(journal, "wal", "journal_mode 应为 wal，实测 {journal}");
    let busy: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
        .fetch_one(store.pool())
        .await
        .expect("PRAGMA busy_timeout");
    assert_eq!(busy, 15000, "busy_timeout 应为 15000ms，实测 {busy}");
    let sync: i64 = sqlx::query_scalar("PRAGMA synchronous")
        .fetch_one(store.pool())
        .await
        .expect("PRAGMA synchronous");
    assert_eq!(sync, 1, "synchronous 应为 1（NORMAL），实测 {sync}");

    // 池内第二条连接同样生效：busy_timeout 是连接级，after_connect
    // 钩子必须对每条新建连接都跑，只在 migrate() 里执行一次是不够的。
    let mut conn = store.pool().acquire().await.expect("acquire");
    let busy2: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
        .fetch_one(&mut *conn)
        .await
        .expect("PRAGMA busy_timeout on 2nd conn");
    assert_eq!(
        busy2, 15000,
        "第 2 条连接 busy_timeout 也应为 15000，实测 {busy2}"
    );
    drop(conn);

    // WAL 模式下写过库应有 -wal 伴生文件（与主库同在数据卷内，
    // 备份须主库 + -wal + -shm 三文件一起拷，见部署文档提示）。
    assert!(
        path.with_extension("db-wal").exists(),
        "WAL 模式下应存在 -wal 伴生文件"
    );

    drop(store);
    cleanup(&path);
}

#[tokio::test]
async fn sqlite_busy_timeout_survives_long_write_lock() {
    // 区分度测试：一条连接 BEGIN IMMEDIATE 持有写锁 8 秒，期间并发 PATCH。
    // 修前 busy_timeout=5 秒 → PATCH 在 5 秒后报 `database is locked`；
    // 修后 15 秒 → PATCH 等到锁释放后成功。WAL 下读不受写锁阻塞。
    let (url, path) = fresh_url("holdlock");
    let store = Arc::new(Store::connect(&url).await.expect("connect sqlite"));
    store.migrate().await.expect("migrate");
    let src = store
        .create_source(&CreateSourceRequest {
            name: "M-B 持锁源".to_string(),
            url: Some("https://example.com/mb-hold.m3u".to_string()),
            sync_interval_sec: None,
            http_headers: None,
            epg_url: None,
            on_missing: None,
            auto_enable_new: None,
            enabled: None,
        })
        .await
        .expect("create source");
    let out = store
        .upsert_channels(src.id, vec![ch(0, 99)])
        .await
        .expect("seed upsert");
    assert_eq!(out.added, 1);
    let id = store
        .list_channels(
            &PageQuery {
                page: 1,
                page_size: 100,
                q: None,
            },
            None,
            None,
        )
        .await
        .expect("list seed")
        .items[0]
        .id;

    let mut holder = store.pool().acquire().await.expect("acquire holder");
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *holder)
        .await
        .expect("begin immediate");
    let hold = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(8)).await;
        sqlx::query("COMMIT")
            .execute(&mut *holder)
            .await
            .expect("commit");
    });
    let s = Arc::clone(&store);
    let patched = tokio::spawn(async move {
        s.update_channel(
            id,
            &UpdateChannelRequest {
                tvg_id: None,
                name: Some("MB 持锁 PATCH".to_string()),
                display_name: None,
                logo_url: None,
                group_title: None,
                status: None,
                visibility: None,
                sort_weight: None,
            },
        )
        .await
    });
    let got = patched.await.expect("patch task").expect("patch succeeds");
    assert_eq!(got.unwrap().name, "MB 持锁 PATCH");
    hold.await.expect("hold task");
    drop(store);
    cleanup(&path);
}

#[tokio::test]
async fn sqlite_concurrent_upsert_and_patch_no_locked_errors() {
    let (url, path) = fresh_url("concurrent");
    let store = Arc::new(Store::connect(&url).await.expect("connect sqlite"));
    store.migrate().await.expect("migrate");

    let src = store
        .create_source(&CreateSourceRequest {
            name: "M-B 并发源".to_string(),
            url: Some("https://example.com/mb.m3u".to_string()),
            sync_interval_sec: None,
            http_headers: None,
            epg_url: None,
            on_missing: None,
            auto_enable_new: None,
            enabled: None,
        })
        .await
        .expect("create source");

    // 先导一批频道，供 PATCH 命中。
    let seed: Vec<NormalizedChannel> = (0..28).map(|i| ch(i, 0)).collect();
    let out = store
        .upsert_channels(src.id, seed)
        .await
        .expect("seed upsert");
    assert_eq!(out.added, 28);
    let seed_ids: Vec<i64> = store
        .list_channels(
            &PageQuery {
                page: 1,
                page_size: 100,
                q: None,
            },
            None,
            None,
        )
        .await
        .expect("list seed")
        .items
        .iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(seed_ids.len(), 28);

    // 服务器场景缩小版：8 路并发 upsert（各 300 台）+ 28 路并发 PATCH
    // + 10 路并发读轮询，同时跑。修前（回滚日志 + 5 秒 busy_timeout）
    // 写风暴下 PATCH 极易撞 `database is locked` 报 500；修后应全部成功。
    let mut handles = Vec::new();
    for batch in 1..=8usize {
        let s = Arc::clone(&store);
        let items: Vec<NormalizedChannel> = (0..300).map(|i| ch(i, batch)).collect();
        handles.push(tokio::spawn(async move {
            s.upsert_channels(src.id, items)
                .await
                .map(|o| o.added + o.updated)
        }));
    }
    for (k, id) in seed_ids.iter().enumerate() {
        let s = Arc::clone(&store);
        let id = *id;
        handles.push(tokio::spawn(async move {
            let req = UpdateChannelRequest {
                tvg_id: None,
                name: Some(format!("MB PATCH {k}")),
                display_name: None,
                logo_url: None,
                group_title: None,
                status: None,
                visibility: None,
                sort_weight: None,
            };
            s.update_channel(id, &req).await.map(|c| c.is_some() as i64)
        }));
    }
    for _ in 0..10 {
        let s = Arc::clone(&store);
        handles.push(tokio::spawn(async move {
            s.list_channels(
                &PageQuery {
                    page: 1,
                    page_size: 100,
                    q: None,
                },
                None,
                None,
            )
            .await
            .map(|p| p.total as i64)
        }));
    }

    let mut locked = 0usize;
    let mut other_errs = Vec::new();
    for h in handles {
        match h.await.expect("task panicked") {
            Ok(_) => {}
            Err(e) => {
                let msg = e.to_string().to_ascii_lowercase();
                if msg.contains("database is locked") || msg.contains("database table is locked") {
                    locked += 1;
                } else {
                    other_errs.push(e.to_string());
                }
            }
        }
    }
    assert_eq!(locked, 0, "并发写期间出现 {locked} 次 database is locked");
    assert!(
        other_errs.is_empty(),
        "并发写期间出现非锁错误：{other_errs:?}"
    );

    // PATCH 的改名在 upsert 完成后仍在（未被随后 upsert 的旧值覆盖——
    // 本批 upsert 的 tvg_id 与种子不同源，不存在交错覆盖）。
    for (k, id) in seed_ids.iter().enumerate() {
        let got = store.get_channel(*id).await.expect("get").expect("exists");
        assert_eq!(got.name, format!("MB PATCH {k}"));
    }

    drop(store);
    cleanup(&path);
}
