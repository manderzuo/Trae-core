//! One new paid video attempt, explicitly separate from original result/billing recovery.
use std::sync::Arc;
use aiwork_core::{BeginRequest,BeginRequestInput,Principal,VideoWorkSnapshot,VideoWorkVersion,WorkVersionState,EncryptedWorkSnapshot};
use axum::{body::Bytes,extract::{State,Path},http::{HeaderMap,StatusCode},response::{Response,IntoResponse},Extension};
use serde::Deserialize;
use serde_json::{json,Value};
use sha2::{Digest,Sha256};
use crate::{state::StarlinkRouterState,work_context,work_execution::ContextBinding};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RetryRequest {max_retries:u8}

fn retry_input(root:&str,snapshot:&VideoWorkSnapshot)->Value {
    json!({"model":"seedance","prompt":snapshot.effective_prompt,"duration":snapshot.duration,
        "ratio":snapshot.ratio,"resolution":snapshot.resolution,"watermark":snapshot.watermark,"retry_source_request_id":root})
}
/// Recover only the identity of an already-created child after an HTTP reply
/// was lost. This route never prepares, dispatches or restarts any worker.
pub async fn lookup(State(state):State<Arc<StarlinkRouterState>>,Path(root):Path<String>,Extension(p):Extension<Principal>)->Response {
    let found=(||->Result<Option<String>,String>{
        let Some(version)=state.store.work_version_for_request(&p,&root).map_err(|_|"work_context_unavailable")? else{return Ok(None)};
        let snapshot=work_context::read_snapshot(&state,&p,&version)?;
        let found=state.store.lookup_idempotent_request(&p.user_id,&p.key_id,"videos","seedance",&retry_input(&root,&snapshot),&format!("reference-retry-once:{root}")).map_err(|_|"work_context_unavailable")?;
        match found{Some(BeginRequest::Existing(request))=>Ok(Some(request.id)),Some(_)=>Err("budget_identity_conflict".into()),None=>Ok(None)}
    })();
    match found{
        Ok(Some(id))=>axum::Json(json!({"request_id":id,"task":{"id":id,"status":"queued"},"retry_of_request_id":root,"retry_attempt":1})).into_response(),
        Ok(None)=>StatusCode::NOT_FOUND.into_response(),Err(code)=>crate::budget_flow::fail(&code),
    }
}

fn source(state:&StarlinkRouterState,p:&Principal,request:&str)->Result<(VideoWorkVersion,VideoWorkSnapshot),String> {
    let version=state.store.work_version_for_request(p,request).map_err(|_|"work_context_unavailable")?.ok_or("work_parent_unavailable")?;
    let snapshot=work_context::read_snapshot(state,p,&version)?;
    if snapshot.retry_source_request_id.is_some(){return Err("video_retry_limit_reached".into());}
    let step=crate::budget_flow::owned_video_step(state,p,request)?.ok_or("video_retry_not_eligible")?;
    let result=crate::budget_flow::read_result(state,&step)?;
    if result["status"]!="ready" || result["result"]["status"]!="failed"
        || crate::seedance_feedback::Failure::video(&result["result"]).code!="reference_resource_download_failed" {
        return Err("video_retry_not_eligible".into());
    }
    if snapshot.dispatch_body.is_none() || snapshot.user_media_ids.is_empty() && snapshot.tail_frame_media_id.is_none() && snapshot.continuation_video_media_id.is_none() {
        return Err("video_retry_not_eligible".into());
    }
    Ok((version,snapshot))
}

pub async fn retry(State(state):State<Arc<StarlinkRouterState>>,Path(root):Path<String>,Extension(p):Extension<Principal>,body:Bytes)->Response {
    if aiwork_core::require_scope(&p,"videos:submit").is_err(){return StatusCode::FORBIDDEN.into_response();}
    if !state.config.budget_billing_v2 || !state.config.work_context_for_key(&p.key_id){return crate::budget_flow::fail("video_retry_not_eligible");}
    if !matches!(serde_json::from_slice::<RetryRequest>(&body),Ok(i)if i.max_retries==1){return crate::budget_flow::fail("video_retry_invalid");}
    if !matches!(state.store.work_version_for_request(&p,&root),Ok(Some(_))){
        return if matches!(state.store.active_principal_for_request(&root),Ok(Some(owner))if owner.key_id==p.key_id&&owner.user_id==p.user_id) {
            crate::budget_flow::fail("video_retry_not_eligible")
        }else{StatusCode::NOT_FOUND.into_response()};
    }
    let worker=state.clone();let owner=p.clone();let original=root.clone();
    let prepared=tokio::task::spawn_blocking(move ||->Result<(String,Value,bool),String>{
        let (version,snapshot)=source(&worker,&owner,&original)?;
        let input=retry_input(&original,&snapshot);
        crate::user_routes::require_video_admission(&worker,&owner,"seedance",&input).map_err(|_|"video_billing_paused")?;
        // All callers, regardless of their supplied idempotency header, receive
        // the same durable child. Admission/checkpoint write is process-serialized;
        // SQLite's unique idempotency binding also survives restarts.
        let _lock=worker.seedance_results.admission.lock().map_err(|_|"work_context_unavailable")?;
        let begun=worker.store.begin_billed_request(BeginRequestInput{user_id:owner.user_id.clone(),api_key_id:owner.key_id.clone(),
            protocol:"openai".into(),endpoint:"videos".into(),model:"seedance".into(),idempotency_key:format!("reference-retry-once:{original}"),body:input.clone()}).map_err(|_|"work_context_unavailable")?;
        let (request,fresh)=match begun{BeginRequest::Created(r)=>(r,true),BeginRequest::Existing(r)=>(r,false),BeginRequest::Conflict=>return Err("budget_identity_conflict".into())};
        if worker.store.budget_continuation(&request.id).map_err(|_|"budget_checkpoint_invalid")?.is_none() {
            let fingerprint=worker.store.request_fingerprint_for_billing(&request.id).map_err(|_|"budget_checkpoint_invalid")?;
            let context=format!("budget-continuation-v1:{}:{}:{fingerprint}",request.id,owner.key_id);
            let binding=ContextBinding{work_id:Some(version.work_id),base_version_id:snapshot.parent_version_id,
                association:format!("retry:{original}"),clarification:None,retry_source_request_id:Some(original.clone())};
            let raw=zeroize::Zeroizing::new(json!({"format":"aiwork-checkpoint-v2","body":input,"binding":binding,"retained_inputs":{},"retained_input_origin":"current"}).to_string());
            let sealed=worker.key_vault.encrypt(&context,&raw).map_err(|_|"work_snapshot_unavailable")?;
            worker.store.save_budget_continuation(&owner,&request.id,&input,sealed.key_version,&sealed.ciphertext).map_err(|_|"budget_checkpoint_invalid")?;
        }
        Ok((request.id,input,fresh))
    }).await;
    match prepared {
        Ok(Ok((request,input,fresh)))=>{
            // A repeated recovery request must not replace the child's original
            // terminal error or run preparation again after it has ended.
            if !fresh && state.store.budget_operation(&request).ok().flatten().is_some_and(|op|matches!(op.execution_state,
                aiwork_core::BudgetExecutionState::Failed|aiwork_core::BudgetExecutionState::Canceled|aiwork_core::BudgetExecutionState::Succeeded)) {
                return (StatusCode::ACCEPTED,axum::Json(json!({"request_id":request,"task":{"id":request,"status":"queued"},"core_replay":true}))).into_response();
            }
            crate::work_execution::submit_direct(state,p,HeaderMap::new(),request,input,fresh).await
        },
        Ok(Err(code))=>crate::budget_flow::fail(crate::budget_errors::public_code(&code).unwrap_or("work_context_unavailable")),
        Err(_)=>crate::budget_flow::fail("work_context_unavailable"),
    }
}

pub(crate) fn bind_retry_version(state:&StarlinkRouterState,p:&Principal,request:&str,root:&str)->Result<VideoWorkVersion,String> {
    let (version,mut snapshot)=source(state,p,root)?;
    let now=chrono::Utc::now().timestamp_millis();
    let mut wire=snapshot.dispatch_body.clone().ok_or("work_snapshot_invalid")?;
    let old_images=wire["image_asset_ids"].as_array().map_or(0,Vec::len);
    let old_videos=wire["video_asset_ids"].as_array().map_or(0,Vec::len);
    let mut images=Vec::new();let mut videos=Vec::new();
    for id in snapshot.continuation_video_media_id.iter().chain(snapshot.user_media_ids.iter()).chain(snapshot.tail_frame_media_id.iter()) {
        let media=state.store.owned_work_media(p,id).map_err(|_|"work_media_unavailable")?.filter(|m|m.work_id==version.work_id).ok_or("reference_asset_missing")?;
        if snapshot.continuation_video_media_id.is_some() && media.kind=="video" && Some(id)!=snapshot.continuation_video_media_id.as_ref(){continue;}
        if snapshot.tail_frame_media_id.is_some() && media.kind=="video"{continue;}
        if media.expires_at_ms<=now && !state.store.work_media_has_lease(p,id,None).map_err(|_|"work_media_unavailable")?{return Err("reference_asset_expired".into());}
        state.store.acquire_work_media_lease(p,id,request).map_err(|_|"work_media_unavailable")?;
        let asset=crate::work_media::materialize(state,p,&media,now)?;
        let owned=crate::assets::read_reference(state,p,&asset.id)?;
        let _permit=state.asset_limiter.acquire(&p.key_id,owned.bytes.len()).map_err(|_|"reference_upload_limited")?;
        // New asset identity forces fresh native upload/resolve at preparation.
        let fresh=state.bridge_client().upload_asset(&owned.record.filename,&owned.record.mime_type,&owned.bytes,request).map_err(|_|"reference_materialization_failed")?;
        if owned.record.mime_type.starts_with("image/"){images.push(fresh);}else{videos.push(fresh);}
    }
    if images.len()!=old_images || videos.len()!=old_videos || images.len()+videos.len()==0 {return Err("work_snapshot_invalid".into());}
    let object=wire.as_object_mut().ok_or("work_snapshot_invalid")?;
    object.remove("image_asset_ids");object.remove("video_asset_ids");
    if !images.is_empty(){wire["image_asset_ids"]=json!(images);}
    if !videos.is_empty(){wire["video_asset_ids"]=json!(videos);}
    snapshot.dispatch_body=Some(wire);snapshot.source_request_id=Some(request.into());snapshot.retry_source_request_id=Some(root.into());
    let raw=zeroize::Zeroizing::new(serde_json::to_string(&snapshot).map_err(|_|"work_snapshot_invalid")?);
    let sealed=state.key_vault.encrypt(&aiwork_core::work_snapshot_context(&p.key_id,&version.work_id,request),&raw).map_err(|_|"work_snapshot_unavailable")?;
    state.store.bind_work_version(p,&version.work_id,snapshot.parent_version_id.as_deref(),request,version.action,
        &EncryptedWorkSnapshot{key_version:sealed.key_version,ciphertext:sealed.ciphertext,snapshot_sha256:format!("{:x}",Sha256::digest(raw.as_bytes()))})
        .map(|m|m.version).map_err(|_|"work_snapshot_unavailable".into())
}
