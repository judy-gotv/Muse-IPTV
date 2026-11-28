//! m18 M3U 粘贴导入 + m19 `epg_channel_map` 可配置映射（v0.1.5）。
//!
//! 单独成模块的原因：`handlers/admin.rs` 正由并行 worker（M16 异步上传）
//! 整文件重写，paste/映射 handler 放进去会被覆盖；本模块只依赖公开
//! 的 store/dto 接口，与 admin.rs 共用的两段小逻辑（`require` /
//! `audit`）在此各复制一份并注明出处，改动 admin.rs 对应逻辑时请同步
//! 这里。N8：粘贴导入不再自带解析/入库逻辑，直接复用 admin.rs 的
//! `run_upload_task`（与文件上传同一任务执行体），台标转存等尾段
//! 逻辑也随之共用，不再另复制 `mirror_channel_logos`。
// 作者: Passwd Word

use crate::handlers::admin::{run_upload_task, UploadTaskInput};
use crate::{ok, AdminAuth, ApiError, ApiResult, AppState};
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::Json;
use iptv_dto::{
    AdminRole, EpgChannelMap, ErrorCode, PasteM3uRequest, UploadTaskCreated,
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

// ---------------------------------------------------------------------------
// m18：M3U 粘贴文本导入
// ---------------------------------------------------------------------------

/// POST /api/v1/admin/sources/paste（m18）：粘贴 M3U 文本导入。
///
/// N8（v0.1.6）：与文件上传走同一异步任务注册表——本接口只注册任务、
/// 起后台任务后立即返回 `{ task_id }`；解析 + 入库在后台
/// `run_upload_task`（`UploadTaskInput::Text`）里做，进度见
/// GET /api/v1/admin/upload-tasks/{id}。终态数字（total_lines /
/// channels_parsed / channels_imported / error_count）与上传一致，
/// 从进度接口读，不再同步返回 `M3uImportReport`。
/// 保留的同步语义：空文本直接 400（`ValidationFailed` → 40001）；`source_id` 给了就导入该源，否则
/// 按 `name`（缺省 `pasted.m3u`）新建源；审计动作仍为 `source.paste`。
/// 未知的 `source_id` 不再同步 404，改为任务 Failed（与上传一致，
/// error 为 "source not found"）；源认领语义亦与上传一致（M-D：
/// 未认领到则任务 Failed）。仍受 axum 默认 2 MB JSON 体限保护，
/// 超大文本请走上传接口。
#[utoipa::path(post, path = "/api/v1/admin/sources/paste", tag = "admin", request_body = PasteM3uRequest, responses((status = 200, description = "粘贴 M3U 文本导入（异步解析，返回任务 id），UploadTaskCreated")))]
pub async fn paste_m3u(
    State(state): State<Arc<AppState>>,
    admin: AdminAuth,
    headers: HeaderMap,
    Json(req): Json<PasteM3uRequest>,
) -> ApiResult<UploadTaskCreated> {
    require(&admin, &[AdminRole::SuperAdmin], "paste sources")?;
    if req.content.trim().is_empty() {
        return Err(ApiError::new(
            ErrorCode::ValidationFailed,
            "content is empty",
        ));
    }
    // 源名沿用旧逻辑：给了非空 name 就用它，否则缺省 `pasted.m3u`；
    // 该名字同时是任务行的 filename 与新建源的名字（与上传以文件名
    // 建源一致）。
    let filename = req
        .name
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| "pasted.m3u".to_string());
    // N8：只注册任务、起后台任务，立即返回任务 id，不等解析完成。
    let task_id = uuid::Uuid::new_v4().to_string();
    state
        .upload_tasks
        .create_queued(task_id.clone(), filename.clone(), req.source_id)
        .await
        .map_err(ApiError::from)?;
    let bg_state = state.clone();
    let bg_admin = AdminAuth {
        admin_id: admin.admin_id,
        username: admin.username.clone(),
        role: admin.role,
    };
    let bg_ip = ip_of(&headers);
    tokio::spawn(run_upload_task(
        bg_state,
        task_id.clone(),
        UploadTaskInput::Text(req.content),
        filename,
        req.source_id,
        bg_admin,
        bg_ip,
        "source.paste",
    ));
    ok(UploadTaskCreated { task_id })
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
