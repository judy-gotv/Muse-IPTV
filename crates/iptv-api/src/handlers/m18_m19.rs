//! m18 M3U 粘贴导入 + m19 `epg_channel_map` 可配置映射（v0.1.5）。
//!
//! 单独成模块的原因：`handlers/admin.rs` 正由并行 worker（M16 异步上传）
//! 整文件重写，paste/映射 handler 放进去会被覆盖；本模块只依赖公开
//! 的 store/dto 接口，与 admin.rs 共用的三段小逻辑（`require` /
//! `audit` / `mirror_channel_logos`）在此各复制一份并注明出处，
//! 改动 admin.rs 对应逻辑时请同步这里。
// 作者: Passwd Word

use crate::{ok, AdminAuth, ApiError, ApiResult, AppState};
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::Json;
use iptv_core::{mirror_logo_if_configured, S3Config, LOGO_MIRROR_PER_SYNC};
use iptv_dto::{
    AdminRole, Channel, EpgChannelMap, ErrorCode, M3uImportReport, PasteM3uRequest,
    UpsertEpgChannelMapRequest,
};
use iptv_store::NewAuditLog;
use serde::Deserialize;
use std::sync::Arc;

fn require(admin: &AdminAuth, ok_roles: &[AdminRole], what: &str) -> Result<(), ApiError> {
    if ok_roles.contains(&admin.role) {
        Ok(())
    } else {
        Err(ApiError::new(
            ErrorCode::Forbidden,
            format!("role {:?} cannot {what}", admin.role),
        ))
    }
}

#[allow(clippy::too_many_arguments)]
async fn audit(
    state: &Arc<AppState>,
    admin: &AdminAuth,
    action: &str,
    object_type: &str,
    object_id: &str,
    before: Option<serde_json::Value>,
    after: Option<serde_json::Value>,
    ip: Option<&str>,
) {
    let _ = state
        .store
        .write_audit(&NewAuditLog {
            actor: admin.username.clone(),
            actor_role: Some(format!("{:?}", admin.role)),
            action: action.to_string(),
            object_type: object_type.to_string(),
            object_id: object_id.to_string(),
            before_json: before,
            after_json: after,
            ip_address: ip.map(|s| s.to_string()),
        })
        .await;
}

fn ip_of(headers: &HeaderMap) -> Option<String> {
    crate::client_ip(headers, None)
}

/// 与 `handlers/admin.rs::mirror_channel_logos` 同款（m9 台标转存）：
/// 未配置存储时直接返回；单张转存失败只记日志、保留直链；成功才
/// 写回 `logo_url` 并更新传入的频道对象。
async fn mirror_channel_logos(state: &Arc<AppState>, channels: &mut [Channel]) {
    let Some(cfg) = S3Config::from_env() else {
        return;
    };
    let mut seen = std::collections::HashSet::new();
    let mut n = 0;
    for ch in channels.iter_mut() {
        let Some(logo_url) = ch.logo_url.clone() else {
            continue;
        };
        if logo_url.trim().is_empty() || !seen.insert(logo_url.clone()) {
            continue;
        }
        if n >= LOGO_MIRROR_PER_SYNC {
            break;
        }
        n += 1;
        match mirror_logo_if_configured(Some(&cfg), &logo_url).await {
            Ok(Some(new_url)) => {
                if let Err(e) = state.store.update_channel_logo_url(ch.id, &new_url).await {
                    tracing::warn!("update logo url channel {} failed: {e:#}", ch.id);
                } else {
                    ch.logo_url = Some(new_url);
                }
            }
            Ok(None) => {}
            Err(e) => tracing::warn!("logo mirror channel {} failed: {e:#}", ch.id),
        }
    }
}

// ---------------------------------------------------------------------------
// m18：M3U 粘贴文本导入
// ---------------------------------------------------------------------------

/// POST /api/v1/admin/sources/paste（m18）：粘贴 M3U 文本导入。
///
/// 与文件上传走同一解析器：`iptv_m3u::parse_m3u` 与上传的
/// `parse_m3u_reader` 共用 `M3uParser` 状态机（BOM/CRLF/重复/
/// 非法 URL 逐条报错，异常行不整批失败）；入库路径（定源 →
/// upsert 频道 → 台标转存 → 审计 → 报告）与上传的
/// `upload_m3u` 尾段相同。`source_id` 给了就导入该源，否则按
/// `name`（缺省 `pasted.m3u`）新建源；空文本直接 422。
/// 仍受 axum 默认 2 MB JSON 体限保护，超大文本请走上传接口。
#[utoipa::path(post, path = "/api/v1/admin/sources/paste", tag = "admin", request_body = PasteM3uRequest, responses((status = 200, description = "粘贴 M3U 文本导入，M3uImportReport")))]
pub async fn paste_m3u(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    headers: HeaderMap,
    Json(req): Json<PasteM3uRequest>,
) -> ApiResult<M3uImportReport> {
    require(&admin, &[AdminRole::SuperAdmin], "paste sources")?;
    if req.content.trim().is_empty() {
        return Err(ApiError::new(
            ErrorCode::ValidationFailed,
            "content is empty",
        ));
    }
    let outcome = iptv_m3u::parse_m3u(&req.content);
    let source = match req.source_id {
        Some(id) => state
            .store
            .get_source(id)
            .await
            .map_err(ApiError::from)?
            .ok_or_else(|| ApiError::new(ErrorCode::NotFound, "source not found"))?,
        None => state
            .store
            .create_source(&iptv_dto::CreateSourceRequest {
                name: req
                    .name
                    .filter(|n| !n.trim().is_empty())
                    .unwrap_or_else(|| "pasted.m3u".to_string()),
                url: None,
                sync_interval_sec: None,
                http_headers: None,
                epg_url: outcome.x_tvg_url.clone(),
                on_missing: None,
                auto_enable_new: None,
                enabled: Some(true),
            })
            .await
            .map_err(ApiError::from)?,
    };
    let sync = state
        .store
        .upsert_channels(source.id, outcome.channels.clone())
        .await
        .map_err(ApiError::from)?;
    let mut to_mirror: Vec<Channel> = sync
        .added_channels
        .iter()
        .chain(sync.metadata_changed.iter())
        .chain(sync.url_changed.iter())
        .cloned()
        .collect();
    mirror_channel_logos(&state, &mut to_mirror).await;
    audit(
        &state,
        &admin,
        "source.paste",
        "source",
        &source.id.to_string(),
        None,
        Some(serde_json::json!({ "added": sync.added, "updated": sync.updated })),
        ip_of(&headers).as_deref(),
    )
    .await;
    ok(M3uImportReport {
        total_lines: outcome.total_lines,
        channels_parsed: outcome.channels.len(),
        channels_imported: (sync.added + sync.updated) as usize,
        errors: outcome.errors,
    })
}

// ---------------------------------------------------------------------------
// m19：`epg_channel_map` 可配置映射
// ---------------------------------------------------------------------------

/// GET `/admin/epg/maps` 的过滤参数（都可空，都不给 = 全表）。
#[derive(Debug, Default, Deserialize, utoipa::IntoParams)]
pub struct EpgMapQuery {
    pub epg_source_id: Option<i64>,
    pub channel_id: Option<i64>,
}

/// GET /api/v1/admin/epg/sources/{id}/maps（m19）：列出某 EPG 源的全部映射。
#[utoipa::path(get, path = "/api/v1/admin/epg/sources/{id}/maps", tag = "admin", responses((status = 200, description = "某 EPG 源的频道映射列表，EpgChannelMap[]")))]
pub async fn list_epg_channel_maps_for_source(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    Path(epg_source_id): Path<i64>,
) -> ApiResult<Vec<EpgChannelMap>> {
    if !admin.role.can_read_sources() {
        return Err(ApiError::new(ErrorCode::Forbidden, "cannot read sources"));
    }
    let _ = state
        .store
        .get_epg_source(epg_source_id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::new(ErrorCode::NotFound, "epg source not found"))?;
    let maps = state
        .store
        .list_epg_channel_maps(Some(epg_source_id), None)
        .await
        .map_err(ApiError::from)?;
    ok(maps)
}

/// GET /api/v1/admin/epg/maps?epg_source_id=&channel_id=（m19）：按 EPG 源
/// 或按频道查映射（供“该频道对应哪个 EPG 频道”反查）。
#[utoipa::path(get, path = "/api/v1/admin/epg/maps", tag = "admin", params(EpgMapQuery), responses((status = 200, description = "EPG 频道映射列表，EpgChannelMap[]")))]
pub async fn list_epg_channel_maps(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    Query(q): Query<EpgMapQuery>,
) -> ApiResult<Vec<EpgChannelMap>> {
    if !admin.role.can_read_sources() {
        return Err(ApiError::new(ErrorCode::Forbidden, "cannot read sources"));
    }
    let maps = state
        .store
        .list_epg_channel_maps(q.epg_source_id, q.channel_id)
        .await
        .map_err(ApiError::from)?;
    ok(maps)
}

/// PUT /api/v1/admin/epg/sources/{id}/maps（m19）：新建或改写一条映射。
/// 入参 `{ tvg_id, channel_id }`；`tvg_id` 为该 EPG 源内的 EPG 频道 id
///（XMLTV `<channel id>`），`channel_id` 必须是已存在的本台频道。
/// 同一 (epg_source_id, tvg_id) 重复 PUT 只改写目标频道，不产生重复行。
#[utoipa::path(put, path = "/api/v1/admin/epg/sources/{id}/maps", tag = "admin", request_body = UpsertEpgChannelMapRequest, responses((status = 200, description = "新建/改写 EPG 频道映射（EpgChannelMap）")))]
pub async fn upsert_epg_channel_map(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    headers: HeaderMap,
    Path(epg_source_id): Path<i64>,
    Json(req): Json<UpsertEpgChannelMapRequest>,
) -> ApiResult<EpgChannelMap> {
    require(&admin, &[AdminRole::SuperAdmin], "upsert epg channel maps")?;
    let tvg_id = req.tvg_id.trim().to_string();
    if tvg_id.is_empty() {
        return Err(ApiError::new(
            ErrorCode::ValidationFailed,
            "tvg_id is empty",
        ));
    }
    let _ = state
        .store
        .get_epg_source(epg_source_id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::new(ErrorCode::NotFound, "epg source not found"))?;
    let _ = state
        .store
        .get_channel(req.channel_id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::new(ErrorCode::NotFound, "channel not found"))?;
    let before = state
        .store
        .get_epg_channel_map(epg_source_id, &tvg_id)
        .await
        .map_err(ApiError::from)?;
    let map = state
        .store
        .upsert_epg_channel_map(epg_source_id, &tvg_id, req.channel_id)
        .await
        .map_err(ApiError::from)?;
    audit(
        &state,
        &admin,
        "epg_channel_map.upsert",
        "epg_channel_map",
        &map.id.to_string(),
        before.map(|m| serde_json::to_value(m).unwrap_or_default()),
        Some(serde_json::to_value(&map).unwrap_or_default()),
        ip_of(&headers).as_deref(),
    )
    .await;
    ok(map)
}

/// DELETE /api/v1/admin/epg/maps/{id}（m19）：按映射行 id 删除一条映射。
#[utoipa::path(delete, path = "/api/v1/admin/epg/maps/{id}", tag = "admin", responses((status = 200, description = "ok")))]
pub async fn delete_epg_channel_map(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> ApiResult<serde_json::Value> {
    require(&admin, &[AdminRole::SuperAdmin], "delete epg channel maps")?;
    let before = state
        .store
        .list_epg_channel_maps(None, None)
        .await
        .map_err(ApiError::from)?
        .into_iter()
        .find(|m| m.id == id);
    let deleted = state
        .store
        .delete_epg_channel_map(id)
        .await
        .map_err(ApiError::from)?;
    if !deleted {
        return Err(ApiError::new(
            ErrorCode::NotFound,
            "epg channel map not found",
        ));
    }
    audit(
        &state,
        &admin,
        "epg_channel_map.delete",
        "epg_channel_map",
        &id.to_string(),
        before.map(|m| serde_json::to_value(m).unwrap_or_default()),
        None,
        ip_of(&headers).as_deref(),
    )
    .await;
    ok(serde_json::json!({ "deleted": true }))
}
