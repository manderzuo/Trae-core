#[path = "support/work_fixture.rs"]
mod fixture;
use fixture::*;

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
