#[path = "support/work_fixture.rs"]
mod fixture;
use fixture::*;

#[tokio::test]
async fn interrupted_video_is_unknown_without_releasing_hold_or_replaying() {
    let f=Fixture::new();
    *f.bridge.video_diagnostic.lock().unwrap()=Some(json!({"code":"video_execution_interrupted","stage":"video_stream","outcome_known":false}));
    let response=f.app.clone().oneshot(Request::post("/v1/videos/generations")
        .header("authorization",format!("Bearer {}",f.key)).header("content-type","application/json")
        .header("idempotency-key","video-stream-unknown")
        .body(Body::from(json!({"model":"seedance","duration":5,"resolution":"480p","ratio":"9:16","prompt":"生成橘猫散步的视频"}).to_string())).unwrap()).await.unwrap();
    assert_eq!(response.status(),StatusCode::ACCEPTED);
    let accepted:Value=serde_json::from_slice(&to_bytes(response.into_body(),65536).await.unwrap()).unwrap();let rid=accepted["request_id"].as_str().unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5),async {
        while f.bridge.video_sends.load(Ordering::SeqCst)==0 {tokio::time::sleep(std::time::Duration::from_millis(10)).await;}
    }).await.unwrap();
    for _ in 0..2 {
        let response=f.app.clone().oneshot(Request::get(format!("/v1/videos/{rid}")).header("authorization",format!("Bearer {}",f.key)).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(),StatusCode::OK);
        let value:Value=serde_json::from_slice(&to_bytes(response.into_body(),65536).await.unwrap()).unwrap();
        assert_eq!(value["task"]["status"],"unknown");
        assert_eq!(value["task"]["error"]["code"],"video_execution_interrupted");
        assert_eq!(value["task"]["error"]["stage"],"video_stream");
        assert!(value["task"]["content_url"].is_null());
        let operation=f.state.store.budget_operation(rid).unwrap().unwrap();
        let video=operation.steps.iter().find(|s|s.kind==aiwork_core::BudgetStepKind::Video).unwrap();
        assert_eq!(video.execution_state,aiwork_core::BudgetExecutionState::Unknown);
        assert_eq!(video.financial_state,aiwork_core::BudgetFinancialState::Held);assert_eq!(video.actual_credits,None);
    }
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst),1);assert_eq!(f.bridge.assist_sends.load(Ordering::SeqCst),1);
    let other=f.state.store.issue_api_key_with_max_concurrency("admin","other-unknown",BTreeSet::from(["admin:*".into()]),1,"bootstrap").unwrap();
    let forbidden=f.app.clone().oneshot(Request::get(format!("/v1/videos/{rid}")).header("authorization",format!("Bearer {}",other.plaintext)).body(Body::empty()).unwrap()).await.unwrap();assert_eq!(forbidden.status(),StatusCode::NOT_FOUND);
    *f.bridge.video_diagnostic.lock().unwrap()=None;f.bridge.unknown.store(true,Ordering::SeqCst);
    let no_diagnostic=f.app.clone().oneshot(Request::get(format!("/v1/videos/{rid}")).header("authorization",format!("Bearer {}",f.key)).body(Body::empty()).unwrap()).await.unwrap();
    let no_diagnostic:Value=serde_json::from_slice(&to_bytes(no_diagnostic.into_body(),65536).await.unwrap()).unwrap();assert_eq!(no_diagnostic["task"]["status"],"unknown");assert_eq!(no_diagnostic["task"]["error"]["code"],"video_execution_unconfirmed");
    f.bridge.unknown.store(false,Ordering::SeqCst);
    *f.bridge.video_diagnostic.lock().unwrap()=None;
    let late=f.app.clone().oneshot(Request::get(format!("/v1/videos/{rid}")).header("authorization",format!("Bearer {}",f.key)).body(Body::empty()).unwrap()).await.unwrap();
    let late:Value=serde_json::from_slice(&to_bytes(late.into_body(),65536).await.unwrap()).unwrap();assert_eq!(late["task"]["status"],"completed");assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst),1);
}
