use std::{collections::BTreeSet,sync::Arc};
use aiwork_core::*;
use serde_json::json;
struct Directory(std::path::PathBuf);
impl Drop for Directory {fn drop(&mut self) {let _=std::fs::remove_dir_all(&self.0);}}

#[test]
fn completed_checkpoint_retention_is_bounded_and_never_releases_financial_holds() {
    let dir=Directory(std::env::temp_dir().join(format!("core-continuation-retention-{}",rand::random::<u128>())));
    let store=CoreStore::open(&dir.0).unwrap();store.migrate().unwrap();
    store.create_user(NewUser {id:"admin".into(),name:"Admin".into(),role:UserRole::Admin},"bootstrap").unwrap();
    store.create_user(NewUser {id:"user".into(),name:"User".into(),role:UserRole::User},"admin").unwrap();
    let admin_key=store.issue_api_key("admin","admin",BTreeSet::from(["admin:*".into()]),"bootstrap").unwrap();
    let admin=store.authenticate_api_key(&admin_key.plaintext).unwrap();
    let key=store.issue_api_key_as_admin_with_max_concurrency("user","test",BTreeSet::from(["videos:submit".into()]),8,&admin).unwrap();
    let principal=store.authenticate_api_key(&key.plaintext).unwrap();
    store.key_quota_grant_as_admin(&admin,KeyQuotaGrant {api_key_id:key.id.clone(),resource_kind:"credits".into(),amount:100_000_000,actor_user_id:"admin".into(),reason:"isolated test".into()}).unwrap();
    let body=json!({"model":"seedance","prompt":"original instruction"});
    let mut ids=Vec::new();
    for index in 0..5 {
        let BeginRequest::Created(request)=store.begin_billed_request(BeginRequestInput {user_id:"user".into(),api_key_id:key.id.clone(),protocol:"openai".into(),endpoint:"videos".into(),model:"seedance".into(),idempotency_key:format!("request-{index}"),body:body.clone()}).unwrap() else {panic!("request")};
        store.save_budget_continuation(&principal,&request.id,&body,1,&[0x51;64]).unwrap();
        let budget=format!("budget-{index}");
        store.begin_budget_operation(&request.id,BudgetStepInput {kind:BudgetStepKind::Video,authorization:BudgetAuthorization {
            budget_id:budget.clone(),parent_request_id:request.id.clone(),request_id:request.id.clone(),core_key_id:key.id.clone(),
            request_fingerprint:store.request_fingerprint_for_billing(&request.id).unwrap(),endpoint:"videos".into(),model:"seedance".into(),account_ref:"account".into(),bridge_instance_id:"bridge".into(),profile_fingerprint:"profile".into(),policy_version:"policy".into(),hold_credits:CreditAmount::parse("1","credits").unwrap(),expires_at_ms:chrono::Utc::now().timestamp_millis()+60_000,
        }}).unwrap();
        store.mark_budget_step_dispatched(&request.id,&budget).unwrap();
        if index<4 {
            store.bind_budget_video_task(&request.id,&format!("task-{index}")).unwrap();
            store.mark_budget_step_execution(&request.id,BudgetExecutionState::Succeeded).unwrap();
            store.finish_budget_execution(&request.id,BudgetExecutionState::Succeeded).unwrap();
        }
        ids.push(request.id);
    }
    // Two old completed records, one recent completion, one recently re-saved
    // body, and one old running request. Financial receipts are still unknown.
    let now=chrono::Utc::now().timestamp_millis();let old=now-86_400_001;
    let connection=rusqlite::Connection::open(dir.0.join("data").join(CORE_DB_FILE)).unwrap();
    for (index,id) in ids.iter().enumerate() {
        if index!=3 {connection.execute("UPDATE budget_continuations SET created_at_ms=?1 WHERE request_id=?2",rusqlite::params![old,id]).unwrap();}
        if index!=2 {connection.execute("UPDATE budget_operations SET execution_finished_at_ms=?1 WHERE parent_request_id=?2 AND execution_state='succeeded'",rusqlite::params![old,id]).unwrap();}
    }
    let before=ids.iter().map(|id|store.budget_operation(id).unwrap()).collect::<Vec<_>>();
    let holds_before:i64=connection.query_row("SELECT SUM(hold_microcredits) FROM budget_steps WHERE financial_state='held'",[],|r|r.get(0)).unwrap();
    assert_eq!(holds_before,5_000_000);
    assert_eq!(store.prune_completed_budget_continuations(now,1).unwrap(),1,"one pass must respect its row bound");
    assert_eq!(store.prune_completed_budget_continuations(now,8).unwrap(),1);
    assert_eq!(store.prune_completed_budget_continuations(now,8).unwrap(),0);
    for (index,id) in ids.iter().enumerate() {
        assert_eq!(store.budget_continuation(id).unwrap().is_some(),index>=2);
        assert_eq!(store.budget_operation(id).unwrap(),before[index],"cleanup must not change execution or billing facts");
    }
    let holds_after:i64=connection.query_row("SELECT SUM(hold_microcredits) FROM budget_steps WHERE financial_state='held'",[],|r|r.get(0)).unwrap();
    assert_eq!(holds_after,holds_before);
    assert!(store.prune_completed_budget_continuations(now,0).is_err());
    assert!(store.prune_completed_budget_continuations(now,101).is_err());
    drop(connection);drop(store);
}

#[test]
fn continuation_is_request_bound_immutable_and_survives_reopen() {
    let dir=Directory(std::env::temp_dir().join(format!("core-continuation-{}",rand::random::<u128>())));
    let store=Arc::new(CoreStore::open(&dir.0).unwrap());store.migrate().unwrap();
    store.create_user(NewUser {id:"admin".into(),name:"Admin".into(),role:UserRole::Admin},"bootstrap").unwrap();
    store.create_user(NewUser {id:"user".into(),name:"User".into(),role:UserRole::User},"admin").unwrap();
    let key=store.issue_api_key("user","test",BTreeSet::from(["videos:submit".into()]),"admin").unwrap();
    let principal=store.authenticate_api_key(&key.plaintext).unwrap();
    let body=json!({"model":"seedance","messages":[{"role":"user","content":"secret original video instruction"}]});
    let BeginRequest::Created(request)=store.begin_billed_request(BeginRequestInput {user_id:"user".into(),api_key_id:key.id.clone(),protocol:"openai".into(),endpoint:"videos".into(),model:"seedance".into(),idempotency_key:"once".into(),body:body.clone()}).unwrap() else {panic!("request")};
    let ciphertext=vec![0x51;64];
    assert!(store.save_budget_continuation(&principal,&request.id,&body,1,&ciphertext).unwrap());
    assert!(!store.save_budget_continuation(&principal,&request.id,&body,1,&[0x52;64]).unwrap(),"retry cannot overwrite a saved checkpoint");
    assert!(store.save_budget_continuation(&principal,&request.id,&json!({"prompt":"changed"}),1,&ciphertext).is_err());
    let mut other=principal.clone();other.key_id="wrong-key".into();
    assert!(store.save_budget_continuation(&other,&request.id,&body,1,&ciphertext).is_err());
    let reopened=CoreStore::open(&dir.0).unwrap();reopened.migrate().unwrap();
    let saved=reopened.budget_continuation(&request.id).unwrap().unwrap();
    assert_eq!(saved.ciphertext,ciphertext);assert_eq!(saved.key_version,1);
    assert_eq!(saved.principal.unwrap(),principal);
    assert_eq!(saved.fingerprint,store.request_fingerprint_for_billing(&request.id).unwrap());
    assert!(reopened.pending_budget_continuations_after("",100).unwrap().is_empty(),"a checkpoint without a successful helper cannot start paid work");
    let connection=rusqlite::Connection::open(dir.0.join("data").join(CORE_DB_FILE)).unwrap();
    connection.execute("UPDATE api_keys SET status='revoked' WHERE id=?1",[&key.id]).unwrap();
    assert!(reopened.budget_continuation(&request.id).unwrap().unwrap().principal.is_none(),"recovery cannot reuse submit-time authorization after revocation");
    assert!(reopened.save_budget_continuation(&principal,&request.id,&body,1,&ciphertext).is_err());
    connection.execute("UPDATE budget_continuations SET request_hash=?1 WHERE request_id=?2",rusqlite::params![vec![1u8;32],request.id]).unwrap();
    assert!(reopened.budget_continuation(&request.id).is_err(),"cross-request checkpoint replacement must fail binding checks");
    drop(connection);
    drop(reopened);drop(store);
}
