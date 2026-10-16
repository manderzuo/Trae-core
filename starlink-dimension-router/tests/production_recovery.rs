#[path = "support/work_fixture.rs"]
mod fixture;
use fixture::*;
use starlink_dimension_router::{work_planner,work_continuation};

#[test]
fn planner_unicode_length_matches_declared_character_contract() {
    let limit=work_planner::helper_output_schema()["properties"]["effective_prompt"]["maxLength"].as_u64().unwrap() as usize;
    for ch in ["a","中","🧭"] {
        let value=json!({"action":"create","effective_prompt":ch.repeat(limit),"spec_patch":{},"reference_policy":"inherit","clarification":null});
        assert!(work_planner::parse_decision(&value.to_string()).is_ok(),"declared {limit} characters must be accepted for every Unicode scalar");
        let value=json!({"action":"create","effective_prompt":ch.repeat(limit+1),"spec_patch":{},"reference_policy":"inherit","clarification":null});
        assert!(work_planner::parse_decision(&value.to_string()).is_err());
    }
}

#[tokio::test]
async fn expired_tail_is_regenerated_from_exact_owned_video() {
    for (missing_file,expired) in [(false,true),(true,true),(true,false)] {
        let f=Fixture::with_continuation(true);let reply=f.chat("recover-tail-base",&create()).await;
        let request=reply["request_id"].as_str().unwrap();let parent=f.state.store.work_version_for_request(&f.owner,request).unwrap().unwrap();
        let media=work_continuation::warm_tail_frame(f.state.clone(),f.owner.clone(),parent).await.unwrap();
        let before=f.bridge.frame_reads.load(Ordering::SeqCst);
        if missing_file {
            let context=format!("work-media:{}:{}:{}:ref",media.owner_key_id,media.work_id,media.media_id);
            let r:Value=serde_json::from_str(&f.state.key_vault.decrypt(&context,media.key_version,&media.encrypted_storage_ref).unwrap()).unwrap();
            std::fs::remove_file(f.dir.join("data/work-media").join(r["file"].as_str().unwrap())).unwrap();
        }
        let db=rusqlite::Connection::open(f.dir.join("data").join(aiwork_core::CORE_DB_FILE)).unwrap();
        if expired {db.execute("UPDATE video_work_media SET expires_at_ms=?1 WHERE media_id=?2",rusqlite::params![chrono::Utc::now().timestamp_millis()-1,&media.media_id]).unwrap();}
        let parent=f.state.store.work_version_for_request(&f.owner,request).unwrap().unwrap();
        let renewed=work_continuation::warm_tail_frame(f.state.clone(),f.owner.clone(),parent).await.unwrap();
        assert_eq!(renewed.media_id,media.media_id);assert!(renewed.expires_at_ms>chrono::Utc::now().timestamp_millis());
        assert_eq!(f.bridge.frame_reads.load(Ordering::SeqCst),before+1);
    }
}

#[tokio::test]
async fn accepted_request_without_operation_is_a_recovery_candidate() {
    let f=Fixture::new();let body=create();
    let aiwork_core::BeginRequest::Created(request)=f.state.store.begin_billed_request(aiwork_core::BeginRequestInput {
        user_id:f.owner.user_id.clone(),api_key_id:f.owner.key_id.clone(),protocol:"openai".into(),endpoint:"videos".into(),model:"seedance".into(),idempotency_key:"recover-crash-before-helper".into(),body:body.clone(),
    }).unwrap()else{panic!("new request")};
    f.state.store.save_budget_continuation(&f.owner,&request.id,&body,1,&[0x51;64]).unwrap();
    let reopened=CoreStore::open(&f.dir).unwrap();reopened.migrate().unwrap();
    assert_eq!(reopened.pending_budget_continuation_ids_after("",100).unwrap(),vec![request.id]);
}

#[tokio::test]
async fn video_policy_rejection_happens_before_paid_helper() {
    let f=Fixture::new();*f.bridge.video_preflight_error.lock().unwrap()=Some((503,"budget_policy_expired".into()));
    let response=f.response(&f.key,"recover-policy-expired",&create()).await;
    let value:Value=serde_json::from_slice(&to_bytes(response.into_body(),65536).await.unwrap()).unwrap();
    assert_eq!(value["error"]["code"],"budget_policy_expired");
    assert_eq!(f.bridge.assist_sends.load(Ordering::SeqCst),0);assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst),0);
}

#[tokio::test]
async fn original_video_lookup_recovers_identity_without_creating_or_sending() {
    let f=Fixture::new();let body=create();let reply=f.chat("recover-original-video",&body).await;
    let sends=f.bridge.video_sends.load(Ordering::SeqCst);
    let call=|key:&str,body:Value|Request::post("/v1/videos/generations/lookup").header("authorization",format!("Bearer {}",f.key)).header("idempotency-key",key).header("content-type","application/json").body(Body::from(body.to_string())).unwrap();
    let response=f.app.clone().oneshot(call("recover-original-video",body.clone())).await.unwrap();
    assert_eq!(response.status(),StatusCode::OK);
    let value:Value=serde_json::from_slice(&to_bytes(response.into_body(),65536).await.unwrap()).unwrap();
    assert_eq!(value["request_id"],reply["request_id"]);
    let response=f.app.clone().oneshot(call("never-sent",body.clone())).await.unwrap();assert_eq!(response.status(),StatusCode::NOT_FOUND);
    let response=f.app.clone().oneshot(call("recover-original-video",json!({"model":"seedance","prompt":"different"}))).await.unwrap();assert_eq!(response.status(),StatusCode::CONFLICT);
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst),sends);
}
