#[path = "support/work_fixture.rs"]
mod fixture;
use fixture::*;

async fn failed_reply(f: &Fixture, id: &str, body: &Value) -> Value {
    let response = f.response(&f.key, id, body).await;
    let stream = body["stream"] == true;
    let status = response.status();
    let raw = to_bytes(response.into_body(), 256 * 1024).await.unwrap();
    assert_eq!(status, if stream { StatusCode::OK } else { StatusCode::BAD_REQUEST }, "{}", String::from_utf8_lossy(&raw));
    if stream {
        std::str::from_utf8(&raw).unwrap().lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|line| serde_json::from_str(line).ok()).last().unwrap()
    } else { serde_json::from_slice(&raw).unwrap() }
}

#[tokio::test]
async fn reference_prepare_rejection_releases_slot_without_refunding_paid_helper() {
    use aiwork_core::{BudgetExecutionState as E, BudgetFinancialState as F, BudgetReceiptInput};
    use starlink_dimension_router::assets::{self, ParsedAssetUpload};
    for code in ["reference_video_metadata_invalid", "reference_video_format_unsupported", "reference_asset_type_mismatch"] {
        for stream in [false, true] {
            let f = Fixture::with_max_concurrency(1);
            let stored = assets::write_asset(&f.state.config.data_dir, &f.owner, ParsedAssetUpload {
                filename: "source.mp4".into(), declared_mime: Some("video/mp4".into()),
                bytes: b"\x00\x00\x00\x18ftypisom\x00\x00\x00\x00isomiso2".to_vec(),
            }).unwrap();
            let asset = assets::persist_asset(&f.state.store, &f.owner, &stored).unwrap();
            *f.bridge.video_prepare_error.lock().unwrap() = Some((400, code.into()));
            let mut body = create(); body["video_asset_ids"] = json!([asset.id]); body["stream"] = json!(stream);
            let result = failed_reply(&f, "rejected-reference", &body).await;
            assert_eq!(result["error"]["code"], code);
            let rid = result["request_id"].as_str().unwrap();
            let op = f.state.store.budget_operation(rid).unwrap().unwrap();
            assert_eq!(op.execution_state, E::Failed, "deterministic reference rejection left the concurrency slot running: {code}");
            assert_eq!(f.state.store.request_state(rid).unwrap(), aiwork_core::RequestState::Failed);
            assert_eq!(op.steps.len(), 1, "rejected video must never have a reservation");
            let helper = &op.steps[0];
            assert_eq!(helper.kind, aiwork_core::BudgetStepKind::Assist);
            assert_eq!(helper.execution_state, E::Succeeded);
            assert_eq!(helper.financial_state, F::Held, "no receipt yet: do not invent a refund");
            assert_eq!(helper.hold_credits.as_microcredits(), 2_000_000);
            assert_eq!(helper.actual_credits, None);
            let version = f.state.store.work_version_for_request(&f.owner, rid).unwrap().unwrap();
            assert_eq!(version.state, aiwork_core::WorkVersionState::Failed);
            assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst), 0);
            let replay = f.response(&f.key, "rejected-reference", &body).await;
            assert_eq!(replay.status(), if stream { StatusCode::OK } else { StatusCode::CONFLICT });
            let replay_raw = to_bytes(replay.into_body(), 256 * 1024).await.unwrap();
            let replay: Value = if stream {
                std::str::from_utf8(&replay_raw).unwrap().lines().filter_map(|l| l.strip_prefix("data: "))
                    .filter_map(|l| serde_json::from_str(l).ok()).last().unwrap()
            } else { serde_json::from_slice(&replay_raw).unwrap() };
            assert_eq!(replay["request_id"], rid);
            assert_eq!(replay["error"]["code"], "video_continuation_not_active");
            assert_eq!(f.bridge.assist_sends.load(Ordering::SeqCst), 1, "failed idempotent replay must not charge again");

            // A different request can run immediately, before the old helper bill arrives.
            *f.bridge.video_prepare_error.lock().unwrap() = None;
            let next = f.chat("next-after-reference-rejection", &create()).await;
            assert_ne!(next["request_id"], rid);
            assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst), 1);
            let receipt = BudgetReceiptInput {
                budget_id: helper.budget_id.clone(), account_ref: helper.account_ref.clone(),
                bridge_instance_id: helper.bridge_instance_id.clone(),
                receipt: serde_json::from_value(json!({"request_id":helper.request_id,"status":"final","actual_credits":"1.250000","unit":"credits","source_ref":"verified-helper-final","task_ref":null,"observed_at_ms":chrono::Utc::now().timestamp_millis()})).unwrap(),
            };
            assert!(matches!(f.state.store.apply_budget_receipt(receipt.clone()).unwrap(),
                aiwork_core::BudgetReceiptResult::Settled { released_microcredits: 750_000, debt: false, .. }));
            assert_eq!(f.state.store.apply_budget_receipt(receipt).unwrap(), aiwork_core::BudgetReceiptResult::Duplicate);
            let settled = f.state.store.budget_operation(rid).unwrap().unwrap();
            assert_eq!(settled.execution_state, E::Failed);
            assert_eq!(settled.steps[0].financial_state, F::Settled);
            assert_eq!(settled.steps[0].actual_credits.unwrap().as_microcredits(), 1_250_000);
        }
    }
}

#[tokio::test]
async fn legacy_reference_rejection_finishes_parent_without_video_dispatch() {
    let f = Fixture::with_gray_keys(false, Some(vec!["other-context-key".into()]));
    *f.bridge.video_prepare_error.lock().unwrap() = Some((400, "reference_video_metadata_invalid".into()));
    let result = failed_reply(&f, "legacy-reference-rejection", &create()).await;
    assert_eq!(result["error"]["code"], "reference_video_metadata_invalid");
    let op = f.state.store.budget_operation(result["request_id"].as_str().unwrap()).unwrap().unwrap();
    assert_eq!(op.execution_state, aiwork_core::BudgetExecutionState::Failed);
    assert_eq!(op.steps.len(), 1);
    assert_eq!(op.steps[0].financial_state, aiwork_core::BudgetFinancialState::Held);
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn async_direct_reference_rejection_finishes_parent_and_version() {
    let f = Fixture::new();
    *f.bridge.video_prepare_error.lock().unwrap() = Some((400, "reference_video_metadata_invalid".into()));
    let response = f.app.clone().oneshot(Request::post("/v1/videos/generations")
        .header("authorization", format!("Bearer {}", f.key)).header("idempotency-key", "direct-reference-rejection")
        .header("content-type", "application/json").body(Body::from(json!({"model":"seedance","prompt":"生成橘猫散步视频","duration":5,"resolution":"480p","ratio":"16:9"}).to_string())).unwrap()).await.unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let accepted: Value = serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
    let rid = accepted["request_id"].as_str().unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if f.state.store.request_state(rid).unwrap() == aiwork_core::RequestState::Failed { break; }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await.expect("deterministic metadata rejection left the async task queued");
    let op = f.state.store.budget_operation(rid).unwrap().unwrap();
    assert_eq!(op.execution_state, aiwork_core::BudgetExecutionState::Failed);
    assert_eq!(op.steps.len(), 1);
    assert_eq!(op.steps[0].financial_state, aiwork_core::BudgetFinancialState::Held);
    assert_eq!(f.state.store.work_version_for_request(&f.owner, rid).unwrap().unwrap().state, aiwork_core::WorkVersionState::Failed);
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn recoverable_reference_reads_and_bridge_errors_do_not_fake_terminal_billing() {
    for (status, code) in [(400, "reference_asset_unavailable"), (503, "bridge_state_unavailable"), (503, "budget_preparation_requires_recovery")] {
        let f = Fixture::with_max_concurrency(1);
        *f.bridge.video_prepare_error.lock().unwrap() = Some((status, code.into()));
        let response = f.response(&f.key, "recoverable-preparation", &create()).await;
        assert_eq!(response.status().as_u16(), status);
        let result: Value = serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
        assert_eq!(result["error"]["code"], code);
        let rid = result["request_id"].as_str().unwrap();
        let op = f.state.store.budget_operation(rid).unwrap().unwrap();
        assert_eq!(op.execution_state, aiwork_core::BudgetExecutionState::Running, "recoverable error is not terminal evidence: {code}");
        assert_eq!(op.steps[0].financial_state, aiwork_core::BudgetFinancialState::Held);
        assert_eq!(op.steps[0].actual_credits, None);
        assert_eq!(f.state.store.request_state(rid).unwrap(), aiwork_core::RequestState::Reserved);
        *f.bridge.video_prepare_error.lock().unwrap() = None;
        let next = f.response(&f.key, "while-preparation-needs-recovery", &create()).await;
        assert_eq!(next.status(), StatusCode::TOO_MANY_REQUESTS, "do not admit another task into an unresolved slot");
        let next: Value = serde_json::from_slice(&to_bytes(next.into_body(), 65536).await.unwrap()).unwrap();
        assert_eq!(next["error"]["code"], "key_concurrency_exceeded");
        assert_eq!(f.bridge.assist_sends.load(Ordering::SeqCst), 1);
        assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst), 0);
    }
}
#[tokio::test]
async fn invalid_fenced_helper_reports_format_error_without_video_or_running_parent() {
    let f=Fixture::new();
    *f.bridge.helper_raw.lock().unwrap()=Some("```json\n{\"action\":\"create\",\"effective_prompt\":\"橘猫散步\",\"spec_patch\":{},\"reference_policy\":\"inherit\",\"clarification\":null,\"charge\":0}\n```".into());
    for stream in [false,true] {
        let mut b=create();b["stream"]=json!(stream);
        let response=f.response(&f.key,if stream{"bad-fenced-stream"}else{"bad-fenced-json"},&b).await;
        let raw=to_bytes(response.into_body(),65536).await.unwrap();
        let r:Value=if stream {
            std::str::from_utf8(&raw).unwrap().lines().filter_map(|l|l.strip_prefix("data: ")).filter_map(|l|serde_json::from_str(l).ok()).last().unwrap()
        } else {serde_json::from_slice(&raw).unwrap()};
        assert_eq!(r["error"]["code"],"work_decision_invalid");
        let message=r["error"]["message"].as_str().unwrap();
        assert!(message.contains("格式或字段") && message.contains("未提交视频"));
        let op=f.state.store.budget_operation(r["request_id"].as_str().unwrap()).unwrap().unwrap();
        assert_eq!(op.execution_state,aiwork_core::BudgetExecutionState::Failed);
        assert_eq!(op.steps.len(),1);
    }
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst),0);
}
#[tokio::test]
async fn helper_spec_rejection_reports_planning_failure_and_retains_real_billing() {
    let f=Fixture::new();
    *f.bridge.helper_decision.lock().unwrap()=Some(json!({"action":"create","effective_prompt":"橘猫散步","spec_patch":{"duration":15},"reference_policy":"inherit","clarification":null}));
    let b=json!({"model":"seedance","messages":[{"role":"user","content":"生成橘猫散步的视频"}]});
    let response=f.response(&f.key,"invalid-planner-result",&b).await;
    assert_eq!(response.status(),StatusCode::BAD_REQUEST);
    let result:Value=serde_json::from_slice(&to_bytes(response.into_body(),65536).await.unwrap()).unwrap();
    assert_eq!(result["error"]["code"],"work_decision_invalid");
    let message=result["error"]["message"].as_str().unwrap();
    assert!(message.contains("规划"),"{message}");
    assert!(message.contains("未提交视频"),"{message}");
    assert!(message.contains("辅助模型"),"{message}");
    let op=f.state.store.budget_operation(result["request_id"].as_str().unwrap()).unwrap().unwrap();
    assert_eq!(op.steps.len(),1);
    assert_eq!(op.steps[0].kind,aiwork_core::BudgetStepKind::Assist);
    assert_eq!(op.steps[0].financial_state,aiwork_core::BudgetFinancialState::Held);
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst),0);
}
#[tokio::test]
async fn ambiguous_natural_specs_fail_before_any_paid_model() {
    let f=Fixture::new();
    let b=json!({"model":"seedance","messages":[{"role":"user","content":"生成横屏或竖屏的视频"}]});
    let response=f.response(&f.key,"ambiguous-natural-spec",&b).await;
    assert_eq!(response.status(),StatusCode::BAD_REQUEST);
    let result:Value=serde_json::from_slice(&to_bytes(response.into_body(),65536).await.unwrap()).unwrap();
    assert_eq!(result["error"]["code"],"work_spec_unsupported");
    assert!(result["error"]["message"].as_str().unwrap().contains("规格"));
    assert_eq!(f.bridge.assist_sends.load(Ordering::SeqCst),0);
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst),0);
}

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
