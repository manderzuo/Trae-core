#[path = "support/work_fixture.rs"]
mod fixture;
use fixture::*;
use starlink_dimension_router::work_continuation;
async fn base(f: &Fixture) -> (Value, aiwork_core::VideoWorkVersion) {
    let r = f.chat("base", &create()).await;
    let v = f
        .state
        .store
        .work_version_for_request(&f.owner, r["request_id"].as_str().unwrap())
        .unwrap()
        .unwrap();
    (r, v)
}
fn continue_body(r: &Value) -> Value {
    let mut b = revise(r, "继续下一段，其他不变");
    b["action"] = json!("continue");
    b
}
#[tokio::test]
async fn natural_language_tail_request_does_not_auto_select_native_extension() {
    let f=Fixture::with_continuation(true);let(r,_)=base(&f).await;
    f.bridge.native_capability.store(true,Ordering::SeqCst);
    let mut b=continue_body(&r);b["messages"][1]["content"]=json!("截取尾帧做参考，续写10秒视频");
    let child=f.chat("text-tail",&b).await;
    assert_eq!(child["work_context"]["reference_mode"],"tail_reference");
    assert!(f.bridge.source_reads.lock().unwrap().is_empty());
}
#[tokio::test]
async fn uploaded_video_tail_reference_replaces_video_and_replay_never_pays_again() {
    use starlink_dimension_router::assets;
    let f=Fixture::with_continuation(true);
    let bytes=b"\x00\x00\x00\x18ftypmp42source-video".to_vec();
    let stored=assets::write_asset(&f.dir,&f.owner,assets::ParsedAssetUpload{filename:"source.mp4".into(),declared_mime:Some("video/mp4".into()),bytes}).unwrap();
    let asset=assets::persist_asset(&f.state.store,&f.owner,&stored).unwrap();
    let b=json!({"model":"seedance","video_asset_ids":[asset.id],"messages":[{"role":"user","content":"截取上传视频的尾帧做参考，生成5秒视频"}]});
    let reply=f.chat("upload-tail",&b).await;
    let v=f.state.store.work_version_for_request(&f.owner,reply["request_id"].as_str().unwrap()).unwrap().unwrap();
    let s=work_context::read_snapshot(&f.state,&f.owner,&v).unwrap();
    assert_eq!(s.reference_mode,"tail_reference");
    let wire=s.dispatch_body.unwrap();
    assert_eq!(wire["image_asset_ids"].as_array().unwrap().len(),1);
    assert!(wire["video_asset_ids"].as_array().is_none_or(|v|v.is_empty()));
    assert_eq!(f.bridge.uploaded_frame_reads.load(Ordering::SeqCst),1);
    assert_eq!(f.chat("upload-tail",&b).await["request_id"],reply["request_id"]);
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst),1);
    assert_eq!(f.bridge.assist_sends.load(Ordering::SeqCst),1);
    let mut next=revise(&reply,"改成夜景，其他不变");
    let revision=f.chat("upload-tail-revise",&next).await;
    let revised=f.state.store.work_version_for_request(&f.owner,revision["request_id"].as_str().unwrap()).unwrap().unwrap();
    let revised=work_context::read_snapshot(&f.state,&f.owner,&revised).unwrap();
    assert_eq!(revised.dispatch_body.unwrap()["image_asset_ids"].as_array().unwrap().len(),1,"revision must retain the uploaded tail reference");
    next=revise(&reply,"截取本次上传的视频尾帧，继续生成5秒视频");
    next["action"]=json!("continue");next["video_asset_ids"]=b["video_asset_ids"].clone();
    f.bridge.native_capability.store(true,Ordering::SeqCst);
    let replaced=f.chat("upload-tail-parent",&next).await;
    assert_eq!(replaced["work_context"]["reference_mode"],"tail_reference");
    assert!(f.bridge.source_reads.lock().unwrap().is_empty(),"explicit uploaded source must not use the old parent video");
}
#[tokio::test]
async fn uploaded_video_tail_failure_or_mismatched_proof_never_dispatches_video() {
    use starlink_dimension_router::assets;
    for mismatch in [false,true] {
        let f=Fixture::with_continuation(true);
        f.bridge.frame_failure.store(!mismatch,Ordering::SeqCst);
        f.bridge.source_identity_bad.store(mismatch,Ordering::SeqCst);
        let stored=assets::write_asset(&f.dir,&f.owner,assets::ParsedAssetUpload{filename:"source.mp4".into(),declared_mime:Some("video/mp4".into()),bytes:b"\x00\x00\x00\x18ftypmp42source-video".to_vec()}).unwrap();
        let asset=assets::persist_asset(&f.state.store,&f.owner,&stored).unwrap();
        let b=json!({"model":"seedance","video_asset_ids":[asset.id],"messages":[{"role":"user","content":"截取尾帧做参考，生成5秒视频"}]});
        let r=f.response(&f.key,"failed-tail",&b).await;
        let text=String::from_utf8(to_bytes(r.into_body(),65536).await.unwrap().to_vec()).unwrap();
        assert!(text.contains(if mismatch{"frame_identity_invalid"}else{"frame_extraction_unavailable"}),"{text}");
        assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst),0);
    }
}
#[tokio::test]
async fn parent_tail_does_not_reextract_an_inherited_original_video() {
    use starlink_dimension_router::assets;
    let f=Fixture::with_continuation(true);
    let stored=assets::write_asset(&f.dir,&f.owner,assets::ParsedAssetUpload{filename:"original.mp4".into(),declared_mime:Some("video/mp4".into()),bytes:b"\x00\x00\x00\x18ftypmp42original".to_vec()}).unwrap();
    let asset=assets::persist_asset(&f.state.store,&f.owner,&stored).unwrap();
    let mut b=create();b["video_asset_ids"]=json!([asset.id]);
    let first=f.chat("original-video",&b).await;
    let parent=f.state.store.work_version_for_request(&f.owner,first["request_id"].as_str().unwrap()).unwrap().unwrap();
    let frame=work_continuation::warm_tail_frame(f.state.clone(),f.owner.clone(),parent).await.unwrap();
    f.bridge.frame_failure.store(true,Ordering::SeqCst);
    let mut b=revise(&first,"截取刚生成的视频尾帧做参考，继续5秒");b["action"]=json!("continue");
    let child=f.chat("parent-tail",&b).await;
    let version=f.state.store.work_version_for_request(&f.owner,child["request_id"].as_str().unwrap()).unwrap().unwrap();
    let snapshot=work_context::read_snapshot(&f.state,&f.owner,&version).unwrap();
    assert_eq!(snapshot.tail_frame_media_id,Some(frame.media_id));
    let wire=snapshot.dispatch_body.unwrap();
    assert!(wire["video_asset_ids"].as_array().is_none_or(|v|v.is_empty()));
    assert_eq!(wire["image_asset_ids"].as_array().unwrap().len(),1);
}
#[tokio::test]
async fn native_auto_uses_exact_parent_video_and_replay_does_not_resubmit() {
    let f=Fixture::with_continuation(true);
    let (r,v)=base(&f).await;
    work_continuation::warm_tail_frame(f.state.clone(),f.owner.clone(),v.clone()).await.unwrap();
    f.bridge.frame_failure.store(true,Ordering::SeqCst);
    f.bridge.native_capability.store(true,Ordering::SeqCst);
    let before_frames=f.bridge.frame_reads.load(Ordering::SeqCst);
    let mut b=continue_body(&r);b["duration"]=json!(7);
    let child=f.chat("native-child",&b).await;
    assert_eq!(child["work_context"]["reference_mode"],"native_video_extend");
    let version=f.state.store.work_version_for_request(&f.owner,child["request_id"].as_str().unwrap()).unwrap().unwrap();
    assert_eq!(version.parent_version_id,Some(v.version_id));
    let s=work_context::read_snapshot(&f.state,&f.owner,&version).unwrap();
    let wire=s.dispatch_body.unwrap();
    assert_eq!(wire["duration"],7);
    assert_eq!(wire["video_asset_ids"].as_array().unwrap().len(),1);
    assert!(wire["image_asset_ids"].is_null());
    assert!(wire["prompt"].as_str().unwrap().contains("新片段"));
    assert_eq!(f.bridge.source_reads.lock().unwrap().as_slice(),[r["request_id"].as_str().unwrap()]);
    assert_eq!(f.bridge.frame_reads.load(Ordering::SeqCst),before_frames);
    let replay=f.chat("native-child",&b).await;
    assert_eq!(replay["request_id"],child["request_id"]);
    assert_eq!(f.bridge.source_reads.lock().unwrap().len(),1);
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst),2);
    let grandchild=f.chat("native-grandchild",&continue_body(&child)).await;
    assert_eq!(grandchild["work_context"]["reference_mode"],"native_video_extend");
    assert_eq!(f.bridge.source_reads.lock().unwrap()[1],child["request_id"].as_str().unwrap());
    let op=f.state.store.budget_operation(grandchild["request_id"].as_str().unwrap()).unwrap().unwrap();
    let claims=f.bridge.claims.lock().unwrap();
    assert_eq!(claims[&op.parent_request_id]["body"]["video_asset_ids"].as_array().unwrap().len(),1,"must not accumulate ancestor video clips");
}
#[tokio::test]
async fn native_source_failure_never_falls_back_to_tail_or_text_only() {
    for wrong_identity in [false,true] {
        let f=Fixture::with_continuation(true);let (r,_)=base(&f).await;
        f.bridge.native_capability.store(true,Ordering::SeqCst);
        f.bridge.source_failure.store(!wrong_identity,Ordering::SeqCst);
        f.bridge.source_identity_bad.store(wrong_identity,Ordering::SeqCst);
        let mut b=continue_body(&r);b["continuation_mode"]=json!("native_video_extend");
        let resp=f.response(&f.key,"missing-source",&b).await;
        let text=String::from_utf8(to_bytes(resp.into_body(),65536).await.unwrap().to_vec()).unwrap();
        assert!(text.contains(if wrong_identity{"source_video_identity_invalid"}else{"source_video_unavailable"}),"{text}");
        assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst),1);
        let replay=f.response(&f.key,"missing-source",&b).await;drop(replay);
        assert_eq!(f.bridge.assist_sends.load(Ordering::SeqCst),2,"must not repay helper");
    }
}
#[tokio::test]
async fn native_does_not_read_foreign_parent_and_explicit_tail_stays_tail() {
    let f=Fixture::with_continuation(true);let(r,v)=base(&f).await;f.bridge.native_capability.store(true,Ordering::SeqCst);
    let mut alien=f.owner.clone();alien.key_id="foreign-key".into();
    let result=work_continuation::prepare_continuation(f.state.clone(),alien,v,json!({"prompt":"continue","continuation_mode":"native_video_extend"}),"request-foreign").await;
    assert!(result.is_err());assert!(f.bridge.source_reads.lock().unwrap().is_empty());
    let mut b=continue_body(&r);b["continuation_mode"]=json!("tail_reference");
    let child=f.chat("explicit-tail",&b).await;
    assert_eq!(child["work_context"]["reference_mode"],"tail_reference");assert!(f.bridge.source_reads.lock().unwrap().is_empty());
}
#[tokio::test]
async fn last_frame_bound_to_parent_video() {
    let f = Fixture::with_continuation(true);
    let (_, v) = base(&f).await;
    let m = work_continuation::warm_tail_frame(f.state.clone(), f.owner.clone(), v.clone())
        .await
        .unwrap();
    assert_eq!(m.work_id, v.work_id);
    assert_eq!(m.kind, "tail_frame");
    let v = f
        .state
        .store
        .owned_work_version(&f.owner, &v.version_id)
        .unwrap()
        .unwrap();
    assert_eq!(v.tail_frame_media_id, Some(m.media_id));
    assert_eq!(v.frame_state, "ready");
}
#[tokio::test]
async fn auto_uses_only_verified_capability() {
    let f = Fixture::with_continuation(true);
    let (r, v) = base(&f).await;
    let result = f.chat("next", &continue_body(&r)).await;
    assert_eq!(result["work_context"]["reference_mode"], "tail_reference");
    assert!(result["choices"][0]["message"]["content"]
        .as_str()
        .unwrap()
        .contains("近似参考"));
    let next = f
        .state
        .store
        .work_version_for_request(&f.owner, result["request_id"].as_str().unwrap())
        .unwrap()
        .unwrap();
    let s = work_context::read_snapshot(&f.state, &f.owner, &next).unwrap();
    assert_eq!(s.parent_version_id, Some(v.version_id));
    assert_eq!(s.reference_mode, "tail_reference");
    assert!(s.tail_frame_media_id.is_some());
    f.bridge.tail_capability.store(false, Ordering::SeqCst);
    let r = f
        .response(&f.key, "disabled", &continue_body(&result))
        .await;
    assert_ne!(r.status(), StatusCode::OK);
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst), 2);
}
#[tokio::test]
async fn strict_mode_rejects_without_silent_fallback() {
    let f = Fixture::with_continuation(true);
    let (r, _) = base(&f).await;
    let mut b = continue_body(&r);
    b["continuation_mode"] = json!("native_first_frame");
    let response = f.response(&f.key, "strict", &b).await;
    assert_ne!(response.status(), StatusCode::OK);
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn missing_parent_never_text_only() {
    let f = Fixture::with_continuation(true);
    let mut b = create();
    b["action"] = json!("continue");
    let r = f.response(&f.key, "no-parent", &b).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn reference_pricing_uses_actual_counts() {
    let f = Fixture::with_continuation(true);
    let mut b = create();
    b["messages"][0]["content"] = json!([{"type":"text","text":"参考图生成视频"},{"type":"image_url","image_url":{"url":"data:image/png;base64,iVBORw0KGgpmaXh0dXJl"}}]);
    let r = f.chat("image-base", &b).await;
    let next = f.chat("image-next", &continue_body(&r)).await;
    let v = f
        .state
        .store
        .work_version_for_request(&f.owner, next["request_id"].as_str().unwrap())
        .unwrap()
        .unwrap();
    let s = work_context::read_snapshot(&f.state, &f.owner, &v).unwrap();
    assert_eq!(s.user_media_ids.len(), 1);
    assert_eq!(
        s.dispatch_body.unwrap()["image_asset_ids"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}
#[tokio::test]
async fn continue_new_request_not_parent_retry() {
    let f = Fixture::with_continuation(true);
    let (r, _) = base(&f).await;
    let b = continue_body(&r);
    let next = f.chat("new-segment", &b).await;
    assert_ne!(r["request_id"], next["request_id"]);
    let replay = f.chat("new-segment", &b).await;
    assert_eq!(replay["work_context"], next["work_context"]);
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst), 2);
}
#[tokio::test]
async fn old_client_create_unchanged() {
    let f = Fixture::new();
    let r = f.chat("old", &create()).await;
    assert!(r["request_id"].is_string());
    assert_eq!(f.bridge.frame_reads.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn saved_tail_survives_original_mp4_cache_expiry() {
    let f = Fixture::with_continuation(true);
    let (r, v) = base(&f).await;
    work_continuation::warm_tail_frame(f.state.clone(), f.owner.clone(), v)
        .await
        .unwrap();
    let reads = f.bridge.frame_reads.load(Ordering::SeqCst);
    f.bridge.frame_failure.store(true, Ordering::SeqCst);
    let next = f.chat("saved", &continue_body(&r)).await;
    assert!(next["request_id"].is_string());
    assert_eq!(f.bridge.frame_reads.load(Ordering::SeqCst), reads);
}
#[tokio::test]
async fn preextract_failure_does_not_block_delivery_or_settlement() {
    let f = Fixture::with_continuation(true);
    f.bridge.frame_failure.store(true, Ordering::SeqCst);
    f.bridge.billing_final.store(true, Ordering::SeqCst);
    let (r, v) = base(&f).await;
    assert!(r["choices"][0]["message"]["content"]
        .as_str()
        .unwrap()
        .contains("视频"));
    assert_eq!(v.state, aiwork_core::WorkVersionState::Completed);
    assert!(
        work_continuation::warm_tail_frame(f.state.clone(), f.owner.clone(), v)
            .await
            .is_err()
    );
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst), 1);
    let started = std::time::Instant::now();
    let op = loop {
        let op = f
            .state
            .store
            .budget_operation(r["request_id"].as_str().unwrap())
            .unwrap()
            .unwrap();
        if op
            .steps
            .iter()
            .all(|s| s.financial_state == aiwork_core::BudgetFinancialState::Settled)
        {
            break op;
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "frame failure blocked background settlement"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    };
    for step in op.steps {
        assert_eq!(
            step.financial_state,
            aiwork_core::BudgetFinancialState::Settled
        );
        assert_eq!(step.actual_credits.unwrap().as_microcredits(), 1_250_000);
    }
}
#[tokio::test]
async fn public_work_read_and_continue_are_key_owned_and_keep_same_task_protocol() {
    let f = Fixture::with_continuation(true);
    let (r, v) = base(&f).await;
    let view = f
        .app
        .clone()
        .oneshot(
            Request::get(format!("/v1/video-works/{}", v.work_id))
                .header("authorization", format!("Bearer {}", f.key))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(view.status(), StatusCode::OK);
    let bytes = to_bytes(view.into_body(), 64 * 1024).await.unwrap();
    let view: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(view["versions"][0]["version_id"], v.version_id);
    assert!(!String::from_utf8_lossy(&bytes).contains("ciphertext"));
    let payload = json!({"base_version_id":v.version_id,"prompt":"继续下一段"});
    let next = f
        .app
        .clone()
        .oneshot(
            Request::post(format!("/v1/video-works/{}/continue", v.work_id))
                .header("authorization", format!("Bearer {}", f.key))
                .header("idempotency-key", "public-next")
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(next.status(), StatusCode::ACCEPTED);
    let next: Value =
        serde_json::from_slice(&to_bytes(next.into_body(), 64 * 1024).await.unwrap()).unwrap();
    assert_ne!(next["task"]["id"], r["request_id"]);
    assert_eq!(next["work_context"]["work_id"], v.work_id);
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
    let view = f
        .app
        .clone()
        .oneshot(
            Request::get(format!("/v1/video-works/{}", v.work_id))
                .header("authorization", format!("Bearer {}", other.plaintext))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(view.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn restart_recovers_completed_budget_before_frame_prefetch() {
    let f = Fixture::with_continuation(true);
    let (_, v) = base(&f).await;
    let con = rusqlite::Connection::open(f.dir.join("data/core.sqlite3")).unwrap();
    con.execute("UPDATE video_work_versions SET state='running',frame_state='pending',tail_frame_media_id=NULL,frame_key_version=NULL,encrypted_frame_metadata=NULL,frame_metadata_sha256=NULL WHERE version_id=?1",[&v.version_id]).unwrap();
    let rows = f.state.store.work_versions_needing_frames(8).unwrap();
    assert!(
        rows.iter()
            .any(|(_, candidate)| candidate.version_id == v.version_id),
        "completed budget lost its frame recovery candidate"
    );
    let media = work_continuation::warm_tail_frame(
        f.state.clone(),
        f.owner.clone(),
        rows.into_iter()
            .find(|(_, c)| c.version_id == v.version_id)
            .unwrap()
            .1,
    )
    .await
    .unwrap();
    assert_eq!(media.kind, "tail_frame");
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn frame_metadata_is_rotated_and_cross_version_ciphertext_is_rejected() {
    let f = Fixture::with_continuation(true);
    let (r, v) = base(&f).await;
    work_continuation::warm_tail_frame(f.state.clone(), f.owner.clone(), v.clone())
        .await
        .unwrap();
    let next = f.chat("second", &revise(&r, "改为夜景")).await;
    let second = f
        .state
        .store
        .work_version_for_request(&f.owner, next["request_id"].as_str().unwrap())
        .unwrap()
        .unwrap();
    work_continuation::warm_tail_frame(f.state.clone(), f.owner.clone(), second.clone())
        .await
        .unwrap();
    let vault = starlink_dimension_router::key_vault::KeyVault::from_material(
        2,
        [0x77; 32],
        BTreeMap::from([(1, [0x5a; 32])]),
    )
    .unwrap();
    let state = StarlinkRouterState::for_test_with_key_vault(
        f.state.store.clone(),
        BridgeClient::from_transport("http://bridge", "bridge-only", f.bridge.clone()),
        f.state.config.clone(),
        vault,
    );
    starlink_dimension_router::work_media::rotate(&state).unwrap();
    let rotated = state
        .store
        .owned_work_version(&f.owner, &v.version_id)
        .unwrap()
        .unwrap();
    assert_eq!(rotated.sealed_frame.as_ref().unwrap().key_version, 2);
    work_continuation::warm_tail_frame(state.clone(), f.owner.clone(), rotated)
        .await
        .unwrap();
    let con = rusqlite::Connection::open(f.dir.join("data/core.sqlite3")).unwrap();
    con.execute("UPDATE video_work_versions SET encrypted_frame_metadata=(SELECT encrypted_frame_metadata FROM video_work_versions WHERE version_id=?2) WHERE version_id=?1",rusqlite::params![v.version_id,second.version_id]).unwrap();
    assert_eq!(
        work_continuation::warm_tail_frame(state, f.owner.clone(), v)
            .await
            .unwrap_err(),
        "frame_identity_invalid"
    );
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst), 2);
}
