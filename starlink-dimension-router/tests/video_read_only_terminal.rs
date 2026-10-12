#[path = "support/work_fixture.rs"]
mod fixture;
use fixture::*;

async fn request(f: &Fixture, method: &str, path: &str, body: Value) -> axum::response::Response {
    f.app.clone().oneshot(Request::builder().method(method).uri(path)
        .header("authorization", format!("Bearer {}", f.key))
        .header("content-type", "application/json")
        .header("idempotency-key", "readonly-video-terminal")
        .body(Body::from(body.to_string())).unwrap()).await.unwrap()
}

#[tokio::test]
async fn finished_helper_without_video_is_terminal_and_never_rebilled_by_queries() {
    let f = Fixture::new();
    *f.bridge.helper_decision.lock().unwrap() = Some(json!({
        "action":"clarify", "effective_prompt":null, "spec_patch":null,
        "reference_policy":null, "clarification":"请补充镜头要求；本次未提交视频。"
    }));
    let body = json!({"model":"seedance", "duration":5,"ratio":"16:9", "resolution":"480p",
        "prompt":"鹈鹕在海边骑自行车。"});
    let accepted = request(&f,"POST","/v1/videos/generations",body).await;
    assert_eq!(accepted.status(),StatusCode::ACCEPTED);
    let accepted: Value = serde_json::from_slice(&to_bytes(accepted.into_body(),65536).await.unwrap()).unwrap();
    let rid = accepted["request_id"].as_str().unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5),async {
        loop {
            if f.state.store.request_state(rid).unwrap() == aiwork_core::RequestState::Succeeded { break; }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await.unwrap();
    let before = f.state.store.budget_operation(rid).unwrap().unwrap();
    assert_eq!(before.execution_state,aiwork_core::BudgetExecutionState::Succeeded);
    assert_eq!(before.steps.len(),1);
    assert_eq!(before.steps[0].kind,aiwork_core::BudgetStepKind::Assist);
    for _ in 0..2 {
        let response = request(&f,"GET",&format!("/v1/videos/{rid}"),Value::Null).await;
        assert_eq!(response.status(),StatusCode::OK);
        let result: Value = serde_json::from_slice(&to_bytes(response.into_body(),65536).await.unwrap()).unwrap();
        assert_eq!(result["task"]["status"],"failed","ended read-only work cannot remain queued: {result}");
        assert_eq!(result["task"]["error"]["code"],"video_not_submitted");
        assert!(result["task"]["content_url"].is_null());
    }
    let other = f.state.store.issue_api_key_with_max_concurrency("admin","other-key",
        BTreeSet::from(["admin:*".into()]),1,"bootstrap").unwrap();
    let forbidden = f.app.clone().oneshot(Request::get(format!("/v1/videos/{rid}"))
        .header("authorization",format!("Bearer {}",other.plaintext))
        .body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(forbidden.status(),StatusCode::NOT_FOUND,"another key must not learn this terminal disposition");
    let after = f.state.store.budget_operation(rid).unwrap().unwrap();
    assert_eq!(after.execution_state,before.execution_state);
    assert_eq!(after.steps[0].financial_state,before.steps[0].financial_state);
    assert_eq!(after.steps[0].actual_credits,before.steps[0].actual_credits);
    assert_eq!(f.bridge.assist_sends.load(Ordering::SeqCst),1);
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst),0);
    assert_eq!(f.state.store.active_execution_count_for_key(&f.owner.key_id).unwrap(),0);
}

#[tokio::test]
async fn ordinary_chat_clarification_still_returns_its_text_without_video_dispatch() {
    let f = Fixture::new();
    *f.bridge.helper_decision.lock().unwrap() = Some(json!({
        "action":"clarify", "effective_prompt":null, "spec_patch":null,
        "reference_policy":null, "clarification":"请补充镜头要求；本次未提交视频。"
    }));
    let result = f.chat("readonly-chat-unchanged", &create()).await;
    assert!(result["choices"][0]["message"]["content"].as_str().unwrap().contains("请补充镜头要求"));
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst),0);
}
