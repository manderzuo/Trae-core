use std::{collections::{BTreeMap, BTreeSet}, sync::{Arc, Mutex}, path::PathBuf};
use aiwork_core::{CoreStore, NewUser, UserRole, KeyQuotaGrant, BeginRequest, BeginRequestInput,
    BudgetAuthorization, BudgetStepInput, BudgetStepKind, BudgetFinancialState, CreditAmount};
use serde_json::{json, Value};
use starlink_dimension_router::{bridge_client::{BridgeClient, BridgeTransport, BridgeResponse},
    state::StarlinkRouterState, config::RouterConfig, user_routes::reconcile_pending_billing_requests_once};

struct Replies { execution: Value, billing: Mutex<Value>, events:Mutex<Vec<Value>>, generation:Mutex<String>, event_offsets:Mutex<Vec<i64>>, configuration_lock: Mutex<Option<std::sync::Weak<Mutex<BridgeClient>>>>,
    event_gate:Mutex<Option<std::sync::mpsc::Receiver<()>>>, execution_seen:Mutex<Option<std::sync::mpsc::Sender<()>>> }
impl BridgeTransport for Replies {
    fn send(&self, method: &str, url: &str, headers: &BTreeMap<String,String>, _: &[u8]) -> Result<BridgeResponse,String> {
        assert_eq!(method, "GET", "recovery must never create a paid task");
        assert_eq!(headers.get("authorization").unwrap(), "Bearer test-bridge-only");
        if let Some(lock)=self.configuration_lock.lock().unwrap().as_ref().and_then(|weak|weak.upgrade()) {
            if lock.try_lock().is_err() {return Err("network was called while holding shared bridge configuration lock".into());}
        }
        let value = if url.contains("/receipt-events?") {
                if let Some(gate)=self.event_gate.lock().unwrap().take() {gate.recv().unwrap();}
                let after=url.split("after=").nth(1).and_then(|s|s.split('&').next()).unwrap_or("0").parse::<i64>().unwrap();
                self.event_offsets.lock().unwrap().push(after);
                json!({"wire_version":2,"bridge_instance_id":"instance","generation":self.generation.lock().unwrap().clone(),"events":self.events.lock().unwrap().iter().filter(|v|v["sequence"].as_i64().unwrap()>after).cloned().collect::<Vec<_>>()})
            } else if url.ends_with("/v1/models") {json!({"data":[]})}
            else if url.contains("/execution?") { if let Some(seen)=self.execution_seen.lock().unwrap().take() {seen.send(()).unwrap();} self.execution.clone() }
            else if url.contains("/billing?") { self.billing.lock().unwrap().clone() }
            else if url.contains("/result?") {let mut v=self.execution.clone();v["status"]=json!("not_ready");v["result"]=Value::Null;v}
            else { return Err("unexpected recovery route".into()); };
        Ok(BridgeResponse {status:200,headers:BTreeMap::new(),body:serde_json::to_vec(&value).unwrap()})
    }
}
struct Directory(PathBuf);
impl Drop for Directory { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }

#[test]
fn router_v2_recovery_frees_concurrency_before_bill_and_settles_exactly_once() {
    run_recovery_case(false);
}
#[test]
fn router_v2_confirmed_no_send_releases_both_budget_and_execution() {
    run_recovery_case(true);
}
fn run_recovery_case(no_send: bool) {
    run_recovery_with_dispatch(no_send,true);
}
#[test]
fn prepared_but_never_dispatched_core_step_releases_on_verified_no_send() {
    run_recovery_with_dispatch(true,false);
}
fn run_recovery_with_dispatch(no_send:bool,dispatched:bool) {
    run_recovery_with_event_gate(no_send,dispatched,false);
}
#[test]
fn slow_receipt_event_feed_does_not_block_request_recovery_or_slot_release() {
    run_recovery_with_event_gate(false,true,true);
}
fn run_recovery_with_event_gate(no_send:bool,dispatched:bool,block_events:bool) {
    let directory = Directory(std::env::temp_dir().join(format!("core-router-budget-{:032x}",rand::random::<u128>())));
    let store = Arc::new(CoreStore::open(&directory.0).unwrap()); store.migrate().unwrap();
    store.create_user(NewUser {id:"admin".into(),name:"Admin".into(),role:UserRole::Admin},"bootstrap").unwrap();
    store.create_user(NewUser {id:"user".into(),name:"User".into(),role:UserRole::User},"admin").unwrap();
    let admin_key = store.issue_api_key("admin","admin",BTreeSet::from(["admin:*".into()]),"bootstrap").unwrap();
    let admin = store.authenticate_api_key(&admin_key.plaintext).unwrap();
    let key = store.issue_api_key_as_admin_with_max_concurrency("user","key",BTreeSet::from(["video:submit".into()]),1,&admin).unwrap();
    store.key_quota_grant_as_admin(&admin,KeyQuotaGrant {api_key_id:key.id.clone(),resource_kind:"credits".into(),amount:100_000_000,actor_user_id:"admin".into(),reason:"isolated test".into()}).unwrap();
    let BeginRequest::Created(request) = store.begin_billed_request(BeginRequestInput {user_id:"user".into(),api_key_id:key.id.clone(),protocol:"openai".into(),endpoint:"videos".into(),model:"seedance".into(),idempotency_key:"first".into(),body:json!({"prompt":"test"})}).unwrap() else {panic!("request")};
    let auth = BudgetAuthorization {budget_id:"budget-a".into(),parent_request_id:request.id.clone(),request_id:request.id.clone(),core_key_id:key.id.clone(),
        request_fingerprint:store.request_fingerprint_for_billing(&request.id).unwrap(),endpoint:"videos".into(),model:"seedance".into(),account_ref:"account".into(),bridge_instance_id:"instance".into(),profile_fingerprint:"profile".into(),policy_version:"test-v1".into(),hold_credits:CreditAmount::parse("40","credits").unwrap(),expires_at_ms:chrono::Utc::now().timestamp_millis()+60_000};
    store.begin_budget_operation(&request.id,BudgetStepInput {kind:BudgetStepKind::Video,authorization:auth}).unwrap();
    if dispatched {store.mark_budget_step_dispatched(&request.id,"budget-a").unwrap();}
    let mut envelope = json!({"wire_version":2,"request_id":request.id,"core_key_id":key.id,"budget_id":"budget-a","account_ref":"account","bridge_instance_id":"instance"});
    let mut execution = envelope.clone(); execution["status"]=json!("succeeded");
    execution["execution"] = json!({"request_id":request.id,"core_key_id":key.id,"budget_id":"budget-a","account_ref":"account","bridge_instance_id":"instance","state":"succeeded","step_kind":"video","task_ref":"task-a","started_at_ms":1,"finished_at_ms":2,"result_available":true});
    if no_send {execution["status"]=json!("not_started");execution["execution"]=Value::Null;}
    envelope["status"]=json!("pending"); envelope["event"]=Value::Null; envelope["receipt"]=Value::Null;
    let replies = Arc::new(Replies {execution,billing:Mutex::new(envelope.clone()),events:Mutex::new(Vec::new()),generation:Mutex::new("generation".into()),event_offsets:Mutex::new(Vec::new()),configuration_lock:Mutex::new(None),event_gate:Mutex::new(None),execution_seen:Mutex::new(None)});
    let state = StarlinkRouterState::for_test(store.clone(),BridgeClient::from_transport("http://bridge","test-bridge-only",replies.clone()),RouterConfig::defaults(directory.0.clone()));
    *replies.configuration_lock.lock().unwrap()=Some(Arc::downgrade(&state.bridge));
    let principal=store.authenticate_api_key(&key.plaintext).unwrap();
    let models=tokio::runtime::Runtime::new().unwrap().block_on(starlink_dimension_router::user_routes::models(
        axum::extract::State(state.clone()),axum::http::HeaderMap::new(),axum::Extension(principal)));
    assert_eq!(models.status(),axum::http::StatusCode::OK,"legacy routes must not hold the lock needed by the v2 reconciler across I/O");
    if block_events {
        let (release,gate)=std::sync::mpsc::channel();let (seen,received)=std::sync::mpsc::channel();
        *replies.event_gate.lock().unwrap()=Some(gate);*replies.execution_seen.lock().unwrap()=Some(seen);
        let s=state.clone();let worker=std::thread::spawn(move ||reconcile_pending_billing_requests_once(&s));
        let independent=received.recv_timeout(std::time::Duration::from_secs(1)).is_ok();
        release.send(()).unwrap();worker.join().unwrap();
        assert!(independent,"slow event discovery must not delay request recovery");
    } else {reconcile_pending_billing_requests_once(&state);}
    assert_eq!(store.active_execution_count_for_key(&key.id).unwrap(),if no_send {1} else {0},"only a proven terminal result releases execution before receipt");
    assert_eq!(store.budget_operation(&request.id).unwrap().unwrap().steps[0].financial_state,BudgetFinancialState::Held);
    let mut receipt = json!({"request_id":request.id,"status":"final","actual_credits":"12.345678","unit":"credits","source_ref":"session-final","task_ref":"task-a","observed_at_ms":chrono::Utc::now().timestamp_millis()});
    if no_send {receipt["status"]=json!("failed_no_charge");receipt["actual_credits"]=json!("0");receipt["source_ref"]=json!("aiwork-v2-local-no-send:proof-a");receipt["task_ref"]=Value::Null;}
    envelope["status"]=receipt["status"].clone(); envelope["receipt"]=receipt.clone();
    envelope["event"]=json!({"wire_version":2,"generation":"generation","sequence":1,"event_id":"event-1","request_id":request.id,"core_key_id":key.id,"budget_id":"budget-a","account_ref":"account","bridge_instance_id":"instance","kind":"final","receipt":receipt,"conflict":null,"confirmation_policy":"post-terminal-session-observation-v1","evidence_hash":"hash"});
    if no_send {envelope["event"]["kind"]=json!("failed_no_charge");envelope["event"]["confirmation_policy"]=json!("durable-local-no-send-v1");}
    *replies.billing.lock().unwrap()=envelope;
    if no_send {
        let principal=store.authenticate_api_key(&key.plaintext).unwrap();
        let rt=tokio::runtime::Runtime::new().unwrap();
        let response=rt.block_on(starlink_dimension_router::user_routes::video_task(
            axum::extract::State(state.clone()),axum::extract::Path(request.id.clone()),axum::http::HeaderMap::new(),axum::Extension(principal)));
        assert_eq!(response.status(),axum::http::StatusCode::OK);
        let bytes=rt.block_on(axum::body::to_bytes(response.into_body(),65536)).unwrap();
        let value:Value=serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["task"]["status"],"failed","durable no-send must stop client polling, not remain processing");
    }
    reconcile_pending_billing_requests_once(&state);
    reconcile_pending_billing_requests_once(&state);
    let step=store.budget_operation(&request.id).unwrap().unwrap().steps.remove(0);
    assert_eq!(step.financial_state,if dispatched {BudgetFinancialState::Settled} else {BudgetFinancialState::Released});
    if dispatched {assert_eq!(step.actual_credits.unwrap().as_microcredits(),if no_send {0} else {12_345_678});}
    else {assert!(step.actual_credits.is_none(),"never dispatched is a released hold, not an invented upstream zero bill");}
    assert_eq!(store.active_execution_count_for_key(&key.id).unwrap(),0);
    assert!(store.pending_budget_steps(100).unwrap().is_empty());
    if !no_send {
        let mut conflict=replies.billing.lock().unwrap().clone();
        conflict["status"]=json!("conflict");conflict["event"]["kind"]=json!("conflict");
        conflict["event"]["evidence_hash"]=json!("late-conflict");conflict["event"]["event_id"]=json!("event-2");
        conflict["event"]["sequence"]=json!(2);
        conflict["event"]["receipt"]["actual_credits"]=json!("13");
        replies.events.lock().unwrap().push(conflict["event"].clone());
        *replies.generation.lock().unwrap()="after-bridge-restart".into();
        // Request-only pending scans no longer include this settled step. The
        // event channel must still quarantine a later incompatible receipt.
        *replies.billing.lock().unwrap()=conflict;
        reconcile_pending_billing_requests_once(&state);
        let step=store.budget_operation(&request.id).unwrap().unwrap().steps.remove(0);
        assert_eq!(step.financial_state,BudgetFinancialState::Conflict);
        assert_eq!(step.actual_credits.unwrap().as_microcredits(),12_345_678,"late evidence must not debit twice");
        let reopened=Arc::new(CoreStore::open(&directory.0).unwrap());reopened.migrate().unwrap();
        let restarted=StarlinkRouterState::for_test(reopened,BridgeClient::from_transport("http://bridge","test-bridge-only",replies.clone()),RouterConfig::defaults(directory.0.clone()));
        reconcile_pending_billing_requests_once(&restarted);
        assert_eq!(replies.event_offsets.lock().unwrap().last(),Some(&2),"durable event cursor must survive Core restart");
    }
}
