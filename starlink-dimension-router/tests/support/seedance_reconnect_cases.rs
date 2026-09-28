//! Exercise real request identity, billing and SSE; delay only the external bridge.
use super::*;
use std::{sync::atomic::AtomicBool,time::Duration};

struct DelayedBridge {inner:Bridge,kind:&'static str,open:AtomicBool,seen:AtomicUsize,fail:bool}
impl BridgeTransport for DelayedBridge {
    fn send(&self,method:&str,url:&str,headers:&BTreeMap<String,String>,body:&[u8])->Result<BridgeResponse,String> {
        let mut response=self.inner.send(method,url,headers,body)?;
        let gated=self.inner.claims.lock().unwrap().iter().any(|(id,c)|url.contains(&format!("/requests/{id}/")) && c["step_kind"]==self.kind);
        if gated && (url.contains("/execution?") || url.contains("/result?")) {
            let mut value:Value=serde_json::from_slice(&response.body).unwrap();
            if !self.open.load(Ordering::SeqCst) {
                if url.contains("/result?") {self.seen.fetch_add(1,Ordering::SeqCst);value["status"]=json!("not_ready");value["result"]=Value::Null;}
                else {value["status"]=json!("running");value["execution"]["state"]=json!("running");value["execution"]["finished_at_ms"]=Value::Null;value["execution"]["result_available"]=json!(false);}
            } else if self.fail {
                if url.contains("/result?") {value["result"]=json!({"id":"native-video","status":"failed"});}
                else {value["status"]=json!("failed");value["execution"]["state"]=json!("failed");}
            }
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
        let bridge=Arc::new(DelayedBridge {inner:Bridge {claims:Mutex::new(BTreeMap::new()),sends:AtomicUsize::new(0),video_intent:true,large_downloads:AtomicBool::new(false),active_downloads:Arc::new(AtomicUsize::new(0)),download_status:AtomicUsize::new(200)},kind,open:AtomicBool::new(false),seen:AtomicUsize::new(0),fail});
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
