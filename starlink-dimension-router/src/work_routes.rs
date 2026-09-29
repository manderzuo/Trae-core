use crate::{state::StarlinkRouterState, work_continuation::ContinuationMode};
use aiwork_core::Principal;
use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Extension, Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkContinueRequest {
    pub base_version_id: String,
    pub prompt: String,
    pub duration: Option<i64>,
    pub resolution: Option<String>,
    pub ratio: Option<String>,
    pub continuation_mode: Option<ContinuationMode>,
}
pub async fn get(
    State(state): State<Arc<StarlinkRouterState>>,
    Path(work): Path<String>,
    Extension(p): Extension<Principal>,
) -> Response {
    if !state.config.work_context_for_key(&p.key_id) {
        return StatusCode::NOT_FOUND.into_response();
    }
    if aiwork_core::require_scope(&p, "videos:read").is_err() {
        return StatusCode::FORBIDDEN.into_response();
    }
    let reply=tokio::task::spawn_blocking(move||->Result<Option<Value>,String>{
        let Some(work)=state.store.owned_video_work(&p,&work).map_err(|_|"work_context_unavailable")?else{return Ok(None)};
        let versions=state.store.work_versions(&p,&work.work_id).map_err(|_|"work_context_unavailable")?;
        Ok(Some(json!({"work_id":work.work_id,"versions":versions,"created_at_ms":work.created_at_ms,"updated_at_ms":work.updated_at_ms})))
    }).await;
    match reply {
        Ok(Ok(Some(v))) => Json(v).into_response(),
        Ok(Ok(None)) => StatusCode::NOT_FOUND.into_response(),
        _ => crate::budget_flow::fail("work_context_unavailable"),
    }
}
pub async fn continue_work(
    State(state): State<Arc<StarlinkRouterState>>,
    Path(work): Path<String>,
    headers: HeaderMap,
    Extension(p): Extension<Principal>,
    body: Bytes,
) -> Response {
    if !state.config.work_context_for_key(&p.key_id) || !state.config.continuation_enabled {
        return crate::budget_flow::fail("continuation_disabled");
    }
    if aiwork_core::require_scope(&p, "videos:submit").is_err() {
        return StatusCode::FORBIDDEN.into_response();
    }
    let input: WorkContinueRequest = match serde_json::from_slice(&body) {
        Ok(i) => i,
        Err(_) => return crate::budget_flow::fail("work_decision_invalid"),
    };
    if input.prompt.trim().is_empty() || input.prompt.len() > 12 * 1024 {
        return crate::budget_flow::fail("work_decision_invalid");
    }
    if !matches!(state.store.owned_work_version(&p,&input.base_version_id),Ok(Some(v))if v.work_id==work)
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let mut raw = json!({"model":"seedance","action":"continue","prompt":input.prompt,"work_context":{"work_id":work,"base_version_id":input.base_version_id}});
    for (field, value) in [
        ("duration", json!(input.duration)),
        ("resolution", json!(input.resolution)),
        ("ratio", json!(input.ratio)),
        ("continuation_mode", json!(input.continuation_mode)),
    ] {
        if !value.is_null() {
            raw[field] = value;
        }
    }
    crate::user_routes::video_generations(
        State(state),
        headers,
        Extension(p),
        Bytes::from(raw.to_string()),
    )
    .await
}
