#[path = "support/work_fixture.rs"]
mod fixture;
use fixture::*;

fn receipt(request: &str, status: &str) -> Value {
    json!({"model":"seedance","messages":[{"role":"tool",
        "tool_call_id":format!("call_seedance_save_{request}"),
        "content":{"seedance_delivery":1,"request_id":request,"status":status,
        "path":"C:\\Users\\Test\\Downloads\\video.mp4","bytes":1024}}]})
}

async fn detail(f: &Fixture, work: &str) -> Value {
    let reply=f.app.clone().oneshot(Request::get(format!("/v1/video-works/{work}"))
        .header("authorization",format!("Bearer {}",f.key)).body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(reply.status(),StatusCode::OK);
    serde_json::from_slice(&to_bytes(reply.into_body(),65536).await.unwrap()).unwrap()
}

#[tokio::test]
async fn saved_receipt_persists_across_store_reopen_without_new_billing() {
    let f=Fixture::new();
    let generated=f.chat("generate",&create()).await;
    let rid=generated["request_id"].as_str().unwrap();
    let before=f.state.store.budget_operation(rid).unwrap().unwrap();
    let version=f.state.store.work_version_for_request(&f.owner,rid).unwrap().unwrap();
    assert_eq!(version.delivery_state,"pending");
    for _ in 0..2 {
        let reply=f.chat("receipt",&receipt(rid,"saved")).await;
        assert_eq!(reply["video_delivery"]["status"],"saved");
    }
    let reopened=CoreStore::open(&f.dir).unwrap();
    assert_eq!(reopened.work_version_for_request(&f.owner,rid).unwrap().unwrap().delivery_state,"saved");
    assert_eq!(detail(&f,&version.work_id).await["versions"][0]["delivery_state"],"saved");
    assert_eq!(f.state.store.budget_operation(rid).unwrap().unwrap(),before);
    assert_eq!(f.bridge.video_sends.load(Ordering::SeqCst),1);
    assert_eq!(f.bridge.assist_sends.load(Ordering::SeqCst),1);
}

#[tokio::test]
async fn failed_download_can_recover_but_late_failure_cannot_regress_saved() {
    let f=Fixture::new();
    let generated=f.chat("generate",&create()).await;
    let rid=generated["request_id"].as_str().unwrap();
    for (status,want) in [("failed","download_failed"),("saved","saved"),("failed","saved")] {
        f.chat("receipt",&receipt(rid,status)).await;
        assert_eq!(f.state.store.work_version_for_request(&f.owner,rid).unwrap().unwrap().delivery_state,want);
    }
}

#[tokio::test]
async fn receipt_updates_only_its_request_not_a_newer_version() {
    let f=Fixture::new();
    let first=f.chat("first",&create()).await;
    let second=f.chat("second",&revise(&first,"修改视频为橘猫在草地上散步")).await;
    let rid=first["request_id"].as_str().unwrap();
    let newer=second["request_id"].as_str().unwrap();
    assert_ne!(rid,newer);
    f.chat("receipt",&receipt(rid,"saved")).await;
    assert_eq!(f.state.store.work_version_for_request(&f.owner,rid).unwrap().unwrap().delivery_state,"saved");
    assert_eq!(f.state.store.work_version_for_request(&f.owner,newer).unwrap().unwrap().delivery_state,"pending");
}

#[tokio::test]
async fn foreign_key_receipt_cannot_mark_another_keys_file_saved() {
    let f=Fixture::new();
    let generated=f.chat("generate",&create()).await;
    let rid=generated["request_id"].as_str().unwrap();
    let other=f.state.store.issue_api_key_with_max_concurrency("admin","other",BTreeSet::from(["admin:*".into()]),4,"bootstrap").unwrap();
    let response=f.response(&other.plaintext,"foreign",&receipt(rid,"saved")).await;
    assert_eq!(response.status(),StatusCode::BAD_REQUEST);
    assert_eq!(f.state.store.work_version_for_request(&f.owner,rid).unwrap().unwrap().delivery_state,"pending");
}
