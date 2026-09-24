use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Cursor, Read},
    path::PathBuf,
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Router,
};
use chrono::Utc;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use starlink_dimension_router::{
    bridge_client::{BridgeClient, BridgeResponse, BridgeStreamingResponse, BridgeTransport},
    config::RouterConfig,
    server::build_router,
    state::StarlinkRouterState,
};
use tower::util::ServiceExt;

struct CapturedRequest {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

struct QuoteUnavailableBridge {
    requests: Mutex<Vec<CapturedRequest>>,
    quote_available: Mutex<bool>,
    receipt: Mutex<Value>,
    interrupt_chat: Mutex<bool>,
    video_status: Mutex<Value>,
    receipt_gate: Mutex<Option<Arc<TestGate>>>,
    stream_body_gate: Mutex<Option<Arc<TestGate>>>,
}

impl QuoteUnavailableBridge {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
            quote_available: Mutex::new(false),
            receipt: Mutex::new(json!({
                "status":"unknown",
                "actual_credits":null,
                "unit":"credits",
                "source_ref":null,
                "task_ref":null,
                "observed_at_ms":Utc::now().timestamp_millis()
            })),
            interrupt_chat: Mutex::new(false),
            video_status: Mutex::new(json!({
                "task":{"id":"video-upstream-1","status":"queued"}
            })),
            receipt_gate: Mutex::new(None),
            stream_body_gate: Mutex::new(None),
        })
    }

    fn set_quote_available(&self, available: bool) {
        *self.quote_available.lock().unwrap() = available;
    }

    fn set_receipt(&self, status: &str, actual_credits: Option<&str>) {
        *self.receipt.lock().unwrap() = json!({
            "status":status,
            "actual_credits":actual_credits,
            "unit":"credits",
            "source_ref":"upstream-request-7",
            "task_ref":"video-upstream-1",
            "observed_at_ms":Utc::now().timestamp_millis()
        });
    }

    fn interrupt_chat(&self, interrupt: bool) {
        *self.interrupt_chat.lock().unwrap() = interrupt;
    }

    fn set_video_status(&self, status: &str) {
        *self.video_status.lock().unwrap() = json!({
            "task":{"id":"video-upstream-1","status":status}
        });
    }

    fn set_receipt_gate(&self, gate: Arc<TestGate>) {
        *self.receipt_gate.lock().unwrap() = Some(gate);
    }

    fn set_stream_body_gate(&self, gate: Arc<TestGate>) {
        *self.stream_body_gate.lock().unwrap() = Some(gate);
    }

    fn requests_to(&self, path: &str) -> Vec<CapturedRequestView> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.path == path)
            .map(|request| CapturedRequestView {
                method: request.method.clone(),
                headers: request.headers.clone(),
                body: request.body.clone(),
            })
            .collect()
    }

    fn count_prefix(&self, prefix: &str) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.path.starts_with(prefix))
            .count()
    }
}

#[derive(Clone)]
struct CapturedRequestView {
    method: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

impl BridgeTransport for QuoteUnavailableBridge {
    fn send(
        &self,
        method: &str,
        url: &str,
        headers: &BTreeMap<String, String>,
        body: &[u8],
    ) -> Result<BridgeResponse, String> {
        let path = url.trim_start_matches("http://bridge").to_string();
        self.requests.lock().unwrap().push(CapturedRequest {
            method: method.into(),
            path: path.clone(),
            headers: headers.clone(),
            body: body.to_vec(),
        });
        let request_body = serde_json::from_slice::<Value>(body).unwrap_or(Value::Null);
        let response = match (method, path.as_str()) {
            ("GET", "/internal/bridge/summary") => BridgeResponse {
                status: 200,
                headers: BTreeMap::from([("content-type".into(), "application/json".into())]),
                body: serde_json::to_vec(&json!({
                    "upstream_credits": {
                        "general":"90.000000",
                        "work":"10.000000",
                        "video_available":"100.000000",
                        "value":"100.000000",
                        "source":"aiwork-upstream-aggregate",
                        "fresh":true,
                        "updated_at":Utc::now().timestamp_millis()
                    }
                })).unwrap(),
            },
            ("POST", "/internal/bridge/quotes") => {
                let request_id = request_body["request_id"].as_str().unwrap_or_default();
                if *self.quote_available.lock().unwrap() {
                    BridgeResponse {
                        status: 200,
                        headers: BTreeMap::from([("content-type".into(), "application/json".into())]),
                        body: serde_json::to_vec(&json!({
                            "request_id":request_id,
                            "status":"quoted",
                            "quote_id":format!("quote-{request_id}"),
                            "request_fingerprint":request_body["request_fingerprint"],
                            "endpoint":request_body["endpoint"],
                            "model":request_body["model"],
                            "max_credits":"2.000000",
                            "unit":"credits",
                            "expires_at_ms":Utc::now().timestamp_millis()+60_000,
                            "source_ref":"upstream-quote-7"
                        })).unwrap(),
                    }
                } else {
                    BridgeResponse {
                        status: 503,
                        headers: BTreeMap::from([("content-type".into(), "application/json".into())]),
                        body: serde_json::to_vec(&json!({
                            "request_id":request_id,
                            "status":"unavailable",
                            "unit":"credits",
                            "error_code":"quote_unavailable",
                            "message":"No verified per-request credit ceiling."
                        })).unwrap(),
                    }
                }
            }
            ("POST", "/v1/chat/completions") if *self.interrupt_chat.lock().unwrap() => {
                return Err("synthetic upstream connection interruption".into());
            }
            ("POST", "/v1/chat/completions") => {
                let streaming = request_body.get("stream").and_then(Value::as_bool).unwrap_or(false);
                BridgeResponse {
                    status: 200,
                    headers: BTreeMap::from([
                        ("content-type".into(), if streaming { "text/event-stream" } else { "application/json" }.into()),
                        ("x-core-request-id".into(), "must-not-leak".into()),
                        ("x-core-billing-receipt".into(), "must-not-leak".into()),
                    ]),
                    body: if streaming {
                        b"data: {\"id\":\"chatcmpl-upstream\",\"choices\":[{\"delta\":{\"content\":\"generated\"}}]}\n\ndata: [DONE]\n\n".to_vec()
                    } else {
                        br#"{"id":"chatcmpl-upstream","choices":[{"message":{"content":"generated"}}]}"#.to_vec()
                    },
                }
            }
            ("POST", "/v1/videos/generations") => BridgeResponse {
                status: 202,
                headers: BTreeMap::from([("content-type".into(), "application/json".into())]),
                body: br#"{"task":{"id":"video-upstream-1","status":"queued"}}"#.to_vec(),
            },
            ("GET", "/v1/videos/video-upstream-1") => BridgeResponse {
                status: 200,
                headers: BTreeMap::from([("content-type".into(), "application/json".into())]),
                body: serde_json::to_vec(&*self.video_status.lock().unwrap()).unwrap(),
            },
            ("GET", path) if path.starts_with("/internal/bridge/requests/") && path.ends_with("/billing") => {
                if let Some(gate) = self.receipt_gate.lock().unwrap().clone() {
                    gate.wait();
                }
                let request_id = path
                    .trim_start_matches("/internal/bridge/requests/")
                    .trim_end_matches("/billing");
                let mut receipt = self.receipt.lock().unwrap().clone();
                receipt["request_id"] = json!(request_id);
                BridgeResponse {
                    status: 200,
                    headers: BTreeMap::from([("content-type".into(), "application/json".into())]),
                    body: serde_json::to_vec(&receipt).unwrap(),
                }
            }
            _ => BridgeResponse {
                status: 404,
                headers: BTreeMap::from([("content-type".into(), "application/json".into())]),
                body: br#"{"error":{"code":"not_found"}}"#.to_vec(),
            },
        };
        Ok(response)
    }

    fn send_stream(
        &self,
        method: &str,
        url: &str,
        headers: &BTreeMap<String, String>,
        body: &[u8],
    ) -> Result<BridgeStreamingResponse, String> {
        let response = self.send(method, url, headers, body)?;
        let stream_body: Box<dyn Read + Send> = if response.headers.get("content-type").is_some_and(|value| value == "text/event-stream") {
            if let Some(gate) = self.stream_body_gate.lock().unwrap().clone() {
                Box::new(BlockingAfterFirstReader { cursor: Cursor::new(response.body), gate, waited: false })
            } else {
                Box::new(Cursor::new(response.body))
            }
        } else {
            Box::new(Cursor::new(response.body))
        };
        Ok(BridgeStreamingResponse { status: response.status, headers: response.headers, body: stream_body })
    }
}

struct TestGate {
    released: Mutex<bool>,
    changed: Condvar,
}

impl TestGate {
    fn new() -> Self {
        Self { released: Mutex::new(false), changed: Condvar::new() }
    }

    fn wait(&self) {
        let mut released = self.released.lock().unwrap();
        while !*released {
            released = self.changed.wait(released).unwrap();
        }
    }

    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.changed.notify_all();
    }
}

struct BlockingAfterFirstReader {
    cursor: Cursor<Vec<u8>>,
    gate: Arc<TestGate>,
    waited: bool,
}

impl Read for BlockingAfterFirstReader {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        let count = self.cursor.read(output)?;
        if count == 0 && !self.waited {
            self.waited = true;
            self.gate.wait();
        }
        Ok(count)
    }
}

struct Fixture {
    app: Router,
    store: Arc<aiwork_core::CoreStore>,
    key: String,
    key_id: String,
    second_key: String,
    second_key_id: String,
    bridge: Arc<QuoteUnavailableBridge>,
    dir: TestDir,
}

struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new(prefix: &str) -> Self {
        Self {
            path: std::env::temp_dir().join(format!(
                "starlink-per-key-real-settlement-{prefix}-{}",
                rand::random::<u64>()
            )),
        }
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn fixture() -> Fixture {
    let dir = TestDir::new("fixture");
    let store = Arc::new(aiwork_core::CoreStore::open(dir.path()).unwrap());
    store.migrate().unwrap();
    store
        .create_bootstrap_admin(
            aiwork_core::NewUser {
                id: "admin".into(),
                name: "管理员".into(),
                role: aiwork_core::UserRole::Admin,
            },
            "bootstrap",
        )
        .unwrap();
    let admin = aiwork_core::Principal {
        user_id: "admin".into(),
        key_id: "admin_session:billing-test".into(),
        scopes: BTreeSet::from(["admin:*".into()]),
    };
    let issued = store
        .issue_api_key_for_new_user_as_admin(
            &admin,
            "周",
            BTreeSet::from(["chat:invoke".into(), "videos:submit".into()]),
            2,
        )
        .unwrap();
    let second = store
        .issue_api_key_for_new_user_as_admin(
            &admin,
            "测试用户二",
            BTreeSet::from(["chat:invoke".into(), "videos:submit".into()]),
            2,
        )
        .unwrap();
    store
        .quota_pool_grant_as_admin(
            &admin,
            aiwork_core::QuotaGrant {
                user_id: issued.user_id.clone(),
                resource_kind: "credits".into(),
                amount: 10_000_000,
                actor_user_id: "admin".into(),
                reason: "per-key settlement integration test".into(),
            },
        )
        .unwrap();
    store
        .quota_pool_grant_as_admin(
            &admin,
            aiwork_core::QuotaGrant {
                user_id: second.user_id.clone(),
                resource_kind: "credits".into(),
                amount: 10_000_000,
                actor_user_id: "admin".into(),
                reason: "per-key settlement integration test".into(),
            },
        )
        .unwrap();
    store
        .key_quota_allocate_from_pool_as_admin(
            &admin,
            aiwork_core::KeyQuotaGrant {
                api_key_id: issued.id.clone(),
                resource_kind: "credits".into(),
                amount: 10_000_000,
                actor_user_id: "admin".into(),
                reason: "per-key settlement integration test".into(),
            },
        )
        .unwrap();
    store
        .key_quota_allocate_from_pool_as_admin(
            &admin,
            aiwork_core::KeyQuotaGrant {
                api_key_id: second.id.clone(),
                resource_kind: "credits".into(),
                amount: 10_000_000,
                actor_user_id: "admin".into(),
                reason: "per-key settlement integration test".into(),
            },
        )
        .unwrap();
    // Keep the legacy route runnable in the RED test. The implementation
    // must stop using this estimate and require an upstream quote instead.
    store
        .upsert_cost_policy(aiwork_core::CostPolicy {
            id: "test-chat-fallback".into(),
            endpoint: "chat".into(),
            model_pattern: "test-chat".into(),
            resource_kind: "credits".into(),
            reserve_amount: 1,
            max_actual_amount: Some(1),
            version: 1,
            enabled: true,
        })
        .unwrap();
    store
        .upsert_cost_policy(aiwork_core::CostPolicy {
            id: "test-video-fallback".into(),
            endpoint: "videos".into(),
            model_pattern: "seedance".into(),
            resource_kind: "credits".into(),
            reserve_amount: 1,
            max_actual_amount: Some(1),
            version: 1,
            enabled: true,
        })
        .unwrap();
    store
        .set_video_billing_control(aiwork_core::VideoBillingControlInput {
            mode: aiwork_core::VideoBillingMode::Active,
            reason: "测试开启".into(),
            diagnostic_key_id: None,
            diagnostic_request_hash: None,
        })
        .unwrap();
    let bridge = QuoteUnavailableBridge::new();
    let client = BridgeClient::from_transport("http://bridge", "bridge-secret", bridge.clone());
    let state = StarlinkRouterState::for_test(
        store.clone(),
        client,
        RouterConfig::defaults(dir.path().to_path_buf()),
    );
    let app = build_router(state);
    Fixture {
        app,
        store,
        key: issued.plaintext,
        key_id: issued.id,
        second_key: second.plaintext,
        second_key_id: second.id,
        bridge,
        dir,
    }
}

#[tokio::test]
async fn unavailable_quote_fails_unreserved_chat_request_without_upstream_charge() {
    let fixture = fixture();
    let client_request_id = "client-forged-request-id";
    let idempotency_key = "quote-unavailable-chat";
    let response = fixture
        .app
        .clone()
        .oneshot(
            Request::post("/v1/chat/completions")
                .header("authorization", format!("Bearer {}", fixture.key))
                .header("content-type", "application/json")
                .header("idempotency-key", idempotency_key)
                .header("x-core-request-id", client_request_id)
                .body(Body::from(
                    r#"{"model":"test-chat","messages":[{"role":"user","content":"private prompt"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let response_body: Value = serde_json::from_slice(
        &to_bytes(response.into_body(), 1024 * 1024).await.unwrap(),
    )
    .unwrap();

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response_body["error"]["code"], "quote_unavailable");
    assert_eq!(fixture.bridge.requests_to("/internal/bridge/summary").len(), 1);
    assert_eq!(fixture.bridge.requests_to("/internal/bridge/quotes").len(), 1);
    assert_eq!(fixture.bridge.requests_to("/v1/chat/completions").len(), 0);

    let quote = fixture
        .bridge
        .requests_to("/internal/bridge/quotes")
        .pop()
        .expect("Core must ask AI Work for a quote before dispatch");
    assert_eq!(quote.method, "POST");
    assert_eq!(quote.headers["authorization"], "Bearer bridge-secret");
    assert!(!quote.headers.contains_key("x-api-key"));
    assert!(!quote.headers.contains_key("x-user-id"));
    let quote_body: Value = serde_json::from_slice(&quote.body).unwrap();
    let request_id = quote_body["request_id"].as_str().unwrap();
    assert_ne!(request_id, client_request_id);
    assert_eq!(quote.headers["x-core-request-id"], request_id);
    assert_eq!(quote_body["endpoint"], "chat");
    assert_eq!(quote_body["model"], "test-chat");
    assert!(!quote_body.to_string().contains("private prompt"));
    assert!(!quote_body.to_string().contains(&fixture.key));

    let request = match fixture
        .store
        .lookup_idempotent_request(
            fixture
                .store
                .authenticate_api_key(&fixture.key)
                .unwrap()
                .user_id
                .as_str(),
            &fixture.key_id,
            "chat",
            "test-chat",
            &json!({"model":"test-chat","messages":[{"role":"user","content":"private prompt"}]}),
            idempotency_key,
        )
        .unwrap()
        .unwrap()
    {
        aiwork_core::BeginRequest::Existing(request) => request,
        other => panic!("expected persisted Core request, got {other:?}"),
    };
    assert_eq!(request.id, request_id);
    assert_eq!(fixture.store.request_state(request_id).unwrap(), aiwork_core::RequestState::Failed);
    assert!(fixture.store.reservation_for_request(request_id).unwrap().is_none());
    let principal = fixture.store.authenticate_api_key(&fixture.key).unwrap();
    let quota = fixture
        .store
        .key_quota_usage_for_principal(&principal, 20)
        .unwrap();
    assert_eq!(quota.balances[0].held, 0);
    assert_eq!(quota.balances[0].settled, 0);
}

async fn post_chat(fixture: &Fixture, key: &str, idempotency_key: &str, stream: bool) -> axum::response::Response<Body> {
    let body = json!({
        "model":"test-chat",
        "stream":stream,
        "messages":[{"role":"user","content":"private prompt"}]
    });
    fixture.app.clone().oneshot(
        Request::post("/v1/chat/completions")
            .header("authorization", format!("Bearer {key}"))
            .header("content-type", "application/json")
            .header("idempotency-key", idempotency_key)
            .body(Body::from(body.to_string()))
            .unwrap(),
    ).await.unwrap()
}

async fn post_video(fixture: &Fixture, key: &str, idempotency_key: &str) -> axum::response::Response<Body> {
    fixture.app.clone().oneshot(
        Request::post("/v1/videos/generations")
            .header("authorization", format!("Bearer {key}"))
            .header("content-type", "application/json")
            .header("idempotency-key", idempotency_key)
            .body(Body::from(r#"{"model":"seedance","prompt":"private video prompt"}"#))
            .unwrap(),
    ).await.unwrap()
}

async fn poll_video(fixture: &Fixture, key: &str) -> axum::response::Response<Body> {
    fixture.app.clone().oneshot(
        Request::get("/v1/videos/video-upstream-1")
            .header("authorization", format!("Bearer {key}"))
            .body(Body::empty())
            .unwrap(),
    ).await.unwrap()
}

fn quota_for(fixture: &Fixture, key: &str) -> aiwork_core::CoreQuotaUsageView {
    let principal = fixture.store.authenticate_api_key(key).unwrap();
    fixture.store.key_quota_usage_for_principal(&principal, 20).unwrap()
}

#[tokio::test]
async fn final_chat_receipt_settles_exact_credits_only_for_the_request_key() {
    let fixture = fixture();
    fixture.bridge.set_quote_available(true);
    fixture.bridge.set_receipt("final", Some("1.250000"));
    let response = post_chat(&fixture, &fixture.key, "exact-chat-charge", false).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get("x-core-request-id").is_none());
    assert!(response.headers().get("x-core-billing-receipt").is_none());
    let value: Value = serde_json::from_slice(
        &to_bytes(response.into_body(), 1024 * 1024).await.unwrap(),
    ).unwrap();
    assert_eq!(value["id"], "chatcmpl-upstream");

    let first = quota_for(&fixture, &fixture.key);
    let second = quota_for(&fixture, &fixture.second_key);
    assert_eq!(first.balances[0].settled, 1_250_000);
    assert_eq!(first.balances[0].held, 0);
    assert_eq!(second.balances[0].settled, 0);
    assert_eq!(fixture.bridge.requests_to("/v1/chat/completions").len(), 1);
    assert_eq!(fixture.bridge.requests_to("/internal/bridge/quotes").len(), 1);
    assert_eq!(fixture.bridge.count_prefix("/internal/bridge/requests/") , 1);

    let quote = fixture.bridge.requests_to("/internal/bridge/quotes").pop().unwrap();
    let quote_body = serde_json::from_slice::<Value>(&quote.body).unwrap();
    let request_id = quote_body["request_id"].as_str().unwrap().to_string();
    let generation = fixture.bridge.requests_to("/v1/chat/completions").pop().unwrap();
    assert_eq!(generation.headers["x-core-request-id"], request_id);
    assert_eq!(generation.headers["x-core-key-id"], fixture.key_id);
    assert_eq!(generation.headers["x-core-quote-id"], format!("quote-{request_id}"));
    let receipt_query = fixture.bridge.requests_to(&format!("/internal/bridge/requests/{request_id}/billing"));
    assert_eq!(receipt_query.len(), 1);
    assert_eq!(receipt_query[0].headers["x-core-request-id"], request_id);
    assert_eq!(receipt_query[0].headers["authorization"], "Bearer bridge-secret");
    assert!(!receipt_query[0].headers.contains_key("x-api-key"));
    assert_ne!(request_id, "exact-chat-charge");
    assert_ne!(request_id, fixture.second_key_id);
}

#[tokio::test]
async fn successful_streaming_chat_waits_for_the_exact_final_receipt() {
    let fixture = fixture();
    fixture.bridge.set_quote_available(true);
    fixture.bridge.set_receipt("final", Some("0.375000"));
    let response = post_chat(&fixture, &fixture.key, "successful-stream", true).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert!(String::from_utf8_lossy(&body).contains("data: [DONE]"));
    assert_eq!(quota_for(&fixture, &fixture.key).balances[0].settled, 375_000);
    assert_eq!(quota_for(&fixture, &fixture.key).balances[0].held, 0);
    assert_eq!(fixture.bridge.requests_to("/v1/chat/completions").len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streaming_chat_returns_before_the_final_receipt_query_completes() {
    let fixture = fixture();
    fixture.bridge.set_quote_available(true);
    fixture.bridge.set_receipt("final", Some("0.375000"));
    let gate = Arc::new(TestGate::new());
    fixture.bridge.set_receipt_gate(gate.clone());

    let app = fixture.app.clone();
    let key = fixture.key.clone();
    let mut request = tokio::spawn(async move {
        app.oneshot(
            Request::post("/v1/chat/completions")
                .header("authorization", format!("Bearer {key}"))
                .header("content-type", "application/json")
                .header("idempotency-key", "early-stream-response")
                .body(Body::from(json!({
                    "model":"test-chat",
                    "stream":true,
                    "messages":[{"role":"user","content":"private prompt"}]
                }).to_string()))
                .unwrap(),
        ).await.unwrap()
    });
    let early_response = tokio::time::timeout(Duration::from_millis(250), &mut request).await;
    gate.release();
    let response = match early_response {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => panic!("stream handler task failed: {error}"),
        Err(_) => {
            let _ = request.await;
            panic!("stream handler waited for the final billing receipt before returning response headers");
        }
    };

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert!(String::from_utf8_lossy(&body).contains("data: [DONE]"));
    assert_eq!(quota_for(&fixture, &fixture.key).balances[0].settled, 375_000);
    assert_eq!(quota_for(&fixture, &fixture.key).balances[0].held, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streaming_chat_delivers_sse_chunks_before_upstream_eof() {
    let fixture = fixture();
    fixture.bridge.set_quote_available(true);
    fixture.bridge.set_receipt("final", Some("0.375000"));
    let gate = Arc::new(TestGate::new());
    fixture.bridge.set_stream_body_gate(gate.clone());

    let response = post_chat(&fixture, &fixture.key, "incremental-stream", true).await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();
    let first = tokio::time::timeout(Duration::from_millis(250), body.frame()).await
        .expect("first SSE frame must arrive before upstream EOF")
        .expect("SSE body ended before first event")
        .expect("SSE body yielded an error")
        .into_data().expect("first SSE frame should contain data");
    assert!(String::from_utf8_lossy(&first).contains("generated"));
    assert!(!String::from_utf8_lossy(&first).contains("[DONE]"));

    gate.release();
    let mut collected = first.to_vec();
    while let Some(frame) = body.frame().await {
        collected.extend_from_slice(&frame.unwrap().into_data().unwrap());
    }
    assert!(String::from_utf8_lossy(&collected).contains("data: [DONE]"));
    assert_eq!(quota_for(&fixture, &fixture.key).balances[0].settled, 375_000);
    assert_eq!(quota_for(&fixture, &fixture.key).balances[0].held, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unresolved_stream_receipt_withholds_done_and_emits_reconcile_event() {
    let fixture = fixture();
    fixture.bridge.set_quote_available(true);
    let response = post_chat(&fixture, &fixture.key, "unresolved-stream", true).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("generated"));
    assert!(body.contains("reconcile_required"));
    assert!(!body.contains("data: [DONE]"));
    assert_eq!(quota_for(&fixture, &fixture.key).balances[0].settled, 0);
    assert_eq!(quota_for(&fixture, &fixture.key).balances[0].held, 2_000_000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disconnected_stream_is_drained_and_settled_without_resubmitting() {
    let fixture = fixture();
    fixture.bridge.set_quote_available(true);
    fixture.bridge.set_receipt("final", Some("0.375000"));
    let gate = Arc::new(TestGate::new());
    fixture.bridge.set_stream_body_gate(gate.clone());

    let response = post_chat(&fixture, &fixture.key, "disconnected-stream", true).await;
    assert_eq!(response.status(), StatusCode::OK);
    drop(response);
    gate.release();

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if quota_for(&fixture, &fixture.key).balances[0].settled == 375_000 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("stream disconnect must not abandon receipt reconciliation");
    assert_eq!(quota_for(&fixture, &fixture.key).balances[0].held, 0);
    assert_eq!(fixture.bridge.requests_to("/v1/chat/completions").len(), 1);
}

#[tokio::test]
async fn interrupted_stream_stays_held_then_reconciles_without_resubmitting_generation() {
    let fixture = fixture();
    fixture.bridge.set_quote_available(true);
    fixture.bridge.interrupt_chat(true);
    let first = post_chat(&fixture, &fixture.key, "interrupted-stream", true).await;
    assert_eq!(first.status(), StatusCode::BAD_GATEWAY);
    let first_body: Value = serde_json::from_slice(
        &to_bytes(first.into_body(), 1024 * 1024).await.unwrap(),
    ).unwrap();
    assert_eq!(first_body["error"]["code"], "reconcile_required");
    assert_eq!(fixture.bridge.requests_to("/v1/chat/completions").len(), 1);
    assert_eq!(quota_for(&fixture, &fixture.key).balances[0].held, 2_000_000);
    assert_eq!(quota_for(&fixture, &fixture.key).balances[0].settled, 0);

    fixture.bridge.set_receipt("final", Some("1.250000"));
    let retry = post_chat(&fixture, &fixture.key, "interrupted-stream", true).await;
    assert_eq!(retry.status(), StatusCode::CONFLICT);
    assert_eq!(fixture.bridge.requests_to("/v1/chat/completions").len(), 1);
    assert_eq!(fixture.bridge.count_prefix("/internal/bridge/requests/"), 2);
    assert_eq!(quota_for(&fixture, &fixture.key).balances[0].settled, 1_250_000);
    assert_eq!(quota_for(&fixture, &fixture.key).balances[0].held, 0);
}

#[tokio::test]
async fn reopened_core_scans_unknown_chat_by_core_request_id_without_resubmitting() {
    let fixture = fixture();
    fixture.bridge.set_quote_available(true);
    fixture.bridge.interrupt_chat(true);
    let first = post_chat(&fixture, &fixture.key, "restart-scan", true).await;
    assert_eq!(first.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(fixture.bridge.requests_to("/v1/chat/completions").len(), 1);
    assert_eq!(quota_for(&fixture, &fixture.key).balances[0].held, 2_000_000);

    fixture.bridge.set_receipt("final", Some("1.250000"));
    let reopened_store = Arc::new(aiwork_core::CoreStore::open(fixture.dir.path()).unwrap());
    reopened_store.migrate().unwrap();
    let reopened_bridge = BridgeClient::from_transport(
        "http://bridge",
        "bridge-secret",
        fixture.bridge.clone(),
    );
    let reopened_state = StarlinkRouterState::for_test(
        reopened_store.clone(),
        reopened_bridge,
        RouterConfig::defaults(fixture.dir.path().to_path_buf()),
    );
    assert_eq!(
        starlink_dimension_router::user_routes::reconcile_pending_billing_requests_once(&reopened_state),
        1
    );
    assert_eq!(quota_for(&fixture, &fixture.key).balances[0].settled, 1_250_000);
    assert_eq!(quota_for(&fixture, &fixture.key).balances[0].held, 0);
    assert_eq!(fixture.bridge.requests_to("/v1/chat/completions").len(), 1);

    assert_eq!(
        starlink_dimension_router::user_routes::reconcile_pending_billing_requests_once(&reopened_state),
        0
    );
    assert_eq!(fixture.bridge.requests_to("/v1/chat/completions").len(), 1);
}

#[tokio::test]
async fn completed_video_queries_receipt_once_and_repeated_poll_does_not_double_settle() {
    let fixture = fixture();
    fixture.bridge.set_quote_available(true);
    fixture.bridge.set_receipt("final", Some("1.250000"));
    fixture.bridge.set_video_status("completed");
    let submitted = post_video(&fixture, &fixture.key, "video-exact-charge").await;
    assert_eq!(submitted.status(), StatusCode::ACCEPTED);

    let completed = poll_video(&fixture, &fixture.key).await;
    assert_eq!(completed.status(), StatusCode::OK);
    assert_eq!(quota_for(&fixture, &fixture.key).balances[0].settled, 1_250_000);
    assert_eq!(quota_for(&fixture, &fixture.key).balances[0].held, 0);

    let repeated = poll_video(&fixture, &fixture.key).await;
    assert_eq!(repeated.status(), StatusCode::OK);
    assert_eq!(quota_for(&fixture, &fixture.key).balances[0].settled, 1_250_000);
    let submitted_request = fixture.bridge.requests_to("/v1/videos/generations").pop().unwrap();
    assert_eq!(submitted_request.headers["x-core-key-id"], fixture.key_id);
    assert_eq!(fixture.bridge.count_prefix("/internal/bridge/requests/"), 1);
    assert_eq!(quota_for(&fixture, &fixture.second_key).balances[0].settled, 0);
}
