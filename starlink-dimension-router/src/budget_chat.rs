//! Ordinary Chat uses the same bounded step ledger, never legacy whole-Key
//! holds. Output observers are independent of upstream execution and billing.
use std::{sync::Arc,time::{Duration,Instant}};
use aiwork_core::{Principal,BeginRequest,BeginRequestInput,BudgetStepKind,BudgetStepView};
use axum::{http::{HeaderMap,StatusCode},response::{Response,IntoResponse},body::{Body,Bytes},Json};
use serde_json::{json,Value};
use crate::{state::StarlinkRouterState,budget_flow::{fail,prepare_step,read_result,wait_result},budget_observer::Observer};

// Keep exclusive ownership until the blocking preparation worker has stopped,
// even when its HTTP caller disconnects. Cleanup cannot race a new observer or
// refund an admitted/possibly dispatched execution.
struct AdmissionOwner {state:Arc<StarlinkRouterState>,request:String,_observer:Observer}
impl Drop for AdmissionOwner {
    fn drop(&mut self) {
        if let Err(error)=self.state.store.finish_unadmitted_request(&self.request) {
            eprintln!("chat pre-admission cleanup failed for {}: {error}",self.request);
        }
    }
}

pub(crate) async fn chat(state:Arc<StarlinkRouterState>,principal:Principal,headers:HeaderMap,original:Value,model:String)->Response {
    if !original["messages"].as_array().is_some_and(|v|!v.is_empty()) {return fail("invalid_budget_business_request");}
    let key=headers.get("idempotency-key").and_then(|v|v.to_str().ok()).filter(|v|!v.trim().is_empty()).map(str::to_owned)
        .unwrap_or_else(||format!("chat-{:032x}",rand::random::<u128>()));
    let request=match state.store.begin_billed_request(BeginRequestInput {user_id:principal.user_id.clone(),api_key_id:principal.key_id.clone(),protocol:"openai".into(),endpoint:"chat".into(),model:model.clone(),idempotency_key:key,body:original.clone()}) {
        Ok(BeginRequest::Created(r))|Ok(BeginRequest::Existing(r))=>r.id,
        Ok(BeginRequest::Conflict)=>return fail("budget_identity_conflict"),Err(_)=>return fail("budget_request_rejected"),
    };
    let Some(observer)=Observer::acquire_with_capacity_rejection(state.video_stream_observers.clone(),request.clone(),|| {
        if let Err(error)=state.store.finish_unadmitted_request(&request) {eprintln!("chat capacity cleanup failed for {request}: {error}");}
    }) else {return StatusCode::TOO_MANY_REQUESTS.into_response()};
    let mut observer=AdmissionOwner {state:state.clone(),request:request.clone(),_observer:observer};
    // Identity and original-body fingerprint were checked above. A durable
    // budget is already bound to its normalized input; replaying its output
    // must not depend on an input asset that may since have expired.
    let existing=match state.store.budget_operation(&request) {Ok(v)=>v,Err(_)=>return fail("chat_budget_admission_failed")};
    let step=if let Some(op)=existing {
        if op.api_key_id!=principal.key_id {return fail("budget_identity_conflict");}
        match op.steps.into_iter().find(|p|p.kind==BudgetStepKind::Chat && p.request_id==request) {Some(s)=>s,None=>return fail("chat_budget_missing")}
    } else {
        let mut body=original.clone();
        if let Err(r)=crate::user_routes::materialize_text_asset_ids(&state,&principal,&mut body) {return r;}
        if let Err(r)=crate::user_routes::validate_vision_data_urls(&body) {return r;}
        let s=state.clone();let rid=request.clone();let m=model.clone();
        match tokio::task::spawn_blocking(move || {
            let owner=observer;
            let result=prepare_step(&s,&principal,&rid,&rid,&m,body,BudgetStepKind::Chat);
            (owner,result)
        }).await {
            Ok((owner,result))=>{
                observer=owner;
                match result {Ok(step)=>step,Err(e)=>return fail(crate::budget_errors::public_code(&e).unwrap_or("chat_budget_admission_failed"))}
            },
            _=>return fail("chat_budget_admission_failed")
        }
    };
    if !original["stream"].as_bool().unwrap_or(false) {
        let _observer=observer;
        return match wait_result(state,step).await {Ok(mut v)=>{v["request_id"]=json!(request);Json(v).into_response()},Err(_)=>fail("chat_budget_result_unavailable")};
    }
    let (send,recv)=tokio::sync::mpsc::channel::<Result<Bytes,std::io::Error>>(8);
    tokio::spawn(async move {
        let _observer=observer;
        let result=deliver(&state,&step,&request,&model,&send).await;
        if let Err(code)=result {
            let _=send_frame(&send,format!("data: {}\n\ndata: [DONE]\n\n",json!({"error":{"code":code,"message":code,"type":"api_error"},"request_id":request}))).await;
        }
    });
    Response::builder().header("content-type","text/event-stream").header("cache-control","no-cache").header("x-accel-buffering","no")
        .body(Body::from_stream(crate::user_routes::BridgeBodyStream(recv))).unwrap_or_else(|_|fail("chat_stream_unavailable"))
}
async fn send_frame(send:&tokio::sync::mpsc::Sender<Result<Bytes,std::io::Error>>,frame:String)->Result<(),&'static str> {
    match tokio::time::timeout(Duration::from_secs(5),send.send(Ok(Bytes::from(frame)))).await {Ok(Ok(()))=>Ok(()),_=>Err("chat_observer_disconnected")}
}
fn frame(request:&str,model:&str,delta:Value,finish:Value,usage:Option<Value>)->String {
    let mut value=json!({"id":format!("chatcmpl-{request}"),"object":"chat.completion.chunk","created":chrono::Utc::now().timestamp(),"request_id":request,
        "model":model,"choices":[{"index":0,"delta":delta,"finish_reason":finish}]});
    if let Some(u)=usage {value["usage"]=u;}format!("data: {value}\n\n")
}
fn replay_delta(message:&Value)->Result<Value,&'static str> {
    let mut delta=message.clone();let obj=delta.as_object_mut().ok_or("chat_result_invalid")?;obj.remove("role");
    if let Some(calls)=obj.get_mut("tool_calls") {
        let calls=calls.as_array_mut().ok_or("chat_result_invalid")?;
        for (index,call) in calls.iter_mut().enumerate() {call.as_object_mut().ok_or("chat_result_invalid")?.insert("index".into(),json!(index));}
    }
    Ok(delta)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn persistent_tool_result_replay_uses_stream_slot_indices() {
        let original=json!({"role":"assistant","content":"","tool_calls":[{"id":"a","type":"function","function":{"name":"download","arguments":"{}"}},{"id":"b","type":"function","function":{"name":"check","arguments":"{}"}}]});
        let delta=replay_delta(&original).unwrap();assert_eq!(delta["tool_calls"][0]["index"],0);assert_eq!(delta["tool_calls"][1]["index"],1);
        assert_eq!(delta["tool_calls"][1]["function"]["name"],"check");assert!(original["tool_calls"][0].get("index").is_none());
    }
}
async fn deliver(state:&Arc<StarlinkRouterState>,step:&BudgetStepView,request:&str,model:&str,send:&tokio::sync::mpsc::Sender<Result<Bytes,std::io::Error>>)->Result<(),&'static str> {
    send_frame(send,frame(request,model,json!({"role":"assistant"}),Value::Null,None)).await?;
    let mut after=0u64;let start=Instant::now();let mut heartbeat=Instant::now();
    loop {
        if start.elapsed()>Duration::from_secs(14*60) {return Err("chat_execution_wait_timeout");}
        if send.is_closed() {return Err("chat_observer_disconnected");}
        let s=state.clone();let b=step.clone();
        let page=tokio::task::spawn_blocking(move || {
            let client=s.bridge_client();let path=format!("{}&after={after}",crate::budget_reconciler::request_path(&b,"chunks"));
            let v=client.json_request("GET",&path,&[],Some(&b.request_id))?;
            if v["wire_version"]!=2 {return Err("invalid stream protocol".into());}
            crate::budget_reconciler::identity(&v,&b)?;Ok::<_,String>(v)
        }).await.map_err(|_|"chat_stream_unavailable")?.map_err(|_|"chat_stream_unavailable")?;
        let ready_to_read=if page["status"]=="available" {
            let deltas=page["chunks"].as_array().filter(|a|a.len()<=32).ok_or("chat_stream_invalid_page")?;
            let next=page["next"].as_u64().ok_or("chat_stream_invalid_page")?;
            if next!=after.checked_add(deltas.len() as u64).ok_or("chat_stream_invalid_page")? {return Err("chat_stream_invalid_page");}
            for delta in deltas {if !delta.is_object() {return Err("chat_stream_invalid_page");}send_frame(send,frame(request,model,delta.clone(),Value::Null,None)).await?;}
            after=next;
            if deltas.len()==32 {continue;}
            page["finished"].as_bool().ok_or("chat_stream_invalid_page")?
        } else if page["status"]=="unavailable" {
            if after!=0 {return Err("chat_stream_cursor_unavailable");}true
        } else {return Err("chat_stream_invalid_page");};
        if ready_to_read {
            let s=state.clone();let b=step.clone();
            let result=tokio::task::spawn_blocking(move ||read_result(&s,&b)).await.map_err(|_|"chat_result_unavailable")?.map_err(|_|"chat_result_unavailable")?;
            if result["status"]=="failed_no_charge" {return Err("budget_not_sent");}
            if result["status"]=="ready" {
                let choice=&result["result"]["choices"][0];
                if !choice["message"].is_object() || !choice["finish_reason"].is_string() {return Err("chat_result_invalid");}
                if after==0 {send_frame(send,frame(request,model,replay_delta(&choice["message"])?,Value::Null,None)).await?;}
                send_frame(send,frame(request,model,json!({}),choice["finish_reason"].clone(),result["result"].get("usage").cloned())).await?;
                send_frame(send,"data: [DONE]\n\n".into()).await?;return Ok(());
            }
            // The writer closes only after outcome persistence (or unwinding).
            // A closed writer without a durable result cannot produce more
            // chunks. Report uncertainty now; do not free its financial hold.
            if page["status"]=="available" && page["finished"]==true {return Err("chat_execution_unknown");}
        }
        if heartbeat.elapsed()>=Duration::from_secs(10) {send_frame(send,": keep-alive\n\n".into()).await?;heartbeat=Instant::now();}
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}
