#[path="support/work_fixture.rs"]
mod fixture;
use fixture::*;

fn reference_failure()->Value {json!({"status":"failed","error_code":"3003","upstream_error":{"code":"3003","message":"InvalidParameter: content[1]: resource download failed"}})}
async fn source(f:&Fixture,failure:Value)->String {
    use base64::Engine;
    use starlink_dimension_router::assets::{self,ParsedAssetUpload};
    let bytes=base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aXuUAAAAASUVORK5CYII=").unwrap();
    let stored=assets::write_asset(&f.state.config.data_dir,&f.owner,ParsedAssetUpload{filename:"reference.png".into(),declared_mime:Some("image/png".into()),bytes}).unwrap();
    let asset=assets::persist_asset(&f.state.store,&f.owner,&stored).unwrap();
    let mut body=create();body["image_asset_ids"]=json!([asset.id]);
    *f.bridge.next_video_failure.lock().unwrap()=Some(failure);
    let response=f.response(&f.key,"original-with-reference",&body).await;
    let result:Value=serde_json::from_slice(&to_bytes(response.into_body(),65536).await.unwrap()).unwrap();
    result["request_id"].as_str().unwrap().into()
}
async fn retry(app:axum::Router,key:&str,id:&str)->(StatusCode,Value) {
    let response=app.oneshot(Request::post(format!("/v1/videos/{id}/retry")).header("authorization",format!("Bearer {key}"))
        .header("content-type","application/json").body(Body::from(r#"{"max_retries":1}"#)).unwrap()).await.unwrap();
    let status=response.status();let value=serde_json::from_slice(&to_bytes(response.into_body(),65536).await.unwrap()).unwrap_or(Value::Null);(status,value)
}
async fn terminal(app:axum::Router,key:&str,id:&str)->Value {
    for _ in 0..200 {
        let response=app.clone().oneshot(Request::get(format!("/v1/videos/{id}")).header("authorization",format!("Bearer {key}"))
            .body(Body::empty()).unwrap()).await.unwrap();
        let value:Value=serde_json::from_slice(&to_bytes(response.into_body(),65536).await.unwrap()).unwrap();
        if matches!(value["task"]["status"].as_str(),Some("completed"|"failed"|"unknown")){return value;}
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }panic!("retry did not reach a terminal or unknown state")
}
#[tokio::test]
async fn retries_confirmed_reference_failure_once_with_fresh_upload_and_original_prompt_and_bill() {
    let f=Fixture::new();let root=source(&f,reference_failure()).await;
    let before=f.state.store.budget_operation(&root).unwrap().unwrap();
    let old=f.state.store.work_version_for_request(&f.owner,&root).unwrap().unwrap();
    let snapshot=work_context::read_snapshot(&f.state,&f.owner,&old).unwrap();
    let uploads=f.bridge.asset_uploads.load(Ordering::SeqCst);
    let (status,accepted)=retry(f.app.clone(),&f.key,&root).await;
    assert_eq!(status,StatusCode::ACCEPTED,"{accepted}");let child=accepted["request_id"].as_str().unwrap();assert_ne!(child,root);
    assert_eq!(terminal(f.app.clone(),&f.key,child).await["task"]["status"],"completed");
    assert_eq!(f.bridge.assist_sends.load(Ordering::SeqCst),1);
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst),2);
    assert_eq!(f.bridge.asset_uploads.load(Ordering::SeqCst),uploads+1);
    let retried=f.state.store.work_version_for_request(&f.owner,child).unwrap().unwrap();
    let new=work_context::read_snapshot(&f.state,&f.owner,&retried).unwrap();
    assert_eq!(new.effective_prompt,snapshot.effective_prompt);assert_eq!(new.duration,snapshot.duration);
    assert_eq!(new.user_media_ids,snapshot.user_media_ids);assert_eq!(new.parent_version_id,snapshot.parent_version_id);
    assert_eq!(f.state.store.budget_operation(&root).unwrap().unwrap(),before);
    let op=f.state.store.budget_operation(child).unwrap().unwrap();assert_eq!(op.steps.len(),1);assert_eq!(op.steps[0].kind,aiwork_core::BudgetStepKind::Video);
    let replay=retry(f.app.clone(),&f.key,&root).await;assert_eq!(replay.1["request_id"],child);
    let lookup=f.app.clone().oneshot(Request::get(format!("/v1/videos/{root}/retry")).header("authorization",format!("Bearer {}",f.key)).body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(lookup.status(),StatusCode::OK);
    let lookup:Value=serde_json::from_slice(&to_bytes(lookup.into_body(),65536).await.unwrap()).unwrap();assert_eq!(lookup["request_id"],child);
    assert_eq!(retry(f.app.clone(),&f.key,child).await.0,StatusCode::CONFLICT);
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst),2);
}
#[tokio::test]
async fn concurrent_and_restarted_retry_calls_share_one_child_even_when_that_child_fails() {
    let f=Fixture::new();let root=source(&f,reference_failure()).await;
    *f.bridge.next_video_failure.lock().unwrap()=Some(reference_failure());
    let (a,b,c)=tokio::join!(retry(f.app.clone(),&f.key,&root),retry(f.app.clone(),&f.key,&root),retry(f.app.clone(),&f.key,&root));
    assert_eq!(a.0,StatusCode::ACCEPTED,"{}",a.1);assert_eq!(b.0,StatusCode::ACCEPTED);assert_eq!(c.0,StatusCode::ACCEPTED);
    assert_eq!(a.1["request_id"],b.1["request_id"]);assert_eq!(a.1["request_id"],c.1["request_id"]);
    let child=a.1["request_id"].as_str().unwrap();assert_eq!(terminal(f.app.clone(),&f.key,child).await["task"]["status"],"failed");
    let before=f.state.store.budget_operation(child).unwrap().unwrap();
    let state=StarlinkRouterState::for_test(Arc::new(CoreStore::open(&f.dir).unwrap()),BridgeClient::from_transport("http://bridge","bridge-only",f.bridge.clone()),f.state.config.clone());
    let restarted=starlink_dimension_router::server::build_router(state);
    assert_eq!(retry(restarted.clone(),&f.key,&root).await.1["request_id"],child);
    assert_eq!(retry(restarted,&f.key,child).await.0,StatusCode::CONFLICT);
    assert_eq!(f.state.store.budget_operation(child).unwrap().unwrap(),before);
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst),2);assert_eq!(f.bridge.assist_sends.load(Ordering::SeqCst),1);
}
#[tokio::test]
async fn does_not_retry_unknown_safety_or_other_failures_or_foreign_requests() {
    for failure in [json!({"status":"failed","upstream_error":{"code":"3003","message":"image security check failed"}}),json!({"status":"failed","upstream_error":{"code":"5000","message":"provider busy"}})] {
        let f=Fixture::new();let root=source(&f,failure).await;
        assert_eq!(retry(f.app.clone(),&f.key,&root).await.0,StatusCode::CONFLICT);
        assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst),1);
    }
    let f=Fixture::new();f.bridge.unknown.store(true,Ordering::SeqCst);
    let response=f.app.clone().oneshot(Request::post("/v1/videos/generations").header("authorization",format!("Bearer {}",f.key)).header("idempotency-key","unknown")
        .header("content-type","application/json").body(Body::from(json!({"model":"seedance","prompt":"猫","duration":5,"ratio":"9:16","resolution":"480p"}).to_string())).unwrap()).await.unwrap();
    let accepted:Value=serde_json::from_slice(&to_bytes(response.into_body(),65536).await.unwrap()).unwrap();let root=accepted["request_id"].as_str().unwrap();
    assert_eq!(retry(f.app.clone(),&f.key,root).await.0,StatusCode::CONFLICT);
    assert_eq!(retry(f.app.clone(),&f.key,"missing-request").await.0,StatusCode::NOT_FOUND);
}
#[tokio::test]
async fn another_key_cannot_retry_or_lookup_an_owned_failure() {
    let f=Fixture::new();let root=source(&f,reference_failure()).await;
    let other=f.state.store.issue_api_key_with_max_concurrency("admin","other",std::collections::BTreeSet::from(["admin:*".into()]),2,"test").unwrap();
    assert_eq!(retry(f.app.clone(),&other.plaintext,&root).await.0,StatusCode::NOT_FOUND);
    let lookup=f.app.clone().oneshot(Request::get(format!("/v1/videos/{root}/retry")).header("authorization",format!("Bearer {}",other.plaintext)).body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(lookup.status(),StatusCode::NOT_FOUND);assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst),1);
}
