//! Work versions wrap existing budget identities, not a second billing engine.
use crate::{
    state::StarlinkRouterState,
    work_context::{self, WorkResolution},
    work_planner::{self, WorkDecision, WorkIntent},
};
use aiwork_core::{
    BeginRequest, BeginRequestInput, BudgetExecutionState, BudgetStepKind, EncryptedWorkSnapshot,
    Principal, VideoWorkSnapshot, VideoWorkVersion, WorkVersionState,
};
use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextBinding {
    pub work_id: Option<String>,
    pub base_version_id: Option<String>,
    pub association: String,
    pub clarification: Option<String>,
}
pub(crate) fn freeze_context(
    state: &StarlinkRouterState,
    p: &Principal,
    headers: &HeaderMap,
    body: &Value,
) -> Result<ContextBinding, String> {
    let mut association = work_context::client_association(p, headers, body)?
        .unwrap_or_else(|| format!("random:{:032x}", rand::random::<u128>()));
    if body["action"] == "create"
        && state
            .store
            .owned_work_for_conversation(p, &association)
            .map_err(|_| "work_context_unavailable")?
            .is_some()
    {
        association = format!("random:{:032x}", rand::random::<u128>());
    }
    Ok(match work_context::resolve(state, p, headers, body)? {
        WorkResolution::New => ContextBinding {
            work_id: None,
            base_version_id: None,
            association,
            clarification: None,
        },
        WorkResolution::Existing { work, base_version } => ContextBinding {
            work_id: Some(work.work_id),
            base_version_id: base_version.map(|v| v.version_id),
            association,
            clarification: None,
        },
        WorkResolution::Clarify { text } => ContextBinding {
            work_id: None,
            base_version_id: None,
            association,
            clarification: Some(text),
        },
    })
}
fn load_binding(
    state: &StarlinkRouterState,
    p: &Principal,
    request: &str,
    original: &Value,
) -> Result<ContextBinding, String> {
    let checkpoint = state
        .store
        .budget_continuation(request)
        .map_err(|_| "budget_checkpoint_invalid")?
        .ok_or("budget_checkpoint_invalid")?;
    let raw = zeroize::Zeroizing::new(
        state
            .key_vault
            .decrypt(
                &checkpoint.encryption_context(),
                checkpoint.key_version,
                &checkpoint.ciphertext,
            )
            .map_err(|_| "budget_checkpoint_invalid")?,
    );
    let (body, binding) = crate::budget_continuation::decode_checkpoint(&raw)?;
    state
        .store
        .save_budget_continuation(
            p,
            request,
            &body,
            checkpoint.key_version,
            &checkpoint.ciphertext,
        )
        .map_err(|_| "budget_checkpoint_invalid")?;
    if aiwork_core::canonical_json_hash(&body) != aiwork_core::canonical_json_hash(original) {
        return Err("budget_checkpoint_invalid".into());
    }
    binding
        .map(Ok)
        .unwrap_or_else(|| freeze_context(state, p, &HeaderMap::new(), original))
}
fn parent(
    state: &StarlinkRouterState,
    p: &Principal,
    b: &ContextBinding,
) -> Result<Option<VideoWorkVersion>, String> {
    if let Some(id) = &b.work_id {
        state
            .store
            .owned_video_work(p, id)
            .map_err(|_| "work_context_unavailable")?
            .ok_or("work_context_unavailable")?;
    }
    b.base_version_id
        .as_ref()
        .map(|id| {
            state
                .store
                .owned_work_version(p, id)
                .map_err(|_| "work_context_unavailable")?
                .filter(|v| Some(&v.work_id) == b.work_id.as_ref())
                .ok_or("work_context_unavailable")
        })
        .transpose()
        .map_err(str::to_owned)
}
fn finish_read_only(
    state: &StarlinkRouterState,
    p: &Principal,
    request: &str,
    version: Option<&VideoWorkVersion>,
    text: &str,
) -> Result<Value, String> {
    if state
        .store
        .budget_operation(request)
        .map_err(|_| "work_context_unavailable")?
        .is_some()
    {
        state
            .store
            .finish_budget_execution(request, BudgetExecutionState::Succeeded)
            .map_err(|_| "work_context_unavailable")?;
    } else {
        state
            .store
            .finish_unadmitted_request(request)
            .map_err(|_| "work_context_unavailable")?;
    }
    let mut reply = crate::budget_flow::completion(request, text);
    if let Some(v) = version {
        work_context::decorate_owned_request(state, p, &v.operation_request_id, &mut reply)?;
    }
    Ok(reply)
}
async fn read_only(
    state: &Arc<StarlinkRouterState>,
    p: &Principal,
    request: &str,
    version: Option<&VideoWorkVersion>,
    d: &WorkDecision,
    body: &Value,
) -> Result<Value, String> {
    if let Some(v) = version {
        match d.action {
            WorkIntent::Download => {
                let reply =
                    crate::video_delivery::completion(state, p, &v.operation_request_id, body)
                        .await?;
                let _ = finish_read_only(state, p, request, Some(v), "")?;
                return Ok(reply);
            }
            WorkIntent::Status => {
                let r = crate::budget_flow::video_status(
                    state.clone(),
                    p.clone(),
                    v.operation_request_id.clone(),
                )
                .await;
                let bytes = axum::body::to_bytes(r.into_body(), 64 * 1024)
                    .await
                    .map_err(|_| "work_status_unavailable")?;
                let value: Value =
                    serde_json::from_slice(&bytes).map_err(|_| "work_status_unavailable")?;
                let text = match value["task"]["status"].as_str() {
                    Some("completed") => "所选视频已生成完成，可以下载或在此版本上修改、续写。",
                    Some("failed") => "上游已确认所选视频生成失败；本次查询没有提交新视频。",
                    Some("processing") => {
                        "所选任务尚未取得最终结果，系统继续查询同一任务；本次未重新生成。"
                    }
                    _ => "暂时无法核验所选视频最新状态；本次未提交新视频。",
                };
                return finish_read_only(state, p, request, Some(v), text);
            }
            _ => {}
        }
    }
    finish_read_only(
        state,
        p,
        request,
        version,
        d.clarification
            .as_deref()
            .unwrap_or("请说明要生成或修改的内容；本次未提交视频。"),
    )
}
fn normalized_original(
    state: &StarlinkRouterState,
    p: &Principal,
    mut body: Value,
    base: Option<&VideoWorkSnapshot>,
    binding: &ContextBinding,
) -> Result<Value, String> {
    // Internal media bindings can only be constructed after owned asset checks.
    if let Some(o) = body.as_object_mut() {
        o.remove("work_user_media_ids");
        o.remove("dispatch_body");
    }
    if body.get("messages").is_none() {
        body["messages"] = json!([{"role":"user","content":body["prompt"]}]);
    }
    if let Some(id) = &binding.work_id {
        body["work_context"] = json!({"work_id":id,"base_version_id":binding.base_version_id});
    }
    work_context::strip_body_markers(&mut body);
    if work_planner::clear_reference(&work_planner::current_text(&body)) {
        return Ok(body);
    }
    let has_inline = !crate::budget_flow::inline_images(&body)?.is_empty();
    let explicit_refs = ["image_asset_ids", "video_asset_ids"]
        .iter()
        .any(|f| body[*f].as_array().is_some_and(|v| !v.is_empty()));
    if !has_inline && !explicit_refs && body["action"] != "create" {
        if let Some(s) = base {
            let now = chrono::Utc::now().timestamp_millis();
            let mut images = Vec::new();
            let mut videos = Vec::new();
            for id in &s.user_media_ids {
                let media = state
                    .store
                    .owned_work_media(p, id)
                    .map_err(|_| "work_media_unavailable")?
                    .filter(|m| Some(&m.work_id) == binding.work_id.as_ref())
                    .ok_or("work_media_unavailable")?;
                let asset = crate::work_media::materialize(state, p, &media, now)?;
                if asset.mime_type.starts_with("image/") {
                    images.push(asset.id);
                } else {
                    videos.push(asset.id);
                }
            }
            if !images.is_empty() {
                body["image_asset_ids"] = json!(images);
            }
            if !videos.is_empty() {
                body["video_asset_ids"] = json!(videos);
            }
        }
    }
    crate::reference_context::recover(state, p, &mut body).map_err(str::to_owned)?;
    for field in ["image_asset_ids", "video_asset_ids"] {
        if let Some(ids) = body.get(field) {
            let ids = ids
                .as_array()
                .filter(|a| a.len() <= 10)
                .ok_or("invalid_reference_context")?;
            for id in ids {
                let id = id.as_str().ok_or("invalid_reference_context")?;
                crate::assets::read_owned(&state.store, &state.config.data_dir, p, id)
                    .map_err(|_| "reference_asset_unavailable")?;
            }
        }
    }
    if ["image_urls", "video_urls"]
        .iter()
        .any(|f| body[*f].as_array().is_some_and(|v| !v.is_empty()))
    {
        return Err("reference_requires_inline_image_or_owned_asset".into());
    }
    Ok(body)
}
fn pin_and_bind(
    state: &StarlinkRouterState,
    p: &Principal,
    request: &str,
    binding: &ContextBinding,
    base: Option<&VideoWorkSnapshot>,
    d: &WorkDecision,
    mut original: Value,
    continuation: Option<&VideoWorkSnapshot>,
) -> Result<VideoWorkVersion, String> {
    let work = if d.action != WorkIntent::Create || binding.base_version_id.is_none() {
        binding
            .work_id
            .as_ref()
            .map(|id| {
                state
                    .store
                    .owned_video_work(p, id)
                    .map_err(|_| "work_context_unavailable")?
                    .ok_or("work_context_unavailable")
            })
            .transpose()
            .map_err(str::to_owned)?
    } else {
        None
    };
    let work = match work {
        Some(w) => w,
        None => state
            .store
            .create_video_work(
                p,
                if d.action == WorkIntent::Create && binding.work_id.is_some() {
                    request
                } else {
                    &binding.association
                },
            )
            .map_err(|_| "work_context_unavailable")?,
    };
    let images = crate::budget_flow::inline_images(&original)?;
    let fresh = !images.is_empty()
        || ["image_asset_ids", "video_asset_ids"]
            .iter()
            .any(|f| original[*f].as_array().is_some_and(|a| !a.is_empty()));
    if fresh {
        aiwork_core::require_scope(p, "assets:write").map_err(|_| "insufficient_scope")?;
    }
    let now = chrono::Utc::now().timestamp_millis();
    let mut media_ids = Vec::new();
    let uploaded_tail=continuation.is_none()
        && crate::work_continuation::requested_mode(&original)?==crate::work_continuation::ContinuationMode::TailReference
        && original["video_asset_ids"].as_array().is_some_and(|v|!v.is_empty());
    if uploaded_tail && (!state.config.continuation_enabled || original["video_asset_ids"].as_array().is_none_or(|v|v.len()!=1)) {
        return Err("continuation_mode_unsupported".into());
    }
    let mut uploaded_frame=None;
    if !work_planner::clear_reference(&work_planner::current_text(&original)) {
        for image in &images {
            let m = crate::work_media::pin(state, p, &work.work_id, image, now)?;
            media_ids.push(m.media_id);
        }
        for field in ["image_asset_ids", "video_asset_ids"] {
            for id in original[field].as_array().into_iter().flatten() {
                let owned = crate::assets::read_owned(
                    &state.store,
                    &state.config.data_dir,
                    p,
                    id.as_str().ok_or("invalid_reference_context")?,
                )
                .map_err(|_| "reference_asset_unavailable")?;
                if uploaded_tail && field=="video_asset_ids" {
                    let frame=state.bridge_client().reference_last_frame(&p.key_id,&owned.bytes)?;
                    let parsed=crate::assets::ParsedAssetUpload {filename:"reference-tail.png".into(),declared_mime:Some("image/png".into()),bytes:frame.bytes};
                    let m=crate::work_media::pin_kind(state,p,&work.work_id,&parsed,now,Some("tail_frame"))?;
                    uploaded_frame=Some(m.media_id);
                    continue;
                }
                let parsed = crate::assets::ParsedAssetUpload {
                    filename: owned.record.filename,
                    declared_mime: Some(owned.record.mime_type),
                    bytes: owned.bytes,
                };
                let m = crate::work_media::pin(state, p, &work.work_id, &parsed, now)?;
                if !media_ids.contains(&m.media_id) {
                    media_ids.push(m.media_id);
                }
            }
        }
    }
    original["work_user_media_ids"] = json!(media_ids);
    let mut snapshot = work_planner::merge_snapshot(base, d, &original)?;
    if let Some(c) = continuation {
        snapshot.tail_frame_media_id = c.tail_frame_media_id.clone();
        snapshot.continuation_video_media_id = c.continuation_video_media_id.clone();
        snapshot.effective_prompt = c.effective_prompt.clone();
        snapshot.reference_mode = c.reference_mode.clone();
    }
    if let Some(frame)=uploaded_frame {
        // This frame is the user's durable reference, not an expendable frame
        // derived from a previous generated version. Later revisions retain it.
        snapshot.user_media_ids=media_ids;
        snapshot.user_media_ids.push(frame);
        snapshot.tail_frame_media_id=None;
        snapshot.continuation_video_media_id=None;
        snapshot.reference_mode="tail_reference".into();
    }
    snapshot.parent_version_id = if d.action == WorkIntent::Create {
        None
    } else {
        binding.base_version_id.clone()
    };
    snapshot.source_request_id = Some(request.into());
    let mut wire = json!({"model":"seedance","prompt":snapshot.effective_prompt,"duration":snapshot.duration,"resolution":snapshot.resolution,"ratio":snapshot.ratio,"watermark":snapshot.watermark});
    let mut image_assets = Vec::new();
    let mut video_assets = Vec::new();
    let references = snapshot.continuation_video_media_id.iter().chain(snapshot.user_media_ids
        .iter()
        .chain(snapshot.tail_frame_media_id.iter()))
        .collect::<Vec<_>>();
    for id in references {
        let media = state
            .store
            .owned_work_media(p, id)
            .map_err(|_| "work_media_unavailable")?
            .filter(|m| m.work_id == work.work_id)
            .ok_or("work_media_unavailable")?;
        // The full immediate parent replaces historical video references;
        // retaining every ancestor would exceed upstream limits and mix motion.
        if snapshot.continuation_video_media_id.is_some() && media.kind=="video" && Some(id)!=snapshot.continuation_video_media_id.as_ref() {continue;}
        // Explicit tail mode must not also submit an inherited full video.
        if snapshot.tail_frame_media_id.is_some() && media.kind=="video" {continue;}
        if image_assets.len()+video_assets.len()>=10 {return Err("reference_image_limit".into());}
        let asset = crate::work_media::materialize(state, p, &media, now)?;
        let owned = crate::assets::read_owned(&state.store, &state.config.data_dir, p, &asset.id)
            .map_err(|_| "work_media_unavailable")?;
        state
            .store
            .acquire_work_media_lease(p, id, request)
            .map_err(|_| "work_media_unavailable")?;
        let _permit = state
            .asset_limiter
            .acquire(&p.key_id, owned.bytes.len())
            .map_err(|_| "reference_upload_limited")?;
        let bridge_id = state
            .bridge_client()
            .upload_asset(
                &owned.record.filename,
                &owned.record.mime_type,
                &owned.bytes,
                request,
            )
            .map_err(|_| "reference_materialization_failed")?;
        if owned.record.mime_type.starts_with("image/") {
            image_assets.push(bridge_id);
        } else {
            video_assets.push(bridge_id);
        }
    }
    if !image_assets.is_empty() {
        wire["image_asset_ids"] = json!(image_assets);
    }
    if !video_assets.is_empty() {
        wire["video_asset_ids"] = json!(video_assets);
    }
    snapshot.dispatch_body = Some(wire);
    let raw = zeroize::Zeroizing::new(
        serde_json::to_string(&snapshot).map_err(|_| "work_snapshot_invalid")?,
    );
    let sealed = state
        .key_vault
        .encrypt(
            &aiwork_core::work_snapshot_context(&p.key_id, &work.work_id, request),
            &raw,
        )
        .map_err(|_| "work_snapshot_unavailable")?;
    let v = state
        .store
        .bind_work_version(
            p,
            &work.work_id,
            snapshot.parent_version_id.as_deref(),
            request,
            d.paid_action().ok_or("work_decision_invalid")?,
            &EncryptedWorkSnapshot {
                key_version: sealed.key_version,
                ciphertext: sealed.ciphertext,
                snapshot_sha256: format!("{:x}", Sha256::digest(raw.as_bytes())),
            },
        )
        .map_err(|_| "work_snapshot_unavailable")?
        .version;
    Ok(v)
}
pub(crate) async fn execute(
    state: Arc<StarlinkRouterState>,
    p: Principal,
    request: String,
    original: Value,
    fresh: bool,
    dispatch_only: bool,
) -> Result<Value, crate::seedance_feedback::Failure> {
    use crate::seedance_feedback::Stage;
    let operation = state
        .store
        .budget_operation(&request)
        .map_err(|_| "work_context_unavailable")?;
    let bound = state
        .store
        .work_version_for_request(&p, &request)
        .map_err(|_| "work_context_unavailable")?;
    let version = if let Some(v) = bound {
        v
    } else {
        let binding = load_binding(&state, &p, &request, &original)?;
        let parent = parent(&state, &p, &binding)?;
        if parent.is_some(){state.seedance_results.progress(&request,Stage::Restoring);}
        if let Some(text) = &binding.clarification {
            return finish_read_only(&state, &p, &request, parent.as_ref(), text).map_err(Into::into);
        }
        if let Some(d) = work_planner::read_only_decision(&original, parent.is_some()) {
            return read_only(&state, &p, &request, parent.as_ref(), &d, &original).await.map_err(Into::into);
        }
        let base = parent
            .as_ref()
            .map(|v| work_context::read_snapshot(&state, &p, v))
            .transpose()?;
        let s = state.clone();
        let owner = p.clone();
        let frozen = binding.clone();
        let b = base.clone();
        let raw = original.clone();
        let mut normalized = tokio::task::spawn_blocking(move || {
            normalized_original(&s, &owner, raw, b.as_ref(), &frozen)
        })
        .await
        .map_err(|_| "reference_worker_unavailable")??;
        let model = state.config.seedance_assistant_model.clone();
        state.seedance_results.progress(&request, Stage::Assistant);
        let assist = if let Some(step) = operation
            .as_ref()
            .and_then(|op| op.steps.iter().find(|s| s.kind == BudgetStepKind::Assist))
            .cloned()
        {
            step
        } else {
            if !fresh {
                return Err("budget_preparation_requires_recovery".into());
            }
            let body = work_planner::build_helper_input(base.as_ref(), &normalized, &model)?;
            let child = match state
                .store
                .begin_budget_assist_request(
                    &request,
                    BeginRequestInput {
                        user_id: p.user_id.clone(),
                        api_key_id: p.key_id.clone(),
                        protocol: "openai".into(),
                        endpoint: "chat".into(),
                        model: model.clone(),
                        idempotency_key: format!("budget-assist:{request}"),
                        body: body.clone(),
                    },
                )
                .map_err(|_| "work_context_unavailable")?
            {
                BeginRequest::Created(r) | BeginRequest::Existing(r) => r.id,
                BeginRequest::Conflict => return Err("assist_identity_conflict".into()),
            };
            let s = state.clone();
            let p = p.clone();
            let parent = request.clone();
            let prepared=tokio::task::spawn_blocking(move || {
                crate::budget_flow::prepare_step(
                    &s,
                    &p,
                    &parent,
                    &child,
                    &model,
                    body,
                    BudgetStepKind::Assist,
                )
            })
            .await
            .map_err(|_| "assist_worker_unavailable")?;
            match prepared {
                Ok(step)=>step,
                Err(error)=>{
                    // Core records dispatch before paid I/O. This existing CAS
                    // aborts only a preparation with no execution/billing evidence;
                    // admitted or unknown paid work is retained, never refunded.
                    crate::budget_flow::abort_preparation_failure(&state,&request,&error);
                    return Err(error.into());
                }
            }
        };
        let result = crate::budget_flow::wait_result(state.clone(), assist).await?;
        if result
            .pointer("/choices/0/finish_reason")
            .and_then(Value::as_str)
            != Some("stop")
        {
            return Err("work_decision_invalid".into());
        }
        let mut decision = work_planner::parse_decision(
            result
                .pointer("/choices/0/message/content")
                .and_then(Value::as_str)
                .ok_or("work_decision_invalid")?,
        )?;
        work_planner::resolve_uploaded_video_action(&mut decision, parent.is_some(), &normalized);
        if decision.paid_action().is_none() {
            return read_only(&state, &p, &request, parent.as_ref(), &decision, &original).await.map_err(Into::into);
        }
        if parent.is_none() && matches!(decision.action,WorkIntent::Revise|WorkIntent::Continue) {
            return Err("work_parent_required".into());
        }
        if let Some(explicit) = original["action"].as_str() {
            if serde_json::to_value(decision.action)
                .map_err(|_| "work_decision_invalid")?
                .as_str()
                != Some(explicit)
            {
                return Err("work_decision_invalid".into());
            }
        }
        let uploaded_tail=crate::work_continuation::requested_mode(&normalized)?==crate::work_continuation::ContinuationMode::TailReference
            && original["video_asset_ids"].as_array().is_some_and(|v|!v.is_empty());
        let continuation = if decision.action == WorkIntent::Continue && !uploaded_tail {
            let mut input = normalized.clone();
            input["prompt"] = json!(decision.effective_prompt);
            Some(
                crate::work_continuation::prepare_continuation(
                    state.clone(),
                    p.clone(),
                    parent.as_ref().ok_or("work_parent_required")?.clone(),
                    input,
                    &request,
                )
                .await?,
            )
        } else {
            None
        };
        if decision.action == WorkIntent::Create && base.is_some() {
            // Discard hydrated parent assets, not this turn's explicitly supplied
            // images. Independent creation cannot resurrect old reference markers.
            let mut raw = original.clone();
            raw["action"] = json!("create");
            if let Some(latest) = raw["messages"]
                .as_array()
                .and_then(|a| a.iter().rev().find(|m| m["role"] == "user"))
                .cloned()
            {
                raw["messages"] = json!([latest]);
            }
            let s = state.clone();
            let owner = p.clone();
            let frozen = binding.clone();
            normalized = tokio::task::spawn_blocking(move || {
                normalized_original(&s, &owner, raw, None, &frozen)
            })
            .await
            .map_err(|_| "reference_worker_unavailable")??;
        }
        crate::user_routes::require_video_admission(&state, &p, "seedance", &normalized)
            .map_err(|_| "video_billing_paused")?;
        state.seedance_results.progress(&request, Stage::References);
        let s = state.clone();
        let owner = p.clone();
        let id = request.clone();
        tokio::task::spawn_blocking(move || {
            pin_and_bind(
                &s,
                &owner,
                &id,
                &binding,
                base.as_ref(),
                &decision,
                normalized,
                continuation.as_ref(),
            )
        })
        .await
        .map_err(|_| "work_snapshot_unavailable")??
    };
    if operation.as_ref().is_some_and(|op| {
        matches!(
            op.execution_state,
            BudgetExecutionState::Failed
                | BudgetExecutionState::Canceled
                | BudgetExecutionState::Succeeded
        )
    }) {
        return Err("video_continuation_not_active".into());
    }
    let snapshot = work_context::read_snapshot(&state, &p, &version)?;
    let wire = snapshot.dispatch_body.ok_or("work_snapshot_invalid")?;
    state
        .store
        .set_work_version_state(&p, &request, WorkVersionState::Running)
        .map_err(|_| "work_context_unavailable")?;
    state.seedance_results.progress(&request, if version.action==aiwork_core::WorkAction::Continue {Stage::SubmittingSegment}else{Stage::Submitting});
    let s = state.clone();
    let owner = p.clone();
    let rid = request.clone();
    let step = tokio::task::spawn_blocking(move || {
        crate::budget_flow::prepare_step(
            &s,
            &owner,
            &rid,
            &rid,
            "seedance",
            wire,
            BudgetStepKind::Video,
        )
    })
    .await
    .map_err(|_| "video_worker_unavailable")??;
    if dispatch_only {
        return Ok(json!({"request_id":request,"status":"dispatched"}));
    }
    let result = crate::budget_flow::wait_result(state.clone(), step).await?;
    if result["status"] != "completed" {
        return Err(crate::seedance_feedback::Failure::video(&result));
    }
    complete_version(&state, &p, &request)?;
    state.seedance_results.progress(&request, Stage::Delivery);
    crate::video_delivery::completion(&state, &p, &request, &original).await.map_err(Into::into)
}
pub(crate) fn complete_version(
    state: &Arc<StarlinkRouterState>,
    p: &Principal,
    request: &str,
) -> Result<(), String> {
    if let Some(v) = state
        .store
        .work_version_for_request(p, request)
        .map_err(|_| "work_context_unavailable")?
    {
        if v.state == WorkVersionState::Preparing {
            state
                .store
                .set_work_version_state(p, request, WorkVersionState::Running)
                .map_err(|_| "work_context_unavailable")?;
        }
        state
            .store
            .set_work_version_state(p, request, WorkVersionState::Completed)
            .map_err(|_| "work_context_unavailable")?;
        state
            .store
            .release_work_media_leases(p, request)
            .map_err(|_| "work_media_unavailable")?;
        crate::work_continuation::schedule(state, p, &v);
    }
    Ok(())
}
pub(crate) fn reflect_failure(state: &StarlinkRouterState, request: &str, _code: &str) {
    let Some(p) = state
        .store
        .budget_continuation(request)
        .ok()
        .flatten()
        .and_then(|c| c.principal)
    else {
        return;
    };
    let video = state
        .store
        .budget_operation(request)
        .ok()
        .flatten()
        .and_then(|op| {
            op.steps
                .into_iter()
                .find(|s| s.kind == BudgetStepKind::Video)
        });
    let unknown = video.as_ref().is_some_and(|s| {
        s.dispatch_attempted
            && !matches!(
                s.execution_state,
                BudgetExecutionState::Failed
                    | BudgetExecutionState::Canceled
                    | BudgetExecutionState::Succeeded
            )
    });
    if state
        .store
        .work_version_for_request(&p, request)
        .ok()
        .flatten()
        .is_some()
    {
        let _ = state.store.set_work_version_state(
            &p,
            request,
            if unknown {
                WorkVersionState::Unknown
            } else {
                WorkVersionState::Failed
            },
        );
    }
    if !unknown {
        let _ = state.store.release_work_media_leases(&p, request);
    }
}

pub(crate) async fn submit_direct(
    state: Arc<StarlinkRouterState>,
    p: Principal,
    headers: HeaderMap,
    request: String,
    body: Value,
    fresh: bool,
) -> axum::response::Response {
    use axum::{http::StatusCode, response::IntoResponse, Json};
    let subscribed = {
        let _lock = state
            .seedance_results
            .admission
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Err(code) =
            crate::budget_continuation::save_with_headers(&state, &p, &request, &body, &headers)
        {
            if fresh {
                crate::budget_flow::finish_unadmitted_failure(&state,&request,&code);
            }
            return crate::budget_flow::fail(&code);
        }
        state.seedance_results.subscribe(&request)
    };
    let (_subscription, publisher) = match subscribed {
        Ok(v) => v,
        Err(code) => return crate::budget_flow::fail(code),
    };
    let mut accepted =
        json!({"task":{"id":request,"status":"queued"},"request_id":request,"core_replay":!fresh});
    let decorated = if state
        .store
        .work_version_for_request(&p, &request)
        .ok()
        .flatten()
        .is_some()
    {
        work_context::decorate_owned_request(&state, &p, &request, &mut accepted)
    } else {
        load_binding(&state,&p,&request,&body).and_then(|binding|{
        if let Some(work)=binding.work_id {
            let h=work_context::issue_handle(&state,&p,&work,binding.base_version_id.as_deref())?;
            work_context::decorate_reply(&mut accepted,&h,&json!({"work_id":work,"base_version_id":binding.base_version_id,"request_id":request}));
        }Ok(())
    })
    };
    if let Err(code) = decorated {
        return crate::budget_flow::fail(&code);
    }
    if let Some(publisher) = publisher {
        let rid = request.clone();
        tokio::spawn(async move {
            let owner = tokio::time::timeout(std::time::Duration::from_secs(45 * 60), async {
                loop {
                    match crate::budget_observer::Observer::try_acquire(
                        state.video_stream_observers.clone(),
                        rid.clone(),
                    ) {
                        Ok(owner) => return Ok(owner),
                        Err(crate::budget_observer::AcquireError::Busy) => {
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await
                        }
                        Err(crate::budget_observer::AcquireError::Capacity) => {
                            return Err("budget_preparation_busy".to_string())
                        }
                    }
                }
            })
            .await
            .unwrap_or_else(|_| Err("budget_execution_wait_timeout".into()));
            let outcome = match &owner {
                Ok(_) => {
                    crate::budget_flow::seedance_work(
                        state.clone(),
                        p,
                        rid.clone(),
                        body,
                        fresh,
                        Vec::new(),
                        false,
                    )
                    .await
                }
                Err(code) => Err(code.clone().into()),
            };
            if let Err(failure) = &outcome {
                let code=failure.code;
                let safe=crate::budget_errors::public_code(code).unwrap_or("work_execution_requires_attention");
                eprintln!("video work {rid} failed: {safe}");
                // Covers interrupted workers and replayed orphan preparations,
                // not only ordinary helper rejection. The store CAS refuses
                // termination if any execution/billing evidence exists.
                crate::budget_flow::abort_preparation_failure(&state,&rid,code);
                crate::budget_flow::finish_definite_failure(&state, &rid, code);
                reflect_failure(&state, &rid, code);
                if fresh {
                    crate::budget_flow::finish_unadmitted_failure(&state,&rid,code);
                }
            }
            publisher.complete(outcome);
        });
    }
    (StatusCode::ACCEPTED, Json(accepted)).into_response()
}
