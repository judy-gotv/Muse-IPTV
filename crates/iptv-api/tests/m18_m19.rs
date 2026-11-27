//! m18 M3U 粘贴导入 + m19 `epg_channel_map` 可配置映射回归：
//! 粘贴正常入库、非法 URL/重复逐条报错、粘贴与 `parse_m3u` 同一解析器、
//! 映射建→改→删全流程与审计记录。直接调 handler（不走 HTTP），
//! DB 用临时文件 SQLite 库（与 m19_crud.rs 同款 Fixture）。
// 作者: Passwd Word

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::Json;
use iptv_api::handlers::{admin, m18_m19};
use iptv_api::{AdminAuth, AppState, Config};
use iptv_dto::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static COUNTER: AtomicU64 = AtomicU64::new(1000);

struct Fixture {
    state: Arc<AppState>,
    path: std::path::PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
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

async fn paste(state: &Arc<AppState>, req: PasteM3uRequest) -> Json<Envelope<M3uImportReport>> {
    m18_m19::paste_m3u(
        State(state.clone()),
        super_admin(),
        HeaderMap::new(),
        Json(req),
    )
    .await
    .unwrap()
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
            Json(serde_json::json!({"name": "m18m19-epg", "url": "https://example.com/epg.xml"})),
        )
        .await
        .unwrap(),
    )
}

#[tokio::test]
async fn m18_paste_normal_text_imports_channels() {
    let fx = setup().await;
    let state = fx.state.clone();
    let report = data(
        paste(
            &state,
            PasteM3uRequest {
                content: PASTE_OK.to_string(),
                source_id: None,
                name: Some("paste-test".to_string()),
            },
        )
        .await,
    );
    // BOM + CRLF 被正确处理：2 台入库、0 条错误。
    assert_eq!(report.channels_parsed, 2);
    assert_eq!(report.channels_imported, 2);
    assert!(report.errors.is_empty(), "errors: {:?}", report.errors);
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
    let report = data(
        paste(
            &state,
            PasteM3uRequest {
                content: PASTE_BAD.to_string(),
                source_id: None,
                name: None,
            },
        )
        .await,
    );
    // 非法 URL + 重复地址各 1 条错误，有效频道仍入库，不整批失败。
    assert_eq!(report.channels_parsed, 1);
    assert_eq!(report.channels_imported, 1);
    assert_eq!(report.errors.len(), 2);
    assert!(report
        .errors
        .iter()
        .any(|e| e.message.contains("非法播放地址")));
    assert!(report
        .errors
        .iter()
        .any(|e| e.message.contains("重复频道地址")));
}

#[tokio::test]
async fn m18_paste_uses_same_parser_as_parse_m3u() {
    let fx = setup().await;
    let state = fx.state.clone();
    let report = data(
        paste(
            &state,
            PasteM3uRequest {
                content: PASTE_BAD.to_string(),
                source_id: None,
                name: None,
            },
        )
        .await,
    );
    // 同一段文本直接走解析器：频道数 / 错误数 / 总行数 / x-tvg-url 全一致。
    let direct = iptv_m3u::parse_m3u(PASTE_BAD);
    assert_eq!(report.channels_parsed, direct.channels.len());
    assert_eq!(report.errors.len(), direct.errors.len());
    assert_eq!(report.total_lines, direct.total_lines);
    assert_eq!(
        report.errors[0].line, direct.errors[0].line,
        "first error line must match the shared parser"
    );
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
    let report = data(
        paste(
            &state,
            PasteM3uRequest {
                content: PASTE_OK.to_string(),
                source_id: Some(src.id),
                name: Some("ignored-name".to_string()),
            },
        )
        .await,
    );
    assert_eq!(report.channels_imported, 2);
    // 指定了 source_id 就不再新建源。
    let sources = data(
        admin::list_sources(State(state.clone()), super_admin())
            .await
            .unwrap(),
    );
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].id, src.id);
    // 不存在的 source_id → 404。
    let err = m18_m19::paste_m3u(
        State(state.clone()),
        super_admin(),
        HeaderMap::new(),
        Json(PasteM3uRequest {
            content: PASTE_OK.to_string(),
            source_id: Some(src.id + 999),
            name: None,
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
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
