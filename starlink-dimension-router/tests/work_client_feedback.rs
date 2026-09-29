#[path = "support/work_fixture.rs"]
mod fixture;
use fixture::*;

#[tokio::test]
async fn missing_parent_after_paid_helper_terminates_without_video_or_refund() {
    let f=Fixture::new();
    *f.bridge.helper_decision.lock().unwrap()=Some(json!({"action":"continue","effective_prompt":"接着上一段生成","spec_patch":{},"reference_policy":"inherit","clarification":null}));
    let response=f.response(&f.key,"missing-parent-after-helper",&create()).await;
    assert_eq!(response.status(),StatusCode::BAD_REQUEST);
    let result:Value=serde_json::from_slice(&to_bytes(response.into_body(),65536).await.unwrap()).unwrap();
    let rid=result["request_id"].as_str().unwrap();
    let op=f.state.store.budget_operation(rid).unwrap().unwrap();
    assert_eq!(op.execution_state,aiwork_core::BudgetExecutionState::Failed,"terminal planner rejection left parent running");
    assert_eq!(op.steps.len(),1);
    assert_eq!(op.steps[0].kind,aiwork_core::BudgetStepKind::Assist);
    assert_eq!(op.steps[0].financial_state,aiwork_core::BudgetFinancialState::Held,"already sent helper must retain billing until actual receipt");
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst),0);
    let status=f.app.clone().oneshot(Request::get(format!("/v1/videos/{rid}")).header("authorization",format!("Bearer {}",f.key)).body(Body::empty()).unwrap()).await.unwrap();
    let status:Value=serde_json::from_slice(&to_bytes(status.into_body(),65536).await.unwrap()).unwrap();
    assert_eq!(status["task"]["status"],"failed");
}

#[tokio::test]
async fn interrupted_async_preparation_is_terminal_without_paid_send() {
    let f=Fixture::new();
    f.bridge.assist_prepare_panic.store(true,Ordering::SeqCst);
    let response=f.app.clone().oneshot(Request::post("/v1/videos/generations").header("authorization",format!("Bearer {}",f.key)).header("idempotency-key","direct-worker-interrupted").header("content-type","application/json").body(Body::from(json!({"model":"seedance","prompt":"生成橘猫散步视频","duration":5,"resolution":"480p","ratio":"16:9"}).to_string())).unwrap()).await.unwrap();
    assert_eq!(response.status(),StatusCode::ACCEPTED);
    let accepted:Value=serde_json::from_slice(&to_bytes(response.into_body(),65536).await.unwrap()).unwrap();
    let rid=accepted["request_id"].as_str().unwrap();
    for _ in 0..50 {
        if f.state.store.request_state(rid).unwrap()==aiwork_core::RequestState::Failed {break;}
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(f.state.store.request_state(rid).unwrap(),aiwork_core::RequestState::Failed,"worker interruption must not leave an unsent parent queued forever");
    assert!(f.state.store.budget_operation(rid).unwrap().is_none());
    assert_eq!(f.bridge.assist_sends.load(Ordering::SeqCst),0);
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst),0);
}

#[tokio::test]
async fn async_direct_helper_prepare_rejection_is_failed_not_permanently_queued() {
    let f=Fixture::new();
    f.bridge.assist_prepare_failure.store(true,Ordering::SeqCst);
    let response=f.app.clone().oneshot(Request::post("/v1/videos/generations").header("authorization",format!("Bearer {}",f.key)).header("idempotency-key","direct-policy-failure").header("content-type","application/json").body(Body::from(json!({"model":"seedance","prompt":"生成橘猫散步视频","duration":5,"resolution":"480p","ratio":"16:9"}).to_string())).unwrap()).await.unwrap();
    assert_eq!(response.status(),StatusCode::ACCEPTED);
    let accepted:Value=serde_json::from_slice(&to_bytes(response.into_body(),65536).await.unwrap()).unwrap();
    let rid=accepted["request_id"].as_str().unwrap();
    for _ in 0..50 {
        if f.state.store.request_state(rid).unwrap()==aiwork_core::RequestState::Failed {break;}
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(f.state.store.request_state(rid).unwrap(),aiwork_core::RequestState::Failed,"pre-admission failure must terminate parent and release preparation slot");
    assert!(f.state.store.budget_operation(rid).unwrap().is_none());
    let status=f.app.clone().oneshot(Request::get(format!("/v1/videos/{rid}")).header("authorization",format!("Bearer {}",f.key)).body(Body::empty()).unwrap()).await.unwrap();
    let status:Value=serde_json::from_slice(&to_bytes(status.into_body(),65536).await.unwrap()).unwrap();
    assert_eq!(status["task"]["status"],"failed");
    assert_eq!(status["task"]["error"]["code"],"budget_not_sent");
    assert_eq!(f.bridge.assist_sends.load(Ordering::SeqCst),0);
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst),0);
}

#[tokio::test]
async fn non_gray_key_keeps_legacy_generation_without_work_version() {
    let f = Fixture::with_gray_keys(true, Some(vec!["key_some_other_key".into()]));
    let first = f.chat("non-gray", &create()).await;
    assert!(first["work_context"].is_null(), "non-gray Key entered new work flow: {first}");
    assert!(f.state.store.work_version_for_request(&f.owner, first["request_id"].as_str().unwrap()).unwrap().is_none());
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn stream_and_nonstream_context_roundtrip_with_truthful_unique_progress() {
    let f = Fixture::new();
    let first = f.chat("first", &create()).await;
    let mut body = revise(&first, "改成夜景，其他不变");
    body["stream"] = json!(true);
    let response = f.response(&f.key, "stream-revise", &body).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 512 * 1024).await.unwrap();
    let events: Vec<Value> = std::str::from_utf8(&bytes)
        .unwrap()
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let completion = events
        .iter()
        .find(|e| e["work_context"]["base_version_id"].is_string())
        .expect("SSE must retain owned version");
    assert_eq!(
        completion["work_context"]["work_id"],
        first["work_context"]["work_id"]
    );
    assert_ne!(
        completion["work_context"]["base_version_id"],
        first["work_context"]["base_version_id"]
    );
    let content = completion["choices"][0]["delta"]["content"]
        .as_str()
        .or_else(|| completion["choices"][0]["message"]["content"].as_str())
        .unwrap();
    assert!(content.contains("[AIWORK_WORK:"));
    assert!(
        !content.contains("下载完成"),
        "no local receipt has confirmed a download"
    );
    let stages: Vec<&str> = events
        .iter()
        .filter_map(|e| e["task_progress"]["stage"].as_str())
        .collect();
    assert!(
        stages.windows(2).all(|pair| pair[0] != pair[1]),
        "duplicate progress paragraphs"
    );
    let follow = json!({"model":"seedance","action":"status","work_context":completion["work_context"],"messages":[{"role":"user","content":"状态如何"}]});
    let status = f.chat("read-only", &follow).await;
    assert_eq!(status["work_context"], completion["work_context"]);
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst), 2);
}
