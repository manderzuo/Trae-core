#[path = "support/work_fixture.rs"]
mod fixture;
use fixture::*;

fn uploaded_tail(f:&Fixture)->Value {
    use starlink_dimension_router::assets::{self,ParsedAssetUpload};
    let stored=assets::write_asset(&f.state.config.data_dir,&f.owner,ParsedAssetUpload {filename:"source.mp4".into(),declared_mime:Some("video/mp4".into()),bytes:b"\x00\x00\x00\x18ftypisom\x00\x00\x00\x00isomiso2".to_vec()}).unwrap();
    let asset=assets::persist_asset(&f.state.store,&f.owner,&stored).unwrap();
    let mut body=create();body["video_asset_ids"]=json!([asset.id]);body["continuation_mode"]=json!("tail_reference");body
}
#[tokio::test]
async fn unavailable_frame_tool_is_rejected_before_helper_payment() {
    let f=Fixture::with_continuation(true);let body=uploaded_tail(&f);
    *f.bridge.frame_health_error.lock().unwrap()=Some("frame_extractor_unconfigured".into());
    let response=f.response(&f.key,"tool-preflight",&body).await;
    let value:Value=serde_json::from_slice(&to_bytes(response.into_body(),65536).await.unwrap()).unwrap();
    assert_eq!(value["error"]["code"],"frame_extractor_unconfigured","{value}");
    assert_eq!(f.bridge.assist_sends.load(Ordering::SeqCst),0);
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst),0);
    assert_eq!(f.state.store.active_execution_count_for_key(&f.owner.key_id).unwrap(),0);
    let replay=f.response(&f.key,"tool-preflight",&body).await;
    let replay:Value=serde_json::from_slice(&to_bytes(replay.into_body(),65536).await.unwrap()).unwrap();
    assert_eq!(replay["error"]["code"],"frame_extractor_unconfigured");
}
#[tokio::test]
async fn changed_frame_tool_ends_execution_without_erasing_helper_bill_or_replaying_video() {
    let f=Fixture::with_continuation(true);let body=uploaded_tail(&f);
    *f.bridge.frame_tool_error.lock().unwrap()=Some("frame_extractor_digest_mismatch".into());
    let response=f.response(&f.key,"changed-tool",&body).await;
    let value:Value=serde_json::from_slice(&to_bytes(response.into_body(),65536).await.unwrap()).unwrap();
    assert_eq!(value["error"]["code"],"frame_extractor_digest_mismatch","{value}");
    let request=value["request_id"].as_str().unwrap();
    let op=f.state.store.budget_operation(request).unwrap().unwrap();
    assert_eq!(op.execution_state,aiwork_core::BudgetExecutionState::Failed);
    assert_eq!(op.steps.len(),1);assert_eq!(op.steps[0].financial_state,aiwork_core::BudgetFinancialState::Held);
    assert_eq!(f.state.store.active_execution_count_for_key(&f.owner.key_id).unwrap(),0);
    *f.bridge.frame_tool_error.lock().unwrap()=None;
    let replay=f.response(&f.key,"changed-tool",&body).await;
    let replay:Value=serde_json::from_slice(&to_bytes(replay.into_body(),65536).await.unwrap()).unwrap();
    assert_eq!(replay["error"]["code"],"frame_extractor_digest_mismatch");
    assert_eq!(f.bridge.assist_sends.load(Ordering::SeqCst),1);assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst),0);
    assert_eq!(f.state.store.budget_operation(request).unwrap().unwrap().steps[0].actual_credits,op.steps[0].actual_credits);
    assert_eq!(f.bridge.uploaded_frame_reads.load(Ordering::SeqCst),0,"repairing the tool must not revive a definitively failed request");
    assert!(f.state.store.work_version_for_request(&f.owner,request).unwrap().is_none());
}
#[tokio::test]
async fn malformed_helper_json_reports_cause_and_releases_slot_without_resubmission() {
    let cases=[
        (r#"{"action":"create","effective_prompt":"猫说:"你好。"","spec_patch":{},"reference_policy":"inherit","clarification":null}"#,None,"assistant_json_invalid"),
        (r#"{"action":"create","action":"continue","effective_prompt":"橘猫散步","spec_patch":{},"reference_policy":"inherit","clarification":null}"#,None,"assistant_json_duplicate_key"),
        (r#"{"action":"create","effective_prompt":"橘猫散步","spec_patch":{},"reference_policy":"inherit","clarification":null}"#,Some("length"),"assistant_output_truncated"),
        (r#"{"action":"create","effective_prompt":"bad\q","spec_patch":{},"reference_policy":"inherit","clarification":null}"#,None,"assistant_json_invalid_escape"),
        ("{\"action\":\"create\",\"effective_prompt\":\"bad\ntext\",\"spec_patch\":{},\"reference_policy\":\"inherit\",\"clarification\":null}",None,"assistant_json_control_character"),
        (r#"{"action":"create","effective_prompt":"橘猫散步","spec_patch":{},"reference_policy":"inherit","clarification":null,"charge":0}"#,None,"assistant_schema_invalid"),
        (r#"{"action":"create","effective_prompt":"橘猫散步","spec_patch":{},"reference_policy":"inherit","clarification":null}"#,Some("content_filter"),"assistant_output_blocked"),
    ];
    for (i,(raw,finish,code)) in cases.into_iter().enumerate() {
        let f=Fixture::new();
        *f.bridge.helper_raw.lock().unwrap()=Some(raw.into());
        *f.bridge.helper_finish_reason.lock().unwrap()=finish.map(str::to_owned);
        let key=format!("malformed-json-{i}");let b=create();
        let response=f.response(&f.key,&key,&b).await;
        assert_eq!(response.status(),StatusCode::BAD_GATEWAY,"invalid helper output is not a user input error");
        let result:Value=serde_json::from_slice(&to_bytes(response.into_body(),65536).await.unwrap()).unwrap();
        assert_eq!(result["error"]["code"],code,"{result}");
        assert!(result["error"]["message"].as_str().unwrap().contains("未提交视频"));
        let rid=result["request_id"].as_str().unwrap();
        let before=f.state.store.budget_operation(rid).unwrap().unwrap();
        assert_eq!(before.execution_state,aiwork_core::BudgetExecutionState::Failed);
        assert_eq!(before.steps.len(),1);
        assert_eq!(before.steps[0].financial_state,aiwork_core::BudgetFinancialState::Held,"a model format error cannot invent a zero bill");
        assert_eq!(f.state.store.active_execution_count_for_key(&f.owner.key_id).unwrap(),0);
        let replay=f.response(&f.key,&key,&b).await;
        let replay:Value=serde_json::from_slice(&to_bytes(replay.into_body(),65536).await.unwrap()).unwrap();
        assert_eq!(replay["error"]["code"],code,"replay must retain first cause");
        assert_eq!(f.bridge.assist_sends.load(Ordering::SeqCst),1);
        assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst),0);
        let after=f.state.store.budget_operation(rid).unwrap().unwrap();
        assert_eq!(after.steps[0].actual_credits,before.steps[0].actual_credits);
    }
}
#[tokio::test]
async fn escaped_dialogue_reaches_video_once_and_is_not_rewritten_or_lost() {
    let f=Fixture::new();
    let prompt="猫说：\"你好。\"\n窗外显示路径 C:\\clips\\猫.mp4，背景标签９：１６。";
    *f.bridge.helper_decision.lock().unwrap()=Some(json!({"action":"create","effective_prompt":prompt,"spec_patch":{},"reference_policy":"inherit","clarification":null}));
    let b=create();let result=f.chat("valid-dialogue",&b).await;
    let rid=result["request_id"].as_str().unwrap();
    let version=f.state.store.work_version_for_request(&f.owner,rid).unwrap().unwrap();
    let snapshot=work_context::read_snapshot(&f.state,&f.owner,&version).unwrap();
    assert_eq!(snapshot.effective_prompt,prompt);
    assert_eq!(snapshot.dispatch_body.unwrap()["prompt"],prompt);
    assert_eq!(f.chat("valid-dialogue",&b).await["request_id"],rid);
    assert_eq!(f.bridge.assist_sends.load(Ordering::SeqCst),1);
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst),1);
}
#[tokio::test]
async fn legacy_helper_keeps_dialogue_punctuation_outside_spec_normalization() {
    let f=Fixture::with_gray_keys(false,Some(vec!["other-context-key".into()]));
    let prompt="猫说：\"你好。\"，停顿后继续。";
    *f.bridge.helper_decision.lock().unwrap()=Some(json!({"intent":"video","prompt":prompt}));
    let b=create();let result=f.chat("legacy-dialogue",&b).await;
    assert!(result.get("error").is_none(),"{result}");
    let claims=f.bridge.claims.lock().unwrap();
    let video=claims.values().find(|c|c["step_kind"]=="video").unwrap();
    assert_eq!(video["body"]["prompt"],prompt);
}
#[tokio::test]
async fn temporary_video_preparation_error_keeps_work_recoverable_without_repaying_helper() {
    let f=Fixture::new();let b=create();
    *f.bridge.video_prepare_error.lock().unwrap()=Some((429,"bridge_workers_busy".into()));
    let response=f.response(&f.key,"temporary-prepare",&b).await;
    let _=to_bytes(response.into_body(),256*1024).await.unwrap();
    let request=f.bridge.claims.lock().unwrap().values().find(|v|v["step_kind"]=="assist").unwrap()["parent_request_id"].as_str().unwrap().to_owned();
    let version=f.state.store.work_version_for_request(&f.owner,&request).unwrap().unwrap();
    assert_ne!(version.state,aiwork_core::WorkVersionState::Failed,"a temporary capacity error is not a permanent work failure");
    assert_eq!(f.state.store.active_execution_count_for_key(&f.owner.key_id).unwrap(),1);
    *f.bridge.video_prepare_error.lock().unwrap()=None;
    let response=f.chat("temporary-prepare",&b).await;
    assert_eq!(response["request_id"],request);
    assert_eq!(f.bridge.assist_sends.load(Ordering::SeqCst),1);
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst),1);
    assert_eq!(f.state.store.active_execution_count_for_key(&f.owner.key_id).unwrap(),0);
}
#[tokio::test]
async fn failed_work_with_orphan_parent_releases_execution_without_repaying_helper() {
    let f=Fixture::new();let b=create();
    *f.bridge.video_prepare_error.lock().unwrap()=Some((400,"invalid_budget_business_request".into()));
    let initial=f.response(&f.key,"orphan-failed-work",&b).await;
    let _=to_bytes(initial.into_body(),256*1024).await.unwrap();
    let request=f.bridge.claims.lock().unwrap().values().find(|v|v["step_kind"]=="assist").unwrap()["parent_request_id"].as_str().unwrap().to_owned();
    let version=f.state.store.work_version_for_request(&f.owner,&request).unwrap().unwrap();
    assert_eq!(version.state,aiwork_core::WorkVersionState::Failed);
    let before=f.state.store.budget_operation(&request).unwrap().unwrap().steps;
    let db=rusqlite::Connection::open(f.dir.join("data/core.sqlite3")).unwrap();
    // Reproduce the historical split: work failed, helper ended, parent did not.
    db.execute("UPDATE budget_operations SET execution_state='running',execution_finished_at_ms=NULL WHERE parent_request_id=?1",[&request]).unwrap();
    db.execute("UPDATE requests SET state='unknown',error_code=NULL,result_status=NULL WHERE id=?1",[&request]).unwrap();drop(db);
    assert_eq!(f.state.store.active_execution_count_for_key(&f.owner.key_id).unwrap(),1);
    let replay=f.response(&f.key,"orphan-failed-work",&b).await;
    let text=String::from_utf8(to_bytes(replay.into_body(),256*1024).await.unwrap().to_vec()).unwrap();
    assert!(text.contains("video_continuation_not_active"),"{text}");
    assert_eq!(f.state.store.active_execution_count_for_key(&f.owner.key_id).unwrap(),0);
    let after=f.state.store.budget_operation(&request).unwrap().unwrap();
    assert_eq!(after.execution_state,aiwork_core::BudgetExecutionState::Failed);
    assert_eq!(after.steps[0].financial_state,before[0].financial_state);
    assert_eq!(after.steps[0].actual_credits,before[0].actual_credits);
    assert_eq!(f.bridge.assist_sends.load(Ordering::SeqCst),1);
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst),0);
}
#[tokio::test]
async fn fenced_legacy_helper_dispatch_and_replay_keep_one_video_step() {
    let f=Fixture::with_gray_keys(false,Some(vec!["other-context-key".into()]));
    assert!(!f.state.config.work_context_for_key(&f.owner.key_id));
    *f.bridge.helper_raw.lock().unwrap()=Some("```json\n{\"intent\":\"video\",\"prompt\":\"橘猫在草地上散步\"}\n```".into());
    let b=create();
    let first=f.chat("fenced-legacy",&b).await;
    assert_eq!(f.chat("fenced-legacy",&b).await["request_id"],first["request_id"]);
    let op=f.state.store.budget_operation(first["request_id"].as_str().unwrap()).unwrap().unwrap();
    assert_eq!(op.execution_state,aiwork_core::BudgetExecutionState::Succeeded);
    assert_eq!(op.steps.len(),2);
    assert_eq!(f.bridge.assist_sends.load(Ordering::SeqCst),1);
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst),1);
}
#[tokio::test]
async fn fenced_read_only_helper_completes_without_video_dispatch() {
    let f=Fixture::new();
    *f.bridge.helper_raw.lock().unwrap()=Some("```json\n{\"action\":\"clarify\",\"effective_prompt\":null,\"spec_patch\":null,\"reference_policy\":null,\"clarification\":\"请说明新片段的动作；本次未提交视频。\"}\n```".into());
    for stream in [false,true] {
        let mut b=create();b["stream"]=json!(stream);
        let r=f.chat(if stream{"fenced-read-only-stream"}else{"fenced-read-only-json"},&b).await;
        let choice=&r["choices"][0];
        let text=if stream{choice["delta"]["content"].as_str()}else{choice["message"]["content"].as_str()}.unwrap();
        assert!(text.contains("请说明新片段的动作"));
    }
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst),0);
}
#[tokio::test]
async fn fenced_helper_video_plan_dispatches_once_with_original_reference_and_specs() {
    use starlink_dimension_router::assets::{self,ParsedAssetUpload};
    let f=Fixture::new();
    let stored=assets::write_asset(&f.state.config.data_dir,&f.owner,ParsedAssetUpload{filename:"source.mp4".into(),declared_mime:Some("video/mp4".into()),bytes:b"\x00\x00\x00\x18ftypisom\x00\x00\x00\x00isomiso2".to_vec()}).unwrap();
    let asset=assets::persist_asset(&f.state.store,&f.owner,&stored).unwrap();
    *f.bridge.helper_raw.lock().unwrap()=Some("```json\n{\"action\":\"create\",\"effective_prompt\":\"从上传视频结尾生成新片段,保留人物与服装\",\"spec_patch\":{\"duration\":10,\"resolution\":\"720p\",\"ratio\":\"9:16\",\"watermark\":false},\"reference_policy\":\"replace\",\"clarification\":null}\n```".into());
    let b=json!({"model":"seedance","video_asset_ids":[asset.id],"messages":[{"role":"user","content":"使用本次上传的视频作为参考，从它的结尾继续生成一个新片段。总时长10秒720P竖屏9:16，保留人物与服装，无水印。"}]});
    let response=f.response(&f.key,"fenced-reference-plan",&b).await;
    let status=response.status();
    let result:Value=serde_json::from_slice(&to_bytes(response.into_body(),256*1024).await.unwrap()).unwrap();
    assert_eq!(status,StatusCode::OK,"{result}");
    assert!(result.get("error").is_none(),"valid fenced planning JSON must not become a provider error: {result}");
    let request=result["request_id"].as_str().unwrap();
    let v=f.state.store.work_version_for_request(&f.owner,request).unwrap().unwrap();
    assert_eq!(v.state,aiwork_core::WorkVersionState::Completed);
    let snapshot=work_context::read_snapshot(&f.state,&f.owner,&v).unwrap();
    let wire=snapshot.dispatch_body.unwrap();
    assert_eq!((wire["duration"].clone(),wire["resolution"].clone(),wire["ratio"].clone()),(json!(10),json!("720p"),json!("9:16")));
    assert_eq!(wire["video_asset_ids"].as_array().unwrap().len(),1);
    let media=f.state.store.owned_work_media(&f.owner,&snapshot.user_media_ids[0]).unwrap().unwrap();
    assert_eq!(media.content_sha256,asset.sha256);
    assert_eq!(f.chat("fenced-reference-plan",&b).await["request_id"],request);
    assert_eq!(f.bridge.assist_sends.load(Ordering::SeqCst),1);
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst),1);
}
#[tokio::test]
async fn helper_null_reference_policy_clarification_completes_without_video_or_generic_error() {
    let f=Fixture::new();
    *f.bridge.helper_decision.lock().unwrap()=Some(json!({"action":"clarify","effective_prompt":null,"spec_patch":null,"reference_policy":null,"clarification":"请说明新片段中的动作与时长；本次未提交视频。"}));
    for stream in [false,true] {
        let mut b=create();b["stream"]=json!(stream);
        let r=f.chat(if stream{"null-policy-stream"}else{"null-policy-json"},&b).await;
        let choice=&r["choices"][0];
        let text=if stream{choice["delta"]["content"].as_str()}else{choice["message"]["content"].as_str()}.unwrap();
        assert!(text.contains("请说明新片段中的动作与时长"));
    }
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst),0);
    assert_eq!(f.bridge.assist_sends.load(Ordering::SeqCst),2);
}
#[tokio::test]
async fn video_attachment_without_prior_script_never_dispatches_helper_or_video() {
    use starlink_dimension_router::assets::{self,ParsedAssetUpload};
    let f=Fixture::new();
    let stored=assets::write_asset(&f.state.config.data_dir,&f.owner,ParsedAssetUpload{filename:"source.mp4".into(),declared_mime:Some("video/mp4".into()),bytes:b"\x00\x00\x00\x18ftypisom\x00\x00\x00\x00isomiso2".to_vec()}).unwrap();
    let asset=assets::persist_asset(&f.state.store,&f.owner,&stored).unwrap();
    let b=json!({"model":"seedance","video_asset_ids":[asset.id],"messages":[{"role":"user","content":"<system-reminder>Use PowerShell</system-reminder>"}]});
    let r=f.chat("empty-video-script",&b).await;
    assert!(r["choices"][0]["message"]["content"].as_str().unwrap().contains("请说明"));
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst),0);
    assert_eq!(f.bridge.assist_sends.load(Ordering::SeqCst),0);
}
#[tokio::test]
async fn natural_portrait_specs_reach_paid_wire_once_and_survive_revision() {
    let f=Fixture::new();
    *f.bridge.helper_decision.lock().unwrap()=Some(json!({"action":"create","effective_prompt":"普通亚洲女性夜晚独自走在小巷中","spec_patch":{"ratio":"9:16","resolution":"720p","watermark":false},"reference_policy":"replace","clarification":null}));
    let b=json!({"model":"seedance","messages":[{"role":"user","content":"<user_input>之前生成5秒480P 16:9视频</user_input><user_input>写实电影感夜景人像：一位普通亚洲女性走在小巷中。画面为竖构图，高分辨率，无文字、无水印。</user_input>"}]});
    let r=f.chat("natural-portrait",&b).await;
    let version=f.state.store.work_version_for_request(&f.owner,r["request_id"].as_str().unwrap()).unwrap().unwrap();
    let snapshot=work_context::read_snapshot(&f.state,&f.owner,&version).unwrap();
    let wire=snapshot.dispatch_body.unwrap();
    assert_eq!((wire["duration"].clone(),wire["resolution"].clone(),wire["ratio"].clone()),(json!(5),json!("720p"),json!("9:16")));
    assert_eq!(f.chat("natural-portrait",&b).await["request_id"],r["request_id"]);
    assert_eq!(f.bridge.assist_sends.load(Ordering::SeqCst),1);
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst),1);
    *f.bridge.helper_decision.lock().unwrap()=None;
    let next=f.chat("natural-portrait-revise",&revise(&r,"动作放慢，其他不变")).await;
    let version=f.state.store.work_version_for_request(&f.owner,next["request_id"].as_str().unwrap()).unwrap().unwrap();
    let snapshot=work_context::read_snapshot(&f.state,&f.owner,&version).unwrap();
    assert_eq!((snapshot.duration,snapshot.resolution.as_str(),snapshot.ratio.as_str()),(5,"720p","9:16"));
}
#[tokio::test]
async fn source_video_extension_preserves_full_asset_without_tail_fallback() {
    use starlink_dimension_router::assets::{self,ParsedAssetUpload};
    let f=Fixture::new();
    let stored=assets::write_asset(&f.state.config.data_dir,&f.owner,ParsedAssetUpload{filename:"source.mp4".into(),declared_mime:Some("video/mp4".into()),bytes:b"\x00\x00\x00\x18ftypisom\x00\x00\x00\x00isomiso2".to_vec()}).unwrap();
    let asset=assets::persist_asset(&f.state.store,&f.owner,&stored).unwrap();
    *f.bridge.helper_decision.lock().unwrap()=Some(json!({"action":"continue","effective_prompt":"保留原视频前5秒，再向后延长5秒","spec_patch":{"duration":10,"resolution":"480p","ratio":"16:9","watermark":false},"reference_policy":"replace","clarification":null}));
    let b=json!({"model":"seedance","duration":10,"resolution":"480p","ratio":"16:9","video_asset_ids":[asset.id],"messages":[{"role":"user","content":"向后延长上传视频"}]});
    let result=f.chat("source-video-extend",&b).await;
    let version=f.state.store.work_version_for_request(&f.owner,result["request_id"].as_str().unwrap()).unwrap().unwrap();
    assert_eq!(version.action,aiwork_core::WorkAction::Create);
    assert!(version.parent_version_id.is_none());
    let snapshot=work_context::read_snapshot(&f.state,&f.owner,&version).unwrap();
    assert!(snapshot.tail_frame_media_id.is_none());
    assert_eq!(snapshot.user_media_ids.len(),1);
    let media=f.state.store.owned_work_media(&f.owner,&snapshot.user_media_ids[0]).unwrap().unwrap();
    assert_eq!(media.content_sha256,asset.sha256);
    let wire=snapshot.dispatch_body.unwrap();
    assert_eq!(wire["duration"],10);
    assert_eq!(wire["prompt"],"保留原视频前5秒，再向后延长5秒");
    assert_eq!(wire["video_asset_ids"].as_array().unwrap().len(),1);
    assert!(wire["image_asset_ids"].is_null());
    assert_eq!(f.bridge.frame_reads.load(Ordering::SeqCst),0);
    let replay=f.chat("source-video-extend",&b).await;
    assert_eq!(replay["request_id"],result["request_id"]);
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst),1);
    assert_eq!(f.bridge.assist_sends.load(Ordering::SeqCst),1);
}
#[tokio::test]
async fn request_unique_binding() {
    let f = Fixture::new();
    let result = f.chat("create", &create()).await;
    let request = result["request_id"].as_str().unwrap();
    let v = f
        .state
        .store
        .work_version_for_request(&f.owner, request)
        .unwrap()
        .expect("every paid video needs its exact version");
    assert_eq!(result["work_context"]["base_version_id"], v.version_id);
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn same_retry_reuses_immutable_snapshot() {
    let f = Fixture::new();
    let b = create();
    let first = f.chat("stable", &b).await;
    let replay = f.chat("stable", &b).await;
    assert_eq!(first["work_context"], replay["work_context"]);
    let v = f
        .state
        .store
        .work_version_for_request(&f.owner, first["request_id"].as_str().unwrap())
        .unwrap()
        .unwrap();
    let snapshot = work_context::read_snapshot(&f.state, &f.owner, &v).unwrap();
    assert_eq!(snapshot.duration, 5);
    assert_eq!(snapshot.ratio, "9:16");
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst), 1);
    assert_eq!(f.bridge.assist_sends.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn parallel_branches_keep_parents_and_balances() {
    let f = Fixture::new();
    let v1 = f.chat("v1", &create()).await;
    let a = revise(&v1, "改成夜景，其他不变");
    let b = revise(&v1, "动作放慢，其他不变");
    let (a, b) = tokio::join!(f.chat("a", &a), f.chat("b", &b));
    assert_eq!(a["work_context"]["work_id"], v1["work_context"]["work_id"]);
    assert_ne!(a["request_id"], b["request_id"]);
    for result in [a, b] {
        let v = f
            .state
            .store
            .work_version_for_request(&f.owner, result["request_id"].as_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            v.parent_version_id.as_deref(),
            v1["work_context"]["base_version_id"].as_str()
        );
        let snapshot = work_context::read_snapshot(&f.state, &f.owner, &v).unwrap();
        assert_eq!(snapshot.resolution, "480p");
        let op = f
            .state
            .store
            .budget_operation(&v.operation_request_id)
            .unwrap()
            .unwrap();
        assert_eq!(op.api_key_id, f.owner.key_id);
        assert_eq!(op.steps.len(), 2);
    }
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst), 3);
}
#[tokio::test]
async fn different_keys_never_share_budget() {
    let f = Fixture::new();
    let first = f.chat("first", &create()).await;
    let other = f
        .state
        .store
        .issue_api_key(
            "admin",
            "other",
            BTreeSet::from(["admin:*".into()]),
            "bootstrap",
        )
        .unwrap();
    let r = f
        .response(&other.plaintext, "foreign", &revise(&first, "改成夜景"))
        .await;
    assert_ne!(r.status(), StatusCode::OK);
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn late_completion_does_not_overwrite_selected_version() {
    let f = Fixture::new();
    let first = f.chat("first", &create()).await;
    f.chat("other", &create()).await;
    let second = f.chat("revision", &revise(&first, "改成夜景")).await;
    let v = f
        .state
        .store
        .work_version_for_request(&f.owner, second["request_id"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(
        v.parent_version_id.as_deref(),
        first["work_context"]["base_version_id"].as_str()
    );
}
#[tokio::test]
async fn restart_after_dispatch_does_not_resubmit() {
    let f = Fixture::new();
    let b = create();
    let first = f.chat("restart", &b).await;
    let store = Arc::new(CoreStore::open(&f.dir).unwrap());
    store.migrate().unwrap();
    let state = StarlinkRouterState::for_test(
        store,
        BridgeClient::from_transport("http://bridge", "bridge-only", f.bridge.clone()),
        f.state.config.clone(),
    );
    let app = starlink_dimension_router::server::build_router(state);
    let r = app
        .oneshot(
            Request::post("/v1/chat/completions")
                .header("authorization", format!("Bearer {}", f.key))
                .header("idempotency-key", "restart")
                .header("content-type", "application/json")
                .body(Body::from(b.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let replay: Value =
        serde_json::from_slice(&to_bytes(r.into_body(), 256 * 1024).await.unwrap()).unwrap();
    assert_eq!(replay["work_context"], first["work_context"]);
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn resume_ignores_expired_transient_asset() {
    let f = Fixture::new();
    let mut b = create();
    b["messages"][0]["content"] = json!([{"type":"text","text":"这是参考图，生成视频"},{"type":"image_url","image_url":{"url":"data:image/png;base64,iVBORw0KGgpmaXh0dXJl"}}]);
    let result = f.chat("with-image", &b).await;
    let v = f
        .state
        .store
        .work_version_for_request(&f.owner, result["request_id"].as_str().unwrap())
        .unwrap()
        .unwrap();
    let s = work_context::read_snapshot(&f.state, &f.owner, &v).unwrap();
    assert_eq!(s.user_media_ids.len(), 1);
    let mut child = revise(&result, "使用参考图把刚才的视频改成夜景");
    child["action"] = json!("revise");
    let child_result = f.chat("child", &child).await;
    let before = f.bridge.video_sends.load(Ordering::SeqCst);
    let db = rusqlite::Connection::open(f.dir.join("data/core.sqlite3")).unwrap();
    db.execute("UPDATE assets SET created_at_ms=0,expires_at_ms=1", [])
        .unwrap();
    drop(db);
    let replay = f.chat("child", &child).await;
    assert_eq!(replay["work_context"], child_result["work_context"]);
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst), before);
    let next = f
        .chat("next", &revise(&result, "使用参考图把刚才的视频改成夜景"))
        .await;
    assert_eq!(
        next["work_context"]["work_id"],
        result["work_context"]["work_id"]
    );
}

#[tokio::test]
async fn status_and_clarification_do_not_prepare_or_dispatch_paid_work() {
    let f = Fixture::new();
    let first = f.chat("initial", &create()).await;
    let before = f.bridge.assist_sends.load(Ordering::SeqCst);
    for (id, text) in [("status", "查看任务状态"), ("vague", "这个视频不满意")] {
        let result = f.chat(id, &revise(&first, text)).await;
        assert_eq!(result["work_context"], first["work_context"]);
    }
    assert_eq!(f.bridge.assist_sends.load(Ordering::SeqCst), before+2, "natural-language status and dissatisfaction should be classified by the helper");
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn direct_video_endpoint_returns_owned_context_and_replay_never_resubmits() {
    let f = Fixture::new();
    let body = json!({"model":"seedance","prompt":"生成橘猫散步视频","duration":5,"resolution":"480p","ratio":"9:16"});
    let send = || {
        f.app.clone().oneshot(
            Request::post("/v1/videos/generations")
                .header("authorization", format!("Bearer {}", f.key))
                .header("idempotency-key", "direct")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
    };
    let r = send().await.unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    let accepted: Value =
        serde_json::from_slice(&to_bytes(r.into_body(), 64 * 1024).await.unwrap()).unwrap();
    let request = accepted["task"]["id"].as_str().unwrap();
    assert!(accepted["work_context"]["work_id"].is_string());
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let r = f
                .app
                .clone()
                .oneshot(
                    Request::get(format!("/v1/videos/{request}"))
                        .header("authorization", format!("Bearer {}", f.key))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::OK);
            let result: Value =
                serde_json::from_slice(&to_bytes(r.into_body(), 64 * 1024).await.unwrap()).unwrap();
            if result["task"]["status"] == "completed" {
                let v = f
                    .state
                    .store
                    .work_version_for_request(&f.owner, request)
                    .unwrap()
                    .unwrap();
                assert_eq!(result["work_context"]["base_version_id"], v.version_id);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    let replay = send().await.unwrap();
    assert_eq!(replay.status(), StatusCode::ACCEPTED);
    let replay: Value =
        serde_json::from_slice(&to_bytes(replay.into_body(), 64 * 1024).await.unwrap()).unwrap();
    assert_eq!(replay["task"]["id"], request);
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst), 1);
    assert_eq!(f.bridge.assist_sends.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn independent_creation_never_inherits_previous_media() {
    let f = Fixture::new();
    let mut b = create();
    b["messages"][0]["content"] = json!([{"type":"text","text":"这是参考图，生成视频"},{"type":"image_url","image_url":{"url":"data:image/png;base64,iVBORw0KGgpmaXh0dXJl"}}]);
    let first = f.chat("ref", &b).await;
    let second = f
        .chat(
            "independent",
            &revise(&first, "独立生成一个新视频，不继承旧场景"),
        )
        .await;
    let v = f
        .state
        .store
        .work_version_for_request(&f.owner, second["request_id"].as_str().unwrap())
        .unwrap()
        .unwrap();
    let s = work_context::read_snapshot(&f.state, &f.owner, &v).unwrap();
    assert_ne!(v.work_id, first["work_context"]["work_id"]);
    assert!(v.parent_version_id.is_none());
    assert!(s.user_media_ids.is_empty());
    assert!(s.dispatch_body.unwrap()["image_asset_ids"].is_null());
}
#[tokio::test]
async fn explicit_independent_work_in_same_client_conversation_is_followable() {
    let f = Fixture::new();
    let mut b = create();
    b["client_namespace"] = json!("client-a");
    b["conversation_id"] = json!("conversation-a");
    let first = f.chat("client-first", &b).await;
    b["action"] = json!("create");
    let second = f.chat("client-second", &b).await;
    assert_ne!(
        second["work_context"]["work_id"],
        first["work_context"]["work_id"]
    );
    let mut child = revise(&second, "改成夜景");
    child["client_namespace"] = json!("client-a");
    child["conversation_id"] = json!("conversation-a");
    let revised = f.chat("client-child", &child).await;
    assert_eq!(
        revised["work_context"]["work_id"],
        second["work_context"]["work_id"]
    );
    assert!(revised["request_id"].is_string());
}
#[tokio::test]
async fn unknown_execution_keeps_original_hold() {
    let f = Fixture::new();
    f.bridge.unknown.store(true, Ordering::SeqCst);
    let mut b = create();
    b["stream"] = json!(true);
    let r = f.response(&f.key, "unknown", &b).await;
    assert_eq!(r.status(), StatusCode::OK);
    drop(r);
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while f.bridge.video_sends.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    let claims = f.bridge.claims.lock().unwrap();
    let request = claims.values().find(|c| c["step_kind"] == "video").unwrap()["request_id"]
        .as_str()
        .unwrap()
        .to_owned();
    drop(claims);
    let op = f.state.store.budget_operation(&request).unwrap().unwrap();
    let video = op
        .steps
        .iter()
        .find(|s| s.kind == aiwork_core::BudgetStepKind::Video)
        .unwrap();
    assert!(video.dispatch_attempted);
    assert!(!matches!(
        video.financial_state,
        aiwork_core::BudgetFinancialState::Released
    ));
    assert!(f
        .state
        .store
        .work_version_for_request(&f.owner, &request)
        .unwrap()
        .is_some());
    f.bridge.unknown.store(false, Ordering::SeqCst);
    let result = f.chat("unknown", &b).await;
    assert_eq!(result["request_id"], request);
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst), 1);
}
