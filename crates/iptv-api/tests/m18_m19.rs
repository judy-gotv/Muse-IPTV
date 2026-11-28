//! m18 M3U 粘贴导入 + m19 `epg_channel_map` 可配置映射回归：
//! 粘贴异步入库（N8：POST 立返 task_id，后台解析 + 入库，进度接口
//! 轮询终态）、非法 URL/重复计入 error_count、粘贴与 `parse_m3u`
//! 同一解析器、粘贴与上传同一份文本结果一致、映射建→改→删全流程
//! 与审计记录。直接调 handler（不走 HTTP），DB 用临时文件 SQLite 库
//!（与 m19_crud.rs 同款 Fixture）。
// 作者: Passwd Word

use axum::body::Body;
use axum::extract::{FromRequest, Multipart, Path, Query, State};
use axum::http::{HeaderMap, Request};
use axum::response::IntoResponse;
use axum::Json;
use iptv_api::handlers::{admin, m18_m19};
use iptv_api::{AdminAuth, AppState, Config};
use iptv_dto::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tower::ServiceExt;

static COUNTER: AtomicU64 = AtomicU64::new(1000);

struct Fixture {
    state: Arc<AppState>,
    path: std::path::PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // -wal/-shm 伴生文件一并清掉（/tmp 是 512M tmpfs，别留给后轮）。
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(format!("{}-wal", self.path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", self.path.display()));
    }
}

async fn setup() -> Fixture {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let path = std::env::temp_dir().join(format!("iptv-api-test-{}-{}.db", std::process::id(), n));
    let config = Config {
        database_url: format!("sqlite://{}?mode=rwc", path.display()),
        redis_url: None,
        play_secret: "test-play-secret".to_string(),
        admin_secret: "test-admin-secret".to_string(),
        portal_base_url: "http://127.0.0.1:8081".to_string(),
        bind_addr: "127.0.0.1:0".to_string(),
        cors_origins: vec![],
    };
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

/// handler 返回 `Json<Envelope<T>>`，解出 data。
fn data<T>(r: Json<Envelope<T>>) -> T {
    r.0.data.unwrap()
}

async fn audit_total(state: &Arc<AppState>, action: &str) -> u64 {
    state
        .store
        .list_audit(None, Some(action), None, None, &PageQuery::default())
        .await
        .unwrap()
        .total
}

const PASTE_OK: &str = "\u{feff}#EXTM3U x-tvg-url=\"https://example.com/epg.xml\"\r\n#EXTINF:-1 tvg-id=\"cctv1.cn\" tvg-name=\"CCTV-1\",CCTV-1\r\nhttps://example.com/live/cctv1.m3u8\r\n#EXTINF:-1 tvg-id=\"cctv5.cn\",CCTV-5\r\nhttps://example.com/live/cctv5.m3u8\r\n";

const PASTE_BAD: &str = "#EXTM3U\n#EXTINF:-1 tvg-id=\"good\",Good\nhttps://example.com/live/good.m3u8\n#EXTINF:-1 tvg-id=\"bad\",Bad\nnot a url\n#EXTINF:-1 tvg-id=\"dup\",Dup\nhttps://example.com/live/good.m3u8\n";

/// N8：粘贴只注册任务、立即返回 task_id（不再同步返回报告）。
async fn paste(state: &Arc<AppState>, req: PasteM3uRequest) -> UploadTaskCreated {
    data(
        m18_m19::paste_m3u(
            State(state.clone()),
            super_admin(),
            HeaderMap::new(),
            Json(req),
        )
        .await
        .unwrap(),
    )
}

async fn get_task(state: &Arc<AppState>, id: &str) -> UploadTaskProgress {
    data(
        admin::get_upload_task(State(state.clone()), super_admin(), Path(id.to_string()))
            .await
            .unwrap(),
    )
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
            "poll paste task {id} timeout, last status {:?}",
            p.status
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
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

/// 把 M3U 文本包成 multipart 请求体，调 upload_m3u handler（与
/// upload_tasks.rs 的同款 helper 对照用：同一份文本走粘贴与走上传，
/// 终态数字应一致）。
async fn upload(state: &Arc<AppState>, body: &str, filename: &str) -> UploadTaskCreated {
    let boundary = "----m18testboundary";
    let parts = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n{body}\r\n--{boundary}--\r\n"
    );
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

fn channel_req(tvg_id: &str, name: &str) -> CreateChannelRequest {
    CreateChannelRequest {
        tvg_id: Some(tvg_id.to_string()),
        name: name.to_string(),
        display_name: None,
        logo_url: None,
        group_title: None,
        language: None,
        country: None,
        play_url: format!("https://example.com/live/{name}.m3u8"),
        backup_url: None,
        protocol: None,
        sort_weight: None,
        visibility: None,
    }
}

async fn make_channel(state: &Arc<AppState>, tvg_id: &str, name: &str) -> Channel {
    data(
        admin::create_channel(
            State(state.clone()),
            super_admin(),
            HeaderMap::new(),
            Json(channel_req(tvg_id, name)),
        )
        .await
        .unwrap(),
    )
}

async fn make_epg_source(state: &Arc<AppState>) -> EpgSource {
    data(
        admin::create_epg_source(
            State(state.clone()),
            super_admin(),
            HeaderMap::new(),
            Json(CreateEpgSourceRequest {
                name: "m18m19-epg".to_string(),
                url: "https://example.com/epg.xml".to_string(),
            }),
        )
        .await
        .unwrap(),
    )
}

#[tokio::test]
async fn m18_paste_normal_text_imports_channels() {
    let fx = setup().await;
    let state = fx.state.clone();
    let created = paste(
        &state,
        PasteM3uRequest {
            content: PASTE_OK.to_string(),
            source_id: None,
            name: Some("paste-test".to_string()),
        },
    )
    .await;
    assert!(!created.task_id.is_empty());
    // 立即就能查到任务（queued 或 running），不必等解析完成。
    let first = get_task(&state, &created.task_id).await;
    assert!(matches!(
        first.status,
        UploadTaskStatus::Queued | UploadTaskStatus::Running | UploadTaskStatus::Done
    ));
    let (seen, done) = poll_until_terminal(&state, &created.task_id, 30_000).await;
    // BOM + CRLF 被正确处理：2 台入库、0 条错误。
    assert_eq!(
        done.status,
        UploadTaskStatus::Done,
        "error: {:?}",
        done.error
    );
    assert_eq!(done.channels_parsed, 2);
    assert_eq!(done.channels_imported, 2);
    assert_eq!(done.error_count, 0);
    assert!(done.error.is_none());
    assert!(done.source_id.is_some());
    assert!(seen.contains(&UploadTaskStatus::Done));
    // 新建的源以给定 name 落库，且订阅级 x-tvg-url 被带上。
    let sources = data(
        admin::list_sources(State(state.clone()), super_admin())
            .await
            .unwrap(),
    );
    let src = sources
        .iter()
        .find(|s| s.name == "paste-test")
        .expect("source");
    assert_eq!(src.epg_url.as_deref(), Some("https://example.com/epg.xml"));
    assert_eq!(audit_total(&state, "source.paste").await, 1);
}

#[tokio::test]
async fn m18_paste_bad_lines_reported_per_line() {
    let fx = setup().await;
    let state = fx.state.clone();
    let created = paste(
        &state,
        PasteM3uRequest {
            content: PASTE_BAD.to_string(),
            source_id: None,
            name: None,
        },
    )
    .await;
    let (_, done) = poll_until_terminal(&state, &created.task_id, 30_000).await;
    // 非法 URL + 重复地址各 1 条错误（进度接口只给 error_count，不再
    // 逐条列 message，与上传一致），有效频道仍入库，不整批失败。
    assert_eq!(
        done.status,
        UploadTaskStatus::Done,
        "error: {:?}",
        done.error
    );
    assert_eq!(done.channels_parsed, 1);
    assert_eq!(done.channels_imported, 1);
    assert_eq!(done.error_count, 2);
}

#[tokio::test]
async fn m18_paste_uses_same_parser_as_parse_m3u() {
    let fx = setup().await;
    let state = fx.state.clone();
    let created = paste(
        &state,
        PasteM3uRequest {
            content: PASTE_BAD.to_string(),
            source_id: None,
            name: None,
        },
    )
    .await;
    let (_, done) = poll_until_terminal(&state, &created.task_id, 30_000).await;
    // 同一段文本直接走解析器：频道数 / 错误数 / 总行数 / x-tvg-url 全一致。
    let direct = iptv_m3u::parse_m3u(PASTE_BAD);
    assert_eq!(
        done.status,
        UploadTaskStatus::Done,
        "error: {:?}",
        done.error
    );
    assert_eq!(done.channels_parsed, direct.channels.len());
    assert_eq!(done.error_count, direct.errors.len());
    assert_eq!(done.total_lines, direct.total_lines);
    // 逐条错误仍由同一解析器产生（进度接口只汇总条数，明细见解析器）。
    assert!(direct.errors[0].message.contains("非法播放地址"));
    assert!(direct
        .errors
        .iter()
        .any(|e| e.message.contains("重复频道地址")));
}

#[tokio::test]
async fn m18_paste_empty_content_rejected() {
    let fx = setup().await;
    let state = fx.state.clone();
    let err = m18_m19::paste_m3u(
        State(state.clone()),
        super_admin(),
        HeaderMap::new(),
        Json(PasteM3uRequest {
            content: "  \n".to_string(),
            source_id: None,
            name: None,
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::ValidationFailed);
    assert_eq!(err.code.http_status(), 400);
}

#[tokio::test]
async fn m18_paste_into_existing_source() {
    let fx = setup().await;
    let state = fx.state.clone();
    let src = data(
        admin::create_source(
            State(state.clone()),
            super_admin(),
            HeaderMap::new(),
            Json(CreateSourceRequest {
                name: "existing".to_string(),
                url: None,
                sync_interval_sec: None,
                http_headers: None,
                epg_url: None,
                on_missing: None,
                auto_enable_new: None,
                enabled: Some(true),
            }),
        )
        .await
        .unwrap(),
    );
    let created = paste(
        &state,
        PasteM3uRequest {
            content: PASTE_OK.to_string(),
            source_id: Some(src.id),
            name: Some("ignored-name".to_string()),
        },
    )
    .await;
    let (_, done) = poll_until_terminal(&state, &created.task_id, 30_000).await;
    assert_eq!(
        done.status,
        UploadTaskStatus::Done,
        "error: {:?}",
        done.error
    );
    assert_eq!(done.channels_imported, 2);
    // 指定了 source_id 就不再新建源。
    let sources = data(
        admin::list_sources(State(state.clone()), super_admin())
            .await
            .unwrap(),
    );
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].id, src.id);
    // 不存在的 source_id → 任务 Failed（与上传一致，不再同步 404）。
    let created = paste(
        &state,
        PasteM3uRequest {
            content: PASTE_OK.to_string(),
            source_id: Some(src.id + 999),
            name: None,
        },
    )
    .await;
    let (_, failed) = poll_until_terminal(&state, &created.task_id, 30_000).await;
    assert_eq!(failed.status, UploadTaskStatus::Failed);
    assert!(failed
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("source not found"));
}

#[tokio::test]
async fn m18_paste_large_text_returns_task_id_immediately() {
    // N8 验证：2MB+ 粘贴文本请求秒级返回 task_id，轮询见 done。
    let fx = setup().await;
    let state = fx.state.clone();
    let body = gen_m3u(30_000);
    assert!(
        body.len() > 2 * 1024 * 1024,
        "fixture < 2MB: {}",
        body.len()
    );
    let start = std::time::Instant::now();
    let created = paste(
        &state,
        PasteM3uRequest {
            content: body,
            source_id: None,
            name: Some("paste-large".to_string()),
        },
    )
    .await;
    assert!(
        start.elapsed().as_secs() < 10,
        "paste did not return immediately: {:?}",
        start.elapsed()
    );
    assert!(!created.task_id.is_empty());
    let (_, done) = poll_until_terminal(&state, &created.task_id, 300_000).await;
    assert_eq!(
        done.status,
        UploadTaskStatus::Done,
        "error: {:?}",
        done.error
    );
    assert_eq!(done.total_lines, 1 + 30_000 * 2);
    assert_eq!(done.channels_parsed, 30_000);
    assert_eq!(done.channels_imported, 30_000);
    assert_eq!(done.error_count, 0);
}

#[tokio::test]
async fn m18_paste_matches_upload_same_text() {
    // N8 验证：同一份 M3U 走粘贴与走上传，终态数字一致。各用全新库
    //（同一库里跑两遍，第二遍的频道已存在、无变更可记，imported
    // 会是 0——那是 upsert 的去重语义，不是两路不一致）。
    let fx_paste = setup().await;
    let paste_created = paste(
        &fx_paste.state,
        PasteM3uRequest {
            content: PASTE_BAD.to_string(),
            source_id: None,
            name: Some("paste-parity".to_string()),
        },
    )
    .await;
    let (_, paste_done) =
        poll_until_terminal(&fx_paste.state, &paste_created.task_id, 30_000).await;
    assert_eq!(
        paste_done.status,
        UploadTaskStatus::Done,
        "paste failed: {:?}",
        paste_done.error
    );
    let fx_upload = setup().await;
    let upload_created = upload(&fx_upload.state, PASTE_BAD, "upload-parity.m3u").await;
    let (_, upload_done) =
        poll_until_terminal(&fx_upload.state, &upload_created.task_id, 30_000).await;
    assert_eq!(
        upload_done.status,
        UploadTaskStatus::Done,
        "upload failed: {:?}",
        upload_done.error
    );
    assert_eq!(paste_done.total_lines, upload_done.total_lines);
    assert_eq!(paste_done.channels_parsed, upload_done.channels_parsed);
    assert_eq!(paste_done.channels_imported, upload_done.channels_imported);
    assert_eq!(paste_done.error_count, upload_done.error_count);
}

#[tokio::test]
async fn m19_epg_channel_map_crud() {
    let fx = setup().await;
    let state = fx.state.clone();
    let epg = make_epg_source(&state).await;
    let ch1 = make_channel(&state, "cctv1.cn", "CCTV-1").await;
    let ch2 = make_channel(&state, "cctv5.cn", "CCTV-5").await;

    // upsert 新建：EPG 频道 `epg.cctv1` → 频道 1。
    let map = data(
        m18_m19::upsert_epg_channel_map(
            State(state.clone()),
            super_admin(),
            HeaderMap::new(),
            Path(epg.id),
            Json(UpsertEpgChannelMapRequest {
                tvg_id: "epg.cctv1".to_string(),
                channel_id: ch1.id,
            }),
        )
        .await
        .unwrap(),
    );
    assert_eq!(map.epg_source_id, epg.id);
    assert_eq!(map.tvg_id.as_deref(), Some("epg.cctv1"));
    assert_eq!(map.channel_id, Some(ch1.id));

    // 按源列出：1 条；按频道反查：频道 1 有 1 条，频道 2 为 0 条。
    let by_source = data(
        m18_m19::list_epg_channel_maps_for_source(
            State(state.clone()),
            super_admin(),
            Path(epg.id),
        )
        .await
        .unwrap(),
    );
    assert_eq!(by_source.len(), 1);
    let by_ch1 = data(
        m18_m19::list_epg_channel_maps(
            State(state.clone()),
            super_admin(),
            Query(m18_m19::EpgMapQuery {
                epg_source_id: None,
                channel_id: Some(ch1.id),
            }),
        )
        .await
        .unwrap(),
    );
    assert_eq!(by_ch1.len(), 1);
    let by_ch2 = data(
        m18_m19::list_epg_channel_maps(
            State(state.clone()),
            super_admin(),
            Query(m18_m19::EpgMapQuery {
                epg_source_id: None,
                channel_id: Some(ch2.id),
            }),
        )
        .await
        .unwrap(),
    );
    assert!(by_ch2.is_empty());

    // 同一 (epg_source_id, tvg_id) 重复 upsert 只改写目标频道，不增行。
    let map2 = data(
        m18_m19::upsert_epg_channel_map(
            State(state.clone()),
            super_admin(),
            HeaderMap::new(),
            Path(epg.id),
            Json(UpsertEpgChannelMapRequest {
                tvg_id: "epg.cctv1".to_string(),
                channel_id: ch2.id,
            }),
        )
        .await
        .unwrap(),
    );
    assert_eq!(map2.id, map.id);
    assert_eq!(map2.channel_id, Some(ch2.id));
    let by_source = data(
        m18_m19::list_epg_channel_maps_for_source(
            State(state.clone()),
            super_admin(),
            Path(epg.id),
        )
        .await
        .unwrap(),
    );
    assert_eq!(by_source.len(), 1);
    assert_eq!(audit_total(&state, "epg_channel_map.upsert").await, 2);

    // 删除：再列为空；重复删 → 404。
    let del = data(
        m18_m19::delete_epg_channel_map(
            State(state.clone()),
            super_admin(),
            HeaderMap::new(),
            Path(map.id),
        )
        .await
        .unwrap(),
    );
    assert_eq!(del["deleted"], true);
    let by_source = data(
        m18_m19::list_epg_channel_maps_for_source(
            State(state.clone()),
            super_admin(),
            Path(epg.id),
        )
        .await
        .unwrap(),
    );
    assert!(by_source.is_empty());
    let err = m18_m19::delete_epg_channel_map(
        State(state.clone()),
        super_admin(),
        HeaderMap::new(),
        Path(map.id),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
    assert_eq!(audit_total(&state, "epg_channel_map.delete").await, 1);
}

#[tokio::test]
async fn m19_epg_channel_map_upsert_validates() {
    let fx = setup().await;
    let state = fx.state.clone();
    let epg = make_epg_source(&state).await;
    let ch = make_channel(&state, "cctv1.cn", "CCTV-1").await;

    // 不存在的 EPG 源 → 404。
    let err = m18_m19::upsert_epg_channel_map(
        State(state.clone()),
        super_admin(),
        HeaderMap::new(),
        Path(epg.id + 999),
        Json(UpsertEpgChannelMapRequest {
            tvg_id: "epg.x".to_string(),
            channel_id: ch.id,
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
    // 不存在的频道 → 404。
    let err = m18_m19::upsert_epg_channel_map(
        State(state.clone()),
        super_admin(),
        HeaderMap::new(),
        Path(epg.id),
        Json(UpsertEpgChannelMapRequest {
            tvg_id: "epg.x".to_string(),
            channel_id: ch.id + 999,
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
    // 空 tvg_id → 422。
    let err = m18_m19::upsert_epg_channel_map(
        State(state.clone()),
        super_admin(),
        HeaderMap::new(),
        Path(epg.id),
        Json(UpsertEpgChannelMapRequest {
            tvg_id: "   ".to_string(),
            channel_id: ch.id,
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::ValidationFailed);
}
