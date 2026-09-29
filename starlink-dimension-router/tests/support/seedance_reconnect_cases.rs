//! Exercise real request identity, billing and SSE; delay only the external bridge.
use super::*;
use std::{sync::atomic::AtomicBool,time::Duration};

struct DelayedBridge {inner:Bridge,kind:&'static str,open:AtomicBool,seen:AtomicUsize,fail:bool,query_outage:AtomicBool,query_unknown:AtomicBool,missing_task_ref:AtomicBool,failure_reason:Mutex<String>,failure_detail:Mutex<Option<Value>>,result_task_id:Mutex<String>}
impl BridgeTransport for DelayedBridge {
    fn send(&self,method:&str,url:&str,headers:&BTreeMap<String,String>,body:&[u8])->Result<BridgeResponse,String> {
        let mut response=self.inner.send(method,url,headers,body)?;
        let gated=self.inner.claims.lock().unwrap().iter().any(|(id,c)|url.contains(&format!("/requests/{id}/")) && c["step_kind"]==self.kind);
        if gated && (url.contains("/execution?") || url.contains("/result?")) {
            if self.query_outage.load(Ordering::SeqCst) {
                return Ok(BridgeResponse {status:503,headers:BTreeMap::new(),body:serde_json::to_vec(&json!({"error":{"code":"bridge_state_unavailable"}})).unwrap()});
            }
            let mut value:Value=serde_json::from_slice(&response.body).unwrap();
            if !self.open.load(Ordering::SeqCst) {
                if url.contains("/result?") {self.seen.fetch_add(1,Ordering::SeqCst);value["status"]=json!("not_ready");value["result"]=Value::Null;}
                else {value["status"]=json!("running");value["execution"]["state"]=json!("running");value["execution"]["finished_at_ms"]=Value::Null;value["execution"]["result_available"]=json!(false);}
            } else if self.fail {
                if url.contains("/result?") {value["result"]=self.failure_detail.lock().unwrap().clone().unwrap_or_else(||json!({"id":"native-video","status":"failed","error":self.failure_reason.lock().unwrap().clone()}));}
                else {value["status"]=json!("failed");value["execution"]["state"]=json!("failed");}
            }
            if self.query_unknown.load(Ordering::SeqCst) && url.contains("/execution?") {
                value["status"]=json!("unknown");value["execution"]["state"]=json!("unknown");
                value["execution"]["finished_at_ms"]=Value::Null;value["execution"]["result_available"]=json!(false);
            }
            if self.missing_task_ref.load(Ordering::SeqCst) && url.contains("/execution?") {value["execution"]["task_ref"]=Value::Null;}
            if self.kind=="video" && url.contains("/result?") && value["status"]=="ready" {value["result"]["id"]=json!(self.result_task_id.lock().unwrap().clone());}
            response.body=serde_json::to_vec(&value).unwrap();
        }
        Ok(response)
    }
}
struct Fixture {state:Arc<StarlinkRouterState>,principal:aiwork_core::Principal,admin:aiwork_core::Principal,bridge:Arc<DelayedBridge>,_dir:Directory}
impl Fixture {
    fn new(kind:&'static str,fail:bool)->Self {
        let dir=Directory(std::env::temp_dir().join(format!("core-seedance-reconnect-{:032x}",rand::random::<u128>())));
        let store=Arc::new(CoreStore::open(dir.path()).unwrap());store.migrate().unwrap();
        store.create_user(NewUser {id:"admin".into(),name:"Admin".into(),role:UserRole::Admin},"bootstrap").unwrap();
        store.create_user(NewUser {id:"user".into(),name:"User".into(),role:UserRole::User},"admin").unwrap();
        let a=store.issue_api_key("admin","admin",BTreeSet::from(["admin:*".into()]),"bootstrap").unwrap();let admin=store.authenticate_api_key(&a.plaintext).unwrap();
        let k=store.issue_api_key_as_admin_with_max_concurrency("user","Key",BTreeSet::from(["videos:submit".into(),"assets:write".into()]),1,&admin).unwrap();
        store.key_quota_grant_as_admin(&admin,KeyQuotaGrant {api_key_id:k.id.clone(),resource_kind:"credits".into(),amount:500_000_000,actor_user_id:"admin".into(),reason:"isolated".into()}).unwrap();
        store.set_video_billing_control(aiwork_core::VideoBillingControlInput {mode:aiwork_core::VideoBillingMode::Active,reason:"fixture".into(),diagnostic_key_id:None,diagnostic_request_hash:None}).unwrap();
        let bridge=Arc::new(DelayedBridge {inner:Bridge {claims:Mutex::new(BTreeMap::new()),sends:AtomicUsize::new(0),video_intent:true,large_downloads:AtomicBool::new(false),active_downloads:Arc::new(AtomicUsize::new(0)),download_status:AtomicUsize::new(200)},kind,open:AtomicBool::new(false),seen:AtomicUsize::new(0),fail,query_outage:AtomicBool::new(false),query_unknown:AtomicBool::new(false),missing_task_ref:AtomicBool::new(false),failure_reason:Mutex::new(String::new()),failure_detail:Mutex::new(None),result_task_id:Mutex::new("native-video".into())});
        let principal=store.authenticate_api_key(&k.plaintext).unwrap();
        let mut cfg=RouterConfig::defaults(dir.path().into());cfg.budget_billing_v2=true;cfg.public_base_url="https://core.example".into();
        let state=StarlinkRouterState::for_test(store,BridgeClient::from_transport("http://bridge","bridge-only",bridge.clone()),cfg);
        Self {state,principal,admin,bridge,_dir:dir}
    }
    async fn waiting(&self) {
        tokio::time::timeout(Duration::from_secs(5),async {
            while self.bridge.seen.load(Ordering::SeqCst)==0 {tokio::time::sleep(Duration::from_millis(5)).await;}
        }).await.expect("the first worker must reach the delayed bridge result");
    }
    fn body(&self,stream:bool,tools:bool)->Value {
        let mut body=json!({"model":"seedance","messages":[{"role":"user","content":"生成5秒480p猫视频"}],"stream":stream});
        if tools {body["tools"]=json!([delivery_tool()]);}
        body
    }
    async fn request(&self,body:Value,explicit:bool)->axum::response::Response {
        let mut h=HeaderMap::new();if explicit {h.insert("idempotency-key","reconnect-same".parse().unwrap());}
        user_routes::chat_completions(State(self.state.clone()),h,Extension(self.principal.clone()),Bytes::from(body.to_string())).await
    }
}
async fn read_stream(response:axum::response::Response)->Value {
    assert_eq!(response.status(),StatusCode::OK,"a retry must attach to the running result, not return budget_observer_busy");
    let bytes=tokio::time::timeout(Duration::from_secs(8),axum::body::to_bytes(response.into_body(),128*1024)).await.expect("result must arrive").unwrap();
    let wire=std::str::from_utf8(&bytes).unwrap();assert!(wire.ends_with("data: [DONE]\n\n"));
    wire.lines().filter_map(|l|l.strip_prefix("data: ")).filter_map(|s|serde_json::from_str::<Value>(s).ok()).last().unwrap()
}

#[tokio::test(flavor="multi_thread",worker_threads=4)]
async fn seedance_reconnect_during_helper_shares_one_result_and_one_paid_video() {
    let f=Fixture::new("assist",false);let body=f.body(true,true);
    let first=f.request(body.clone(),false).await;f.waiting().await;
    let second=f.request(body,false).await;
    assert_eq!(second.status(),StatusCode::OK,"duplicate connection is not a concurrency violation");
    f.bridge.open.store(true,Ordering::SeqCst);
    let (a,b)=tokio::join!(read_stream(first),read_stream(second));
    assert_eq!(a,b,"subscribers must receive the same completion/tool call, not rerun the planner");
    assert_eq!(a["choices"][0]["finish_reason"],"tool_calls");
    assert_eq!(f.bridge.inner.sends.load(Ordering::SeqCst),3,"one helper, one video, one delivery planner");
    assert_eq!(f.state.store.active_execution_count_for_key(&f.principal.key_id).unwrap(),0);
}

#[tokio::test(flavor="multi_thread",worker_threads=4)]
async fn seedance_reconnect_after_disconnect_keeps_original_video() {
    let f=Fixture::new("video",false);let body=f.body(true,false);
    let first=f.request(body.clone(),true).await;f.waiting().await;
    drop(first);
    let second=f.request(body.clone(),true).await;
    assert_eq!(second.status(),StatusCode::OK,"disconnect must not strand a client behind its original worker");
    f.bridge.open.store(true,Ordering::SeqCst);
    let result=read_stream(second).await;
    assert_eq!(result["video_task"]["status"],"completed");
    let replay=read_stream(f.request(body,true).await).await;
    assert_eq!(result["request_id"],replay["request_id"]);
    assert_eq!(f.bridge.inner.sends.load(Ordering::SeqCst),2);
}

#[tokio::test(flavor="multi_thread",worker_threads=4)]
async fn seedance_reconnect_nonstream_waits_instead_of_failing() {
    let f=Arc::new(Fixture::new("video",false));let body=f.body(false,false);
    let a=f.clone();let b=body.clone();let first=tokio::spawn(async move {a.request(b,true).await});
    f.waiting().await;
    let a=f.clone();let second=tokio::spawn(async move {a.request(body,true).await});
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!second.is_finished(),"nonstream retry must wait for the original video");
    f.bridge.open.store(true,Ordering::SeqCst);
    let mut results=Vec::new();
    for task in [first,second] {
        let r=tokio::time::timeout(Duration::from_secs(8),task).await.unwrap().unwrap();assert_eq!(r.status(),StatusCode::OK);
        results.push(serde_json::from_slice::<Value>(&axum::body::to_bytes(r.into_body(),65536).await.unwrap()).unwrap());
    }
    assert_eq!(results[0],results[1]);assert_eq!(f.bridge.inner.sends.load(Ordering::SeqCst),2);
}

#[tokio::test(flavor="multi_thread",worker_threads=4)]
async fn seedance_reconnect_propagates_original_failure_to_all_subscribers() {
    let f=Fixture::new("video",true);let body=f.body(true,false);
    let first=f.request(body.clone(),true).await;f.waiting().await;
    let second=f.request(body,true).await;assert_eq!(second.status(),StatusCode::OK);
    f.bridge.open.store(true,Ordering::SeqCst);
    let (a,b)=tokio::join!(read_stream(first),read_stream(second));
    assert_eq!(a["error"]["code"],"video_execution_failed");assert_eq!(a["error"],b["error"]);
    assert_eq!(f.bridge.inner.sends.load(Ordering::SeqCst),2);
}

#[tokio::test(flavor="multi_thread",worker_threads=4)]
async fn seedance_reconnect_simultaneous_headerless_callers_use_one_identity() {
    let f=Arc::new(Fixture::new("assist",false));let barrier=Arc::new(tokio::sync::Barrier::new(12));
    let mut calls=Vec::new();
    for _ in 0..12 {let f=f.clone();let barrier=barrier.clone();calls.push(tokio::spawn(async move {barrier.wait().await;f.request(f.body(true,false),false).await}));}
    let mut replies=Vec::new();
    for call in calls {let response=call.await.unwrap();assert_eq!(response.status(),StatusCode::OK);replies.push(response);}
    f.waiting().await;f.bridge.open.store(true,Ordering::SeqCst);
    let first=read_stream(replies.remove(0)).await;
    for reply in replies {assert_eq!(read_stream(reply).await,first);}
    assert_eq!(f.bridge.inner.sends.load(Ordering::SeqCst),2);
}

#[tokio::test(flavor="multi_thread",worker_threads=4)]
async fn seedance_reconnect_different_key_never_subscribes_to_other_keys_video() {
    let f=Fixture::new("video",false);let body=f.body(true,false);
    let k=f.state.store.issue_api_key("user","Other",BTreeSet::from(["videos:submit".into()]),"admin").unwrap();
    f.state.store.key_quota_grant_as_admin(&f.admin,KeyQuotaGrant {api_key_id:k.id.clone(),resource_kind:"credits".into(),amount:100_000_000,actor_user_id:"admin".into(),reason:"test".into()}).unwrap();
    let first=f.request(body.clone(),true).await;f.waiting().await;
    let mut h=HeaderMap::new();h.insert("idempotency-key","reconnect-same".parse().unwrap());
    let other=user_routes::chat_completions(State(f.state.clone()),h,Extension(f.state.store.authenticate_api_key(&k.plaintext).unwrap()),Bytes::from(body.to_string())).await;
    f.bridge.open.store(true,Ordering::SeqCst);
    let (a,b)=tokio::join!(read_stream(first),read_stream(other));
    assert_ne!(a["request_id"],b["request_id"]);assert_eq!(a["video_task"]["status"],"completed");assert_eq!(b["video_task"]["status"],"completed");
    assert_eq!(f.bridge.inner.sends.load(Ordering::SeqCst),4);
}

#[tokio::test(flavor="multi_thread",worker_threads=4)]
async fn seedance_reconnect_does_not_bypass_key_concurrency_for_different_input() {
    let f=Fixture::new("video",false);let body=f.body(true,false);
    let first=f.request(body.clone(),false).await;f.waiting().await;
    let mut different=body;different["messages"][0]["content"]=json!("生成5秒480p另一只狗视频");
    let other=f.request(different,false).await;
    let b=read_stream(other).await;
    assert_eq!(b["error"]["code"],"key_concurrency_exceeded");
    f.bridge.open.store(true,Ordering::SeqCst);let a=read_stream(first).await;
    assert_ne!(a["request_id"],b["error"]["request_id"]);
    assert_eq!(f.bridge.inner.sends.load(Ordering::SeqCst),2,"rejected distinct task never dispatches");
}

#[tokio::test(flavor="multi_thread",worker_threads=4)]
async fn seedance_reconnect_during_delivery_planner_replays_same_download_tool() {
    let f=Fixture::new("chat",false);let body=f.body(true,true);
    let first=f.request(body.clone(),true).await;f.waiting().await;
    let second=f.request(body,true).await;assert_eq!(second.status(),StatusCode::OK);
    f.bridge.open.store(true,Ordering::SeqCst);
    let (a,b)=tokio::join!(read_stream(first),read_stream(second));
    assert_eq!(a,b);assert_eq!(a["choices"][0]["finish_reason"],"tool_calls");
    assert_eq!(f.bridge.inner.sends.load(Ordering::SeqCst),3);
}

// These exercise observable wire behavior: clients must see useful progress,
// retries must not create paid tasks, and known failures must retain their cause.
#[tokio::test(flavor="multi_thread",worker_threads=4)]
async fn seedance_feedback_starts_with_visible_text_while_helper_is_running() {
    use http_body_util::BodyExt;
    let f=Fixture::new("assist",false);let response=f.request(f.body(true,false),true).await;
    f.waiting().await;
    let mut body=response.into_body();
    let frame=tokio::time::timeout(Duration::from_secs(1),body.frame()).await.unwrap().unwrap().unwrap().into_data().unwrap();
    let chunk:Value=serde_json::from_str(std::str::from_utf8(&frame).unwrap().trim().strip_prefix("data: ").unwrap()).unwrap();
    assert!(!chunk["choices"][0]["delta"]["content"].as_str().unwrap().is_empty(),"comments and empty deltas cannot keep clients informed");
    assert!(chunk["choices"][0]["delta"]["content"].as_str().unwrap().ends_with("\n\n"),"each status must form a Markdown paragraph instead of a soft line break");
    assert!(chunk["request_id"].as_str().unwrap().starts_with("request_"));
    assert!(chunk["choices"][0]["finish_reason"].is_null(),"progress cannot finish the model turn");
    f.bridge.open.store(true,Ordering::SeqCst);drop(body);
}

#[tokio::test(flavor="multi_thread",worker_threads=4)]
async fn seedance_feedback_thirty_two_connections_observe_one_paid_task() {
    let f=Fixture::new("video",false);let body=f.body(true,false);let mut replies=Vec::new();
    for _ in 0..32 {let response=f.request(body.clone(),true).await;assert_eq!(response.status(),StatusCode::OK,"normal reconnects must not hit a per-task 16-connection trap");replies.push(response);}
    f.waiting().await;f.bridge.open.store(true,Ordering::SeqCst);
    let first=read_stream(replies.remove(0)).await;
    for reply in replies {assert_eq!(read_stream(reply).await,first);}
    assert_eq!(f.bridge.inner.sends.load(Ordering::SeqCst),2);
}

#[tokio::test(flavor="multi_thread",worker_threads=4)]
async fn seedance_feedback_safety_failure_is_readable_for_stream_and_nonstream() {
    let f=Fixture::new("video",true);*f.bridge.failure_reason.lock().unwrap()="video security check failed".into();
    let first=f.request(f.body(true,false),true).await;f.waiting().await;f.bridge.open.store(true,Ordering::SeqCst);
    let result=read_stream(first).await;
    assert_eq!(result["error"]["code"],"video_safety_check_failed");
    assert!(result["choices"][0]["delta"]["content"].as_str().unwrap().contains("安全检查未通过"));
    assert!(!result["choices"][0]["delta"]["content"].as_str().unwrap().contains("未扣费"),"execution failure alone proves no refund");
    let response=f.request(f.body(false,false),false).await;
    assert_eq!(response.status(),StatusCode::BAD_REQUEST,"a confirmed safety refusal must not look like a transient HTTP 503");
    let value:Value=serde_json::from_slice(&axum::body::to_bytes(response.into_body(),65536).await.unwrap()).unwrap();
    assert_eq!(value["error"]["code"],"video_safety_check_failed");
    assert!(value["error"]["message"].as_str().unwrap().contains("安全检查未通过"));
}

#[tokio::test(flavor="multi_thread",worker_threads=4)]
async fn seedance_feedback_preserves_unknown_upstream_reason_across_stream_replay_and_query() {
    let f=Fixture::new("video",true);
    *f.bridge.failure_reason.lock().unwrap()="Reference video exceeds supported duration; token=private-token; https://internal.example/task?ticket=private-ticket".into();
    let first=f.request(f.body(true,false),true).await;f.waiting().await;
    f.bridge.open.store(true,Ordering::SeqCst);
    let result=read_stream(first).await;
    let request=result["request_id"].as_str().unwrap().to_string();
    let message=result["error"]["message"].as_str().unwrap();
    assert!(message.contains("Reference video exceeds supported duration"),"an unrecognized cause must not be replaced by a generic server error: {message}");
    assert!(!result.to_string().contains("private-token"));
    assert!(!result.to_string().contains("internal.example"));
    assert!(!result.to_string().contains("private-ticket"));
    assert!(!message.contains("安全检查未通过"));
    assert!(!message.contains("未扣费"));
    let replay=read_stream(f.request(f.body(true,false),true).await).await;
    assert_eq!(replay["error"],result["error"]);
    // An empty in-flight registry simulates a Core restart. The durable bridge
    // result, not a transient map, must supply exactly the same error.
    let restarted=StarlinkRouterState::for_test(f.state.store.clone(),BridgeClient::from_transport("http://bridge","bridge-only",f.bridge.clone()),f.state.config.clone());
    let response=user_routes::video_task(State(restarted),axum::extract::Path(request.clone()),HeaderMap::new(),Extension(f.principal.clone())).await;
    assert_eq!(response.status(),StatusCode::OK);
    let query:Value=serde_json::from_slice(&axum::body::to_bytes(response.into_body(),65536).await.unwrap()).unwrap();
    assert_eq!(query["task"]["error"],result["error"]);
    assert_eq!(f.bridge.inner.sends.load(Ordering::SeqCst),2,"replay/query must never redispatch");
    let mut other=f.principal.clone();other.key_id="other-key".into();
    let response=user_routes::video_task(State(f.state.clone()),axum::extract::Path(request),HeaderMap::new(),Extension(other)).await;
    assert_eq!(response.status(),StatusCode::NOT_FOUND,"another Key cannot read a failure reason");
    // A separately submitted non-stream request exercises that response format.
    let response=f.request(f.body(false,false),false).await;
    assert_eq!(response.status(),StatusCode::UNPROCESSABLE_ENTITY,"a confirmed upstream failure must not appear as a gateway 503");
    let nonstream:Value=serde_json::from_slice(&axum::body::to_bytes(response.into_body(),65536).await.unwrap()).unwrap();
    assert_eq!(nonstream["error"]["message"],result["error"]["message"]);
    assert_eq!(nonstream["error"]["upstream"],result["error"]["upstream"]);
}

#[tokio::test(flavor="multi_thread",worker_threads=4)]
async fn seedance_feedback_http_provider_code_is_preserved_without_raw_body() {
    let f=Fixture::new("video",true);
    *f.bridge.failure_reason.lock().unwrap()=r#"Seedance 上游 HTTP 422: {"error":{"code":"INVALID_DURATION","message":"Reference duration exceeds maximum","account_id":"private-account"},"token":"private-token"}"#.into();
    let response=f.request(f.body(true,false),true).await;f.waiting().await;f.bridge.open.store(true,Ordering::SeqCst);
    let result=read_stream(response).await;
    assert_eq!(result["error"]["upstream"]["code"],"INVALID_DURATION");
    assert_eq!(result["error"]["upstream"]["http_status"],422);
    assert!(result["error"]["message"].as_str().unwrap().contains("Reference duration exceeds maximum"));
    assert!(!result.to_string().contains("private-account"));assert!(!result.to_string().contains("private-token"));
    assert_eq!(result["error"]["billing_state"],"pending");
}

#[tokio::test(flavor="multi_thread",worker_threads=4)]
async fn seedance_feedback_structured_bridge_failure_survives_restart_query() {
    let f=Fixture::new("video",true);
    *f.bridge.failure_detail.lock().unwrap()=Some(json!({"id":"native-video","status":"failed","error":"Reference duration exceeds maximum","upstream_error":{"code":"INVALID_DURATION","message":"Reference duration exceeds maximum","http_status":422}}));
    let response=f.request(f.body(true,false),true).await;f.waiting().await;f.bridge.open.store(true,Ordering::SeqCst);
    let streamed=read_stream(response).await;
    assert_eq!(streamed["error"]["upstream"]["code"],"INVALID_DURATION");
    assert_eq!(streamed["error"]["upstream"]["http_status"],422);
    assert_eq!(streamed["error"]["billing_state"],"pending");
    let restarted=StarlinkRouterState::for_test(f.state.store.clone(),BridgeClient::from_transport("http://bridge","bridge-only",f.bridge.clone()),f.state.config.clone());
    let id=streamed["request_id"].as_str().unwrap().to_string();
    let response=user_routes::video_task(State(restarted),axum::extract::Path(id),HeaderMap::new(),Extension(f.principal.clone())).await;
    let queried:Value=serde_json::from_slice(&axum::body::to_bytes(response.into_body(),65536).await.unwrap()).unwrap();
    assert_eq!(queried["task"]["error"],streamed["error"]);
    assert_eq!(f.bridge.inner.sends.load(Ordering::SeqCst),2);
}

#[tokio::test(flavor="multi_thread",worker_threads=4)]
async fn seedance_feedback_temporary_query_failure_keeps_original_task_running() {
    use http_body_util::BodyExt;
    let f=Fixture::new("video",false);let response=f.request(f.body(true,false),true).await;f.waiting().await;
    f.bridge.query_outage.store(true,Ordering::SeqCst);let mut body=response.into_body();
    tokio::time::timeout(Duration::from_secs(7),async {
        loop {let frame=body.frame().await.expect("query failure must not end the stream").unwrap().into_data().unwrap();
            for line in std::str::from_utf8(&frame).unwrap().lines().filter_map(|l|l.strip_prefix("data: ")) {
                if let Ok(v)=serde_json::from_str::<Value>(line) {if v["task_progress"]["stage"]=="status_query_delayed" {
                    assert!(v["choices"][0]["finish_reason"].is_null());return;
                }}
            }
        }
    }).await.expect("client must be told the status query is temporarily unavailable");
    f.bridge.query_outage.store(false,Ordering::SeqCst);f.bridge.open.store(true,Ordering::SeqCst);
    let rest=axum::body::to_bytes(body,65536).await.unwrap();let wire=std::str::from_utf8(&rest).unwrap();
    assert!(wire.contains("\"status\":\"completed\""));assert!(wire.ends_with("data: [DONE]\n\n"));
    assert_eq!(f.bridge.inner.sends.load(Ordering::SeqCst),2,"observation failures must never repeat dispatch");
}

#[tokio::test(flavor="multi_thread",worker_threads=4)]
async fn seedance_feedback_unknown_execution_is_not_claimed_to_be_running() {
    use http_body_util::BodyExt;
    let f=Fixture::new("video",false);let response=f.request(f.body(true,false),true).await;f.waiting().await;
    f.bridge.query_unknown.store(true,Ordering::SeqCst);let mut body=response.into_body();
    tokio::time::timeout(Duration::from_secs(7),async {
        loop {let frame=body.frame().await.unwrap().unwrap().into_data().unwrap();
            for line in std::str::from_utf8(&frame).unwrap().lines().filter_map(|l|l.strip_prefix("data: ")) {
                if let Ok(v)=serde_json::from_str::<Value>(line) {if v["task_progress"]["stage"]=="status_query_delayed" {return;}}
            }
        }
    }).await.expect("unknown upstream execution must be visible instead of saying generation is healthy");
    f.bridge.query_unknown.store(false,Ordering::SeqCst);f.bridge.open.store(true,Ordering::SeqCst);drop(body);
}

#[tokio::test(flavor="multi_thread",worker_threads=4)]
async fn seedance_feedback_mismatched_video_identity_cannot_supply_a_failure_reason() {
    let f=Fixture::new("video",true);*f.bridge.failure_reason.lock().unwrap()="video security check failed".into();
    *f.bridge.result_task_id.lock().unwrap()="some-other-video".into();
    let response=f.request(f.body(true,false),true).await;f.waiting().await;f.bridge.open.store(true,Ordering::SeqCst);
    let result=read_stream(response).await;
    assert_eq!(result["error"]["code"],"seedance_budget_execution_failed");
    assert!(!result["choices"][0]["delta"]["content"].as_str().unwrap().contains("安全检查未通过"));
}

#[tokio::test(flavor="multi_thread",worker_threads=4)]
async fn seedance_feedback_long_wait_uses_silent_heartbeats_and_minute_reminders() {
    use http_body_util::BodyExt;
    let f=Fixture::new("video",false);let response=f.request(f.body(true,false),true).await;f.waiting().await;let mut body=response.into_body();
    let mut heartbeats=0;let mut phases=BTreeSet::new();
    tokio::time::timeout(Duration::from_secs(68),async {
        loop {let frame=body.frame().await.unwrap().unwrap().into_data().unwrap();
            let wire=std::str::from_utf8(&frame).unwrap();
            if wire.starts_with(':') {heartbeats+=1;assert!(!wire.contains("data:"));continue;}
            for line in wire.lines().filter_map(|l|l.strip_prefix("data: ")) {
                if let Ok(v)=serde_json::from_str::<Value>(line) {
                    let elapsed=v["task_progress"]["connection_wait_seconds"].as_u64().unwrap();
                    let stage=v["task_progress"]["stage"].as_str().unwrap();
                    let text=v["choices"][0]["delta"]["content"].as_str().unwrap();
                    if elapsed<60 {
                        assert!(phases.insert(stage.to_owned()),"unchanged stages must not be appended every twenty seconds");
                    } else {
                        assert!(heartbeats>=2,"the stream must stay alive silently before the first minute reminder");
                        assert_eq!(v["task_progress"]["kind"],"waiting");
                        assert!(text.contains("等待") && text.contains("分钟"));
                        assert!(!text.contains("最近一次成功查询"),"reminders must not repeat the full internal status explanation");
                        assert!(v["choices"][0]["finish_reason"].is_null());return;
                    }
                }
            }
        }
    }).await.expect("a long-running video needs one concise reminder after a minute, not a wall of repeated status");
    f.bridge.open.store(true,Ordering::SeqCst);drop(body);assert_eq!(f.bridge.inner.sends.load(Ordering::SeqCst),2);
}

#[tokio::test(flavor="multi_thread",worker_threads=4)]
async fn seedance_feedback_reconnect_starts_one_paragraph_without_replaying_old_phases() {
    use http_body_util::BodyExt;
    let f=Fixture::new("video",false);let first=f.request(f.body(true,false),true).await;let mut first_body=first.into_body();
    tokio::time::timeout(Duration::from_secs(5),async {
        loop {let frame=first_body.frame().await.unwrap().unwrap().into_data().unwrap();
            for line in std::str::from_utf8(&frame).unwrap().lines().filter_map(|l|l.strip_prefix("data: ")) {
                if let Ok(v)=serde_json::from_str::<Value>(line) {if v["task_progress"]["stage"]=="processing" {return;}}
            }
        }
    }).await.expect("wait for the actual submitted stage before reconnecting");
    drop(first_body);
    let second=f.request(f.body(true,false),true).await;let mut body=second.into_body();
    let frame=tokio::time::timeout(Duration::from_secs(1),body.frame()).await.unwrap().unwrap().unwrap().into_data().unwrap();
    let chunk:Value=serde_json::from_str(std::str::from_utf8(&frame).unwrap().trim().strip_prefix("data: ").unwrap()).unwrap();
    assert_eq!(chunk["task_progress"]["stage"],"processing");
    assert!(chunk["choices"][0]["delta"]["content"].as_str().unwrap().ends_with("\n\n"));
    assert!(tokio::time::timeout(Duration::from_millis(300),body.frame()).await.is_err(),"a reconnect must not immediately repeat its initial status");
    f.bridge.open.store(true,Ordering::SeqCst);drop(body);
}

#[tokio::test(flavor="multi_thread",worker_threads=4)]
async fn seedance_feedback_without_upstream_task_id_does_not_claim_video_submitted() {
    use http_body_util::BodyExt;
    let f=Fixture::new("video",false);f.bridge.missing_task_ref.store(true,Ordering::SeqCst);
    let response=f.request(f.body(true,false),true).await;f.waiting().await;let mut body=response.into_body();
    let observed=tokio::time::timeout(Duration::from_secs(3),async {
        loop {let frame=body.frame().await.unwrap().unwrap().into_data().unwrap();
            for line in std::str::from_utf8(&frame).unwrap().lines().filter_map(|l|l.strip_prefix("data: ")) {
                if let Ok(v)=serde_json::from_str::<Value>(line) {
                    assert_ne!(v["task_progress"]["stage"],"processing","a running bridge worker is not proof of native video submission");
                }
            }
        }
    }).await;
    assert!(observed.is_err(),"the probe observes the entire pending interval");
    f.bridge.missing_task_ref.store(false,Ordering::SeqCst);f.bridge.open.store(true,Ordering::SeqCst);drop(body);
}
