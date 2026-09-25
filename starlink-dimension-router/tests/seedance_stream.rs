#[path = "../src/seedance_sse.rs"]
mod seedance_sse;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Router,
};
use seedance_sse::{encode_event, keep_alive, VideoStreamEvent};
use starlink_dimension_router::{
    bridge_client::{BridgeClient, BridgeResponse, BridgeTransport},
    config::RouterConfig,
    server::build_router,
    state::StarlinkRouterState,
};
use tower::util::ServiceExt;

fn payloads(frames: &[u8]) -> Vec<serde_json::Value> {
    String::from_utf8(frames.to_vec())
        .unwrap()
        .split("\n\n")
        .filter_map(|frame| frame.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .map(|data| serde_json::from_str(data).unwrap())
        .collect()
}

#[test]
fn progress_is_a_chat_completion_chunk_with_visible_text() {
    let frames = encode_event(
        "req-123",
        VideoStreamEvent::Progress("视频已提交，正在生成".into()),
    );
    let text = String::from_utf8(frames.clone()).unwrap();
    assert_eq!(payloads(&frames)[0]["object"], "chat.completion.chunk");
    assert_eq!(payloads(&frames)[0]["choices"][0]["delta"]["content"], "视频已提交，正在生成");
    assert!(!text.contains("[DONE]"));
}

#[test]
fn completion_contains_the_content_url_and_one_terminal_marker() {
    let url = "https://api.gemstory.cn/v1/videos/video-123/content";
    let frames = encode_event(
        "req-123",
        VideoStreamEvent::Completed {
            task_id: "video-123".into(),
            content_url: url.into(),
            request_id: "req-123".into(),
        },
    );
    let text = String::from_utf8(frames).unwrap();
    let values = payloads(text.as_bytes());
    assert_eq!(values.len(), 1);
    assert_eq!(values[0]["object"], "chat.completion.chunk");
    assert_eq!(values[0]["video_task"]["content_url"], url);
    assert!(values[0]["choices"][0]["delta"]["content"]
        .as_str()
        .unwrap()
        .contains(url));
    assert_eq!(text.matches("data: [DONE]\n\n").count(), 1);
    assert!(!text.contains("bridge-secret"));
}

#[test]
fn failure_is_not_reported_as_success_and_ends_once() {
    let frames = encode_event(
        "req-456",
        VideoStreamEvent::Failed {
            code: "billing_reconcile_required".into(),
            request_id: "req-456".into(),
        },
    );
    let text = String::from_utf8(frames).unwrap();
    let values = payloads(text.as_bytes());
    assert_eq!(values.len(), 1);
    assert_eq!(values[0]["error"]["code"], "billing_reconcile_required");
    assert!(!values[0]["choices"][0]["delta"]["content"]
        .as_str()
        .unwrap()
        .contains("生成成功"));
    assert_eq!(text.matches("data: [DONE]\n\n").count(), 1);
}

#[test]
fn keep_alive_is_a_comment_frame() {
    assert_eq!(keep_alive(), b": keep-alive\n\n");
}

struct StreamTestDir(PathBuf);

impl StreamTestDir {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "starlink-seedance-stream-{}",
            rand::random::<u64>()
        )))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for StreamTestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct FakeGateway {
    requests: Mutex<Vec<(String, String, serde_json::Value, BTreeMap<String, String>)>>,
    unavailable_quote_endpoints: Mutex<BTreeSet<String>>,
    task_status: Mutex<serde_json::Value>,
    video_receipt_status: Mutex<String>,
    assistant_receipt_status: Mutex<String>,
    assistant_finalization_status: Mutex<String>,
    video_preflight_rejection: Mutex<Option<&'static str>>,
    video_no_charge_proof: Mutex<bool>,
    content_status: Mutex<u16>,
}

impl FakeGateway {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
            unavailable_quote_endpoints: Mutex::new(BTreeSet::new()),
            task_status: Mutex::new(serde_json::json!({
                "task":{"id":"video-stream-test","status":"completed"}
            })),
            video_receipt_status: Mutex::new("final".into()),
            assistant_receipt_status: Mutex::new("final".into()),
            assistant_finalization_status: Mutex::new("unknown".into()),
            video_preflight_rejection: Mutex::new(None),
            video_no_charge_proof: Mutex::new(false),
            content_status: Mutex::new(200),
        })
    }

    fn set_quote_unavailable(&self, endpoint: &str) {
        self.unavailable_quote_endpoints
            .lock()
            .unwrap()
            .insert(endpoint.to_string());
    }

    fn request_count(&self, method: &str, path: &str) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(seen_method, seen_path, body, _)| {
                seen_method == method
                    && seen_path == path
                    && (method != "POST" || path != "/v1/chat/completions" || body["model"] == "seedance")
            })
            .count()
    }

    fn model_request_count(&self, path: &str, model: &str) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(method, seen_path, body, _)| {
                method == "POST" && seen_path == path && body["model"] == model
            })
            .count()
    }

    fn submitted_body(&self) -> serde_json::Value {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .find(|(method, path, body, _)| method == "POST" && path == "/v1/chat/completions" && body["model"] == "seedance")
            .map(|(_, _, body, _)| body.clone())
            .unwrap_or(serde_json::Value::Null)
    }

    fn assistant_body(&self) -> serde_json::Value {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .find(|(method, path, body, _)| method == "POST" && path == "/v1/chat/completions" && body["model"] == "deepseek-v4-flash")
            .map(|(_, _, body, _)| body.clone())
            .unwrap_or(serde_json::Value::Null)
    }

    fn core_request_id_for_model(&self, model: &str) -> Option<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .find(|(method, path, body, _)| method == "POST" && path == "/v1/chat/completions" && body["model"] == model)
            .and_then(|(_, _, _, headers)| headers.get("x-core-request-id").cloned())
    }
}

impl BridgeTransport for FakeGateway {
    fn send(
        &self,
        method: &str,
        url: &str,
        headers: &BTreeMap<String, String>,
        body: &[u8],
    ) -> Result<BridgeResponse, String> {
        let path = url.trim_start_matches("http://bridge").to_owned();
        let request_body = serde_json::from_slice(body).unwrap_or(serde_json::Value::Null);
        self.requests
            .lock()
            .unwrap()
            .push((method.to_string(), path.clone(), request_body.clone(), headers.clone()));

        let response = match (method, path.as_str()) {
            ("GET", "/internal/bridge/summary") => BridgeResponse {
                status: 200,
                headers: BTreeMap::new(),
                body: serde_json::to_vec(&serde_json::json!({
                    "upstream_credits": {
                        "general":"900.000000",
                        "work":"100.000000",
                        "video_available":"1000.000000",
                        "value":"1000.000000",
                        "source":"aiwork-upstream-aggregate",
                        "fresh":true,
                        "updated_at":chrono::Utc::now().timestamp_millis()
                    }
                }))
                .unwrap(),
            },
            ("POST", "/internal/bridge/quotes") => {
                let request_id = request_body["request_id"].as_str().unwrap_or_default();
                let endpoint = request_body["endpoint"].as_str().unwrap_or_default();
                if self
                    .unavailable_quote_endpoints
                    .lock()
                    .unwrap()
                    .contains(endpoint)
                {
                    BridgeResponse {
                        status: 503,
                        headers: BTreeMap::new(),
                        body: serde_json::to_vec(&serde_json::json!({
                            "request_id":request_id,
                            "status":"unavailable",
                            "error_code":"quote_unavailable"
                        }))
                        .unwrap(),
                    }
                } else {
                    BridgeResponse {
                        status: 200,
                        headers: BTreeMap::new(),
                        body: serde_json::to_vec(&serde_json::json!({
                            "request_id":request_id,
                            "status":"quoted",
                            "quote_id":format!("quote-{request_id}"),
                            "request_fingerprint":request_body["request_fingerprint"],
                            "endpoint":request_body["endpoint"],
                            "model":request_body["model"],
                            "max_credits":"20.000000",
                            "unit":"credits",
                            "expires_at_ms":chrono::Utc::now().timestamp_millis()+60_000,
                            "source_ref":"seedance-stream-test-quote"
                        }))
                        .unwrap(),
                    }
                }
            }
            ("POST", path) if path.starts_with("/internal/bridge/requests/") && path.ends_with("/billing/finalize-chat") => {
                let request_id = path.trim_start_matches("/internal/bridge/requests/").trim_end_matches("/billing/finalize-chat");
                let status = self.assistant_finalization_status.lock().unwrap().clone();
                BridgeResponse {
                    status: 200,
                    headers: BTreeMap::new(),
                    body: serde_json::to_vec(&serde_json::json!({
                        "request_id":request_id,
                        "status":status,
                        "actual_credits":if status == "final" { serde_json::json!("0.050400") } else { serde_json::Value::Null },
                        "unit":if status == "final" { serde_json::json!("credits") } else { serde_json::Value::Null },
                        "source_ref":if status == "final" { serde_json::json!("trae-usage-session:assistant-session") } else { serde_json::Value::Null },
                        "task_ref":null,
                        "observed_at_ms":chrono::Utc::now().timestamp_millis()
                    })).unwrap(),
                }
            }
            ("GET", path) if path.starts_with("/internal/bridge/requests/") && path.ends_with("/video-task") => {
                let request_id = path.trim_start_matches("/internal/bridge/requests/").trim_end_matches("/video-task");
                let submitted = self.requests.lock().unwrap().iter().any(|(method, route, _, seen_headers)| {
                    method == "POST" && route == "/v1/chat/completions"
                        && seen_headers.get("x-core-request-id").is_some_and(|value| value == request_id)
                });
                let submitted = submitted && self.video_preflight_rejection.lock().unwrap().is_none();
                BridgeResponse { status: 200, headers: BTreeMap::new(),
                    body: serde_json::to_vec(&serde_json::json!({"request_id":request_id,
                        "task":if submitted { serde_json::json!({"id":"video-stream-test","status":"queued"}) } else { serde_json::Value::Null }})).unwrap() }
            }
            (method, path)
                if path.starts_with("/internal/bridge/requests/")
                    && ((method == "GET" && path.ends_with("/billing"))
                        || (method == "POST" && path.ends_with("/billing/finalize"))) =>
            {
                let request_id = path
                    .trim_start_matches("/internal/bridge/requests/")
                    .trim_end_matches(if method == "POST" {
                        "/billing/finalize"
                    } else {
                        "/billing"
                    });
                let model = self.requests.lock().unwrap().iter()
                    .find(|(_, _, _, seen_headers)| seen_headers.get("x-core-request-id").is_some_and(|value| value == request_id))
                    .map(|(_, _, body, _)| body["model"].as_str().unwrap_or_default().to_string())
                    .unwrap_or_default();
                let status = if model == "seedance" {
                    self.video_receipt_status.lock().unwrap().clone()
                } else {
                    self.assistant_receipt_status.lock().unwrap().clone()
                };
                let no_charge = model == "seedance" && *self.video_no_charge_proof.lock().unwrap();
                BridgeResponse {
                    status: 200,
                    headers: BTreeMap::new(),
                    body: serde_json::to_vec(&serde_json::json!({
                        "request_id":request_id,
                        "status":if no_charge { "final" } else { status.as_str() },
                        "actual_credits":if no_charge { "0.000000" } else { "2.500000" },
                        "unit":"credits",
                        "source_ref":if no_charge { format!("aiwork-pre-dispatch-no-charge:{request_id}") } else { "trae-usage-session:test-account:test-session".into() },
                        "task_ref":if no_charge { serde_json::Value::Null } else { serde_json::json!("video-stream-test") },
                        "observed_at_ms":chrono::Utc::now().timestamp_millis()
                    }))
                    .unwrap(),
                }
            }
            ("POST", "/v1/chat/completions") if request_body["model"] == "seedance" => {
                if let Some(code) = *self.video_preflight_rejection.lock().unwrap() {
                    BridgeResponse {
                        status: 400,
                        headers: BTreeMap::new(),
                        body: serde_json::to_vec(&serde_json::json!({"error":{"code":code,"message":"video request rejected before task creation"}})).unwrap(),
                    }
                } else {
                    BridgeResponse {
                        status: 202,
                        headers: BTreeMap::new(),
                        body: br#"{"id":"chatcmpl-video-stream-test","object":"chat.completion","video_task":{"id":"video-stream-test","status":"queued"}}"#.to_vec(),
                    }
                }
            },
            ("POST", "/v1/chat/completions") => BridgeResponse {
                status: 200,
                headers: BTreeMap::from([("content-type".into(), "application/json".into())]),
                body: r#"{"choices":[{"message":{"content":"{\"prompt\":\"整理后的视频提示词\"}"}}]}"#.as_bytes().to_vec(),
            },
            ("GET", "/v1/videos/video-stream-test") => BridgeResponse {
                status: 200,
                headers: BTreeMap::new(),
                body: serde_json::to_vec(&*self.task_status.lock().unwrap()).unwrap(),
            },
            ("HEAD", "/v1/videos/video-stream-test/content") => BridgeResponse {
                status: *self.content_status.lock().unwrap(),
                headers: BTreeMap::from([("content-type".into(), "video/mp4".into())]),
                body: Vec::new(),
            },
            ("GET", "/v1/videos/video-stream-test/content") => BridgeResponse {
                status: *self.content_status.lock().unwrap(),
                headers: BTreeMap::from([("content-type".into(), "video/mp4".into())]),
                body: b"test-video".to_vec(),
            },
            _ => BridgeResponse {
                status: 404,
                headers: BTreeMap::new(),
                body: b"{}".to_vec(),
            },
        };
        Ok(response)
    }
}

struct StreamFixture {
    app: Router,
    state: Arc<StarlinkRouterState>,
    gateway: Arc<FakeGateway>,
    store: Arc<aiwork_core::CoreStore>,
    billing_principal: aiwork_core::Principal,
    key: String,
    second_key: String,
    dir: StreamTestDir,
}

fn stream_fixture() -> StreamFixture {
    let dir = StreamTestDir::new();
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
        key_id: "admin_session:test".into(),
        scopes: BTreeSet::from(["admin:*".into()]),
    };
    let scopes = BTreeSet::from(["videos:submit".into()]);
    let first = store
        .issue_api_key_for_new_user_as_admin(&admin, "视频 Key", scopes.clone(), 2)
        .unwrap();
    let second = store
        .issue_api_key_as_admin(&first.user_id, "另一个视频 Key", scopes, &admin)
        .unwrap();
    let billing_principal = aiwork_core::Principal {
        user_id: first.user_id.clone(),
        key_id: first.id.clone(),
        scopes: BTreeSet::from(["videos:submit".into()]),
    };
    store
        .quota_pool_grant_as_admin(
            &admin,
            aiwork_core::QuotaGrant {
                user_id: first.user_id.clone(),
                resource_kind: "credits".into(),
                amount: 100_000_000,
                actor_user_id: "admin".into(),
                reason: "Seedance stream fixture".into(),
            },
        )
        .unwrap();
    store
        .key_quota_allocate_from_pool_as_admin(
            &admin,
            aiwork_core::KeyQuotaGrant {
                api_key_id: first.id.clone(),
                resource_kind: "credits".into(),
                amount: 100_000_000,
                actor_user_id: "admin".into(),
                reason: "Seedance stream fixture".into(),
            },
        )
        .unwrap();
    store
        .set_video_billing_control(aiwork_core::VideoBillingControlInput {
            mode: aiwork_core::VideoBillingMode::Active,
            reason: "仅本地 Mock 测试".into(),
            diagnostic_key_id: None,
            diagnostic_request_hash: None,
        })
        .unwrap();

    let gateway = FakeGateway::new();
    let bridge = BridgeClient::from_transport("http://bridge", "test-bridge-secret", gateway.clone());
    let mut config = RouterConfig::defaults(dir.path().to_path_buf());
    config.public_base_url = "https://api.gemstory.cn".into();
    let state = StarlinkRouterState::for_test(store, bridge, config);
    let app = build_router(state.clone());
    StreamFixture {
        app,
        state: state.clone(),
        gateway,
        store: state.store.clone(),
        billing_principal,
        key: first.plaintext,
        second_key: second.plaintext,
        dir,
    }
}

fn video_chat_body(stream: bool) -> serde_json::Value {
    serde_json::json!({
        "model":"seedance",
        "stream":stream,
        "messages":[{"role":"user","content":[
            {"type":"text","text":"将参考图制作成视频"},
            {"type":"image_url","image_url":{"url":"data:image/png;base64,iVBORw0KGgo="}}
        ]}]
    })
}

async fn post_chat(
    fixture: &StreamFixture,
    key: &str,
    idempotency_key: Option<&str>,
    body: serde_json::Value,
) -> axum::response::Response {
    let mut request = Request::post("/v1/chat/completions")
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json");
    if let Some(idempotency_key) = idempotency_key {
        request = request.header("idempotency-key", idempotency_key);
    }
    fixture
        .app
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap()
}

fn current_key_concurrency(fixture: &StreamFixture) -> i64 {
    let admin = aiwork_core::Principal {
        user_id: "admin".into(),
        key_id: "admin_session:test".into(),
        scopes: BTreeSet::from(["admin:*".into()]),
    };
    fixture
        .store
        .list_api_keys_as_admin(&admin, None)
        .unwrap()
        .into_iter()
        .find(|key| key.id == fixture.billing_principal.key_id)
        .expect("the test key must be listed")
        .current_concurrency
}

#[tokio::test]
async fn seedance_stream_forwards_nonstream_with_reference_and_waits_for_verified_video() {
    let fixture = stream_fixture();
    let response = post_chat(
        &fixture,
        &fixture.key,
        Some("seedance-stream-success"),
        video_chat_body(true),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/event-stream"));
    assert_eq!(response.headers()["cache-control"], "no-cache, no-transform");
    assert_eq!(response.headers()["x-accel-buffering"], "no");
    let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(text.contains("chat.completion.chunk"));
    assert!(text.contains("https://api.gemstory.cn/v1/videos/video-stream-test/content"));
    assert_eq!(text.matches("data: [DONE]\n\n").count(), 1);
    assert_eq!(fixture.gateway.request_count("POST", "/v1/chat/completions"), 1);
    assert_eq!(fixture.gateway.model_request_count("/v1/chat/completions", "deepseek-v4-flash"), 1);
    let submitted = fixture.gateway.submitted_body();
    let assistant = fixture.gateway.assistant_body();
    assert_eq!(submitted["stream"], false);
    assert_eq!(submitted["messages"][0]["content"][0]["text"], "整理后的视频提示词");
    assert_eq!(
        submitted["messages"][0]["content"][1]["image_url"]["url"],
        "data:image/png;base64,iVBORw0KGgo="
    );
    assert!(!assistant.to_string().contains("data:image/png"));
    assert_eq!(
        fixture
            .gateway
            .request_count("HEAD", "/v1/videos/video-stream-test/content"),
        1
    );
    let values = payloads(text.as_bytes());
    let parent_request_id = values
        .iter()
        .find_map(|value| value.pointer("/video_task/request_id").and_then(serde_json::Value::as_str))
        .expect("completion must identify the parent request");
    let child_request_id = fixture
        .store
        .seedance_assist_request_for_parent(parent_request_id)
        .unwrap()
        .expect("DeepSeek request must be linked to the video request");
    assert_ne!(parent_request_id, child_request_id);
    assert_eq!(
        fixture.gateway.core_request_id_for_model("deepseek-v4-flash").as_deref(),
        Some(child_request_id.as_str())
    );
    assert_ne!(
        fixture.gateway.core_request_id_for_model("seedance").as_deref(),
        Some(child_request_id.as_str())
    );
    let balance = fixture
        .store
        .key_quota_balance_for_principal(&fixture.billing_principal, "credits")
        .unwrap();
    assert_eq!(balance.settled, 5_000_000, "both exact upstream receipts must settle against this Key");
    assert_eq!(balance.held, 0, "a completed request must leave no quota held");
    let _ = fixture.dir.path();
}

#[tokio::test]
async fn stream_without_idempotency_key_uses_a_core_request_id_and_completes() {
    let fixture = stream_fixture();
    let response = post_chat(&fixture, &fixture.key, None, video_chat_body(true)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers()["content-type"].to_str().unwrap().starts_with("text/event-stream"));
    let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("https://api.gemstory.cn/v1/videos/video-stream-test/content"));
    assert_eq!(text.matches("data: [DONE]\n\n").count(), 1);
    let request_id = payloads(text.as_bytes())
        .iter()
        .find_map(|chunk| chunk.pointer("/video_task/request_id").and_then(serde_json::Value::as_str))
        .expect("server-generated parent request id must be returned")
        .to_owned();
    assert!(!request_id.is_empty());
    assert_eq!(fixture.gateway.core_request_id_for_model("seedance").as_deref(), Some(request_id.as_str()));
    assert_eq!(
        fixture.gateway.request_count("POST", "/v1/chat/completions"),
        1
    );
    assert_eq!(fixture.gateway.model_request_count("/v1/chat/completions", "deepseek-v4-flash"), 1);
    let balance = fixture.store.key_quota_balance_for_principal(&fixture.billing_principal, "credits").unwrap();
    assert_eq!(balance.settled, 5_000_000);
    assert_eq!(balance.held, 0);
    let _ = fixture.dir.path();
}

#[tokio::test]
async fn seedance_chat_preserves_video_specs_from_original_prompt_before_text_assistance() {
    let fixture = stream_fixture();
    let mut body = video_chat_body(true);
    body["messages"][0]["content"][0]["text"] =
        serde_json::Value::String("一只橘猫在窗边伸懒腰，5秒，720P，16:9".into());

    let response = post_chat(&fixture, &fixture.key, Some("seedance-video-specs"), body).await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = to_bytes(response.into_body(), 64 * 1024).await.unwrap();

    let submitted = fixture.gateway.submitted_body();
    assert_eq!(submitted["duration"], 5);
    assert_eq!(submitted["resolution"], "720p");
    assert_eq!(submitted["ratio"], "16:9");
    assert_eq!(submitted["messages"][0]["content"][0]["text"], "整理后的视频提示词");
}

#[tokio::test]
async fn seedance_chat_keeps_explicit_video_specs_ahead_of_prompt_hints() {
    let fixture = stream_fixture();
    let mut body = video_chat_body(true);
    body["duration"] = 8.into();
    body["resolution"] = "1080p".into();
    body["ratio"] = "9:16".into();
    body["messages"][0]["content"][0]["text"] =
        serde_json::Value::String("一只橘猫在窗边伸懒腰，5秒，720P，16:9".into());

    let response = post_chat(&fixture, &fixture.key, Some("seedance-explicit-specs"), body).await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = to_bytes(response.into_body(), 64 * 1024).await.unwrap();

    let submitted = fixture.gateway.submitted_body();
    assert_eq!(submitted["duration"], 8);
    assert_eq!(submitted["resolution"], "1080p");
    assert_eq!(submitted["ratio"], "9:16");
}

#[tokio::test]
async fn completed_video_waits_for_late_receipt_instead_of_ending_stream_with_error() {
    let fixture = stream_fixture();
    *fixture.gateway.video_receipt_status.lock().unwrap() = "unknown".into();
    let response = post_chat(
        &fixture,
        &fixture.key,
        Some("seedance-late-receipt"),
        video_chat_body(true),
    ).await;
    assert_eq!(response.status(), StatusCode::OK);

    let gateway = fixture.gateway.clone();
    let store = fixture.store.clone();
    let release_receipt = tokio::spawn(async move {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Some(request_id) = gateway.core_request_id_for_model("seedance") {
                    if store.request_state(&request_id).unwrap() == aiwork_core::RequestState::Unknown {
                        *gateway.video_receipt_status.lock().unwrap() = "final".into();
                        break;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }).await.expect("the unresolved receipt must be observed before the final one arrives");
    });

    let bytes = tokio::time::timeout(
        std::time::Duration::from_secs(8),
        to_bytes(response.into_body(), 64 * 1024),
    ).await.expect("the stream must finish after the receipt arrives").unwrap();
    release_receipt.await.unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(text.contains("video_task"), "the final video must reach the client");
    assert!(!text.contains("billing_receipt_unresolved"));
    assert_eq!(text.matches("data: [DONE]\n\n").count(), 1);
    assert_eq!(fixture.gateway.model_request_count("/v1/chat/completions", "seedance"), 1);
    let balance = fixture.store.key_quota_balance_for_principal(&fixture.billing_principal, "credits").unwrap();
    assert_eq!(balance.settled, 5_000_000);
    assert_eq!(balance.held, 0);
}

#[tokio::test]
async fn claimed_one_shot_video_can_be_replayed_without_new_charge_after_gate_closes() {
    let fixture = stream_fixture();
    let body = video_chat_body(true);
    let request_hash = starlink_dimension_router::video_billing::request_hash("seedance", &body);
    fixture.store.set_video_billing_control(aiwork_core::VideoBillingControlInput::diagnostic(
        &fixture.billing_principal.key_id,
        &request_hash,
        "同一任务断线回放验收",
    )).unwrap();

    let first = post_chat(&fixture, &fixture.key, Some("seedance-claimed-replay"), body.clone()).await;
    assert_eq!(first.status(), StatusCode::OK);
    let _ = to_bytes(first.into_body(), 64 * 1024).await.unwrap();

    let replay = post_chat(&fixture, &fixture.key, Some("seedance-claimed-replay"), body).await;
    assert_eq!(replay.status(), StatusCode::OK);
    let frames = to_bytes(replay.into_body(), 64 * 1024).await.unwrap();
    let text = String::from_utf8(frames.to_vec()).unwrap();
    assert!(text.contains("video_task"));
    assert_eq!(text.matches("data: [DONE]\n\n").count(), 1);
    assert_eq!(fixture.gateway.model_request_count("/v1/chat/completions", "seedance"), 1);
    let balance = fixture.store.key_quota_balance_for_principal(&fixture.billing_principal, "credits").unwrap();
    assert_eq!(balance.settled, 5_000_000);
    assert_eq!(balance.held, 0);
}

#[tokio::test]
async fn headerless_one_shot_retry_replays_within_window_after_gate_closes() {
    let fixture = stream_fixture();
    let body = video_chat_body(true);
    let request_hash = starlink_dimension_router::video_billing::request_hash("seedance", &body);
    fixture.store.set_video_billing_control(aiwork_core::VideoBillingControlInput::diagnostic(
        &fixture.billing_principal.key_id,
        &request_hash,
        "无需客户端幂等头的短时重连验收",
    )).unwrap();

    let first = post_chat(&fixture, &fixture.key, None, body.clone()).await;
    assert_eq!(first.status(), StatusCode::OK);
    let _ = to_bytes(first.into_body(), 64 * 1024).await.unwrap();

    let replay = post_chat(&fixture, &fixture.key, None, body).await;
    assert_eq!(replay.status(), StatusCode::OK);
    let frames = to_bytes(replay.into_body(), 64 * 1024).await.unwrap();
    assert!(String::from_utf8(frames.to_vec()).unwrap().contains("video_task"));
    assert_eq!(fixture.gateway.model_request_count("/v1/chat/completions", "seedance"), 1);
    let balance = fixture.store.key_quota_balance_for_principal(&fixture.billing_principal, "credits").unwrap();
    assert_eq!(balance.settled, 5_000_000);
    assert_eq!(balance.held, 0);
}

#[tokio::test]
async fn diagnostic_stream_falls_back_for_both_billing_requests_and_settles_independently() {
    let fixture = stream_fixture();
    fixture.gateway.set_quote_unavailable("chat");
    fixture.gateway.set_quote_unavailable("videos");
    let body = video_chat_body(true);
    let request_hash = starlink_dimension_router::video_billing::request_hash("seedance", &body);
    fixture
        .store
        .set_video_billing_control(aiwork_core::VideoBillingControlInput::diagnostic(
            &fixture.billing_principal.key_id,
            &request_hash,
            "受控 Seedance 流式验收",
        ))
        .unwrap();

    let response = post_chat(&fixture, &fixture.key, None, body).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(text.contains("chat.completion.chunk"));
    assert!(text.contains("[DONE]"));
    assert_eq!(fixture.gateway.submitted_body()["messages"][0]["content"][1]["image_url"]["url"],
        "data:image/png;base64,iVBORw0KGgo=", "controlled video must retain its reference image");

    let parent_request_id = payloads(text.as_bytes())
        .iter()
        .find_map(|value| value.pointer("/video_task/request_id").and_then(serde_json::Value::as_str))
        .expect("completed stream must contain the parent request id")
        .to_string();
    let child_request_id = fixture
        .store
        .seedance_assist_request_for_parent(&parent_request_id)
        .unwrap()
        .expect("the helper request must be linked to its video request");
    assert_ne!(parent_request_id, child_request_id);

    let requests = fixture.gateway.requests.lock().unwrap();
    let assistant_headers = requests
        .iter()
        .find(|(method, path, body, _)| {
            method == "POST" && path == "/v1/chat/completions" && body["model"] == "deepseek-v4-flash"
        })
        .map(|(_, _, _, headers)| headers)
        .expect("the text helper must be dispatched once");
    assert_eq!(assistant_headers["x-core-request-id"], child_request_id);
    let operation_id = assistant_headers.get("x-core-controlled-operation-id")
        .expect("helper must carry the parent controlled operation");
    assert!(!assistant_headers.contains_key("x-core-quote-id"));
    assert_eq!(assistant_headers["x-core-key-id"], fixture.billing_principal.key_id);
    let video_headers = requests
        .iter()
        .find(|(method, path, body, _)| {
            method == "POST" && path == "/v1/chat/completions" && body["model"] == "seedance"
        })
        .map(|(_, _, _, headers)| headers)
        .expect("the video request must be dispatched once");
    assert_eq!(video_headers["x-core-request-id"], parent_request_id);
    assert_eq!(video_headers.get("x-core-controlled-operation-id"), Some(operation_id));
    assert!(!video_headers.contains_key("x-core-quote-id"));
    assert_eq!(video_headers["idempotency-key"], parent_request_id,
        "headerless clients still need a stable upstream idempotency key");
    drop(requests);

    let balance = fixture
        .store
        .key_quota_balance_for_principal(&fixture.billing_principal, "credits")
        .unwrap();
    assert_eq!(balance.settled, 5_000_000, "both request-scoped receipts must be charged once");
    assert_eq!(balance.held, 0, "successful completion must release all unused reserved credits");
    assert_eq!(current_key_concurrency(&fixture), 0);
}

#[tokio::test]
async fn controlled_unknown_helper_receipt_blocks_video_and_keeps_the_whole_hold() {
    let fixture = stream_fixture();
    fixture.gateway.set_quote_unavailable("videos");
    *fixture.gateway.assistant_receipt_status.lock().unwrap() = "unknown".into();
    let body = video_chat_body(true);
    let hash = starlink_dimension_router::video_billing::request_hash("seedance", &body);
    fixture.store.set_video_billing_control(aiwork_core::VideoBillingControlInput::diagnostic(
        &fixture.billing_principal.key_id, &hash, "unknown helper receipt must fail closed",
    )).unwrap();
    let response = post_chat(&fixture, &fixture.key, Some("controlled-helper-unknown"), body).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(fixture.gateway.model_request_count("/v1/chat/completions", "deepseek-v4-flash"), 1);
    assert_eq!(fixture.gateway.model_request_count("/v1/chat/completions", "seedance"), 0);
    let balance = fixture.store.key_quota_balance_for_principal(&fixture.billing_principal, "credits").unwrap();
    assert_eq!((balance.available, balance.held, balance.settled), (0, 100_000_000, 0));
}

#[tokio::test]
async fn controlled_reference_asset_failure_happens_before_paid_helper() {
    let fixture = stream_fixture();
    fixture.gateway.set_quote_unavailable("videos");
    let mut body = video_chat_body(true);
    body["image_asset_ids"] = serde_json::json!(["asset-does-not-belong-to-this-key"]);
    let hash = starlink_dimension_router::video_billing::request_hash("seedance", &body);
    fixture.store.set_video_billing_control(aiwork_core::VideoBillingControlInput::diagnostic(
        &fixture.billing_principal.key_id, &hash, "reference image permission check",
    )).unwrap();
    let response = post_chat(&fixture, &fixture.key, Some("controlled-bad-asset"), body).await;
    assert!(response.status().is_client_error());
    assert_eq!(fixture.gateway.model_request_count("/v1/chat/completions", "deepseek-v4-flash"), 0);
    assert_eq!(fixture.gateway.model_request_count("/v1/chat/completions", "seedance"), 0);
    let balance = fixture.store.key_quota_balance_for_principal(&fixture.billing_principal, "credits").unwrap();
    assert_eq!((balance.available, balance.held, balance.settled), (100_000_000, 0, 0));
}

#[tokio::test]
async fn controlled_recovery_queries_existing_helper_receipt_without_restarting_video() {
    let fixture = stream_fixture();
    fixture.gateway.set_quote_unavailable("videos");
    *fixture.gateway.assistant_receipt_status.lock().unwrap() = "unknown".into();
    let body = video_chat_body(false);
    let hash = starlink_dimension_router::video_billing::request_hash("seedance", &body);
    fixture.store.set_video_billing_control(aiwork_core::VideoBillingControlInput::diagnostic(
        &fixture.billing_principal.key_id, &hash, "controlled receipt recovery",
    )).unwrap();
    let response = post_chat(&fixture, &fixture.key, Some("controlled-recovery"), body).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    *fixture.gateway.assistant_receipt_status.lock().unwrap() = "final".into();
    assert!(starlink_dimension_router::user_routes::reconcile_pending_billing_requests_once(&fixture.state) > 0);
    let steps = fixture.store.recoverable_controlled_steps(10).unwrap();
    assert!(steps.iter().any(|step| step.kind == aiwork_core::ControlledStepKind::Assist && step.state == "verified"));
    assert_eq!(fixture.gateway.model_request_count("/v1/chat/completions", "seedance"), 0);
    assert_eq!(fixture.gateway.model_request_count("/v1/chat/completions", "deepseek-v4-flash"), 1);
    let parent = &steps[0].parent_request_id;
    fixture.store.mark_controlled_step_dispatched(parent, parent, aiwork_core::ControlledStepKind::Video).unwrap();
    fixture.store.record_controlled_step(parent, parent, aiwork_core::ControlledStepKind::Video,
        aiwork_core::BillingReceipt {
            request_id: parent.clone(), status: aiwork_core::BillingReceiptStatus::Final,
            actual_credits: Some(aiwork_core::CreditAmount::parse("2.5", "credits").unwrap()),
            unit: "credits".into(), source_ref: "trae-usage-session:recovered-video".into(),
            task_ref: Some("video-recovered".into()), observed_at_ms: chrono::Utc::now().timestamp_millis(),
        }).unwrap();
    starlink_dimension_router::user_routes::reconcile_pending_billing_requests_once(&fixture.state);
    let balance = fixture.store.key_quota_balance_for_principal(&fixture.billing_principal, "credits").unwrap();
    assert_eq!((balance.held, balance.settled), (100_000_000, 0),
        "a billing receipt alone must not classify the unobserved video as successful");
    fixture.store.finish_controlled_operation(parent, Some(true)).unwrap();
    let balance = fixture.store.key_quota_balance_for_principal(&fixture.billing_principal, "credits").unwrap();
    assert_eq!((balance.held, balance.settled), (0, 5_000_000));
}

#[tokio::test]
async fn controlled_recovery_restores_accepted_video_without_resubmitting_it() {
    let fixture = stream_fixture();
    fixture.gateway.set_quote_unavailable("videos");
    let body = video_chat_body(false);
    let hash = starlink_dimension_router::video_billing::request_hash("seedance", &body);
    fixture.store.set_video_billing_control(aiwork_core::VideoBillingControlInput::diagnostic(
        &fixture.billing_principal.key_id, &hash, "accepted task crash recovery",
    )).unwrap();
    let response = post_chat(&fixture, &fixture.key, Some("controlled-task-crash"), body).await;
    assert!(response.status().is_success());
    let parent = fixture.gateway.core_request_id_for_model("seedance").unwrap();
    fixture.state.jobs.lock().unwrap().clear();
    fixture.state.persist_jobs();
    assert!(fixture.store.recoverable_controlled_steps(10).unwrap().iter().any(|step| step.kind == aiwork_core::ControlledStepKind::Video && step.task_ref.as_deref() == Some("video-stream-test")));
    starlink_dimension_router::user_routes::reconcile_pending_billing_requests_once(&fixture.state);
    let restored = fixture.state.jobs.lock().unwrap().get("video-stream-test").cloned().expect("accepted task must be restored");
    assert_eq!(restored.request_id, parent);
    assert_eq!(restored.api_key_id, fixture.billing_principal.key_id);
    assert_eq!(fixture.gateway.model_request_count("/v1/chat/completions", "seedance"), 1);
    assert_eq!(fixture.store.recoverable_controlled_steps(10).unwrap().iter()
        .find(|step| step.kind == aiwork_core::ControlledStepKind::Video).unwrap().task_ref.as_deref(), Some("video-stream-test"));
}

#[tokio::test]
async fn controlled_recovery_finds_task_after_crash_before_core_task_binding() {
    let fixture = stream_fixture();
    let begun = fixture.store.begin_billed_request(aiwork_core::BeginRequestInput {
        user_id: fixture.billing_principal.user_id.clone(),
        api_key_id: fixture.billing_principal.key_id.clone(),
        protocol: "openai".into(), endpoint: "/v1/videos/generations".into(),
        model: "seedance".into(), idempotency_key: "crash-before-bind".into(),
        body: serde_json::json!({"model":"seedance","prompt":"cat"}),
    }).unwrap();
    let parent = match begun { aiwork_core::BeginRequest::Created(handle) => handle.id, other => panic!("{other:?}") };
    fixture.store.begin_controlled_operation(&parent, aiwork_core::UpstreamCreditSnapshot {
        total: aiwork_core::CreditAmount::parse("1000", "credits").unwrap(),
        updated_at_ms: chrono::Utc::now().timestamp_millis(),
    }).unwrap();
    fixture.store.mark_controlled_step_dispatched(&parent, &parent, aiwork_core::ControlledStepKind::Video).unwrap();
    fixture.gateway.requests.lock().unwrap().push(("POST".into(), "/v1/chat/completions".into(),
        serde_json::json!({"model":"seedance"}),
        BTreeMap::from([("x-core-request-id".into(), parent.clone())])));
    starlink_dimension_router::user_routes::reconcile_pending_billing_requests_once(&fixture.state);
    let restored = fixture.state.jobs.lock().unwrap().get("video-stream-test").cloned().expect("task lookup must recover accepted video");
    assert_eq!(restored.request_id, parent);
    assert_eq!(restored.api_key_id, fixture.billing_principal.key_id);
    assert_eq!(fixture.store.recoverable_controlled_steps(10).unwrap().iter()
        .find(|step| step.kind == aiwork_core::ControlledStepKind::Video).unwrap().task_ref.as_deref(), Some("video-stream-test"));
    assert_eq!(fixture.gateway.model_request_count("/v1/chat/completions", "seedance"), 1);
}

#[tokio::test]
async fn restarted_core_settles_verified_assist_without_dispatching_video() {
    let fixture = stream_fixture();
    let begin = |model: &str, key: &str| fixture.store.begin_billed_request(aiwork_core::BeginRequestInput {
        user_id: fixture.billing_principal.user_id.clone(), api_key_id: fixture.billing_principal.key_id.clone(),
        protocol: "openai".into(), endpoint: "/v1/chat/completions".into(), model: model.into(),
        idempotency_key: key.into(), body: serde_json::json!({"model":model,"messages":[{"role":"user","content":"cat"}]}),
    }).unwrap();
    let parent = match begin("seedance", "orphan-parent") { aiwork_core::BeginRequest::Created(handle) => handle.id, other => panic!("{other:?}") };
    let child = match begin("deepseek-v4-flash", "orphan-child") { aiwork_core::BeginRequest::Created(handle) => handle.id, other => panic!("{other:?}") };
    fixture.store.begin_controlled_operation(&parent, aiwork_core::UpstreamCreditSnapshot {
        total: aiwork_core::CreditAmount::parse("1000", "credits").unwrap(),
        updated_at_ms: chrono::Utc::now().timestamp_millis(),
    }).unwrap();
    fixture.store.link_seedance_assist_request(&parent, &child).unwrap();
    fixture.store.mark_controlled_step_dispatched(&parent, &child, aiwork_core::ControlledStepKind::Assist).unwrap();
    fixture.store.record_controlled_step(&parent, &child, aiwork_core::ControlledStepKind::Assist,
        aiwork_core::BillingReceipt { request_id: child.clone(), status: aiwork_core::BillingReceiptStatus::Final,
            actual_credits: Some(aiwork_core::CreditAmount::parse("0.25", "credits").unwrap()),
            unit: "credits".into(), source_ref: "trae-usage-session:assist-only".into(), task_ref: None,
            observed_at_ms: chrono::Utc::now().timestamp_millis() }).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(3));
    let restarted = StarlinkRouterState::for_test(fixture.store.clone(),
        BridgeClient::from_transport("http://bridge", "test-bridge-secret", fixture.gateway.clone()),
        fixture.state.config.clone());
    starlink_dimension_router::user_routes::reconcile_pending_billing_requests_once(&restarted);
    let balance = fixture.store.key_quota_balance_for_principal(&fixture.billing_principal, "credits").unwrap();
    assert_eq!((balance.available, balance.held, balance.settled), (99_750_000, 0, 250_000));
    assert_eq!(fixture.gateway.model_request_count("/v1/chat/completions", "seedance"), 0);
}

#[tokio::test]
async fn controlled_headerless_retry_replays_without_second_upstream_submission() {
    let fixture = stream_fixture();
    fixture.gateway.set_quote_unavailable("videos");
    let body = video_chat_body(true);
    let hash = starlink_dimension_router::video_billing::request_hash("seedance", &body);
    fixture.store.set_video_billing_control(aiwork_core::VideoBillingControlInput::diagnostic(
        &fixture.billing_principal.key_id, &hash, "controlled headerless replay",
    )).unwrap();
    let first = post_chat(&fixture, &fixture.key, None, body.clone()).await;
    assert_eq!(first.status(), StatusCode::OK);
    let _ = to_bytes(first.into_body(), 64 * 1024).await.unwrap();
    let replay = post_chat(&fixture, &fixture.key, None, body).await;
    assert_eq!(replay.status(), StatusCode::OK);
    let _ = to_bytes(replay.into_body(), 64 * 1024).await.unwrap();
    assert_eq!(fixture.gateway.model_request_count("/v1/chat/completions", "deepseek-v4-flash"), 1);
    assert_eq!(fixture.gateway.model_request_count("/v1/chat/completions", "seedance"), 1);
    let balance = fixture.store.key_quota_balance_for_principal(&fixture.billing_principal, "credits").unwrap();
    assert_eq!(balance.settled, 5_000_000);
}

#[tokio::test]
async fn diagnostic_stream_promotes_unknown_helper_usage_before_video_dispatch() {
    let fixture = stream_fixture();
    fixture.gateway.set_quote_unavailable("chat");
    fixture.gateway.set_quote_unavailable("videos");
    *fixture.gateway.assistant_receipt_status.lock().unwrap() = "unknown".into();
    *fixture.gateway.assistant_finalization_status.lock().unwrap() = "final".into();
    let body = video_chat_body(true);
    let request_hash = starlink_dimension_router::video_billing::request_hash("seedance", &body);
    fixture.store.set_video_billing_control(aiwork_core::VideoBillingControlInput::diagnostic(
        &fixture.billing_principal.key_id, &request_hash, "受控文字辅助回执验收",
    )).unwrap();

    let response = post_chat(&fixture, &fixture.key, None, body).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(text.contains("video_task"));
    assert!(text.contains("[DONE]"));
    let parent_request_id = payloads(text.as_bytes()).iter()
        .find_map(|value| value.pointer("/video_task/request_id").and_then(serde_json::Value::as_str))
        .unwrap().to_string();
    let child_request_id = fixture.store.seedance_assist_request_for_parent(&parent_request_id)
        .unwrap().unwrap();
    assert_eq!(fixture.gateway.request_count("POST", &format!(
        "/internal/bridge/requests/{child_request_id}/billing/finalize-chat")), 1);
    assert_eq!(fixture.gateway.model_request_count("/v1/chat/completions", "seedance"), 1);
    let balance = fixture.store.key_quota_balance_for_principal(&fixture.billing_principal, "credits").unwrap();
    assert_eq!(balance.settled, 2_550_400);
    assert_eq!(balance.held, 0);
}

#[tokio::test]
async fn preflight_http_error_without_no_charge_receipt_keeps_controlled_hold() {
    let fixture = stream_fixture();
    fixture.gateway.set_quote_unavailable("chat");
    fixture.gateway.set_quote_unavailable("videos");
    *fixture.gateway.video_receipt_status.lock().unwrap() = "unknown".into();
    *fixture.gateway.video_preflight_rejection.lock().unwrap() = Some("idempotency_key_required");
    let body = video_chat_body(true);
    let request_hash = starlink_dimension_router::video_billing::request_hash("seedance", &body);
    fixture.store.set_video_billing_control(aiwork_core::VideoBillingControlInput::diagnostic(
        &fixture.billing_principal.key_id, &request_hash, "提交前拒绝不应占用视频额度",
    )).unwrap();

    let response = post_chat(&fixture, &fixture.key, None, body).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let parent = fixture.gateway.core_request_id_for_model("seedance").unwrap();
    assert_eq!(fixture.store.request_state(&parent).unwrap(), aiwork_core::RequestState::Unknown,
        "HTTP rejection alone does not prove that no upstream cost occurred");
    let balance = fixture.store.key_quota_balance_for_principal(&fixture.billing_principal, "credits").unwrap();
    assert_eq!(balance.settled, 0, "helper cost remains recorded inside the operation until video proof arrives");
    assert_eq!(balance.held, 100_000_000, "unknown video cost cannot release the operation hold");
    assert_eq!(current_key_concurrency(&fixture), 1);
}

#[tokio::test]
async fn verified_pre_dispatch_rejection_charges_only_helper_and_releases_hold() {
    let fixture = stream_fixture();
    fixture.gateway.set_quote_unavailable("chat");
    fixture.gateway.set_quote_unavailable("videos");
    *fixture.gateway.video_preflight_rejection.lock().unwrap() = Some("invalid_request_error");
    *fixture.gateway.video_no_charge_proof.lock().unwrap() = true;
    let body = video_chat_body(true);
    let request_hash = starlink_dimension_router::video_billing::request_hash("seedance", &body);
    fixture.store.set_video_billing_control(aiwork_core::VideoBillingControlInput::diagnostic(
        &fixture.billing_principal.key_id, &request_hash, "视频本地拒绝零扣费验收",
    )).unwrap();

    let response = post_chat(&fixture, &fixture.key, None, body).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let parent = fixture.gateway.core_request_id_for_model("seedance").unwrap();
    assert_eq!(fixture.store.request_state(&parent).unwrap(), aiwork_core::RequestState::Settled);
    let balance = fixture.store.key_quota_balance_for_principal(&fixture.billing_principal, "credits").unwrap();
    assert_eq!(balance.settled, 2_500_000, "only the verified helper cost may be charged");
    assert_eq!(balance.held, 0);
    assert_eq!(current_key_concurrency(&fixture), 0);
}

#[tokio::test]
async fn delayed_zero_cost_proof_reconciles_without_resubmitting_video() {
    let fixture = stream_fixture();
    fixture.gateway.set_quote_unavailable("chat");
    fixture.gateway.set_quote_unavailable("videos");
    *fixture.gateway.video_preflight_rejection.lock().unwrap() = Some("invalid_request_error");
    let body = video_chat_body(true);
    let request_hash = starlink_dimension_router::video_billing::request_hash("seedance", &body);
    fixture.store.set_video_billing_control(aiwork_core::VideoBillingControlInput::diagnostic(
        &fixture.billing_principal.key_id, &request_hash, "迟到的零扣费证明",
    )).unwrap();
    assert_eq!(post_chat(&fixture, &fixture.key, None, body).await.status(), StatusCode::BAD_GATEWAY);
    let parent = fixture.gateway.core_request_id_for_model("seedance").unwrap();
    assert_eq!(fixture.store.key_quota_balance_for_principal(&fixture.billing_principal, "credits").unwrap().held, 100_000_000);

    *fixture.gateway.video_no_charge_proof.lock().unwrap() = true;
    assert!(starlink_dimension_router::user_routes::reconcile_pending_billing_requests_once(&fixture.state) > 0);
    assert_eq!(fixture.store.request_state(&parent).unwrap(), aiwork_core::RequestState::Settled);
    let balance = fixture.store.key_quota_balance_for_principal(&fixture.billing_principal, "credits").unwrap();
    assert_eq!(balance.settled, 2_500_000);
    assert_eq!(balance.held, 0);
    assert_eq!(fixture.gateway.model_request_count("/v1/chat/completions", "seedance"), 1);
}

#[tokio::test]
async fn unrecognized_video_rejection_keeps_hold_for_safe_reconciliation() {
    let fixture = stream_fixture();
    fixture.gateway.set_quote_unavailable("chat");
    fixture.gateway.set_quote_unavailable("videos");
    *fixture.gateway.video_receipt_status.lock().unwrap() = "unknown".into();
    *fixture.gateway.video_preflight_rejection.lock().unwrap() = Some("upstream_unknown");
    let body = video_chat_body(true);
    let request_hash = starlink_dimension_router::video_billing::request_hash("seedance", &body);
    fixture.store.set_video_billing_control(aiwork_core::VideoBillingControlInput::diagnostic(
        &fixture.billing_principal.key_id, &request_hash, "未知拒绝必须待对账",
    )).unwrap();

    let response = post_chat(&fixture, &fixture.key, None, body).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let parent = fixture.gateway.core_request_id_for_model("seedance").unwrap();
    assert_eq!(fixture.store.request_state(&parent).unwrap(), aiwork_core::RequestState::Unknown);
    let balance = fixture.store.key_quota_balance_for_principal(&fixture.billing_principal, "credits").unwrap();
    assert!(balance.held > 0);
}

#[tokio::test]
async fn helper_quote_failure_marks_child_failed_and_releases_parent_reservation() {
    let fixture = stream_fixture();
    fixture.gateway.set_quote_unavailable("chat");
    let body = video_chat_body(true);

    let response = post_chat(&fixture, &fixture.key, Some("helper-quote-failure"), body).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let parent_request = fixture
        .store
        .lookup_idempotent_request(
            &fixture.billing_principal.user_id,
            &fixture.billing_principal.key_id,
            "videos",
            "seedance",
            &video_chat_body(true),
            "helper-quote-failure",
        )
        .unwrap()
        .expect("parent request must remain queryable by its idempotency key");
    let parent_request_id = match parent_request {
        aiwork_core::BeginRequest::Existing(request) => request.id,
        other => panic!("expected existing parent request, got {other:?}"),
    };
    let child_request_id = fixture
        .store
        .seedance_assist_request_for_parent(&parent_request_id)
        .unwrap()
        .expect("failed helper request must remain linked for diagnosis");
    assert_eq!(fixture.store.request_state(&child_request_id).unwrap(), aiwork_core::RequestState::Failed);
    assert_eq!(fixture.store.request_state(&parent_request_id).unwrap(), aiwork_core::RequestState::Settled);
    let balance = fixture
        .store
        .key_quota_balance_for_principal(&fixture.billing_principal, "credits")
        .unwrap();
    assert_eq!(balance.held, 0);
    assert_eq!(current_key_concurrency(&fixture), 0, "pre-dispatch failures must not leak a Key concurrency slot");
    assert_eq!(fixture.gateway.model_request_count("/v1/chat/completions", "deepseek-v4-flash"), 0);
    assert_eq!(fixture.gateway.model_request_count("/v1/chat/completions", "seedance"), 0);
}

#[tokio::test]
async fn immediate_headerless_retry_reuses_the_same_video_and_charge() {
    let fixture = stream_fixture();
    for _ in 0..2 {
        let response = post_chat(&fixture, &fixture.key, None, video_chat_body(true)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("https://api.gemstory.cn/v1/videos/video-stream-test/content"));
        assert_eq!(text.matches("data: [DONE]\n\n").count(), 1);
    }
    assert_eq!(fixture.gateway.request_count("POST", "/v1/chat/completions"), 1);
    assert_eq!(fixture.gateway.model_request_count("/v1/chat/completions", "deepseek-v4-flash"), 1);
    let balance = fixture.store.key_quota_balance_for_principal(&fixture.billing_principal, "credits").unwrap();
    assert_eq!(balance.settled, 5_000_000);
    assert_eq!(balance.held, 0);
}

#[tokio::test]
async fn changed_headerless_prompt_is_a_new_video_request() {
    let fixture = stream_fixture();
    let first = post_chat(&fixture, &fixture.key, None, video_chat_body(true)).await;
    assert_eq!(first.status(), StatusCode::OK);
    to_bytes(first.into_body(), 64 * 1024).await.unwrap();

    let mut changed = video_chat_body(true);
    changed["messages"][0]["content"][0]["text"] = "请制作另一段视频".into();
    let second = post_chat(&fixture, &fixture.key, None, changed).await;
    assert_eq!(second.status(), StatusCode::OK);
    to_bytes(second.into_body(), 64 * 1024).await.unwrap();

    assert_eq!(fixture.gateway.request_count("POST", "/v1/chat/completions"), 2);
    assert_eq!(fixture.gateway.model_request_count("/v1/chat/completions", "deepseek-v4-flash"), 2);
}

#[tokio::test]
async fn stream_false_keeps_the_existing_async_202_contract() {
    let fixture = stream_fixture();
    let response = post_chat(
        &fixture,
        &fixture.key,
        Some("seedance-nonstream-contract"),
        video_chat_body(false),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["video_task"]["id"], "video-stream-test");
    assert_eq!(fixture.gateway.request_count("POST", "/v1/chat/completions"), 1);
    let _ = fixture.dir.path();
}

#[tokio::test]
async fn reconnecting_with_the_same_idempotency_key_reuses_the_video_task() {
    let fixture = stream_fixture();
    let request = || {
        Request::post("/v1/chat/completions")
            .header("authorization", format!("Bearer {}", fixture.key))
            .header("content-type", "application/json")
            .header("idempotency-key", "seedance-stream-replay")
            .body(Body::from(video_chat_body(true).to_string()))
            .unwrap()
    };

    let first = fixture.app.clone().oneshot(request()).await.unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let first_body = to_bytes(first.into_body(), 64 * 1024).await.unwrap();
    assert!(String::from_utf8(first_body.to_vec()).unwrap().contains("[DONE]"));

    let replay = fixture.app.clone().oneshot(request()).await.unwrap();
    assert_eq!(replay.status(), StatusCode::OK);
    let replay_body = to_bytes(replay.into_body(), 64 * 1024).await.unwrap();
    assert!(String::from_utf8(replay_body.to_vec()).unwrap().contains("[DONE]"));
    assert_eq!(fixture.gateway.request_count("POST", "/v1/chat/completions"), 1);
    let _ = fixture.dir.path();
}

#[tokio::test]
async fn disconnecting_the_stream_does_not_submit_a_second_video_on_reconnect() {
    let fixture = stream_fixture();
    let first = post_chat(
        &fixture,
        &fixture.key,
        Some("seedance-stream-disconnect"),
        video_chat_body(true),
    )
    .await;
    assert_eq!(first.status(), StatusCode::OK);
    drop(first);

    for _ in 0..100 {
        if fixture
            .gateway
            .request_count("HEAD", "/v1/videos/video-stream-test/content")
            > 0
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let resumed = post_chat(
        &fixture,
        &fixture.key,
        Some("seedance-stream-disconnect"),
        video_chat_body(true),
    )
    .await;
    assert_eq!(resumed.status(), StatusCode::OK);
    let body = to_bytes(resumed.into_body(), 64 * 1024).await.unwrap();
    assert!(String::from_utf8(body.to_vec()).unwrap().contains("[DONE]"));
    assert_eq!(fixture.gateway.request_count("POST", "/v1/chat/completions"), 1);
    let _ = fixture.dir.path();
}

#[tokio::test]
async fn a_second_key_of_the_same_user_cannot_read_or_download_the_first_keys_video() {
    let fixture = stream_fixture();
    let submitted = post_chat(
        &fixture,
        &fixture.key,
        Some("seedance-key-isolation"),
        video_chat_body(false),
    )
    .await;
    assert_eq!(submitted.status(), StatusCode::ACCEPTED);

    for path in [
        "/v1/videos/video-stream-test",
        "/v1/videos/video-stream-test/content",
    ] {
        let response = fixture
            .app
            .clone()
            .oneshot(
                Request::get(path)
                    .header("authorization", format!("Bearer {}", fixture.second_key))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
    assert_eq!(fixture.gateway.request_count("GET", "/v1/videos/video-stream-test"), 0);
    assert_eq!(fixture.gateway.request_count("GET", "/v1/videos/video-stream-test/content"), 0);
    let _ = fixture.dir.path();
}

#[tokio::test]
async fn unresolved_receipt_waits_and_missing_content_never_emits_success() {
    let fixture = stream_fixture();
    *fixture.gateway.video_receipt_status.lock().unwrap() = "unknown".into();
    let response = post_chat(
        &fixture,
        &fixture.key,
        Some("seedance-stream-unresolved"),
        video_chat_body(true),
    )
    .await;
    let still_waiting = tokio::time::timeout(
        std::time::Duration::from_millis(250),
        to_bytes(response.into_body(), 64 * 1024),
    ).await;
    assert!(still_waiting.is_err(), "an unknown receipt must keep the stream open, not claim success or fail early");
    let balance = fixture.store.key_quota_balance_for_principal(&fixture.billing_principal, "credits").unwrap();
    assert!(balance.held > 0, "credits remain reserved until a verified receipt arrives");

    let missing_content = stream_fixture();
    *missing_content.gateway.content_status.lock().unwrap() = 404;
    let response = post_chat(
        &missing_content,
        &missing_content.key,
        Some("seedance-stream-no-content"),
        video_chat_body(true),
    )
    .await;
    let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("video_content_unavailable"));
    assert!(!text.contains("视频生成完成"));
    assert_eq!(text.matches("data: [DONE]\n\n").count(), 1);
    let _ = fixture.dir.path();
    let _ = missing_content.dir.path();
}

#[tokio::test]
async fn unresolved_assistant_receipt_never_submits_a_video() {
    let fixture = stream_fixture();
    *fixture.gateway.assistant_receipt_status.lock().unwrap() = "unknown".into();
    let response = post_chat(
        &fixture,
        &fixture.key,
        Some("seedance-assist-unresolved"),
        video_chat_body(true),
    )
    .await;
    let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("reconcile_required"));
    assert_eq!(fixture.gateway.model_request_count("/v1/chat/completions", "deepseek-v4-flash"), 1);
    assert_eq!(fixture.gateway.model_request_count("/v1/chat/completions", "seedance"), 0);
    let parent_request = fixture
        .store
        .lookup_idempotent_request(
            &fixture.billing_principal.user_id,
            &fixture.billing_principal.key_id,
            "videos",
            "seedance",
            &video_chat_body(true),
            "seedance-assist-unresolved",
        )
        .unwrap()
        .expect("the parent request should remain addressable by its idempotency key");
    let parent_request_id = match parent_request {
        aiwork_core::BeginRequest::Existing(request) => request.id,
        other => panic!("expected the original parent request, got {other:?}"),
    };
    let child_request_id = fixture
        .store
        .seedance_assist_request_for_parent(&parent_request_id)
        .unwrap()
        .expect("the assistant request must be related to the parent");
    assert_eq!(
        fixture.store.reservation_for_request(&parent_request_id).unwrap().unwrap().state,
        aiwork_core::ReservationState::Released,
        "the video reservation must be released because no video was submitted"
    );
    assert_eq!(
        fixture.store.reservation_for_request(&child_request_id).unwrap().unwrap().state,
        aiwork_core::ReservationState::Held,
        "an unresolved helper charge must remain held for reconciliation"
    );
    let balance = fixture
        .store
        .key_quota_balance_for_principal(&fixture.billing_principal, "credits")
        .unwrap();
    assert!(balance.held > 0, "unknown helper spending must not be silently released");
    assert_eq!(balance.settled, 0, "an unresolved helper receipt must not be guessed or settled");
    let _ = fixture.dir.path();
}
