//! Read-only recovery of the original paid identity; this module never admits
//! a request, reserves credits, or dispatches a provider operation.
use std::sync::Arc;
use aiwork_core::{BeginRequest,Principal,BudgetStepKind};
use axum::{body::Bytes,extract::{State,Extension},http::{HeaderMap,StatusCode},response::{Response,IntoResponse},Json};
use serde_json::{json,Value};
use crate::state::StarlinkRouterState;

async fn lookup(state:Arc<StarlinkRouterState>,p:Principal,headers:HeaderMap,bytes:Bytes,video:bool)->Response {
    let scope=if video {"videos:query"}else{"chat:invoke"};
    if aiwork_core::require_scope(&p,scope).is_err(){return StatusCode::FORBIDDEN.into_response();}
    let Some(key)=headers.get("idempotency-key").and_then(|v|v.to_str().ok()).filter(|s|!s.is_empty()&&s.len()<=256)else{return StatusCode::BAD_REQUEST.into_response()};
    let body:Value=match serde_json::from_slice(&bytes){Ok(v)=>v,Err(_)=>return StatusCode::BAD_REQUEST.into_response()};
    let model=body["model"].as_str().unwrap_or(if video{"seedance"}else{""});
    let endpoint=if video{"videos"}else{"chat"};
    let request=match state.store.lookup_idempotent_request(&p.user_id,&p.key_id,endpoint,model,&body,key){
        Ok(Some(BeginRequest::Existing(r)))=>r.id,
        Ok(Some(BeginRequest::Conflict))=>return (StatusCode::CONFLICT,Json(json!({"error":{"code":"budget_identity_conflict"}}))).into_response(),
        Ok(None)=>return (StatusCode::NOT_FOUND,Json(json!({"error":{"code":"idempotency_not_found"}}))).into_response(),
        _=>return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let mut response=if video {crate::budget_flow::video_status(state,p,request).await}else{
        let result=tokio::task::spawn_blocking(move||->Result<Value,String>{
            let Some(op)=state.store.budget_operation(&request).map_err(|_|"recovery_unavailable")?else{return Ok(json!({"request_id":request,"status":"not_ready","billing_state":"pending"}))};
            if op.api_key_id!=p.key_id{return Err("recovery_identity_mismatch".into());}
            let step=op.steps.iter().find(|s|s.kind==BudgetStepKind::Chat && s.request_id==request).ok_or("recovery_unavailable")?;
            let reply=crate::budget_flow::read_result(&state,step)?;
            crate::budget_reconciler::sync_receipt(&state,&state.bridge_client(),step)?;
            let current=state.store.budget_operation(&request).map_err(|_|"recovery_unavailable")?.ok_or("recovery_unavailable")?;
            let actual=current.steps.iter().map(|s|s.actual_credits.map(|c|c.as_microcredits())).collect::<Option<Vec<_>>>()
                .and_then(|v|v.into_iter().try_fold(0i64,|sum,v|sum.checked_add(v)));
            Ok(json!({"request_id":request,"status":reply["status"],"result":reply.get("result"),"billing_state":if actual.is_some(){"settled"}else{"pending"},"actual_microcredits":actual}))
        }).await;
        match result {Ok(Ok(value))=>Json(value).into_response(),_=>StatusCode::SERVICE_UNAVAILABLE.into_response()}
    };
    response.headers_mut().insert("cache-control","private, no-store".parse().unwrap());response
}
pub(crate) async fn video(State(state):State<Arc<StarlinkRouterState>>,Extension(p):Extension<Principal>,headers:HeaderMap,body:Bytes)->Response {lookup(state,p,headers,body,true).await}
pub(crate) async fn chat(State(state):State<Arc<StarlinkRouterState>>,Extension(p):Extension<Principal>,headers:HeaderMap,body:Bytes)->Response {lookup(state,p,headers,body,false).await}
