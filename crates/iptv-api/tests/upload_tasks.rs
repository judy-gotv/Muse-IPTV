//! M16 回归：上传立即返回任务 id，后台解析 + 入库，进度接口可查中间态与终态。
//! 直接调 handler（不走 HTTP），DB 用临时文件 SQLite 库。
// 作者: Passwd Word

use axum::body::Body;
use axum::extract::{FromRequest, Multipart, Path, State};
use axum::http::{HeaderMap, Request};
use iptv_api::handlers::admin;
use iptv_api::{AdminAuth, AppState, Config};
use iptv_dto::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    state: Arc<AppState>,
    path: std::path::PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn config_for(path: &std::path::Path) -> Config {
    Config {
        database_url: format!("sqlite://{}?mode=rwc", path.display()),
        redis_url: None,
        play_secret: "test-play-secret".to_string(),
        admin_secret: "test-admin-secret".to_string(),
        portal_base_url: "http://127.0.0.1:8081".to_string(),
        bind_addr: "127.0.0.1:0".to_string(),
        cors_origins: vec![],
    }
}

async fn setup() -> Fixture {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    // 根因修复：文件名必须带进程 id（与 m19 等测试同一约定）。此前只用
    // 计数器（每进程从 0 起），m21 并行跑 `cargo test -p iptv-api` 全量时
    // 两个进程共用同一 `/tmp/iptv-api-upload-0.db`，对方小测试结束 Drop
    // 删文件后，本进程 upsert 写到被删的库上，SQLite 报
    // SQLITE_READONLY_DBMOVED (1032)，后台任务置 Failed；小测试因跑得快、
    // 在对方 Drop 前已完成而不受影响。
    let path =
        std::env::temp_dir().join(format!("iptv-api-upload-{}-{}.db", std::process::id(), n));
    let config = config_for(&path);
    let state = Arc::new(AppState::new(config).await.expect("AppState::new"));
    Fixture { state, path }
}

fn super_admin() -> AdminAuth {
    AdminAuth {
        admin_id: 1,
        username: "root".to_string(),
        role: AdminRole::SuperAdmin,
    }
}

/// 生成 n 个频道的 M3U 文本（每频道 2 行 + 1 行 #EXTM3U 头）。
fn gen_m3u(n: usize) -> String {
    let mut s = String::from("#EXTM3U\n");
    for i in 0..n {
        s.push_str(&format!(
            "#EXTINF:-1 tvg-id=\"ch{i}\",Channel {i}\nhttps://example.com/live/ch{i}.m3u8\n"
        ));
    }
    s
}

/// 把 M3U 文本包成 multipart 请求体，调 upload_m3u handler。
///
/// 注意：`Multipart` 提取器默认受 axum 2 MB 体限（`with_limited_body`），
/// 大文件必须像生产路由那样先过 `DefaultBodyLimit::disable()` 层——
/// 这里用 `ServiceBuilder + service_fn` 复现该层，再在里面调 handler。
async fn upload(
    state: &Arc<AppState>,
    body: &str,
    filename: &str,
    source_id: Option<i64>,
) -> UploadTaskCreated {
    use axum::response::IntoResponse;
    use tower::ServiceExt;
    let boundary = "----m16testboundary";
    let mut parts = String::new();
    if let Some(id) = source_id {
        parts.push_str(&format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"source_id\"\r\n\r\n{id}\r\n"
        ));
    }
    parts.push_str(&format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n{body}\r\n--{boundary}--\r\n"
    ));
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/admin/sources/upload")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(parts))
        .unwrap();
    let state2 = state.clone();
    let svc = tower::ServiceBuilder::new()
        .layer(axum::extract::DefaultBodyLimit::disable())
        .service_fn(move |req: Request<Body>| {
            let state = state2.clone();
            async move {
                let multipart = Multipart::from_request(req, &())
                    .await
                    .expect("multipart parse");
                let res =
                    admin::upload_m3u(State(state), super_admin(), HeaderMap::new(), multipart)
                        .await;
                Ok::<_, std::convert::Infallible>(match res {
                    Ok(json) => json.into_response(),
                    Err(e) => e.into_response(),
                })
            }
        });
    let resp = svc.oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let env: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        env.get("code").and_then(|c| c.as_i64()),
        Some(0),
        "upload failed: {env}"
    );
    UploadTaskCreated {
        task_id: env["data"]["task_id"].as_str().unwrap().to_string(),
    }
}

async fn get_task(state: &Arc<AppState>, id: &str) -> UploadTaskProgress {
    let res = admin::get_upload_task(State(state.clone()), super_admin(), Path(id.to_string()))
        .await
        .expect("get_upload_task ok");
    res.0.data.expect("progress")
}

/// 轮询直到终态（done/failed），返回沿途观测到的全部状态；超时则失败。
async fn poll_until_terminal(
    state: &Arc<AppState>,
    id: &str,
    timeout_ms: u64,
) -> (Vec<UploadTaskStatus>, UploadTaskProgress) {
    let mut seen: Vec<UploadTaskStatus> = Vec::new();
    let start = std::time::Instant::now();
    loop {
        let p = get_task(state, id).await;
        if seen.last() != Some(&p.status) {
            seen.push(p.status);
        }
        if matches!(p.status, UploadTaskStatus::Done | UploadTaskStatus::Failed) {
            return (seen, p);
        }
        assert!(
            start.elapsed().as_millis() < u128::from(timeout_ms),
            "poll upload task {id} timeout, last status {:?}",
            p.status
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn upload_returns_task_id_immediately_and_finishes() {
    let fx = setup().await;
    let body = gen_m3u(3);
    let created = upload(&fx.state, &body, "small.m3u", None).await;
    assert!(!created.task_id.is_empty());
    // 立即就能查到任务（queued 或 running），不必等解析完成。
    let first = get_task(&fx.state, &created.task_id).await;
    assert!(matches!(
        first.status,
        UploadTaskStatus::Queued | UploadTaskStatus::Running | UploadTaskStatus::Done
    ));
    let (seen, done) = poll_until_terminal(&fx.state, &created.task_id, 30_000).await;
    assert_eq!(done.status, UploadTaskStatus::Done);
    assert_eq!(done.total_lines, 7);
    assert_eq!(done.channels_parsed, 3);
    assert_eq!(done.channels_imported, 3);
    assert_eq!(done.error_count, 0);
    assert!(done.error.is_none());
    assert!(done.source_id.is_some());
    assert!(seen.contains(&UploadTaskStatus::Done));
}

#[tokio::test]
async fn upload_duplicate_tvg_id_within_file_dedupes() {
    // M16 回归：同一文件内两个频道共用 tvg_id（不同地址）时，后者应走
    // 更新分支、不得整批失败（§7.2 容错：重复不得整批失败）。此前
    // `upsert_channels` 的 `by_key` 只在循环前建一次、新插入行不补登，
    // 第二条同 tvg_id 会再走 INSERT 撞 `uq_channel_tvg`，整批 upsert
    // 报错、后台任务进 Failed。
    let fx = setup().await;
    let body = "#EXTM3U\n\
        #EXTINF:-1 tvg-id=\"dup1\",First\n\
        https://example.com/live/a.m3u8\n\
        #EXTINF:-1 tvg-id=\"dup1\",Second\n\
        https://example.com/live/b.m3u8\n\
        #EXTINF:-1 tvg-id=\"other\",Other\n\
        https://example.com/live/c.m3u8\n";
    let created = upload(&fx.state, body, "dup.m3u", None).await;
    let (_, done) = poll_until_terminal(&fx.state, &created.task_id, 30_000).await;
    assert_eq!(
        done.status,
        UploadTaskStatus::Done,
        "task failed, error: {:?}",
        done.error
    );
    assert_eq!(done.total_lines, 7);
    assert_eq!(done.channels_parsed, 3);
    // dup1 只入库一次（1 新增 + 1 更新），other 新增 1 条。
    assert_eq!(done.channels_imported, 3);
    let chans = fx
        .state
        .store
        .list_channels(
            &PageQuery {
                page: 1,
                page_size: 20,
                ..Default::default()
            },
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(chans.total, 2);
}

#[tokio::test]
async fn upload_to_missing_source_fails_with_error() {
    let fx = setup().await;
    let body = gen_m3u(2);
    let created = upload(&fx.state, &body, "bad-source.m3u", Some(999_999)).await;
    let (_, done) = poll_until_terminal(&fx.state, &created.task_id, 30_000).await;
    assert_eq!(done.status, UploadTaskStatus::Failed);
    assert!(done
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("source not found"));
}

#[tokio::test]
async fn upload_task_unknown_id_is_404() {
    let fx = setup().await;
    let err = admin::get_upload_task(
        State(fx.state.clone()),
        super_admin(),
        Path("no-such-task".to_string()),
    )
    .await
    .expect_err("unknown task must err");
    assert_eq!(err.code, ErrorCode::NotFound);
}

#[tokio::test]
async fn large_upload_shows_running_mid_state() {
    // M16 任务要求：≥10MB 文件，解析完成前进度接口能读到 running 中间态。
    let fx = setup().await;
    let body = gen_m3u(150_000);
    assert!(
        body.len() >= 10 * 1024 * 1024,
        "fixture < 10MB: {}",
        body.len()
    );
    let created = upload(&fx.state, &body, "big.m3u", None).await;
    // 5ms 轮询抓中间态：解析/入库期间应至少观测到一次 running 且已解析行数 >0。
    let mut saw_running_with_lines = false;
    let start = std::time::Instant::now();
    let final_p = loop {
        let p = get_task(&fx.state, &created.task_id).await;
        if p.status == UploadTaskStatus::Running && p.total_lines > 0 {
            saw_running_with_lines = true;
        }
        if matches!(p.status, UploadTaskStatus::Done | UploadTaskStatus::Failed) {
            break p;
        }
        assert!(
            start.elapsed().as_secs() < 600,
            "large upload timeout, last status {:?}",
            p.status
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    };
    assert!(
        saw_running_with_lines,
        "never observed running with lines > 0"
    );
    assert_eq!(
        final_p.status,
        UploadTaskStatus::Done,
        "task failed, error: {:?}",
        final_p.error
    );
    assert_eq!(final_p.total_lines, 1 + 150_000 * 2);
    assert_eq!(final_p.channels_parsed, 150_000);
    assert_eq!(final_p.channels_imported, 150_000);
}

/// m-uptask：模拟重启——同一库文件上新建 AppState（新 Store + 新
/// registry 实例），已完成任务的进度仍能查到（旧内存版此处会 404）。
#[tokio::test]
async fn upload_task_survives_appstate_restart() {
    let fx = setup().await;
    let body = gen_m3u(3);
    let created = upload(&fx.state, &body, "small.m3u", None).await;
    let (_, done) = poll_until_terminal(&fx.state, &created.task_id, 30_000).await;
    assert_eq!(done.status, UploadTaskStatus::Done);
    // “重启”：同一库文件上新 AppState。
    let state2 = Arc::new(
        AppState::new(config_for(&fx.path))
            .await
            .expect("AppState::new again"),
    );
    let p = get_task(&state2, &created.task_id).await;
    assert_eq!(p.status, UploadTaskStatus::Done);
    assert_eq!(p.total_lines, done.total_lines);
    assert_eq!(p.channels_parsed, 3);
    assert_eq!(p.channels_imported, 3);
    assert_eq!(p.source_id, done.source_id);
    drop(state2);
}

/// N5：已完成 24 小时以上的任务查 404，且 purge 会物理删行。
#[tokio::test]
async fn expired_upload_task_is_404_and_purged() {
    let fx = setup().await;
    let body = gen_m3u(2);
    let created = upload(&fx.state, &body, "small.m3u", None).await;
    let (_, done) = poll_until_terminal(&fx.state, &created.task_id, 30_000).await;
    assert_eq!(done.status, UploadTaskStatus::Done);
    // 把 updated_at 回拨 25 小时（模拟完成已久的任务）。
    let old = chrono::Utc::now().timestamp() - 25 * 3600;
    sqlx::query("UPDATE upload_task SET updated_at = ? WHERE task_id = ?")
        .bind(old)
        .bind(&created.task_id)
        .execute(fx.state.store.pool())
        .await
        .unwrap();
    let err = admin::get_upload_task(
        State(fx.state.clone()),
        super_admin(),
        Path(created.task_id.clone()),
    )
    .await
    .expect_err("expired task must err");
    assert_eq!(err.code, ErrorCode::NotFound);
    assert_eq!(fx.state.upload_tasks.purge_expired().await.unwrap(), 1);
    assert_eq!(fx.state.store.count_upload_tasks().await.unwrap(), 0);
}
