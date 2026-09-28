//! V2 admission and result delivery, deliberately separate from legacy quotes.
use std::sync::Arc;
use aiwork_core::{BudgetAuthorization,BudgetStepInput,BudgetStepKind,BudgetStepView,Principal};
use axum::{http::StatusCode,response::{Response,IntoResponse},Json};
use serde::{Serialize,Deserialize};
use serde_json::{json,Value};
use crate::{state::StarlinkRouterState,bridge_client::BridgeClient};

#[derive(Serialize,Deserialize)]
struct Prepared {
    wire_version:u8,authorization:BudgetAuthorization,dispatch_token:String,evidence_level:String,prepared_at_ms:i64,revision:i64,
}
pub(crate) fn fail(code:&str)->Response {
    let status=match code {
        "key_concurrency_exceeded"|"video_download_busy"|"budget_preparation_busy"|"bridge_workers_busy"|"reference_upload_limited"=>StatusCode::TOO_MANY_REQUESTS,
        "quota_insufficient"=>StatusCode::PAYMENT_REQUIRED,
        "video_not_ready"|"budget_identity_conflict"=>StatusCode::CONFLICT,
        "invalid_budget_business_request"|"invalid_chat_image"|"reference_video_metadata_invalid"|
        "reference_video_format_unsupported"|"reference_asset_type_mismatch"|
        "reference_asset_unavailable"|"reference_video_budget_metadata_required"=>StatusCode::BAD_REQUEST,
        "budget_chat_input_too_large"=>StatusCode::PAYLOAD_TOO_LARGE,
        _=>StatusCode::SERVICE_UNAVAILABLE,
    };
    let mut response=(status,Json(json!({"error":{"type":"billing_error","code":code,"message":code}}))).into_response();
    if status==StatusCode::TOO_MANY_REQUESTS {response.headers_mut().insert("retry-after","1".parse().unwrap());}
    response
}
#[cfg(test)]
mod error_tests {
    #[test]
    fn busy_invalid_and_missing_policy_are_not_the_same_503() {
        use super::*;
        assert_eq!(fail("budget_preparation_busy").status(),StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(fail("invalid_budget_business_request").status(),StatusCode::BAD_REQUEST);
        assert_eq!(fail("budget_identity_conflict").status(),StatusCode::CONFLICT);
        assert_eq!(fail("budget_policy_unconfigured").status(),StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(fail("invalid_chat_image").status(),StatusCode::BAD_REQUEST);
        assert_eq!(fail("budget_chat_input_too_large").status(),StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(fail("reference_video_metadata_invalid").status(),StatusCode::BAD_REQUEST);
        assert_eq!(fail("reference_video_format_unsupported").status(),StatusCode::BAD_REQUEST);
        assert_eq!(fail("reference_asset_type_mismatch").status(),StatusCode::BAD_REQUEST);
    }
}
fn post(client:&BridgeClient,path:&str,body:&Value,request:&str)->Result<Value,String> {
    client.json_request("POST",path,&serde_json::to_vec(body).map_err(|_|"budget encoding failed")?,Some(request))
}
pub(crate) fn prepare_step(state:&StarlinkRouterState,principal:&Principal,parent:&str,request:&str,model:&str,body:Value,kind:BudgetStepKind)->Result<BudgetStepView,String> {
    let endpoint=if kind==BudgetStepKind::Video {"videos"} else {"chat"};
    let fingerprint=state.store.request_fingerprint_for_billing(request).map_err(|e|e.to_string())?;
    let client=state.bridge_client();
    crate::key_registry_sync::sync_now(state)?;
    let value=post(&client,"/internal/bridge/v2/budgets/prepare",&json!({"wire_version":2,"parent_request_id":parent,"request_id":request,
        "core_key_id":principal.key_id,"request_fingerprint":fingerprint,"endpoint":endpoint,"model":model,"step_kind":kind,"body":body}),request)?;
    let prepared:Prepared=serde_json::from_value(value).map_err(|_|"invalid prepared budget")?;
    let auth=&prepared.authorization;
    if prepared.wire_version!=2 || prepared.revision<=0 || prepared.prepared_at_ms<0 || prepared.dispatch_token.is_empty() || prepared.dispatch_token.len()>256
        || auth.parent_request_id!=parent || auth.request_id!=request || auth.core_key_id!=principal.key_id || auth.request_fingerprint!=fingerprint
        || auth.endpoint!=endpoint || auth.model!=model || !matches!(prepared.evidence_level.as_str(),"native_estimate"|"observed_actual"|"policy_only") {
        return Err("budget_binding_mismatch".into());
    }
    let input=BudgetStepInput {kind,authorization:auth.clone()};
    let admission=if state.store.budget_operation(parent).map_err(|e|e.to_string())?.is_some() {
        state.store.add_budget_step(parent,input).map(|_|())
    } else {state.store.begin_budget_operation(parent,input).map(|_|())};
    if let Err(error)=admission {
        // No dispatch was attempted. Cancel only the exact authenticated response
        // whose full request/Key binding was checked above.
        let _=post(&client,"/internal/bridge/v2/budgets/cancel",&serde_json::to_value(&prepared).map_err(|_|"budget encoding failed")?,request);
        return Err(match error {
            aiwork_core::CoreError::KeyConcurrencyExceeded{..}=>"key_concurrency_exceeded".into(),
            aiwork_core::CoreError::QuotaInsufficient{..}|aiwork_core::CoreError::QuotaPoolInsufficient{..}=>"quota_insufficient".into(),
            _=>error.to_string(),
        });
    }
    state.store.mark_budget_step_dispatched(request,&auth.budget_id).map_err(|e|e.to_string())?;
    // A failed transport after this point is unknown, not a refund and never an
    // automatic second paid send. Recovery uses the durable budget identity.
    let sent=post(&client,"/internal/bridge/v2/budgets/dispatch",&serde_json::to_value(&prepared).map_err(|_|"budget encoding failed")?,request);
    if let Ok(reply)=sent {
        if reply["wire_version"]!=2 {return Err("unsupported dispatch protocol".into());}
    }
    state.store.budget_operation(parent).map_err(|e|e.to_string())?.and_then(|op|op.steps.into_iter().find(|s|s.request_id==request)).ok_or("budget admission missing".into())
}
pub(crate) async fn submit_video(state:Arc<StarlinkRouterState>,principal:Principal,request:String,body:Value)->Response {
    let rid=request.clone();let worker=state.clone();
    let result=tokio::task::spawn_blocking(move ||prepare_step(&worker,&principal,&rid,&rid,"seedance",body,BudgetStepKind::Video)).await;
    match result {
        Ok(Ok(_))=>(StatusCode::ACCEPTED,Json(json!({"task":{"id":request,"status":"queued"},"request_id":request}))).into_response(),
        other=>{
            let code=match &other {Ok(Err(code))=>crate::budget_errors::public_code(code).unwrap_or("budget_admission_failed"),_=>"budget_admission_failed"};
            let response=fail(code);
            if matches!(state.store.budget_operation(&request),Ok(None)) {crate::user_routes::finish_failed_quote_request(&state,&request,response)} else {response}
        }
    }
}
pub(crate) fn owned_video_step(state:&StarlinkRouterState,principal:&Principal,request:&str)->Result<Option<BudgetStepView>,String> {
    let op=state.store.budget_operation(request).map_err(|e|e.to_string())?;
    Ok(op.filter(|o|o.api_key_id==principal.key_id).and_then(|o|o.steps.into_iter().find(|s|s.kind==BudgetStepKind::Video && s.request_id==request)))
}
pub(crate) fn read_result(state:&StarlinkRouterState,step:&BudgetStepView)->Result<Value,String> {
    let client=state.bridge_client();
    crate::budget_reconciler::sync_execution(state,&client,step)?;
    let mut result=crate::budget_reconciler::read(&client,step,"result")?;
    if result["status"]=="not_ready" {
        // A durable no-send receipt has no upstream output. Read its proven
        // disposition rather than telling clients to poll a nonexistent task.
        crate::budget_reconciler::sync_receipt(state,&client,step)?;
        let current=state.store.budget_operation(&step.parent_request_id).map_err(|e|e.to_string())?
            .and_then(|op|op.steps.into_iter().find(|s|s.request_id==step.request_id));
        if current.is_some_and(|s|matches!(s.execution_state,aiwork_core::BudgetExecutionState::Failed|aiwork_core::BudgetExecutionState::Canceled)
            && ((s.financial_state==aiwork_core::BudgetFinancialState::Settled && s.actual_credits.is_some_and(|v|v.as_microcredits()==0))
                || (!s.dispatch_attempted && s.financial_state==aiwork_core::BudgetFinancialState::Released))) {
            result["status"]=json!("failed_no_charge");
        }
    }
    Ok(result)
}
pub(crate) async fn video_status(state:Arc<StarlinkRouterState>,principal:Principal,request:String)->Response {
    let result=tokio::task::spawn_blocking(move ||->Result<Option<Value>,String> {
        let Some(step)=owned_video_step(&state,&principal,&request)? else {return Ok(None)};
        let result=read_result(&state,&step)?;
        let (status,content)=if result["status"]=="ready" {
            let status=result["result"]["status"].as_str().filter(|s|matches!(*s,"completed"|"failed")).ok_or("invalid video result")?;
            if result["result"]["id"].as_str()!=state.store.budget_operation(&request).map_err(|e|e.to_string())?.and_then(|op|op.steps.into_iter().find(|s|s.request_id==request)).and_then(|s|s.task_ref).as_deref() {return Err("video result task mismatch".into());}
            (status,Some(format!("{}/v1/videos/{request}/content",state.config.public_base_url.trim_end_matches('/'))))
        } else if result["status"]=="failed_no_charge" {("failed",None)}
        else if result["status"]=="not_ready" {("processing",None)} else {return Err("invalid result status".into())};
        Ok(Some(json!({"task":{"id":request,"status":status,"content_url":if status=="completed" {content} else {None}},"request_id":request})))
    }).await;
    match result {Ok(Ok(Some(value)))=>Json(value).into_response(),Ok(Ok(None))=>StatusCode::NOT_FOUND.into_response(),_=>fail("budget_result_unavailable")}
}

pub(crate) async fn wait_result(state:Arc<StarlinkRouterState>,step:BudgetStepView)->Result<Value,String> {
    let started=std::time::Instant::now();
    loop {
        let s=state.clone();let b=step.clone();
        let reply=tokio::task::spawn_blocking(move ||read_result(&s,&b)).await.map_err(|_|"result worker unavailable")??;
        if reply["status"]=="ready" {return Ok(reply["result"].clone());}
        if reply["status"]=="failed_no_charge" {return Err("budget_not_sent".into());}
        if started.elapsed()>std::time::Duration::from_secs(14*60) {return Err("budget_execution_wait_timeout".into());}
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}
fn completion(request:&str,text:&str)->Value {
    json!({"id":format!("chatcmpl-{request}"),"object":"chat.completion","created":chrono::Utc::now().timestamp(),"model":"seedance","request_id":request,
        "choices":[{"index":0,"message":{"role":"assistant","content":text},"finish_reason":"stop"}]})
}
fn inline_images(body:&Value)->Result<Vec<crate::assets::ParsedAssetUpload>,String> {
    let parts=body["messages"].as_array().and_then(|m|m.iter().rev().find(|v|v["role"]=="user"))
        .and_then(|m|m["content"].as_array());
    let mut images=Vec::new();let mut total=0usize;
    for part in parts.into_iter().flatten().filter(|p|p["type"]=="image_url") {
        let url=part.pointer("/image_url/url").and_then(Value::as_str).ok_or("invalid_reference_image")?;
        total=total.saturating_add(url.len());
        if total>6*1024*1024 || images.len()>=10 {return Err("reference_image_limit".into());}
        // Never fetch arbitrary public/private URLs from this server. Existing
        // uploaded Core asset IDs remain available through the owned-asset path.
        if !url.starts_with("data:image/") {return Err("reference_requires_inline_image_or_owned_asset".into());}
        let bytes=serde_json::to_vec(&json!({"filename":format!("reference-{}.image",images.len()),"data_base64":url})).map_err(|_|"invalid_reference_image")?;
        let parsed=crate::assets::parse_upload(&bytes).map_err(|_|"invalid_reference_image")?;
        if !parsed.declared_mime.as_deref().is_some_and(|m|m.starts_with("image/")) {return Err("invalid_reference_image".into());}
        images.push(parsed);
    }
    Ok(images)
}
async fn seedance_work(state:Arc<StarlinkRouterState>,principal:Principal,request:String,mut original:Value,fresh:bool,images:Vec<crate::assets::ParsedAssetUpload>,dispatch_only:bool)->Result<Value,String> {
    use aiwork_core::{BeginRequest,BeginRequestInput,BudgetExecutionState};
    let operation=state.store.budget_operation(&request).map_err(|e|e.to_string())?;
    if let Some(video)=operation.as_ref().and_then(|op|op.steps.iter().find(|s|s.kind==BudgetStepKind::Video)).cloned() {
        if dispatch_only {return Ok(json!({"request_id":request,"status":"already_dispatched"}));}
        let result=wait_result(state.clone(),video).await?;
        return completed_video(&state,&principal,&request,&result,&original).await;
    }
    let assist=if let Some(step)=operation.and_then(|op|op.steps.into_iter().find(|s|s.kind==BudgetStepKind::Assist)) {step} else {
        if !fresh {return Err("budget_preparation_requires_recovery".into());}
        let prompt=crate::user_routes::normalize_video_spec_text(&crate::user_routes::extract_seedance_prompt(&original).map_err(|_|"invalid_seedance_prompt")?);
        let model=state.config.seedance_assistant_model.clone();
        let body=json!({"model":model,"stream":false,"max_tokens":1024,"temperature":0.2,"messages":[
            {"role":"system","content":"你是视频请求调度和提示词整理助手。只输出严格 JSON。用户明确要求生成、制作视频时输出 {\"intent\":\"video\",\"prompt\":\"视频提示词\"}；普通问候、连接测试、非视频问题输出 {\"intent\":\"text\",\"text\":\"简短回答\"}。纠正规格中的全角数字、字母、冒号和多余空格，例如９：１６整理为9:16；保持用户指定的时长、分辨率、画幅、主体、动作和场景，不得擅自修改规格或新增剧情。不要把 hello 或测试连接转换成视频。不要虚构已经生成的视频。"},
            {"role":"user","content":prompt}]});
        let child=match state.store.begin_budget_assist_request(&request,BeginRequestInput {user_id:principal.user_id.clone(),api_key_id:principal.key_id.clone(),protocol:"openai".into(),endpoint:"chat".into(),model:model.clone(),idempotency_key:format!("budget-assist:{request}"),body:body.clone()}).map_err(|e|e.to_string())? {
            BeginRequest::Created(r)|BeginRequest::Existing(r)=>r.id,BeginRequest::Conflict=>return Err("assist_identity_conflict".into()),
        };
        let s=state.clone();let p=principal.clone();let parent=request.clone();
        match tokio::task::spawn_blocking(move ||prepare_step(&s,&p,&parent,&child,&model,body,BudgetStepKind::Assist)).await.map_err(|_|"assist worker failed")? {
            Ok(step)=>step,
            Err(error)=>{let _=state.store.abort_budget_preparation(&request,"assist_preparation_failed");return Err(error);},
        }
    };
    let result=wait_result(state.clone(),assist).await?;
    let content=result.pointer("/choices/0/message/content").and_then(Value::as_str).ok_or("assist_result_invalid")?;
    let decision:Value=serde_json::from_str(content.trim()).map_err(|_|"assist_result_invalid")?;
    if decision["intent"]=="text" {
        let text=decision["text"].as_str().filter(|s|!s.trim().is_empty() && s.len()<=16*1024).ok_or("assist_result_invalid")?;
        let s=state.clone();let p=principal.clone();let b=original.clone();
        let marker=tokio::task::spawn_blocking(move ||crate::reference_context::caption_reference(&s,&p,&images,&b)).await.map_err(|_|"reference_materialization_failed")??;
        let text=if let Some(marker)=marker {format!("{text}\n参考素材已接收，后续生成请求请保留此素材标记：{marker}")} else {text.to_string()};
        state.store.finish_budget_execution(&request,BudgetExecutionState::Succeeded).map_err(|e|e.to_string())?;
        return Ok(completion(&request,&text));
    }
    if decision["intent"]!="video" {return Err("assist_result_invalid".into());}
    // This retry passed Core's Key-scoped original-body fingerprint check. The
    // helper output was read by its persisted budget identity above; reuse it
    // without another helper send. Video keeps the same parent request identity,
    // so Core step admission and AI Work's consume CAS admit at most one send.
    // A parent already marked failed/canceled cannot be silently resurrected.
    if !fresh && state.store.budget_operation(&request).map_err(|e|e.to_string())?
        .is_none_or(|op|matches!(op.execution_state,BudgetExecutionState::Failed|BudgetExecutionState::Canceled|BudgetExecutionState::Succeeded)) {
        return Err("video_continuation_not_active".into());
    }
    let prompt=crate::user_routes::normalize_video_spec_text(decision["prompt"].as_str().filter(|s|!s.trim().is_empty() && s.len()<=12*1024).ok_or("assist_result_invalid")?);
    crate::user_routes::require_video_admission(&state,&principal,"seedance",&original).map_err(|_|"video_billing_paused")?;
    crate::user_routes::infer_video_parameters_from_prompt(&mut original);
    let mut body=json!({"model":"seedance","prompt":prompt});
    for field in ["duration","resolution","ratio","image_asset_ids","video_asset_ids","image_urls","video_urls","watermark"] {
        if let Some(value)=original.get(field) {body[field]=value.clone();}
    }
    crate::user_routes::materialize_bridge_assets(&state,&principal,&mut body,&request).await.map_err(|_|"reference_materialization_failed")?;
    if !images.is_empty() {
        let existing=body["image_asset_ids"].as_array().map_or(0,Vec::len);
        if existing+images.len()>10 {return Err("reference_image_limit".into());}
        let s=state.clone();let key=principal.key_id.clone();let rid=request.clone();
        let ids=tokio::task::spawn_blocking(move ||->Result<Vec<Value>,String> {
            let mut ids=Vec::new();
            for image in images {
                let _permit=s.asset_limiter.acquire(&key,image.bytes.len()).map_err(|_|"reference_upload_limited")?;
                let id=s.bridge_client().upload_asset(&image.filename,image.declared_mime.as_deref().ok_or("invalid_reference_image")?,&image.bytes,&rid)?;
                ids.push(json!(id));
            }
            Ok(ids)
        }).await.map_err(|_|"reference_upload_failed")??;
        if body["image_asset_ids"].is_null() {body["image_asset_ids"]=json!([]);}
        body["image_asset_ids"].as_array_mut().ok_or("invalid_image_asset_ids")?.extend(ids);
    }
    let s=state.clone();let p=principal.clone();let rid=request.clone();
    let step=tokio::task::spawn_blocking(move ||prepare_step(&s,&p,&rid,&rid,"seedance",body,BudgetStepKind::Video)).await.map_err(|_|"video worker failed")??;
    if dispatch_only {return Ok(json!({"request_id":request,"status":"dispatched"}));}
    let result=wait_result(state.clone(),step).await?;
    completed_video(&state,&principal,&request,&result,&original).await
}
async fn completed_video(state:&Arc<StarlinkRouterState>,principal:&Principal,request:&str,result:&Value,body:&Value)->Result<Value,String> {
    if result["status"]!="completed" {return Err("video_execution_failed".into());}
    crate::video_delivery::completion(state,principal,request,body).await
}

pub(crate) async fn video_content(state:Arc<StarlinkRouterState>,principal:Principal,request:String)->Response {
    match owned_video_step(&state,&principal,&request) {
        Ok(Some(_))=>{},Ok(None)=>return StatusCode::NOT_FOUND.into_response(),Err(_)=>return fail("video_content_unavailable"),
    }
    let permit=match state.budget_download_slots.clone().try_acquire_owned() {
        Ok(permit)=>permit,Err(_)=>return fail("video_download_busy"),
    };
    let s=state.clone();
    let result=tokio::task::spawn_blocking(move ||->Result<Option<_>,String> {
        let Some(step)=owned_video_step(&s,&principal,&request)? else {return Ok(None)};
        let value=read_result(&s,&step)?;
        if value["status"]!="ready" || value["result"]["status"]!="completed" {return Err("video_not_ready".into());}
        s.bridge_client().budget_content(&step).map(|upstream|Some((upstream,permit)))
    }).await;
    let (upstream,permit)=match result {Ok(Ok(Some((value,permit)))) if value.status==200=>(value,permit),Ok(Ok(None))=>return StatusCode::NOT_FOUND.into_response(),
        Ok(Ok(Some((value,_)))) if value.status==429=>return fail("video_download_busy"),
        Ok(Err(code)) if code=="video_not_ready"=>return fail(&code),_=>return fail("video_content_unavailable")};
    let (send,receive)=tokio::sync::mpsc::channel(2);
    tokio::task::spawn_blocking(move || {
        let _permit=permit; // Held until EOF/error/disconnect, not just headers.
        use std::io::Read;let mut reader=upstream.body;let mut buffer=[0u8;64*1024];let mut total=0u64;
        loop {match reader.read(&mut buffer) {
            Ok(0)=>break,
            Ok(n)=>{total+=n as u64;if total>4*1024*1024*1024 {let _=send.blocking_send(Err(std::io::Error::other("video exceeds limit")));break;}
                if send.blocking_send(Ok(axum::body::Bytes::copy_from_slice(&buffer[..n]))).is_err() {break;}},
            Err(e)=>{let _=send.blocking_send(Err(e));break;},
        }}
    });
    Response::builder().header("content-type","video/mp4").header("content-disposition","attachment; filename=video.mp4").header("cache-control","private, no-store")
        .body(axum::body::Body::from_stream(crate::user_routes::BridgeBodyStream(receive))).unwrap_or_else(|_|fail("download_response_failed"))
}

pub(crate) async fn seedance_chat(state:Arc<StarlinkRouterState>,principal:Principal,headers:axum::http::HeaderMap,mut body:Value)->Response {
    use aiwork_core::{BeginRequest,BeginRequestInput};
    if let Some(response)=crate::video_delivery::follow_up(&state,&principal,&body).await {return response;}
    let s=state.clone();let p=principal.clone();
    body=match tokio::task::spawn_blocking(move ||->Result<Value,&'static str> {
        crate::reference_context::recover(&s,&p,&mut body)?;Ok(body)
    }).await {
        Ok(Ok(recovered))=>recovered,
        Ok(Err(code))=>return (StatusCode::BAD_REQUEST,Json(json!({"error":{"code":code,"message":if code=="reference_image_missing" {"本次请求没有携带参考图片或有效素材标记，请重新附加参考图；未提交视频"} else {"参考素材上下文无效或已过期，请重新附加参考图"}}}))).into_response(),
        Err(_)=>return fail("reference_worker_unavailable"),
    };
    if crate::user_routes::extract_seedance_prompt(&body).is_err() {return (StatusCode::BAD_REQUEST,Json(json!({"error":{"code":"seedance_prompt_missing"}}))).into_response();}
    let images=match inline_images(&body) {Ok(images)=>images,Err(code)=>return (StatusCode::BAD_REQUEST,Json(json!({"error":{"code":code}}))).into_response()};
    if !images.is_empty() && !principal.scopes.contains("assets:write") && !principal.scopes.contains("admin:*") {
        return (StatusCode::FORBIDDEN,Json(json!({"error":{"code":"insufficient_scope","message":"参考图上传需要素材上传权限"}}))).into_response();
    }
    let supplied=headers.get("idempotency-key").and_then(|v|v.to_str().ok()).filter(|v|!v.trim().is_empty());
    let input=BeginRequestInput {user_id:principal.user_id.clone(),api_key_id:principal.key_id.clone(),protocol:"openai".into(),endpoint:"videos".into(),model:"seedance".into(),idempotency_key:supplied.unwrap_or_default().into(),body:body.clone()};
    let begun=if supplied.is_some() {state.store.begin_billed_request(input)} else {state.store.begin_implicit_billed_video_request(input)};
    let (request,fresh)=match begun {Ok(BeginRequest::Created(r))=>(r.id,true),Ok(BeginRequest::Existing(r))=>(r.id,false),Ok(BeginRequest::Conflict)=>return StatusCode::CONFLICT.into_response(),Err(_)=>return fail("budget_request_rejected")};
    if crate::budget_continuation::save(&state,&principal,&request,&body).is_err() {return fail("budget_checkpoint_unavailable");}
    let Some(observer)=crate::budget_observer::Observer::acquire(state.video_stream_observers.clone(),request.clone()) else {
        return (StatusCode::TOO_MANY_REQUESTS,Json(json!({"error":{"code":"budget_observer_busy"}}))).into_response();
    };
    let stream=body["stream"].as_bool().unwrap_or(false);
    let (done,receive)=tokio::sync::oneshot::channel();
    let rid=request.clone();
    tokio::spawn(async move {
        let _observer=observer;
        let outcome=seedance_work(state.clone(),principal,rid.clone(),body,fresh,images,false).await;
        if outcome.is_err() {
            // Only terminal steps may release execution. Unknown paid execution
            // is retained; its budget and receipt remain recoverable.
            finish_definite_failure(&state,&rid,outcome.as_ref().err().unwrap());
        }
        let _=done.send(outcome);
    });
    if !stream {
        return match receive.await {
            Ok(Ok(value))=>Json(value).into_response(),
            Ok(Err(code))=>fail(crate::budget_errors::public_code(&code).unwrap_or("seedance_budget_execution_failed")),
            _=>fail("seedance_budget_execution_failed"),
        };
    }
    let (send,recv)=tokio::sync::mpsc::channel(8);
    tokio::spawn(async move {
        let initial=json!({"id":format!("chatcmpl-{request}"),"object":"chat.completion.chunk","model":"seedance",
            "created":chrono::Utc::now().timestamp(),"request_id":request,
            "choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]});
        if send.send(Ok(axum::body::Bytes::from(format!("data: {initial}\n\n")))).await.is_err() {return;}
        // Heartbeats keep provider validation and long video generation alive;
        // this is Seedance orchestration, not a replacement for normal chat SSE.
        let mut receive=receive;
        let mut tick=tokio::time::interval(std::time::Duration::from_secs(10));
        loop {tokio::select! {
            _=tick.tick()=>{if send.send(Ok(axum::body::Bytes::from_static(crate::seedance_sse::keep_alive()))).await.is_err() {break;}},
            result=&mut receive=>{
                let bytes=match result {
                    Ok(Ok(value))=>crate::video_delivery::sse_completion(&value),
                    failure=>{
                        let code=match &failure {Ok(Err(code))=>crate::budget_errors::public_code(code).unwrap_or("seedance_budget_execution_failed"),_=>"seedance_budget_execution_failed"};
                        crate::seedance_sse::encode_event(&request,crate::seedance_sse::VideoStreamEvent::Failed {code:code.into(),request_id:request.clone()})
                    },
                };
                let _=send.send(Ok(axum::body::Bytes::from(bytes))).await;break;
            }
        }}
    });
    let mut response=Response::new(axum::body::Body::from_stream(crate::user_routes::BridgeBodyStream(recv)));
    response.headers_mut().insert("content-type","text/event-stream; charset=utf-8".parse().unwrap());
    response.headers_mut().insert("cache-control","no-cache, no-transform".parse().unwrap());
    response.headers_mut().insert("x-accel-buffering","no".parse().unwrap());response
}

pub(crate) fn finish_definite_failure(state:&StarlinkRouterState,request:&str,code:&str) {
    // Transport/storage/read errors are not evidence the workflow has ended.
    if matches!(code,"assist_result_invalid"|"video_execution_failed"|"budget_not_sent"|"video_billing_paused"|
        "quota_insufficient"|"budget_policy_unconfigured"|"budget_policy_expired"|"budget_policy_invalid"|
        "reference_video_budget_metadata_required"|"invalid_budget_business_request"|"reference_image_limit"|
        "invalid_image_asset_ids"|"invalid_reference_image"|"video_continuation_not_authorized") {
        let _=state.store.finish_budget_execution(request,aiwork_core::BudgetExecutionState::Failed);
    }
}
pub(crate) async fn resume_checkpoint(state:Arc<StarlinkRouterState>,checkpoint:aiwork_core::BudgetContinuation)->Result<(),String> {
    let context=checkpoint.encryption_context();
    let principal=checkpoint.principal.ok_or("video_continuation_not_authorized")?;
    aiwork_core::require_scope(&principal,"videos:submit").map_err(|_|"video_continuation_not_authorized")?;
    let text=zeroize::Zeroizing::new(state.key_vault.decrypt(&context,checkpoint.key_version,&checkpoint.ciphertext).map_err(|_|"budget_checkpoint_invalid")?);
    let body:Value=serde_json::from_str(&text).map_err(|_|"budget_checkpoint_invalid")?;
    // Re-check the original request hash and current user/Key status before any
    // paid continuation. This call cannot replace the immutable checkpoint.
    state.store.save_budget_continuation(&principal,&checkpoint.request_id,&body,checkpoint.key_version,&checkpoint.ciphertext).map_err(|_|"budget_checkpoint_invalid")?;
    let images=inline_images(&body)?;
    if !images.is_empty() {aiwork_core::require_scope(&principal,"assets:write").map_err(|_|"video_continuation_not_authorized")?;}
    seedance_work(state,principal,checkpoint.request_id,body,false,images,true).await.map(|_|())
}
