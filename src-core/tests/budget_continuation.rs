use std::{collections::BTreeSet,sync::Arc};
use aiwork_core::*;
use serde_json::json;
struct Directory(std::path::PathBuf);
impl Drop for Directory {fn drop(&mut self) {let _=std::fs::remove_dir_all(&self.0);}}

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
