#[path="support/work_fixture.rs"]
mod fixture;
use fixture::*;

async fn send(app:&axum::Router,key:&str,path:&str,id:&str,body:&Value)->Value {
    let response=app.clone().oneshot(Request::post(path).header("authorization",format!("Bearer {key}")).header("idempotency-key",id).header("content-type","application/json").body(Body::from(body.to_string())).unwrap()).await.unwrap();
    serde_json::from_slice(&to_bytes(response.into_body(),256*1024).await.unwrap()).unwrap()
}
async fn idle(state:&StarlinkRouterState) {
    tokio::time::timeout(std::time::Duration::from_secs(3),async {
        while !state.video_stream_observers.lock().unwrap().is_empty() {tokio::time::sleep(std::time::Duration::from_millis(5)).await;}
    }).await.unwrap();
}
fn reopened(f:&Fixture)->(Arc<StarlinkRouterState>,axum::Router) {
    let store=Arc::new(CoreStore::open(&f.dir).unwrap());store.migrate().unwrap();
    let state=StarlinkRouterState::for_test(store,BridgeClient::from_transport("http://bridge","bridge-only",f.bridge.clone()),f.state.config.clone());
    let app=starlink_dimension_router::server::build_router(state.clone());(state,app)
}

#[tokio::test]
async fn foreground_replay_obeys_backoff_and_attention_on_both_entrypoints() {
    for path in ["/v1/chat/completions","/v1/videos/generations"] {
        for disposition in ["retry","attention"] {
            let f=Fixture::new();
            let body=if path.contains("chat") {create()} else {json!({"model":"seedance","prompt":"生成橘猫散步的视频","duration":5,"resolution":"480p","ratio":"9:16"})};
            *f.bridge.video_prepare_error.lock().unwrap()=Some((429,"bridge_workers_busy".into()));
            let _=send(&f.app,&f.key,path,"foreground-gate",&body).await;
            tokio::time::timeout(std::time::Duration::from_secs(3),async {while f.bridge.video_prepares.load(Ordering::SeqCst)==0 {tokio::time::sleep(std::time::Duration::from_millis(5)).await;}}).await.unwrap();idle(&f.state).await;
            let request=f.bridge.claims.lock().unwrap().values().find(|v|v["step_kind"]=="assist").unwrap()["parent_request_id"].as_str().unwrap().to_owned();
            let db=rusqlite::Connection::open(f.dir.join("data/core.sqlite3")).unwrap();
            db.execute("INSERT INTO budget_continuation_retries(request_id,failures,next_attempt_at_ms,state,last_error_code) VALUES (?1,?2,?3,?4,'bridge_workers_busy') ON CONFLICT(request_id) DO UPDATE SET state=excluded.state,failures=excluded.failures,next_attempt_at_ms=excluded.next_attempt_at_ms",rusqlite::params![request,if disposition=="attention"{8}else{1},chrono::Utc::now().timestamp_millis()+60_000,disposition]).unwrap();
            let steps=f.state.store.budget_operation(&request).unwrap().unwrap().steps;
            let prepares=f.bridge.video_prepares.load(Ordering::SeqCst);
            *f.bridge.video_prepare_error.lock().unwrap()=None;
            let (state,app)=reopened(&f);
            for _ in 0..2 {
                let reply=send(&app,&f.key,path,"foreground-gate",&body).await;idle(&state).await;
                assert_eq!(reply["error"]["code"],if disposition=="attention"{"budget_continuation_requires_attention"}else{"budget_continuation_deferred"},"{reply}");
            }
            assert_eq!(f.bridge.video_prepares.load(Ordering::SeqCst),prepares,"{path}: {disposition} replay performed preparation");
            assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst),0,"{path}: {disposition} replay sent video");
            assert_eq!(state.store.budget_operation(&request).unwrap().unwrap().steps,steps);
            let failures:i64=db.query_row("SELECT failures FROM budget_continuation_retries WHERE request_id=?1",[&request],|r|r.get(0)).unwrap();
            assert_eq!(failures,if disposition=="attention"{8}else{1},"a blocked replay cannot consume a recovery attempt");
        }
    }
}

#[tokio::test]
async fn foreground_continuation_failure_records_backoff_once() {
    let f=Fixture::new();*f.bridge.video_prepare_error.lock().unwrap()=Some((429,"bridge_workers_busy".into()));
    let reply=f.response(&f.key,"record-foreground",&create()).await;let _=to_bytes(reply.into_body(),256*1024).await.unwrap();idle(&f.state).await;
    let request=f.bridge.claims.lock().unwrap().values().find(|v|v["step_kind"]=="assist").unwrap()["parent_request_id"].as_str().unwrap().to_owned();
    let db=rusqlite::Connection::open(f.dir.join("data/core.sqlite3")).unwrap();
    let rows:i64=db.query_row("SELECT COUNT(*) FROM budget_continuation_retries WHERE request_id=?1 AND failures=1 AND state='retry' AND next_attempt_at_ms>?2",rusqlite::params![request,chrono::Utc::now().timestamp_millis()],|r|r.get(0)).unwrap();
    assert_eq!(rows,1,"the foreground owner must persist its failed continuation before returning");
}


fn upload_image(f:&Fixture,label:&str)->(aiwork_core::CoreAsset,Vec<u8>) {
    let mut bytes=b"\x89PNG\r\n\x1a\n".to_vec();bytes.extend_from_slice(label.as_bytes());
    let stored=starlink_dimension_router::assets::write_asset(&f.dir,&f.owner,starlink_dimension_router::assets::ParsedAssetUpload {filename:format!("{label}.png"),declared_mime:Some("image/png".into()),bytes:bytes.clone()}).unwrap();
    (starlink_dimension_router::assets::persist_asset(&f.state.store,&f.owner,&stored).unwrap(),bytes)
}
fn marker(f:&Fixture,key_id:&str,asset:&str,expires:i64)->String {
    use base64::Engine;
    let sealed=f.state.key_vault.encrypt(&format!("seedance-reference-v1:{key_id}"),&json!({"asset_ids":[asset],"expires_at_ms":expires}).to_string()).unwrap();
    format!("[AIWORK_REFERENCE:{}.{}]",sealed.key_version,base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sealed.ciphertext))
}
fn make_due(f:&Fixture,request:&str) {
    rusqlite::Connection::open(f.dir.join("data/core.sqlite3")).unwrap().execute("UPDATE budget_continuation_retries SET next_attempt_at_ms=0 WHERE request_id=?1 AND state='retry'",[request]).unwrap();
}

#[tokio::test]
async fn independent_create_restores_current_turn_references_after_expiry_and_restart() {
    use sha2::{Digest,Sha256};
    for via_marker in [false,true] {
        let f=Fixture::new();
        let (parent_asset,_)=upload_image(&f,"parent");let mut parent_body=create();parent_body["image_asset_ids"]=json!([parent_asset.id]);
        let parent=f.chat("review-parent",&parent_body).await;
        let (current,bytes)=upload_image(&f,"current");
        let text=if via_marker {format!("独立生成新视频 {}",marker(&f,&f.owner.key_id,&current.id,chrono::Utc::now().timestamp_millis()+3_600_000))}else{"独立生成新视频".into()};
        let mut body=revise(&parent,&text);
        if !via_marker {body["image_asset_ids"]=json!([current.id]);}
        f.bridge.asset_upload_failure.store(true,Ordering::SeqCst);
        let initial=send(&f.app,&f.key,"/v1/chat/completions","new-work-with-reference",&body).await;idle(&f.state).await;
        let request=initial["request_id"].as_str().unwrap().to_owned();
        assert!(f.state.store.work_version_for_request(&f.owner,&request).unwrap().is_none(),"must stop before binding");
        let db=rusqlite::Connection::open(f.dir.join("data/core.sqlite3")).unwrap();
        if via_marker {
            // v2 checkpoints from the initial stability build had no origin tag.
            let saved=f.state.store.budget_continuation(&request).unwrap().unwrap();
            let raw=f.state.key_vault.decrypt(&saved.encryption_context(),saved.key_version,&saved.ciphertext).unwrap();
            let mut envelope:Value=serde_json::from_str(&raw).unwrap();envelope.as_object_mut().unwrap().remove("retained_input_origin");
            let sealed=f.state.key_vault.encrypt(&saved.encryption_context(),&envelope.to_string()).unwrap();
            db.execute("UPDATE budget_continuations SET key_version=?1,ciphertext=?2 WHERE request_id=?3",rusqlite::params![sealed.key_version,sealed.ciphertext,request]).unwrap();
        }
        db.execute("UPDATE assets SET created_at_ms=0,expires_at_ms=1",[]).unwrap();
        starlink_dimension_router::assets::cleanup_expired_assets_once(&f.state.store,&f.dir,chrono::Utc::now().timestamp_millis()).unwrap();
        f.bridge.asset_upload_failure.store(false,Ordering::SeqCst);make_due(&f,&request);
        let (state,app)=reopened(&f);
        let result=send(&app,&f.key,"/v1/chat/completions","new-work-with-reference",&body).await;
        assert!(result["error"].is_null(),"marker={via_marker}: {result}");
        let version=state.store.work_version_for_request(&f.owner,&request).unwrap().unwrap();
        let snapshot=work_context::read_snapshot(&state,&f.owner,&version).unwrap();
        assert_eq!(snapshot.user_media_ids.len(),1);
        let media=state.store.owned_work_media(&f.owner,&snapshot.user_media_ids[0]).unwrap().unwrap();
        assert_eq!(media.content_sha256,format!("{:x}",Sha256::digest(bytes)),"must preserve this turn's bytes, not parent media");
        assert_ne!(result["work_context"]["work_id"],parent["work_context"]["work_id"]);
        assert_eq!(f.bridge.assist_sends.load(Ordering::SeqCst),2);assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst),2);
    }
}


#[tokio::test]
async fn legacy_marker_recovery_classifies_owned_input_without_trusting_foreign_or_malformed_markers() {
    for mode in ["expired","missing","temporary","foreign","malformed","live"] {
        let f=Fixture::new();let (asset,_)=upload_image(&f,"legacy-reference");
        let mut body=create();body["messages"][0]["content"]=json!(format!("生成新视频 {}",marker(&f,&f.owner.key_id,&asset.id,chrono::Utc::now().timestamp_millis()+3_600_000)));
        f.bridge.asset_upload_failure.store(true,Ordering::SeqCst);
        let initial=send(&f.app,&f.key,"/v1/chat/completions","legacy-marker",&body).await;idle(&f.state).await;
        let request=initial["request_id"].as_str().unwrap().to_owned();
        assert!(f.state.store.work_version_for_request(&f.owner,&request).unwrap().is_none());
        let saved=f.state.store.budget_continuation(&request).unwrap().unwrap();
        let plain=f.state.key_vault.decrypt(&saved.encryption_context(),saved.key_version,&saved.ciphertext).unwrap();
        let mut envelope:Value=serde_json::from_str(&plain).unwrap();
        // Build an accepted pre-upgrade checkpoint at its original past time.
        // Fixture-only hashing avoids a wall-clock sleep and leaves paid child
        // identity/financial evidence unchanged. Production never rewrites it.
        let token=if mode=="malformed" {"[AIWORK_REFERENCE:broken]".into()}else {marker(&f,if mode=="foreign"{"other-key"}else{&f.owner.key_id},&asset.id,1)};
        body["messages"][0]["content"]=json!(format!("生成新视频 {token}"));
        envelope["format"]=json!("aiwork-work-checkpoint-v1");envelope["body"]=body.clone();
        envelope.as_object_mut().unwrap().remove("retained_inputs");envelope.as_object_mut().unwrap().remove("retained_input_origin");
        let hash=aiwork_core::canonical_json_hash(&json!({"endpoint":"videos","model":"seedance","body":body}));
        let db=rusqlite::Connection::open(f.dir.join("data/core.sqlite3")).unwrap();
        db.execute("UPDATE requests SET request_hash=?1 WHERE id=?2",rusqlite::params![hash.as_slice(),request]).unwrap();
        db.execute("UPDATE idempotency_keys SET request_hash=?1 WHERE request_id=?2",rusqlite::params![hash.as_slice(),request]).unwrap();
        db.execute("UPDATE budget_continuations SET request_hash=?1 WHERE request_id=?2",rusqlite::params![hash.as_slice(),request]).unwrap();
        let rebound=f.state.store.budget_continuation(&request).unwrap().unwrap();
        let sealed=f.state.key_vault.encrypt(&rebound.encryption_context(),&envelope.to_string()).unwrap();
        db.execute("UPDATE budget_continuations SET key_version=?1,ciphertext=?2 WHERE request_id=?3",rusqlite::params![sealed.key_version,sealed.ciphertext,request]).unwrap();
        if mode=="expired" {db.execute("UPDATE assets SET created_at_ms=0,expires_at_ms=1 WHERE id=?1",[&asset.id]).unwrap();}
        if mode=="missing" || mode=="temporary" {let path=f.dir.join("data").join(&asset.storage_ref);std::fs::remove_file(&path).unwrap();if mode=="temporary" {std::fs::create_dir(path).unwrap();}}
        let steps=f.state.store.budget_operation(&request).unwrap().unwrap().steps;
        f.bridge.asset_upload_failure.store(false,Ordering::SeqCst);make_due(&f,&request);
        let (state,app)=reopened(&f);
        let result=send(&app,&f.key,"/v1/chat/completions","legacy-marker",&body).await;
        let op=state.store.budget_operation(&request).unwrap().unwrap();
        if mode=="live" {
            assert!(result["error"].is_null(),"persisted accepted marker can use its still-owned bytes: {result}");
            assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst),1);
        } else {
            let code=match mode {"expired"=>"reference_asset_expired","missing"=>"reference_asset_missing","temporary"=>"reference_asset_unavailable",_=>"invalid_reference_context"};
            assert_eq!(result["error"]["code"],code,"{mode}: {result}");
            assert_eq!(op.execution_state==aiwork_core::BudgetExecutionState::Failed,matches!(mode,"expired"|"missing"));
            assert_eq!(op.steps,steps,"legacy reference failure must preserve paid helper facts");
            assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst),0);
        }
        assert_eq!(f.bridge.assist_sends.load(Ordering::SeqCst),1);
    }
}


#[tokio::test]
async fn only_current_user_marker_can_supply_references_and_new_requests_cannot_reuse_expired_markers() {
    let f=Fixture::new();let (asset,_)=upload_image(&f,"marker-source");
    let expired=marker(&f,&f.owner.key_id,&asset.id,1);
    let body=json!({"model":"seedance","messages":[{"role":"user","content":expired},{"role":"assistant","content":"已收到之前的参考图"},{"role":"user","content":"独立生成完全新的视频"}]});
    let result=f.chat("ignore-old-marker",&body).await;
    let version=f.state.store.work_version_for_request(&f.owner,result["request_id"].as_str().unwrap()).unwrap().unwrap();
    assert!(work_context::read_snapshot(&f.state,&f.owner,&version).unwrap().user_media_ids.is_empty());
    let sends=f.bridge.assist_sends.load(Ordering::SeqCst);
    for token in [expired,marker(&f,"foreign-key",&asset.id,chrono::Utc::now().timestamp_millis()+60_000)] {
        let mut fresh=create();fresh["messages"][0]["content"]=json!(format!("生成新视频 {token}"));
        let reply=send(&f.app,&f.key,"/v1/chat/completions",&format!("unaccepted-marker-{}",token.len()),&fresh).await;
        assert!(reply["error"].is_object(),"fresh requests cannot bypass marker validity: {reply}");
        assert_eq!(f.bridge.assist_sends.load(Ordering::SeqCst),sends);
    }
}
