use std::{collections::{BTreeMap,BTreeSet},sync::{Arc,Mutex,atomic::{AtomicUsize,Ordering}}};
use aiwork_core::{CoreStore,NewUser,UserRole,KeyQuotaGrant};
use axum::{extract::{State,Path},Extension,http::{HeaderMap,StatusCode},body::Bytes};
use serde_json::{json,Value};
use starlink_dimension_router::{bridge_client::{BridgeClient,BridgeTransport,BridgeResponse},config::RouterConfig,state::StarlinkRouterState,user_routes};

struct Bridge {claims:Mutex<BTreeMap<String,Value>>,sends:AtomicUsize,video_intent:bool,
    large_downloads:std::sync::atomic::AtomicBool,active_downloads:Arc<AtomicUsize>,download_status:AtomicUsize}
struct DownloadReader {active:Arc<AtomicUsize>}
impl std::io::Read for DownloadReader {
    fn read(&mut self,buffer:&mut [u8])->std::io::Result<usize> {buffer.fill(0);Ok(buffer.len())}
}
impl Drop for DownloadReader {fn drop(&mut self) {self.active.fetch_sub(1,Ordering::SeqCst);}}
struct Directory(std::path::PathBuf);
impl Directory {fn path(&self)->&std::path::Path {&self.0}}
impl Drop for Directory {fn drop(&mut self) {let _=std::fs::remove_dir_all(&self.0);}}
impl BridgeTransport for Bridge {
    fn send_stream(&self,method:&str,url:&str,headers:&BTreeMap<String,String>,body:&[u8])->Result<starlink_dimension_router::bridge_client::BridgeStreamingResponse,String> {
        let response=self.send(method,url,headers,body)?;
        let reader:Box<dyn std::io::Read+Send>=if url.contains("/content?") && self.large_downloads.load(Ordering::SeqCst) {
            self.active_downloads.fetch_add(1,Ordering::SeqCst);Box::new(DownloadReader {active:self.active_downloads.clone()})
        } else {Box::new(std::io::Cursor::new(response.body))};
        Ok(starlink_dimension_router::bridge_client::BridgeStreamingResponse {status:response.status,headers:response.headers,body:reader})
    }
    fn send(&self,_method:&str,url:&str,headers:&BTreeMap<String,String>,body:&[u8])->Result<BridgeResponse,String> {
        assert_eq!(headers.get("authorization").map(String::as_str),Some("Bearer bridge-only"));
        if url.contains("/content?") {return Ok(BridgeResponse {status:self.download_status.load(Ordering::SeqCst) as u16,headers:BTreeMap::from([("content-type".into(),"video/mp4".into())]),body:b"fixture-mp4".to_vec()});}
        let value=if url.ends_with("/v1/assets") {
            let upload:Value=serde_json::from_slice(body).unwrap();
            assert_eq!(upload["mime_type"],"image/png");
            assert!(upload["data_base64"].as_str().unwrap().starts_with("iVBOR"));
            json!({"id":"bridge-image"})
        } else if url.ends_with("/key-registry") {json!({"applied":true})} else if url.ends_with("/budgets/prepare") {
            let input:Value=serde_json::from_slice(body).unwrap();
            if input["body"]["messages"][0]["content"]=="reject policy" {
                return Ok(BridgeResponse {status:503,headers:BTreeMap::new(),body:serde_json::to_vec(&json!({"error":{"code":"budget_policy_unconfigured","message":"budget_policy_unconfigured"}})).unwrap()});
            }
            assert_eq!(headers.get("content-type").map(String::as_str),Some("application/json"));
            let claim:Value=serde_json::from_slice(body).unwrap();self.claims.lock().unwrap().insert(claim["request_id"].as_str().unwrap().into(),claim.clone());
            json!({"wire_version":2,"dispatch_token":"fixture-dispatch","evidence_level":"policy_only","prepared_at_ms":chrono::Utc::now().timestamp_millis(),"revision":1,
                "authorization":{"budget_id":format!("budget-{}",claim["request_id"].as_str().unwrap()),"parent_request_id":claim["parent_request_id"],"request_id":claim["request_id"],"core_key_id":claim["core_key_id"],
                    "request_fingerprint":claim["request_fingerprint"],"endpoint":claim["endpoint"],"model":claim["model"],"account_ref":"account","bridge_instance_id":"instance",
                    "profile_fingerprint":"normalized-profile","policy_version":"fixture","hold_credits":if claim["step_kind"]=="assist" {"2"} else {"40"},"expires_at_ms":chrono::Utc::now().timestamp_millis()+60_000}})
        } else if url.ends_with("/budgets/dispatch") {
            self.sends.fetch_add(1,Ordering::SeqCst);
            let p:Value=serde_json::from_slice(body).unwrap();assert_eq!(p["dispatch_token"],"fixture-dispatch");
            json!({"wire_version":2,"status":"accepted","budget_id":p["authorization"]["budget_id"],"request_id":p["authorization"]["request_id"]})
        } else {
            let claims=self.claims.lock().unwrap();let (id,c)=claims.iter().find(|(id,_)|url.contains(&format!("/requests/{id}/"))).ok_or("unknown request")?;
            let mut v=json!({"wire_version":2,"budget_id":format!("budget-{id}"),"request_id":c["request_id"],"core_key_id":c["core_key_id"],"account_ref":"account","bridge_instance_id":"instance"});
            if url.contains("/execution?") {v["status"]=json!("succeeded");v["execution"]=json!({"budget_id":format!("budget-{id}"),"request_id":c["request_id"],"core_key_id":c["core_key_id"],"account_ref":"account","bridge_instance_id":"instance","step_kind":c["step_kind"],"state":"succeeded","task_ref":if c["step_kind"]=="assist" {Value::Null} else {json!("native-video")},"finished_at_ms":chrono::Utc::now().timestamp_millis(),"result_available":true});}
            else if url.contains("/result?") {v["status"]=json!("ready");v["result"]=if c["step_kind"]=="assist" {json!({"choices":[{"message":{"content":if self.video_intent {"{\"intent\":\"video\",\"prompt\":\"cat playing\"}"} else {"{\"intent\":\"text\",\"text\":\"你好，连接正常。\"}"}}}]})} else {json!({"id":"native-video","status":"completed","content_url":"/v1/videos/native-video/content"})};}
            else if url.contains("/billing?") {v["status"]=json!("pending");v["event"]=Value::Null;v["receipt"]=Value::Null;}
            else if url.contains("/chunks?") {v["status"]=json!("available");v["finished"]=json!(true);v["next"]=json!(2);v["chunks"]=if url.ends_with("&after=0") {json!([{"content":"hello "},{"content":"world"}])} else {json!([])};}
            else {return Err("unexpected legacy or paid-retry route".into());}
            if c["step_kind"]=="chat" {
                if url.contains("/execution?") {v["execution"]["task_ref"]=Value::Null;}
                if url.contains("/result?") {v["result"]=json!({"id":format!("chatcmpl-{id}"),"object":"chat.completion","model":"text-model","created":1,"choices":[{"index":0,"message":{"role":"assistant","content":"hello world"},"finish_reason":"stop"}]});}
                if url.contains("/result?") && c["body"]["tool_choice"]=="required" {
                    let name=c["body"]["tools"][0]["function"]["name"].as_str().unwrap();
                    let args:serde_json::Map<String,Value>=c["body"]["tools"][0]["function"]["parameters"]["properties"].as_object().unwrap().iter().map(|(k,v)|(k.clone(),v["enum"][0].clone())).collect();
                    v["result"]=json!({"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"helper-call","type":"function","function":{"name":name,"arguments":Value::Object(args).to_string()}}]},"finish_reason":"tool_calls"}]});
                }
                if c["body"]["messages"][0]["content"]=="force unknown" {
                    if url.contains("/execution?") {v["status"]=json!("unknown");v["execution"]["state"]=json!("unknown");v["execution"]["finished_at_ms"]=Value::Null;v["execution"]["result_available"]=json!(false);}
                    if url.contains("/result?") {v["status"]=json!("not_ready");v["result"]=Value::Null;}
                }
            }
            v
        };
        Ok(BridgeResponse {status:200,headers:BTreeMap::new(),body:serde_json::to_vec(&value).unwrap()})
    }
}
#[tokio::test]
async fn ordinary_chat_v2_streams_real_deltas_and_pending_bill_does_not_block_next_request() {
    let dir=Directory(std::env::temp_dir().join(format!("core-chat-budget-{:032x}",rand::random::<u128>())));
    let store=Arc::new(CoreStore::open(dir.path()).unwrap());store.migrate().unwrap();
    store.create_user(NewUser {id:"admin".into(),name:"Admin".into(),role:UserRole::Admin},"bootstrap").unwrap();
    store.create_user(NewUser {id:"user".into(),name:"User".into(),role:UserRole::User},"admin").unwrap();
    let a=store.issue_api_key("admin","admin",BTreeSet::from(["admin:*".into()]),"bootstrap").unwrap();let admin=store.authenticate_api_key(&a.plaintext).unwrap();
    let k=store.issue_api_key_as_admin_with_max_concurrency("user","Key",BTreeSet::from(["chat:invoke".into()]),1,&admin).unwrap();
    store.key_quota_grant_as_admin(&admin,KeyQuotaGrant {api_key_id:k.id.clone(),resource_kind:"credits".into(),amount:200_000_000,actor_user_id:"admin".into(),reason:"isolated".into()}).unwrap();
    let bridge=Arc::new(Bridge {claims:Mutex::new(BTreeMap::new()),sends:AtomicUsize::new(0),video_intent:false,large_downloads:std::sync::atomic::AtomicBool::new(false),active_downloads:Arc::new(AtomicUsize::new(0)),download_status:AtomicUsize::new(200)});
    let mut cfg=RouterConfig::defaults(dir.path().into());cfg.budget_billing_v2=true;
    let state=StarlinkRouterState::for_test(store.clone(),BridgeClient::from_transport("http://bridge","bridge-only",bridge.clone()),cfg);
    let principal=store.authenticate_api_key(&k.plaintext).unwrap();
    for (id,stream) in [("first",true),("first",true),("second",false)] {
        let mut headers=HeaderMap::new();headers.insert("idempotency-key",id.parse().unwrap());
        let r=user_routes::chat_completions(State(state.clone()),headers,Extension(principal.clone()),Bytes::from(json!({"model":"text-model","messages":[{"role":"user","content":"hello"}],"stream":stream}).to_string())).await;
        assert_eq!(r.status(),StatusCode::OK,"ordinary Chat must not enter the legacy quote/whole-Key reservation path");
        let bytes=axum::body::to_bytes(r.into_body(),65536).await.unwrap();
        let request=if stream {
            let wire=std::str::from_utf8(&bytes).unwrap();assert!(wire.contains("data: [DONE]"));
            let frames:Vec<Value>=wire.lines().filter_map(|s|s.strip_prefix("data: ")).filter_map(|s|serde_json::from_str(s).ok()).collect();
            assert_eq!(frames[1]["choices"][0]["delta"]["content"],"hello ");assert_eq!(frames[2]["choices"][0]["delta"]["content"],"world");
            assert_eq!(frames.last().unwrap()["choices"][0]["finish_reason"],"stop");frames[0]["request_id"].as_str().unwrap().to_owned()
        } else {let result:Value=serde_json::from_slice(&bytes).unwrap();assert_eq!(result["choices"][0]["message"]["content"],"hello world");result["request_id"].as_str().unwrap().into()};
        let op=store.budget_operation(&request).unwrap().unwrap();assert_eq!(op.steps[0].financial_state,aiwork_core::BudgetFinancialState::Held);
        assert_eq!(store.active_execution_count_for_key(&k.id).unwrap(),0);
    }
    assert_eq!(bridge.sends.load(Ordering::SeqCst),2,"same idempotency key never pays twice");
    let r=user_routes::chat_completions(State(state.clone()),HeaderMap::new(),Extension(principal.clone()),Bytes::from(json!({"model":"text-model","messages":[{"role":"user","content":"force unknown"}],"stream":true}).to_string())).await;
    let bytes=tokio::time::timeout(std::time::Duration::from_secs(2),axum::body::to_bytes(r.into_body(),65536)).await.expect("finished failed worker must not leave client waiting fourteen minutes").unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("chat_execution_unknown"));
    assert_eq!(store.active_execution_count_for_key(&k.id).unwrap(),1,"unknown execution is not proof to release its paid slot");
    assert_eq!(bridge.sends.load(Ordering::SeqCst),3);
}

#[tokio::test]
async fn completed_chat_replay_does_not_require_expired_input_asset() {
    let dir=Directory(std::env::temp_dir().join(format!("core-chat-expired-{:032x}",rand::random::<u128>())));
    let store=Arc::new(CoreStore::open(dir.path()).unwrap());store.migrate().unwrap();
    store.create_user(NewUser {id:"admin".into(),name:"Admin".into(),role:UserRole::Admin},"bootstrap").unwrap();
    store.create_user(NewUser {id:"user".into(),name:"User".into(),role:UserRole::User},"admin").unwrap();
    let a=store.issue_api_key("admin","admin",BTreeSet::from(["admin:*".into()]),"bootstrap").unwrap();let admin=store.authenticate_api_key(&a.plaintext).unwrap();
    let k=store.issue_api_key_as_admin_with_max_concurrency("user","Key",BTreeSet::from(["chat:invoke".into(),"assets:write".into()]),1,&admin).unwrap();
    store.key_quota_grant_as_admin(&admin,KeyQuotaGrant {api_key_id:k.id.clone(),resource_kind:"credits".into(),amount:80_000_000,actor_user_id:"admin".into(),reason:"isolated".into()}).unwrap();
    let bridge=Arc::new(Bridge {claims:Mutex::new(BTreeMap::new()),sends:AtomicUsize::new(0),video_intent:false,large_downloads:std::sync::atomic::AtomicBool::new(false),active_downloads:Arc::new(AtomicUsize::new(0)),download_status:AtomicUsize::new(200)});
    let mut cfg=RouterConfig::defaults(dir.path().into());cfg.budget_billing_v2=true;
    let state=StarlinkRouterState::for_test(store.clone(),BridgeClient::from_transport("http://bridge","bridge-only",bridge.clone()),cfg);
    let principal=store.authenticate_api_key(&k.plaintext).unwrap();
    let uploaded=user_routes::assets_upload(State(state.clone()),Extension(principal.clone()),Bytes::from(json!({"filename":"tiny.png","data_base64":"data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jN1sAAAAASUVORK5CYII="}).to_string())).await;
    assert_eq!(uploaded.status(),StatusCode::OK);
    let asset:Value=serde_json::from_slice(&axum::body::to_bytes(uploaded.into_body(),65536).await.unwrap()).unwrap();
    let body=Bytes::from(json!({"model":"text-model","messages":[{"role":"user","content":"describe"}],"image_asset_ids":[asset["id"]]}).to_string());
    let mut header=HeaderMap::new();header.insert("idempotency-key","same-image".parse().unwrap());
    let first=user_routes::chat_completions(State(state.clone()),header.clone(),Extension(principal.clone()),body.clone()).await;
    assert_eq!(first.status(),StatusCode::OK);
    let result=axum::body::to_bytes(first.into_body(),65536).await.unwrap();
    assert_eq!(starlink_dimension_router::assets::cleanup_expired_assets_once(&store,dir.path(),asset["expires_at"].as_i64().unwrap()*1000+1000).unwrap(),1);
    let replay=user_routes::chat_completions(State(state.clone()),header,Extension(principal.clone()),body.clone()).await;
    assert_eq!(replay.status(),StatusCode::OK,"persisted result must survive input expiry");
    assert_eq!(axum::body::to_bytes(replay.into_body(),65536).await.unwrap(),result);
    let fresh=user_routes::chat_completions(State(state),HeaderMap::new(),Extension(principal),body).await;
    assert!(fresh.status().is_client_error(),"expired input cannot authorize a new paid task");
    assert_eq!(bridge.sends.load(Ordering::SeqCst),1);
    assert_eq!(bridge.claims.lock().unwrap().len(),1);
}
#[tokio::test]
async fn ordinary_chat_rejects_foreign_assets_before_budget_or_paid_send() {
    let dir=Directory(std::env::temp_dir().join(format!("core-chat-assets-{:032x}",rand::random::<u128>())));
    let store=Arc::new(CoreStore::open(dir.path()).unwrap());store.migrate().unwrap();
    store.create_user(NewUser {id:"admin".into(),name:"Admin".into(),role:UserRole::Admin},"bootstrap").unwrap();
    let k=store.issue_api_key("admin","limited",BTreeSet::from(["chat:invoke".into()]),"bootstrap").unwrap();let principal=store.authenticate_api_key(&k.plaintext).unwrap();
    let bridge=Arc::new(Bridge {claims:Mutex::new(BTreeMap::new()),sends:AtomicUsize::new(0),video_intent:false,large_downloads:std::sync::atomic::AtomicBool::new(false),active_downloads:Arc::new(AtomicUsize::new(0)),download_status:AtomicUsize::new(200)});
    let mut cfg=RouterConfig::defaults(dir.path().into());cfg.budget_billing_v2=true;
    let state=StarlinkRouterState::for_test(store.clone(),BridgeClient::from_transport("http://bridge","bridge-only",bridge.clone()),cfg);
    let r=user_routes::chat_completions(State(state),HeaderMap::new(),Extension(principal),Bytes::from(json!({"model":"text-model","messages":[{"role":"user","content":"look"}],"image_asset_ids":["asset-other"]}).to_string())).await;
    assert!(r.status().is_client_error());assert!(bridge.claims.lock().unwrap().is_empty());assert_eq!(bridge.sends.load(Ordering::SeqCst),0);
    assert_eq!(store.active_execution_count_for_key(&k.id).unwrap(),0,"rejected asset must not leak concurrency");
}
#[tokio::test]
async fn rejected_chat_policy_releases_slot_and_next_model_can_run() {
    let dir=Directory(std::env::temp_dir().join(format!("core-chat-rejected-{:032x}",rand::random::<u128>())));
    let store=Arc::new(CoreStore::open(dir.path()).unwrap());store.migrate().unwrap();
    store.create_user(NewUser {id:"admin".into(),name:"Admin".into(),role:UserRole::Admin},"bootstrap").unwrap();
    store.create_user(NewUser {id:"user".into(),name:"User".into(),role:UserRole::User},"admin").unwrap();
    let a=store.issue_api_key("admin","admin",BTreeSet::from(["admin:*".into()]),"bootstrap").unwrap();let admin=store.authenticate_api_key(&a.plaintext).unwrap();
    let k=store.issue_api_key_as_admin_with_max_concurrency("user","Key",BTreeSet::from(["chat:invoke".into()]),1,&admin).unwrap();
    store.key_quota_grant_as_admin(&admin,KeyQuotaGrant {api_key_id:k.id.clone(),resource_kind:"credits".into(),amount:200_000_000,actor_user_id:"admin".into(),reason:"isolated".into()}).unwrap();
    let bridge=Arc::new(Bridge {claims:Mutex::new(BTreeMap::new()),sends:AtomicUsize::new(0),video_intent:false,large_downloads:std::sync::atomic::AtomicBool::new(false),active_downloads:Arc::new(AtomicUsize::new(0)),download_status:AtomicUsize::new(200)});
    let mut cfg=RouterConfig::defaults(dir.path().into());cfg.budget_billing_v2=true;
    let state=StarlinkRouterState::for_test(store.clone(),BridgeClient::from_transport("http://bridge","bridge-only",bridge.clone()),cfg);
    let principal=store.authenticate_api_key(&k.plaintext).unwrap();
    for (index,prompt) in ["reject policy","reject policy","hello"].iter().enumerate() {
        let mut headers=HeaderMap::new();headers.insert("idempotency-key",format!("rejected-{index}").parse().unwrap());
        let response=user_routes::chat_completions(State(state.clone()),headers,Extension(principal.clone()),Bytes::from(json!({"model":"text-model","messages":[{"role":"user","content":prompt}]}).to_string())).await;
        assert_eq!(response.status(),if index<2 {StatusCode::SERVICE_UNAVAILABLE} else {StatusCode::OK});
        assert_eq!(store.active_execution_count_for_key(&k.id).unwrap(),0,"policy rejection must not consume a slot");
    }
    assert_eq!(bridge.sends.load(Ordering::SeqCst),1);
}
#[tokio::test(flavor="multi_thread",worker_threads=2)]
async fn disconnected_chat_keeps_preparation_owner_until_worker_stops_then_frees_slot() {
    struct Blocked {
        entered:Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        resume:Mutex<std::sync::mpsc::Receiver<()>>,
    }
    impl BridgeTransport for Blocked {
        fn send(&self,_:&str,url:&str,_:&BTreeMap<String,String>,_:&[u8])->Result<BridgeResponse,String> {
            if url.ends_with("/key-registry") {return Ok(BridgeResponse {status:200,headers:BTreeMap::new(),body:b"{\"applied\":true}".to_vec()});}
            assert!(url.ends_with("/budgets/prepare"));
            self.entered.lock().unwrap().take().unwrap().send(()).unwrap();
            self.resume.lock().unwrap().recv_timeout(std::time::Duration::from_secs(5)).unwrap();
            Err("budget_policy_unconfigured".into())
        }
    }
    let dir=Directory(std::env::temp_dir().join(format!("core-chat-disconnect-{:032x}",rand::random::<u128>())));
    let store=Arc::new(CoreStore::open(dir.path()).unwrap());store.migrate().unwrap();
    store.create_user(NewUser {id:"admin".into(),name:"Admin".into(),role:UserRole::Admin},"bootstrap").unwrap();
    let k=store.issue_api_key("admin","limited",BTreeSet::from(["chat:invoke".into()]),"bootstrap").unwrap();let principal=store.authenticate_api_key(&k.plaintext).unwrap();
    let (entered,ready)=tokio::sync::oneshot::channel();let (resume,receiver)=std::sync::mpsc::channel();
    let bridge=Arc::new(Blocked {entered:Mutex::new(Some(entered)),resume:Mutex::new(receiver)});
    let mut cfg=RouterConfig::defaults(dir.path().into());cfg.budget_billing_v2=true;
    let state=StarlinkRouterState::for_test(store.clone(),BridgeClient::from_transport("http://bridge","bridge-only",bridge),cfg);
    let worker=tokio::spawn(user_routes::chat_completions(State(state.clone()),HeaderMap::new(),Extension(principal),Bytes::from(json!({"model":"text-model","messages":[{"role":"user","content":"hello"}]}).to_string())));
    tokio::time::timeout(std::time::Duration::from_secs(2),ready).await.unwrap().unwrap();
    worker.abort();assert!(worker.await.unwrap_err().is_cancelled());
    assert_eq!(state.video_stream_observers.lock().unwrap().len(),1,"disconnect must not release a still-preparing owner");
    assert_eq!(store.active_execution_count_for_key(&k.id).unwrap(),1);
    resume.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2),async {
        while !state.video_stream_observers.lock().unwrap().is_empty() {tokio::time::sleep(std::time::Duration::from_millis(5)).await;}
    }).await.unwrap();
    assert_eq!(store.active_execution_count_for_key(&k.id).unwrap(),0);
}
#[tokio::test(flavor="multi_thread",worker_threads=4)]
async fn ten_keys_each_admit_two_reject_third_and_reuse_terminal_slot_before_receipt() {
    let dir=Directory(std::env::temp_dir().join(format!("core-concurrent-budget-{:032x}",rand::random::<u128>())));
    let store=Arc::new(CoreStore::open(dir.path()).unwrap());store.migrate().unwrap();
    store.create_user(NewUser {id:"admin".into(),name:"Admin".into(),role:UserRole::Admin},"bootstrap").unwrap();
    store.create_user(NewUser {id:"user".into(),name:"User".into(),role:UserRole::User},"admin").unwrap();
    let issued=store.issue_api_key("admin","admin",BTreeSet::from(["admin:*".into()]),"bootstrap").unwrap();
    let admin=store.authenticate_api_key(&issued.plaintext).unwrap();
    let mut principals=Vec::new();
    for index in 0..10 {
        let issued=store.issue_api_key_as_admin_with_max_concurrency("user",&format!("Key-{index}"),BTreeSet::from(["videos:submit".into()]),2,&admin).unwrap();
        store.key_quota_grant_as_admin(&admin,KeyQuotaGrant {api_key_id:issued.id.clone(),resource_kind:"credits".into(),amount:1_000_000_000,actor_user_id:"admin".into(),reason:"isolated concurrency".into()}).unwrap();
        principals.push(store.authenticate_api_key(&issued.plaintext).unwrap());
    }
    store.set_video_billing_control(aiwork_core::VideoBillingControlInput {mode:aiwork_core::VideoBillingMode::Active,reason:"fixture".into(),diagnostic_key_id:None,diagnostic_request_hash:None}).unwrap();
    let bridge=Arc::new(Bridge {claims:Mutex::new(BTreeMap::new()),sends:AtomicUsize::new(0),video_intent:true,large_downloads:std::sync::atomic::AtomicBool::new(false),active_downloads:Arc::new(AtomicUsize::new(0)),download_status:AtomicUsize::new(200)});
    let mut cfg=RouterConfig::defaults(dir.path().into());cfg.budget_billing_v2=true;
    let state=StarlinkRouterState::for_test(store.clone(),BridgeClient::from_transport("http://bridge","bridge-only",bridge.clone()),cfg);
    let body=Bytes::from(json!({"model":"seedance","prompt":"fixture cat","duration":5,"resolution":"480p"}).to_string());
    let mut tasks=Vec::new();
    for p in &principals {for index in 0..2 {
        let s=state.clone();let p=p.clone();let body=body.clone();
        tasks.push(tokio::spawn(async move {
            let mut h=HeaderMap::new();h.insert("idempotency-key",format!("attempt-{index}").parse().unwrap());
            let response=user_routes::video_generations(State(s),h,Extension(p.clone()),body).await;
            assert_eq!(response.status(),StatusCode::ACCEPTED);
            let value:Value=serde_json::from_slice(&axum::body::to_bytes(response.into_body(),65536).await.unwrap()).unwrap();
            (p.key_id,value["task"]["id"].as_str().unwrap().to_owned())
        }));
    }}
    let mut firsts=BTreeMap::new();
    for task in tasks {let (key,id)=task.await.unwrap();firsts.entry(key).or_insert(id);}
    assert_eq!(bridge.sends.load(Ordering::SeqCst),20);
    for p in &principals {
        assert_eq!(store.active_execution_count_for_key(&p.key_id).unwrap(),2);
        let mut h=HeaderMap::new();h.insert("idempotency-key","third-busy".parse().unwrap());
        let r=user_routes::video_generations(State(state.clone()),h,Extension(p.clone()),body.clone()).await;
        assert_eq!(r.status(),StatusCode::TOO_MANY_REQUESTS,"exhausted execution slots are 429, not quote_unavailable/503");
        let first=&firsts[&p.key_id];
        let r=user_routes::video_task(State(state.clone()),Path(first.clone()),HeaderMap::new(),Extension(p.clone())).await;
        assert_eq!(r.status(),StatusCode::OK);
        let op=store.budget_operation(first).unwrap().unwrap();
        assert_eq!(op.api_key_id,p.key_id);assert_eq!(op.steps[0].financial_state,aiwork_core::BudgetFinancialState::Held);
        assert_eq!(store.active_execution_count_for_key(&p.key_id).unwrap(),1);
        let mut h=HeaderMap::new();h.insert("idempotency-key","fourth-after-result".parse().unwrap());
        let r=user_routes::video_generations(State(state.clone()),h,Extension(p.clone()),body.clone()).await;
        assert_eq!(r.status(),StatusCode::ACCEPTED,"pending bill must not block the newly free execution slot");
        assert_eq!(store.active_execution_count_for_key(&p.key_id).unwrap(),2);
    }
    assert_eq!(bridge.sends.load(Ordering::SeqCst),30,"rejected third requests must never dispatch");
}
#[tokio::test]
async fn public_video_reserves_only_its_budget_and_replays_without_resubmission() {
    run_case(false,false,false).await;
}

fn delivery_tool() -> Value {
    json!({"type":"function","function":{"name":"RunCommand","description":"Executes PowerShell commands, shell type powershell5","parameters":{"type":"object","required":["command","blocking","requires_approval"],"properties":{"command":{"type":"string"},"blocking":{"type":"boolean"},"requires_approval":{"type":"boolean"},"command_type":{"type":"string"},"cwd":{"type":"string"}}}}})
}

#[tokio::test]
async fn completed_video_stream_calls_client_download_tool_and_receipt_never_regenerates() {
    let dir=Directory(std::env::temp_dir().join(format!("core-delivery-{:032x}",rand::random::<u128>())));
    let store=Arc::new(CoreStore::open(dir.path()).unwrap());store.migrate().unwrap();
    store.create_user(NewUser {id:"admin".into(),name:"Admin".into(),role:UserRole::Admin},"bootstrap").unwrap();
    store.create_user(NewUser {id:"user".into(),name:"User".into(),role:UserRole::User},"admin").unwrap();
    let a=store.issue_api_key("admin","admin",BTreeSet::from(["admin:*".into()]),"bootstrap").unwrap();let admin=store.authenticate_api_key(&a.plaintext).unwrap();
    let k=store.issue_api_key_as_admin_with_max_concurrency("user","Key",BTreeSet::from(["videos:submit".into()]),1,&admin).unwrap();
    store.key_quota_grant_as_admin(&admin,KeyQuotaGrant {api_key_id:k.id.clone(),resource_kind:"credits".into(),amount:200_000_000,actor_user_id:"admin".into(),reason:"test".into()}).unwrap();
    store.set_video_billing_control(aiwork_core::VideoBillingControlInput {mode:aiwork_core::VideoBillingMode::Active,reason:"test".into(),diagnostic_key_id:None,diagnostic_request_hash:None}).unwrap();
    let bridge=Arc::new(Bridge {claims:Mutex::new(BTreeMap::new()),sends:AtomicUsize::new(0),video_intent:true,large_downloads:std::sync::atomic::AtomicBool::new(false),active_downloads:Arc::new(AtomicUsize::new(0)),download_status:AtomicUsize::new(200)});
    let mut cfg=RouterConfig::defaults(dir.path().into());cfg.budget_billing_v2=true;cfg.public_base_url="https://api.example.test".into();
    let state=StarlinkRouterState::for_test(store.clone(),BridgeClient::from_transport("http://bridge","bridge-only",bridge.clone()),cfg);
    let principal=store.authenticate_api_key(&k.plaintext).unwrap();
    let input=json!({"model":"seedance","stream":true,"tools":[delivery_tool()],"messages":[
        {"role":"system","content":"Final workspace folder (for final deliverables and supporting assets) should locate in:\n`E:\\delivery test\\workspace`"},
        {"role":"user","content":"生成5秒480p的猫视频"}]});
    let response=user_routes::chat_completions(State(state.clone()),HeaderMap::new(),Extension(principal.clone()),Bytes::from(input.to_string())).await;
    assert_eq!(response.status(),StatusCode::OK);
    let bytes=axum::body::to_bytes(response.into_body(),65536).await.unwrap();let wire=std::str::from_utf8(&bytes).unwrap();assert!(wire.ends_with("data: [DONE]\n\n"));
    let frames:Vec<Value>=wire.lines().filter_map(|l|l.strip_prefix("data: ")).filter_map(|s|serde_json::from_str(s).ok()).collect();
    let final_frame=frames.last().unwrap();let call=&final_frame["choices"][0]["delta"]["tool_calls"][0];
    assert_eq!(call["function"]["name"],"RunCommand","completed video must request a real client tool, not just a link");
    assert_eq!(call["index"],0);assert_eq!(final_frame["choices"][0]["finish_reason"],"tool_calls");
    assert_eq!(final_frame["video_task"]["delivery_planner"],"assistant");
    let args:Value=serde_json::from_str(call["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(args["blocking"],true);assert_eq!(args["cwd"],"E:\\delivery test\\workspace");
    assert!(!args["command"].as_str().unwrap().contains(&k.plaintext));
    let request=final_frame["request_id"].as_str().unwrap();let before=bridge.sends.load(Ordering::SeqCst);assert_eq!(before,3,"intent, video, and bounded delivery planning each have their own billed identity");
    assert!(bridge.claims.lock().unwrap().values().filter(|c|c["step_kind"]!="video").all(|c|c["model"]=="glm-5.3-flash"));
    let mut clean_call=call.clone();clean_call.as_object_mut().unwrap().remove("index");
    for (status,expected) in [("saved","已保存"),("failed","下载未完成")] {
        let follow=json!({"model":"seedance","stream":false,"tools":[delivery_tool()],"messages":[
            {"role":"user","content":"生成5秒480p的猫视频"},
            {"role":"assistant","content":null,"tool_calls":[clean_call]},
            {"role":"tool","tool_call_id":call["id"],"content":format!("Command output:\nSEEDANCE_DELIVERY_RECEIPT={}\n",json!({"seedance_delivery":1,"request_id":request,"status":status,"path":"E:\\delivery test\\workspace\\video.mp4","bytes":1234}))}]});
        let response=user_routes::chat_completions(State(state.clone()),HeaderMap::new(),Extension(principal.clone()),Bytes::from(follow.to_string())).await;
        assert_eq!(response.status(),StatusCode::OK);
        let result:Value=serde_json::from_slice(&axum::body::to_bytes(response.into_body(),65536).await.unwrap()).unwrap();
        assert!(result["choices"][0]["message"]["content"].as_str().unwrap().contains(expected));
        assert_eq!(bridge.sends.load(Ordering::SeqCst),before,"download receipt must never call helper/video again");
        assert_eq!(store.active_execution_count_for_key(&k.id).unwrap(),0);
    }
    let receipt=json!({"seedance_delivery":1,"request_id":request,"status":"saved","path":"E:\\Downloads\\video.mp4","bytes":1234});
    let rewritten=json!({"model":"seedance","messages":[{"role":"tool","tool_call_id":"client-rewritten-id","content":{"stdout":format!("SEEDANCE_DELIVERY_RECEIPT={receipt}")}}]});
    let response=user_routes::chat_completions(State(state.clone()),HeaderMap::new(),Extension(principal.clone()),Bytes::from(rewritten.to_string())).await;
    assert_eq!(response.status(),StatusCode::OK,"owned receipt survives rewritten ID, omitted assistant metadata, and wrapped stdout");
    let result:Value=serde_json::from_slice(&axum::body::to_bytes(response.into_body(),65536).await.unwrap()).unwrap();
    assert_eq!(result["video_delivery"]["status"],"saved");
    let mut streamed_receipt=rewritten.clone();streamed_receipt["stream"]=json!(true);
    let response=user_routes::chat_completions(State(state.clone()),HeaderMap::new(),Extension(principal.clone()),Bytes::from(streamed_receipt.to_string())).await;
    assert_eq!(response.status(),StatusCode::OK);
    let streamed=String::from_utf8(axum::body::to_bytes(response.into_body(),1024*1024).await.unwrap().to_vec()).unwrap();
    assert!(streamed.contains("\"video_delivery\":{\"bytes\":1234,\"path\":") && streamed.contains("\"status\":\"saved\"") && streamed.contains("data: [DONE]"),"SSE must retain the client's structured saved receipt, not only its display text: {streamed}");
    assert_eq!(bridge.sends.load(Ordering::SeqCst),before);
    let invalid=json!({"model":"seedance","messages":[
        {"role":"user","content":"生成5秒480p的猫视频"},
        {"role":"assistant","tool_calls":[clean_call]},
        {"role":"tool","tool_call_id":"wrong_call_id","content":"download failed"}]});
    let response=user_routes::chat_completions(State(state.clone()),HeaderMap::new(),Extension(principal.clone()),Bytes::from(invalid.to_string())).await;
    assert_eq!(response.status(),StatusCode::BAD_REQUEST,"unrecognized download reply must not become another generation");
    assert_eq!(bridge.sends.load(Ordering::SeqCst),before);

    // Delivery can be retried independently of generation and input retention.
    use tower::ServiceExt;
    let router=starlink_dimension_router::server::build_router(state.clone());
    let retry=router.clone().oneshot(axum::http::Request::builder().method("POST").uri(format!("/v1/videos/{request}/delivery"))
        .header("authorization",format!("Bearer {}",k.plaintext)).header("content-type","application/json")
        .body(axum::body::Body::from(json!({"tools":[delivery_tool()]}).to_string())).unwrap()).await.unwrap();
    assert_eq!(retry.status(),StatusCode::OK);
    let retried:Value=serde_json::from_slice(&axum::body::to_bytes(retry.into_body(),65536).await.unwrap()).unwrap();
    assert_eq!(retried["choices"][0]["finish_reason"],"tool_calls");
    let bash_tool=json!({"type":"function","function":{"name":"Bash","description":"Execute commands in Bash","parameters":{"type":"object","required":["command"],"properties":{"command":{"type":"string"}}}}});
    let bash_retry=router.clone().oneshot(axum::http::Request::builder().method("POST").uri(format!("/v1/videos/{request}/delivery"))
        .header("authorization",format!("Bearer {}",k.plaintext)).header("content-type","application/json")
        .body(axum::body::Body::from(json!({"tools":[bash_tool]}).to_string())).unwrap()).await.unwrap();
    assert_eq!(bash_retry.status(),StatusCode::OK);
    let bash_result:Value=serde_json::from_slice(&axum::body::to_bytes(bash_retry.into_body(),131072).await.unwrap()).unwrap();
    assert_eq!(bash_result["choices"][0]["message"]["tool_calls"][0]["function"]["name"],"Bash");
    assert_eq!(bridge.sends.load(Ordering::SeqCst),before+1,"same schema reuses its planner; a different tool schema has one bounded plan");
    let args:Value=serde_json::from_str(retried["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"].as_str().unwrap()).unwrap();
    let command=args["command"].as_str().unwrap();
    let url=command.lines().find(|l|l.starts_with("$deliveryUrl = ")).unwrap().trim_start_matches("$deliveryUrl = '").trim_end_matches('\'');
    let uri=url.trim_start_matches("https://api.example.test");
    let good=router.clone().oneshot(axum::http::Request::builder().uri(uri).body(axum::body::Body::empty()).unwrap()).await.unwrap();
    assert_eq!(good.status(),StatusCode::OK,"download-only ticket needs no permanent Key");
    assert_eq!(axum::body::to_bytes(good.into_body(),65536).await.unwrap(),"fixture-mp4");
    let db=rusqlite::Connection::open(dir.path().join("data").join(aiwork_core::CORE_DB_FILE)).unwrap();
    db.execute("DELETE FROM budget_continuations WHERE request_id=?1",[request]).unwrap();
    let good=router.clone().oneshot(axum::http::Request::builder().uri(uri).body(axum::body::Body::empty()).unwrap()).await.unwrap();
    assert_eq!(good.status(),StatusCode::OK,"temporary input pruning must not revoke a download ticket");
    drop(good);
    for bad in [format!("{uri}broken"),uri.replace(request,"request_foreign")] {
        let r=router.clone().oneshot(axum::http::Request::builder().uri(bad).body(axum::body::Body::empty()).unwrap()).await.unwrap();
        assert_eq!(r.status(),StatusCode::NOT_FOUND);
    }
    for (key_id,issued,expires) in [(&k.id,0,900000),(&a.id,chrono::Utc::now().timestamp_millis(),chrono::Utc::now().timestamp_millis()+900000)] {
        let sealed=state.key_vault.encrypt(&format!("video-download-v1:{request}"),&json!({"key_id":key_id,"issued_at_ms":issued,"expires_at_ms":expires}).to_string()).unwrap();
        use base64::Engine as _;
        let ticket=format!("{}.{}",sealed.key_version,base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sealed.ciphertext));
        let r=router.clone().oneshot(axum::http::Request::builder().uri(format!("/v1/videos/{request}/download?ticket={ticket}")).body(axum::body::Body::empty()).unwrap()).await.unwrap();
        assert_eq!(r.status(),StatusCode::NOT_FOUND,"expired or wrong-Key ticket must be refused");
    }
    let foreign=store.issue_api_key_as_admin_with_max_concurrency("user","Other",BTreeSet::from(["videos:submit".into()]),1,&admin).unwrap();
    let rejected=user_routes::chat_completions(State(state.clone()),HeaderMap::new(),Extension(store.authenticate_api_key(&foreign.plaintext).unwrap()),Bytes::from(rewritten.to_string())).await;
    assert_eq!(rejected.status(),StatusCode::BAD_REQUEST,"rewritten receipt must retain strict Key ownership");
    let r=router.clone().oneshot(axum::http::Request::builder().method("POST").uri(format!("/v1/videos/{request}/delivery"))
        .header("authorization",format!("Bearer {}",foreign.plaintext)).header("content-type","application/json")
        .body(axum::body::Body::from("{}")).unwrap()).await.unwrap();
    assert_eq!(r.status(),StatusCode::NOT_FOUND,"same user different Key may not issue a ticket for this task");
    for sql in ["UPDATE api_keys SET status='revoked' WHERE id=?1","UPDATE api_keys SET status='active',scopes_json='[]' WHERE id=?1"] {
        db.execute(sql,[&k.id]).unwrap();
        let r=router.clone().oneshot(axum::http::Request::builder().uri(uri).body(axum::body::Body::empty()).unwrap()).await.unwrap();
        assert_eq!(r.status(),StatusCode::NOT_FOUND,"current Key status/scope must win over a previously issued ticket");
    }
    assert_eq!(bridge.sends.load(Ordering::SeqCst),before+1,"delivery/download/authorization failures have no new video dispatch");
}
#[tokio::test]
async fn model_add_probe_uses_bounded_helper_and_does_not_generate_a_video() {
    run_case(true,false,false).await;
}
#[tokio::test]
async fn seedance_stream_probe_has_valid_sse_and_needs_no_client_idempotency_header() {run_case(true,true,false).await;}
#[tokio::test]
async fn seedance_helper_to_video_continues_while_both_receipts_are_pending() {run_case(true,false,true).await;}
#[tokio::test]
async fn retry_after_core_restart_recovers_saved_helper_without_paying_for_it_again() {
    run_case_options(true,false,true,false,true,false).await;
}
#[tokio::test]
async fn orphaned_helper_text_completion_releases_parent_without_client_retry() {
    run_case_options(true,false,false,false,true,true).await;
}
#[tokio::test(flavor="multi_thread",worker_threads=4)]
async fn background_restarts_video_from_encrypted_checkpoint_without_a_second_client_request() {
    run_case_options(true,false,true,false,true,true).await;
}
#[tokio::test(flavor="multi_thread",worker_threads=4)]
async fn corrupt_checkpoint_does_not_block_other_background_jobs() {
    run_case_with_fault(true,false,true,false,true,true,BackgroundFault::CorruptNeighbor).await;
}
#[tokio::test(flavor="multi_thread",worker_threads=4)]
async fn background_does_not_dispatch_after_key_revocation() {
    run_case_with_fault(true,false,true,false,true,true,BackgroundFault::RevokedKey).await;
}
#[tokio::test(flavor="multi_thread",worker_threads=4)]
async fn background_does_not_dispatch_after_video_scope_removed() {
    run_case_with_fault(true,false,true,false,true,true,BackgroundFault::RemovedScope).await;
}
#[tokio::test(flavor="multi_thread",worker_threads=4)]
async fn background_and_http_retry_share_one_paid_video_dispatch() {
    run_case_with_fault(true,false,true,false,true,true,BackgroundFault::HttpRace).await;
}
#[tokio::test(flavor="multi_thread",worker_threads=4)]
async fn background_discards_aged_completed_input_without_changing_pending_billing() {
    run_case_with_fault(true,false,false,false,false,false,BackgroundFault::Cleanup).await;
}
#[tokio::test]
async fn chat_inline_reference_is_uploaded_and_bound_to_video_preparation() {run_case_with_reference(true,false,true,true).await;}
async fn run_case(probe:bool,stream:bool,video_intent:bool) {
    run_case_with_reference(probe,stream,video_intent,false).await;
}
async fn run_case_with_reference(probe:bool,stream:bool,video_intent:bool,reference:bool) {
    run_case_options(probe,stream,video_intent,reference,false,false).await;
}
async fn run_case_options(probe:bool,stream:bool,video_intent:bool,reference:bool,resume_helper:bool,background_only:bool) {
    run_case_with_fault(probe,stream,video_intent,reference,resume_helper,background_only,BackgroundFault::None).await;
}
#[derive(Clone,Copy,PartialEq,Eq)]
enum BackgroundFault {None,CorruptNeighbor,RevokedKey,RemovedScope,HttpRace,Cleanup}
async fn run_case_with_fault(probe:bool,stream:bool,video_intent:bool,reference:bool,resume_helper:bool,background_only:bool,fault:BackgroundFault) {
    let corrupt_neighbor=fault==BackgroundFault::CorruptNeighbor;
    let dir=Directory(std::env::temp_dir().join(format!("core-public-budget-{:032x}",rand::random::<u128>())));
    let store=Arc::new(CoreStore::open(dir.path()).unwrap());store.migrate().unwrap();
    store.create_user(NewUser {id:"admin".into(),name:"Admin".into(),role:UserRole::Admin},"bootstrap").unwrap();
    store.create_user(NewUser {id:"user".into(),name:"User".into(),role:UserRole::User},"admin").unwrap();
    let a=store.issue_api_key("admin","admin",BTreeSet::from(["admin:*".into()]),"bootstrap").unwrap();let admin=store.authenticate_api_key(&a.plaintext).unwrap();
    let k=store.issue_api_key_as_admin_with_max_concurrency("user","Key",BTreeSet::from(["videos:submit".into(),"assets:write".into()]),if corrupt_neighbor {2} else {1},&admin).unwrap();
    store.key_quota_grant_as_admin(&admin,KeyQuotaGrant {api_key_id:k.id.clone(),resource_kind:"credits".into(),amount:100_000_000,actor_user_id:"admin".into(),reason:"isolated test".into()}).unwrap();
    store.set_video_billing_control(aiwork_core::VideoBillingControlInput {mode:aiwork_core::VideoBillingMode::Active,reason:"fixture".into(),diagnostic_key_id:None,diagnostic_request_hash:None}).unwrap();
    let bridge=Arc::new(Bridge {claims:Mutex::new(BTreeMap::new()),sends:AtomicUsize::new(0),video_intent,large_downloads:std::sync::atomic::AtomicBool::new(false),active_downloads:Arc::new(AtomicUsize::new(0)),download_status:AtomicUsize::new(200)});
    let mut cfg=RouterConfig::defaults(dir.path().into());cfg.budget_billing_v2=true;
    let state=StarlinkRouterState::for_test(store.clone(),BridgeClient::from_transport("http://bridge","bridge-only",bridge.clone()),cfg);
    let principal=store.authenticate_api_key(&k.plaintext).unwrap();
    let mut headers=HeaderMap::new();headers.insert("idempotency-key","once".parse().unwrap());
    if stream {headers.remove("idempotency-key");}
    let mut input=if probe {json!({"model":"seedance","messages":[{"role":"user","content":if video_intent {"生成5秒480p的猫视频"} else {"hello"}}],"stream":stream})} else {json!({"model":"seedance","prompt":"cat","duration":5,"resolution":"480p"})};
    if reference {input["messages"][0]["content"]=json!([
        {"type":"text","text":"以图片为参考生成5秒480p猫视频"},
        {"type":"image_url","image_url":{"url":"data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jN1sAAAAASUVORK5CYII="}}
    ]);}
    let body=Bytes::from(input.to_string());
    if resume_helper {
        use aiwork_core::{BeginRequest,BeginRequestInput,BudgetAuthorization,BudgetStepInput,BudgetStepKind,CreditAmount};
        let BeginRequest::Created(parent)=store.begin_billed_request(BeginRequestInput {user_id:principal.user_id.clone(),api_key_id:principal.key_id.clone(),protocol:"openai".into(),endpoint:"videos".into(),model:"seedance".into(),idempotency_key:"once".into(),body:input.clone()}).unwrap() else {panic!("parent")};
        let BeginRequest::Created(child)=store.begin_budget_assist_request(&parent.id,BeginRequestInput {user_id:principal.user_id.clone(),api_key_id:principal.key_id.clone(),protocol:"openai".into(),endpoint:"chat".into(),model:"deepseek-v4-flash".into(),idempotency_key:format!("budget-assist:{}",parent.id),body:json!({"messages":[{"role":"user","content":"生成5秒480p的猫视频"}]})}).unwrap() else {panic!("child")};
        let budget=format!("budget-{}",child.id);
        let claim=json!({"parent_request_id":parent.id,"request_id":child.id,"core_key_id":principal.key_id,"step_kind":"assist"});
        bridge.claims.lock().unwrap().insert(child.id.clone(),claim);
        store.begin_budget_operation(&parent.id,BudgetStepInput {kind:BudgetStepKind::Assist,authorization:BudgetAuthorization {
            budget_id:budget.clone(),parent_request_id:parent.id.clone(),request_id:child.id.clone(),core_key_id:principal.key_id.clone(),
            request_fingerprint:store.request_fingerprint_for_billing(&child.id).unwrap(),endpoint:"chat".into(),model:"deepseek-v4-flash".into(),account_ref:"account".into(),bridge_instance_id:"instance".into(),profile_fingerprint:"normalized-profile".into(),policy_version:"fixture".into(),hold_credits:CreditAmount::parse("2","credits").unwrap(),expires_at_ms:chrono::Utc::now().timestamp_millis()+60_000
        }}).unwrap();
        store.mark_budget_step_dispatched(&child.id,&budget).unwrap();
        let fingerprint=store.request_fingerprint_for_billing(&parent.id).unwrap();
        let context=format!("budget-continuation-v1:{}:{}:{}",parent.id,principal.key_id,fingerprint);
        let encrypted=state.key_vault.encrypt(&context,&input.to_string()).unwrap();
        store.save_budget_continuation(&principal,&parent.id,&input,encrypted.key_version,&encrypted.ciphertext).unwrap();
    }
    let state=if resume_helper {
        let reopened=Arc::new(CoreStore::open(dir.path()).unwrap());reopened.migrate().unwrap();
        StarlinkRouterState::for_test(reopened,state.bridge.lock().unwrap().clone(),state.config.clone())
    } else {state};
    if background_only {
        // Simulate a crash after step execution and billing committed but before
        // the parent operation was completed by its HTTP observer.
        let child=bridge.claims.lock().unwrap().keys().next().unwrap().clone();
        let step=store.budget_step_for_request(&child).unwrap().unwrap();
        store.mark_budget_step_execution(&child,aiwork_core::BudgetExecutionState::Succeeded).unwrap();
        store.apply_budget_receipt(aiwork_core::BudgetReceiptInput {
            budget_id:step.budget_id,account_ref:step.account_ref,bridge_instance_id:step.bridge_instance_id,
            receipt:aiwork_core::BillingReceipt {request_id:child,status:aiwork_core::BillingReceiptStatus::Final,
                actual_credits:Some(aiwork_core::CreditAmount::parse("1.234567","credits").unwrap()),unit:"credits".into(),
                source_ref:"fixture-session-final".into(),task_ref:None,observed_at_ms:chrono::Utc::now().timestamp_millis()},
        }).unwrap();
        for _ in 0..2 {starlink_dimension_router::user_routes::reconcile_pending_billing_requests_once(&state);}
        if video_intent {
            if corrupt_neighbor {
                use aiwork_core::{BeginRequest,BeginRequestInput,BudgetAuthorization,BudgetStepInput,BudgetStepKind,CreditAmount};
                let BeginRequest::Created(parent)=store.begin_billed_request(BeginRequestInput {user_id:principal.user_id.clone(),api_key_id:principal.key_id.clone(),protocol:"openai".into(),endpoint:"videos".into(),model:"seedance".into(),idempotency_key:"corrupt-neighbor".into(),body:input.clone()}).unwrap() else {panic!("parent")};
                let BeginRequest::Created(child)=store.begin_budget_assist_request(&parent.id,BeginRequestInput {user_id:principal.user_id.clone(),api_key_id:principal.key_id.clone(),protocol:"openai".into(),endpoint:"chat".into(),model:"deepseek-v4-flash".into(),idempotency_key:format!("budget-assist:{}",parent.id),body:json!({"messages":[]})}).unwrap() else {panic!("child")};
                bridge.claims.lock().unwrap().insert(child.id.clone(),json!({"parent_request_id":parent.id,"request_id":child.id,"core_key_id":principal.key_id,"step_kind":"assist"}));
                let budget=format!("budget-{}",child.id);
                store.begin_budget_operation(&parent.id,BudgetStepInput {kind:BudgetStepKind::Assist,authorization:BudgetAuthorization {
                    budget_id:budget.clone(),parent_request_id:parent.id.clone(),request_id:child.id.clone(),core_key_id:principal.key_id.clone(),
                    request_fingerprint:store.request_fingerprint_for_billing(&child.id).unwrap(),endpoint:"chat".into(),model:"deepseek-v4-flash".into(),account_ref:"account".into(),bridge_instance_id:"instance".into(),profile_fingerprint:"normalized-profile".into(),policy_version:"fixture".into(),hold_credits:CreditAmount::parse("2","credits").unwrap(),expires_at_ms:chrono::Utc::now().timestamp_millis()+60_000
                }}).unwrap();
                store.mark_budget_step_dispatched(&child.id,&budget).unwrap();store.mark_budget_step_execution(&child.id,aiwork_core::BudgetExecutionState::Succeeded).unwrap();
                let context=format!("budget-continuation-v1:{}:{}:{}",parent.id,principal.key_id,store.request_fingerprint_for_billing(&parent.id).unwrap());
                let encrypted=state.key_vault.encrypt(&context,&input.to_string()).unwrap();
                store.save_budget_continuation(&principal,&parent.id,&input,encrypted.key_version,&encrypted.ciphertext).unwrap();
                let db=rusqlite::Connection::open(dir.path().join("data").join(aiwork_core::CORE_DB_FILE)).unwrap();
                db.execute("UPDATE budget_continuations SET request_hash=?1 WHERE request_id=?2",rusqlite::params![vec![1u8;32],parent.id]).unwrap();
            }
            if matches!(fault,BackgroundFault::RevokedKey|BackgroundFault::RemovedScope) {
                let db=rusqlite::Connection::open(dir.path().join("data").join(aiwork_core::CORE_DB_FILE)).unwrap();
                if fault==BackgroundFault::RevokedKey {db.execute("UPDATE api_keys SET status='revoked' WHERE id=?1",[&k.id]).unwrap();}
                else {db.execute("UPDATE api_keys SET scopes_json='[\"models:read\"]' WHERE id=?1",[&k.id]).unwrap();}
            }
            let _router=starlink_dimension_router::server::build_router(state.clone());
            if fault==BackgroundFault::HttpRace {
                let r=user_routes::chat_completions(State(state.clone()),headers.clone(),Extension(principal.clone()),body.clone()).await;
                assert!(matches!(r.status(),StatusCode::OK|StatusCode::TOO_MANY_REQUESTS));
                let _=axum::body::to_bytes(r.into_body(),65536).await.unwrap();
            }
            tokio::time::timeout(std::time::Duration::from_secs(5),async {
                while store.active_execution_count_for_key(&k.id).unwrap()!=if corrupt_neighbor {1} else {0} {tokio::time::sleep(std::time::Duration::from_millis(20)).await;}
            }).await.expect("persisted successful helper must continue without client retry");
            assert_eq!(bridge.sends.load(Ordering::SeqCst),if matches!(fault,BackgroundFault::RevokedKey|BackgroundFault::RemovedScope) {0} else {1},"only an authorized not-yet-started video may be dispatched once");
            return;
        }
        assert_eq!(store.active_execution_count_for_key(&k.id).unwrap(),0,"finished text-only helper must not leave its parent occupying the sole execution slot");
        assert_eq!(bridge.sends.load(Ordering::SeqCst),0,"background recovery must not resend the helper");
        return;
    }
    if probe {
        for _ in 0..2 {
            let r=user_routes::chat_completions(State(state.clone()),headers.clone(),Extension(principal.clone()),body.clone()).await;
            assert_eq!(r.status(),StatusCode::OK,"model-add must use the bounded helper without a legacy quote");
            let bytes=axum::body::to_bytes(r.into_body(),65536).await.unwrap();
            let value:Value=if stream {
                let wire=std::str::from_utf8(&bytes).unwrap();assert!(wire.contains("data: [DONE]"));
                let frames:Vec<Value>=wire.lines().filter_map(|line|line.strip_prefix("data: ")).filter_map(|s|serde_json::from_str(s).ok()).collect();
                assert_eq!(frames[0]["choices"][0]["delta"]["role"],"assistant");
                assert!(frames[0]["choices"][0]["finish_reason"].is_null(),"first data frame must acknowledge an open stream before the final result");
                assert!(!frames.is_empty());let last=frames.last().unwrap().clone();assert_eq!(last["choices"][0]["delta"]["content"],"你好，连接正常。");last
            } else {serde_json::from_slice(&bytes).unwrap()};
            if !stream && !video_intent {assert_eq!(value["choices"][0]["message"]["content"],"你好，连接正常。");}
            if video_intent {assert_eq!(value["video_task"]["status"],"completed");}
            let op=store.budget_operation(value["request_id"].as_str().unwrap()).unwrap().unwrap();
            let saved=store.budget_continuation(value["request_id"].as_str().unwrap()).unwrap().expect("public admission persists recovery before helper dispatch");
            let restored=state.key_vault.decrypt(&saved.encryption_context(),saved.key_version,&saved.ciphertext).unwrap();
            assert_eq!(serde_json::from_str::<Value>(&restored).unwrap(),input);
            assert_eq!(op.steps.len(),if video_intent {2} else {1});assert_eq!(op.steps.iter().map(|s|s.hold_credits.as_microcredits()).sum::<i64>(),if video_intent {42_000_000} else {2_000_000});
            assert_eq!(op.steps[0].financial_state,aiwork_core::BudgetFinancialState::Held);
            assert_eq!(store.active_execution_count_for_key(&k.id).unwrap(),0);
        }
        assert_eq!(bridge.sends.load(Ordering::SeqCst),if resume_helper {1} else if video_intent {2} else {1},"a saved helper must never be dispatched again after restart");
        if reference {
            let claims=bridge.claims.lock().unwrap();
            let video=claims.values().find(|c|c["step_kind"]=="video").unwrap();
            assert_eq!(video["body"]["image_asset_ids"],json!(["bridge-image"]),"the prepared video must contain the uploaded reference, never discard it");
            assert!(!claims.values().find(|c|c["step_kind"]=="assist").unwrap().to_string().contains("iVBOR"),"binary references must not enter the text-only helper");
        }
        if fault==BackgroundFault::Cleanup {
            let id=bridge.claims.lock().unwrap().values().next().unwrap()["parent_request_id"].as_str().unwrap().to_owned();
            let before=store.budget_operation(&id).unwrap();
            let db=rusqlite::Connection::open(dir.path().join("data").join(aiwork_core::CORE_DB_FILE)).unwrap();
            let old=chrono::Utc::now().timestamp_millis()-86_400_001;
            db.execute("UPDATE budget_operations SET execution_finished_at_ms=?1 WHERE parent_request_id=?2",rusqlite::params![old,id]).unwrap();
            db.execute("UPDATE budget_continuations SET created_at_ms=?1 WHERE request_id=?2",rusqlite::params![old,id]).unwrap();
            let _router=starlink_dimension_router::server::build_router(state.clone());
            tokio::time::timeout(std::time::Duration::from_secs(5),async {
                while store.budget_continuation(&id).unwrap().is_some() {tokio::time::sleep(std::time::Duration::from_millis(20)).await;}
            }).await.expect("completed temporary input must be pruned by background maintenance");
            assert_eq!(store.budget_operation(&id).unwrap(),before,"financial state and results are independent of temporary input retention");
            assert_eq!(bridge.sends.load(Ordering::SeqCst),1,"maintenance must not dispatch paid requests");
        }
        return;
    }
    let r=user_routes::video_generations(State(state.clone()),headers.clone(),Extension(principal.clone()),body.clone()).await;
    assert_eq!(r.status(),StatusCode::ACCEPTED,"public route must bypass old quote_unavailable branch");
    let response:Value=serde_json::from_slice(&axum::body::to_bytes(r.into_body(),65536).await.unwrap()).unwrap();
    let id=response["task"]["id"].as_str().unwrap();
    let op=store.budget_operation(id).unwrap().unwrap();assert_eq!(op.steps[0].hold_credits.as_microcredits(),40_000_000);
    let r=user_routes::video_task(State(state.clone()),Path(id.to_string()),HeaderMap::new(),Extension(principal.clone())).await;
    assert_eq!(r.status(),StatusCode::OK);
    let result:Value=serde_json::from_slice(&axum::body::to_bytes(r.into_body(),65536).await.unwrap()).unwrap();
    assert_eq!(result["task"]["status"],"completed");
    assert_eq!(store.active_execution_count_for_key(&k.id).unwrap(),0);
    assert_eq!(store.budget_operation(id).unwrap().unwrap().steps[0].financial_state,aiwork_core::BudgetFinancialState::Held);
    let r=user_routes::video_content(State(state.clone()),Path(id.to_string()),HeaderMap::new(),Extension(principal.clone())).await;
    assert_eq!(r.status(),StatusCode::OK,"video download must not wait for its financial receipt");
    assert_eq!(&axum::body::to_bytes(r.into_body(),65536).await.unwrap()[..],b"fixture-mp4");
    bridge.download_status.store(429,Ordering::SeqCst);
    let busy=user_routes::video_content(State(state.clone()),Path(id.to_string()),HeaderMap::new(),Extension(principal.clone())).await;
    assert_eq!(busy.status(),StatusCode::TOO_MANY_REQUESTS,"upstream artifact recovery contention is retryable, not a misleading 503");
    assert!(busy.headers().contains_key("retry-after"));drop(busy);
    assert_eq!(store.active_execution_count_for_key(&k.id).unwrap(),0);
    assert_eq!(bridge.sends.load(Ordering::SeqCst),1,"download retries must not generate again");
    bridge.download_status.store(200,Ordering::SeqCst);
    bridge.large_downloads.store(true,Ordering::SeqCst);
    let mut downloads=Vec::new();
    for _ in 0..4 {
        let r=user_routes::video_content(State(state.clone()),Path(id.to_string()),HeaderMap::new(),Extension(principal.clone())).await;
        assert_eq!(r.status(),StatusCode::OK);downloads.push(r);
    }
    let fifth=user_routes::video_content(State(state.clone()),Path(id.to_string()),HeaderMap::new(),Extension(principal.clone())).await;
    let fifth_status=fifth.status();drop(fifth);drop(downloads);
    tokio::time::timeout(std::time::Duration::from_secs(2),async {
        while bridge.active_downloads.load(Ordering::SeqCst)>0 {tokio::time::sleep(std::time::Duration::from_millis(5)).await;}
    }).await.expect("disconnect must release download worker");
    assert_eq!(fifth_status,StatusCode::TOO_MANY_REQUESTS,"slow consumers must not create unbounded blocking readers");
    bridge.large_downloads.store(false,Ordering::SeqCst);
    let again=user_routes::video_content(State(state.clone()),Path(id.to_string()),HeaderMap::new(),Extension(principal.clone())).await;
    assert_eq!(again.status(),StatusCode::OK,"released download permit must be reusable");drop(again);
    let mut other=principal.clone();other.key_id="unrelated-key".into();
    let r=user_routes::video_content(State(state.clone()),Path(id.to_string()),HeaderMap::new(),Extension(other)).await;
    assert_eq!(r.status(),StatusCode::NOT_FOUND,"same-user keys must not share video access");
    let r=user_routes::video_generations(State(state),headers,Extension(principal),body).await;
    assert_eq!(r.status(),StatusCode::ACCEPTED);assert_eq!(bridge.sends.load(Ordering::SeqCst),1);
}
