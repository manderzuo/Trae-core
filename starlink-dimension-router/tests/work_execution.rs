#[path = "support/work_fixture.rs"]
mod fixture;
use fixture::*;
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
    assert_eq!(wire["prompt"],"保留原视频前5秒,再向后延长5秒");
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
    assert_eq!(f.bridge.assist_sends.load(Ordering::SeqCst), before);
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
