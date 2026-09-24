use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use axum::{body::{to_bytes, Body}, http::{Request, Response, StatusCode}, Router};
use serde_json::{json, Value};
use starlink_dimension_router::{
    bridge_client::{BridgeClient, BridgeResponse, BridgeTransport},
    config::RouterConfig,
    key_vault::KeyVault,
    server::build_router,
    state::StarlinkRouterState,
};
use tower::util::ServiceExt;

struct VideoFixture {
    app: Router,
    state: Arc<StarlinkRouterState>,
    store: Arc<aiwork_core::CoreStore>,
    key: String,
    key_id: String,
    bridge: Arc<FakeBridge>,
    dir: TestDir,
}

struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new(label: &str) -> Self {
        Self {
            path: std::env::temp_dir().join(format!(
                "starlink-video-billing-{label}-{}",
                rand::random::<u64>()
            )),
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

struct FakeBridge {
    status_body: Mutex<Value>,
    upstream_summary: Mutex<Value>,
    submit_status: Mutex<u16>,
    quote_unavailable: Mutex<bool>,
    quote_unavailable_endpoint: Mutex<Option<String>>,
    quote_unavailable_code: Mutex<Option<String>>,
    requests: Mutex<Vec<String>>,
    chat_request_ids: Mutex<Vec<String>>,
    chat_receipt: Mutex<Value>,
    quote_max_credits: Mutex<String>,
    receipt: Mutex<Value>,
}

impl FakeBridge {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            status_body: Mutex::new(json!({
                "task": {"id": "video-test", "status": "queued"}
            })),
            upstream_summary: Mutex::new(json!({
                "upstream_credits": {
                    "general":"900.000000",
                    "work":"100.000000",
                    "video_available":"1000.000000",
                    "value":"1000.000000",
                    "source":"aiwork-upstream-aggregate",
                    "fresh":true,
                    "updated_at":chrono::Utc::now().timestamp_millis()
                }
            })),
            submit_status: Mutex::new(202),
            quote_unavailable: Mutex::new(false),
            quote_unavailable_endpoint: Mutex::new(None),
            quote_unavailable_code: Mutex::new(None),
            requests: Mutex::new(Vec::new()),
            chat_request_ids: Mutex::new(Vec::new()),
            chat_receipt: Mutex::new(json!({
                "status":"final",
                "actual_credits":"1.000000",
                "unit":"credits",
                "source_ref":"trae-usage-session:seedance-assist",
                "task_ref":null,
                "observed_at_ms":chrono::Utc::now().timestamp_millis()
            })),
            quote_max_credits: Mutex::new("20.000000".into()),
            receipt: Mutex::new(json!({
                "status":"unknown",
                "actual_credits":null,
                "unit":"credits",
                "source_ref":null,
                "task_ref":null,
                "observed_at_ms":chrono::Utc::now().timestamp_millis()
            })),
        })
    }

    fn set_status(&self, value: Value) {
        *self.status_body.lock().unwrap() = value;
    }

    fn set_upstream_summary(&self, value: Value) {
        *self.upstream_summary.lock().unwrap() = value;
    }

    fn set_submit_status(&self, status: u16) {
        *self.submit_status.lock().unwrap() = status;
    }

    fn set_quote_unavailable(&self, unavailable: bool) {
        *self.quote_unavailable.lock().unwrap() = unavailable;
    }

    fn set_quote_unavailable_for(&self, endpoint: &str) {
        *self.quote_unavailable_endpoint.lock().unwrap() = Some(endpoint.into());
    }

    fn set_quote_unavailable_code(&self, code: &str) {
        *self.quote_unavailable_code.lock().unwrap() = Some(code.into());
    }

    fn set_receipt(&self, status: &str, amount: Option<&str>, unit: &str, task_ref: Option<&str>) {
        *self.receipt.lock().unwrap() = json!({
            "status":status,
            "actual_credits":amount,
            "unit":unit,
            "source_ref":"trae-usage-session:video-billing-test",
            "task_ref":task_ref,
            "observed_at_ms":chrono::Utc::now().timestamp_millis()
        });
    }

    fn request_count(&self, path: &str) -> usize {
        self.requests.lock().unwrap().iter().filter(|item| item.as_str() == path).count()
    }

    fn request_count_prefix(&self, prefix: &str) -> usize {
        self.requests.lock().unwrap().iter().filter(|item| item.starts_with(prefix)).count()
    }
}

impl BridgeTransport for FakeBridge {
    fn send(
        &self,
        method: &str,
        url: &str,
        _headers: &BTreeMap<String, String>,
        body: &[u8],
    ) -> Result<BridgeResponse, String> {
        let path = url.trim_start_matches("http://bridge").to_owned();
        self.requests.lock().unwrap().push(path.clone());
        let request_body = serde_json::from_slice::<Value>(body).unwrap_or(Value::Null);
        let response = match (method, path.as_str()) {
            ("GET", "/internal/bridge/summary") => BridgeResponse {
                status: 200,
                headers: BTreeMap::new(),
                body: serde_json::to_vec(&*self.upstream_summary.lock().unwrap()).unwrap(),
            },
            ("POST", "/internal/bridge/quotes") => {
                let request_id = request_body["request_id"].as_str().unwrap_or_default();
                let endpoint = request_body["endpoint"].as_str().unwrap_or_default();
                if endpoint == "chat" {
                    self.chat_request_ids.lock().unwrap().push(request_id.to_string());
                }
                let endpoint_unavailable = self.quote_unavailable_endpoint.lock().unwrap().as_deref() == Some(endpoint);
                if *self.quote_unavailable.lock().unwrap() || endpoint_unavailable {
                    let error_code = self.quote_unavailable_code.lock().unwrap().clone()
                        .unwrap_or_else(|| "quote_unavailable".into());
                    BridgeResponse {
                        status: 503,
                        headers: BTreeMap::new(),
                        body: serde_json::to_vec(&json!({
                            "request_id":request_id,
                            "status":"unavailable",
                            "error_code":error_code
                        })).unwrap(),
                    }
                } else {
                    BridgeResponse {
                        status: 200,
                        headers: BTreeMap::new(),
                        body: serde_json::to_vec(&json!({
                            "request_id":request_id,
                            "status":"quoted",
                            "quote_id":format!("quote-{request_id}"),
                            "request_fingerprint":request_body["request_fingerprint"],
                            "endpoint":request_body["endpoint"],
                            "model":request_body["model"],
                            "max_credits":self.quote_max_credits.lock().unwrap().clone(),
                            "unit":"credits",
                            "expires_at_ms":chrono::Utc::now().timestamp_millis()+60_000,
                            "source_ref":"video-billing-test-quote"
                        })).unwrap(),
                    }
                }
            }
            ("POST", path) if path.starts_with("/internal/bridge/requests/") && path.ends_with("/billing/finalize") => {
                let request_id = path.trim_start_matches("/internal/bridge/requests/").trim_end_matches("/billing/finalize");
                let mut receipt = self.receipt.lock().unwrap().clone();
                receipt["request_id"] = json!(request_id);
                receipt["task_ref"] = request_body["task_ref"].clone();
                BridgeResponse {
                    status: 200,
                    headers: BTreeMap::new(),
                    body: serde_json::to_vec(&receipt).unwrap(),
                }
            }
            ("GET", path) if path.starts_with("/internal/bridge/requests/") && path.ends_with("/billing") => {
                let request_id = path.trim_start_matches("/internal/bridge/requests/").trim_end_matches("/billing");
                let is_chat_request = self.chat_request_ids.lock().unwrap().iter().any(|item| item == request_id);
                let mut receipt = if is_chat_request {
                    self.chat_receipt.lock().unwrap().clone()
                } else {
                    self.receipt.lock().unwrap().clone()
                };
                receipt["request_id"] = json!(request_id);
                BridgeResponse {
                    status: 200,
                    headers: BTreeMap::new(),
                    body: serde_json::to_vec(&receipt).unwrap(),
                }
            }
            ("POST", "/v1/videos/generations") => BridgeResponse {
                status: *self.submit_status.lock().unwrap(),
                headers: BTreeMap::new(),
                body: br#"{"task":{"id":"video-test","status":"queued"}}"#.to_vec(),
            },
            ("POST", "/v1/chat/completions") if request_body["model"] == "seedance" => BridgeResponse {
                status: 200,
                headers: BTreeMap::new(),
                body: br#"{"task":{"id":"video-test","status":"queued"}}"#.to_vec(),
            },
            ("POST", "/v1/chat/completions") => BridgeResponse {
                status: 200,
                headers: BTreeMap::new(),
                body: br#"{"choices":[{"message":{"role":"assistant","content":"{\"prompt\":\"assisted prompt\"}"}}]}"#.to_vec(),
            },
            ("GET", "/v1/videos/video-test") => BridgeResponse {
                status: 200,
                headers: BTreeMap::new(),
                body: serde_json::to_vec(&*self.status_body.lock().unwrap()).unwrap(),
            },
            _ => BridgeResponse { status: 404, headers: BTreeMap::new(), body: br#"{}"#.to_vec() },
        };
        Ok(response)
    }
}

fn fixture(label: &str) -> VideoFixture {
    let dir = TestDir::new(label);
    let store = Arc::new(aiwork_core::CoreStore::open(dir.path()).unwrap());
    store.migrate().unwrap();
    store.create_bootstrap_admin(
        aiwork_core::NewUser { id: "admin".into(), name: "管理员".into(), role: aiwork_core::UserRole::Admin },
        "bootstrap",
    ).unwrap();
    let admin = aiwork_core::Principal { user_id: "admin".into(), key_id: "admin_session:test".into(), scopes: BTreeSet::from(["admin:*".into()]) };
    let issued = store.issue_api_key_for_new_user_as_admin(
        &admin,
        "视频测试用户",
        BTreeSet::from(["videos:submit".into()]),
        2,
    ).unwrap();
    store.quota_pool_grant_as_admin(&admin, aiwork_core::QuotaGrant {
        user_id: issued.user_id.clone(), resource_kind: "credits".into(), amount: 100_000_000,
        actor_user_id: "admin".into(), reason: "video billing test".into(),
    }).unwrap();
    store.key_quota_allocate_from_pool_as_admin(&admin, aiwork_core::KeyQuotaGrant {
        api_key_id: issued.id.clone(), resource_kind: "credits".into(), amount: 100_000_000,
        actor_user_id: "admin".into(), reason: "video billing test".into(),
    }).unwrap();
    store.set_video_billing_control(aiwork_core::VideoBillingControlInput {
        mode: aiwork_core::VideoBillingMode::Active,
        reason: "测试开启".into(),
        diagnostic_key_id: None,
        diagnostic_request_hash: None,
    }).unwrap();
    let bridge = FakeBridge::new();
    let config = RouterConfig::defaults(dir.path().to_path_buf());
    let client = BridgeClient::from_transport("http://bridge", "bridge-secret", bridge.clone());
    let state = StarlinkRouterState::for_test(store.clone(), client, config);
    let app = build_router(state.clone());
    VideoFixture { app, state, store, key: issued.plaintext, key_id: issued.id, bridge, dir }
}

async fn post_video(fixture: &VideoFixture) -> Response<Body> {
    fixture.app.clone().oneshot(
        Request::post("/v1/videos/generations")
            .header("authorization", format!("Bearer {}", fixture.key))
            .header("content-type", "application/json")
            .header("idempotency-key", format!("video-billing-{}", rand::random::<u64>()))
            .body(Body::from(r#"{"model":"seedance","prompt":"test"}"#))
            .unwrap(),
    ).await.unwrap()
}

#[tokio::test]
async fn stale_upstream_credit_snapshot_blocks_video_before_quote_or_submit() {
    let fixture = fixture("stale-upstream");
    fixture.bridge.set_upstream_summary(json!({
        "upstream_credits": {
            "general":"900.000000",
            "work":"100.000000",
            "video_available":"1000.000000",
            "value":"1000.000000",
            "source":"aiwork-upstream-aggregate",
            "fresh":false,
            "error_code":"upstream_balance_stale",
            "updated_at":chrono::Utc::now().timestamp_millis()-300_001
        }
    }));

    let response = post_video(&fixture).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap(),
    )
    .unwrap();
    assert_eq!(body["error"]["code"], "upstream_credits_unavailable");
    assert_eq!(fixture.bridge.request_count("/internal/bridge/quotes"), 0);
    assert_eq!(fixture.bridge.request_count("/v1/videos/generations"), 0);
}

#[tokio::test]
async fn upstream_balance_below_key_commitments_blocks_video_reservation_and_submit() {
    let fixture = fixture("upstream-shortfall");
    fixture.bridge.set_upstream_summary(json!({
        "upstream_credits": {
            "general":"9.000000",
            "work":"1.000000",
            "video_available":"10.000000",
            "value":"10.000000",
            "source":"aiwork-upstream-aggregate",
            "fresh":true,
            "updated_at":chrono::Utc::now().timestamp_millis()
        }
    }));

    let response = post_video(&fixture).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap(),
    )
    .unwrap();
    assert_eq!(body["error"]["code"], "upstream_commitments_exceed_balance", "body: {body}");
    assert_eq!(fixture.bridge.request_count("/internal/bridge/quotes"), 1);
    assert_eq!(fixture.bridge.request_count("/v1/videos/generations"), 0);
}

async fn post_seedance_chat(fixture: &VideoFixture) -> Response<Body> {
    fixture.app.clone().oneshot(
        Request::post("/v1/chat/completions")
            .header("authorization", format!("Bearer {}", fixture.key))
            .header("content-type", "application/json")
            .header("idempotency-key", format!("video-chat-billing-{}", rand::random::<u64>()))
            .body(Body::from(r#"{"model":"seedance","stream":false,"messages":[{"role":"user","content":"test"}]}"#))
            .unwrap(),
    ).await.unwrap()
}

async fn poll_video(fixture: &VideoFixture) -> Response<Body> {
    fixture.app.clone().oneshot(
        Request::get("/v1/videos/video-test")
            .header("authorization", format!("Bearer {}", fixture.key))
            .body(Body::empty())
            .unwrap(),
    ).await.unwrap()
}

fn quota(fixture: &VideoFixture) -> aiwork_core::CoreQuotaUsageView {
    let principal = fixture.store.authenticate_api_key(&fixture.key).unwrap();
    fixture.store.key_quota_usage_for_principal(&principal, 100).unwrap()
}

#[tokio::test]
async fn accepted_video_keeps_one_reservation_held() {
    let fixture = fixture("held");
    let response = post_video(&fixture).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let usage = quota(&fixture);
    assert_eq!(usage.balances[0].held, 20_000_000);
    assert_eq!(usage.balances[0].settled, 0);
    assert_eq!(fixture.state.jobs.lock().unwrap()["video-test"].billing_state, "held");
}

#[tokio::test]
async fn paused_video_gate_rejects_before_reservation_or_bridge() {
    let fixture = fixture("paused-video");
    fixture.store.set_video_billing_control(aiwork_core::VideoBillingControlInput {
        mode: aiwork_core::VideoBillingMode::Paused,
        reason: "本地暂停验收".into(),
        diagnostic_key_id: None,
        diagnostic_request_hash: None,
    }).unwrap();
    assert_eq!(post_video(&fixture).await.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(quota(&fixture).balances[0].held, 0);
    assert_eq!(fixture.bridge.request_count("/v1/videos/generations"), 0);
}

#[tokio::test]
async fn paused_seedance_chat_rejects_before_reservation_or_bridge() {
    let fixture = fixture("paused-chat");
    fixture.store.set_video_billing_control(aiwork_core::VideoBillingControlInput {
        mode: aiwork_core::VideoBillingMode::Paused,
        reason: "本地暂停验收".into(),
        diagnostic_key_id: None,
        diagnostic_request_hash: None,
    }).unwrap();
    assert_eq!(post_seedance_chat(&fixture).await.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(quota(&fixture).balances[0].held, 0);
    assert_eq!(fixture.bridge.request_count("/v1/chat/completions"), 0);
}

#[tokio::test]
async fn diagnostic_claim_allows_one_matching_request_then_pauses_again() {
    let fixture = fixture("diagnostic-match");
    let body = json!({"model":"seedance","prompt":"test"});
    let hash = starlink_dimension_router::video_billing::request_hash("seedance", &body);
    fixture.store.set_video_billing_control(aiwork_core::VideoBillingControlInput::diagnostic(
        &fixture.key_id, &hash, "本地一次性验收",
    )).unwrap();
    assert_eq!(post_video(&fixture).await.status(), StatusCode::ACCEPTED);
    assert_eq!(post_video(&fixture).await.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(quota(&fixture).balances[0].held, 20_000_000);
}

#[tokio::test]
async fn diagnostic_next_request_is_bound_to_the_key_and_consumed_once() {
    let fixture = fixture("diagnostic-next");
    let body = json!({"model":"seedance","prompt":"a different request body"});
    fixture.store.set_video_billing_control(aiwork_core::VideoBillingControlInput::diagnostic(
        &fixture.key_id, &"0".repeat(64), "本地下一请求一次性验收",
    )).unwrap();
    let principal = fixture.store.authenticate_api_key(&fixture.key).unwrap();
    let other_key = aiwork_core::Principal {
        user_id: principal.user_id.clone(),
        key_id: "different-key".into(),
        scopes: BTreeSet::new(),
    };

    assert_eq!(
        starlink_dimension_router::video_billing::admit_video_request(
            &fixture.store, &other_key, "seedance", &body,
        ).unwrap(),
        starlink_dimension_router::video_billing::VideoAdmission::Paused,
    );

    assert_eq!(
        starlink_dimension_router::video_billing::admit_video_request(
            &fixture.store, &principal, "seedance", &body,
        ).unwrap(),
        starlink_dimension_router::video_billing::VideoAdmission::DiagnosticClaimed,
    );
    assert_eq!(
        starlink_dimension_router::video_billing::admit_video_request(
            &fixture.store, &principal, "seedance", &body,
        ).unwrap(),
        starlink_dimension_router::video_billing::VideoAdmission::Paused,
    );
}

#[tokio::test]
async fn approved_one_shot_quote_fallback_reserves_the_entire_key_balance_once() {
    let fixture = fixture("one-shot-unquoted");
    fixture.bridge.set_quote_unavailable(true);
    fixture.store.set_video_billing_control(aiwork_core::VideoBillingControlInput::diagnostic(
        &fixture.key_id, &"0".repeat(64), "本地下一请求一次性验收",
    )).unwrap();

    let response = post_video(&fixture).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let job = fixture.state.jobs.lock().unwrap()["video-test"].clone();
    let reservation = fixture.store.reservation_for_request(&job.request_id).unwrap().unwrap();
    assert_eq!(reservation.amount, 100_000_000);
    assert_eq!(reservation.state, aiwork_core::ReservationState::Held);
    assert_eq!(quota(&fixture).balances[0].held, 100_000_000);

    assert_eq!(post_video(&fixture).await.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(fixture.bridge.request_count("/v1/videos/generations"), 1);
}

#[tokio::test]
async fn one_shot_unrelated_quote_error_fails_unreserved_video_request() {
    let fixture = fixture("one-shot-other-quote-error");
    fixture.bridge.set_quote_unavailable(true);
    fixture.bridge.set_quote_unavailable_code("quote_timeout");
    fixture.store.set_video_billing_control(aiwork_core::VideoBillingControlInput::diagnostic(
        &fixture.key_id, &"0".repeat(64), "本地下一请求一次性验收",
    )).unwrap();

    let response = post_video(&fixture).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = serde_json::from_slice(&to_bytes(response.into_body(), 64 * 1024).await.unwrap()).unwrap();
    let request_id = body["error"]["request_id"].as_str().unwrap();
    assert_eq!(fixture.store.request_state(request_id).unwrap(), aiwork_core::RequestState::Failed);
    assert_eq!(quota(&fixture).balances[0].held, 0);
    assert_eq!(fixture.bridge.request_count("/v1/videos/generations"), 0);
}

#[tokio::test]
async fn active_seedance_chat_without_quote_fails_unreserved_parent_request() {
    let fixture = fixture("active-seedance-unquoted");
    fixture.bridge.set_quote_unavailable(true);

    let response = post_seedance_chat(&fixture).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = serde_json::from_slice(&to_bytes(response.into_body(), 64 * 1024).await.unwrap()).unwrap();
    let request_id = body["error"]["request_id"].as_str().unwrap();
    assert_eq!(fixture.store.request_state(request_id).unwrap(), aiwork_core::RequestState::Failed);
    assert_eq!(quota(&fixture).balances[0].held, 0);
    assert_eq!(fixture.bridge.request_count("/v1/chat/completions"), 0);
}

#[tokio::test]
async fn one_shot_seedance_chat_keeps_the_assist_quote_separate_from_the_video_hold() {
    let fixture = fixture("one-shot-chat");
    fixture.bridge.set_quote_unavailable_for("videos");
    fixture.store.set_video_billing_control(aiwork_core::VideoBillingControlInput::diagnostic(
        &fixture.key_id, &"0".repeat(64), "本地下一请求一次性验收",
    )).unwrap();

    let response = post_seedance_chat(&fixture).await;
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let job = fixture.state.jobs.lock().unwrap()["video-test"].clone();
    assert!(job.one_shot_test);
    assert_eq!(quota(&fixture).balances[0].held, 99_000_000);
    assert_eq!(quota(&fixture).balances[0].settled, 1_000_000);

    assert_eq!(post_seedance_chat(&fixture).await.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(fixture.bridge.request_count("/v1/chat/completions"), 2);
}

#[tokio::test]
async fn one_shot_video_settlement_uses_request_scoped_aiwork_finalization() {
    let fixture = fixture("one-shot-finalize");
    fixture.bridge.set_quote_unavailable(true);
    fixture.store.set_video_billing_control(aiwork_core::VideoBillingControlInput::diagnostic(
        &fixture.key_id, &"0".repeat(64), "本地下一请求一次性验收",
    )).unwrap();
    assert_eq!(post_video(&fixture).await.status(), StatusCode::ACCEPTED);
    let job = fixture.state.jobs.lock().unwrap()["video-test"].clone();
    assert!(job.one_shot_test);

    fixture.bridge.set_receipt("final", Some("120.000000"), "credits", Some("video-test"));
    fixture.bridge.set_status(json!({"task":{"id":"video-test","status":"completed"}}));
    assert_eq!(poll_video(&fixture).await.status(), StatusCode::OK);
    assert_eq!(fixture.bridge.request_count_prefix(&format!("/internal/bridge/requests/{}/billing/finalize", job.request_id)), 1);
    assert_eq!(quota(&fixture).balances[0].held, 0);
    assert_eq!(quota(&fixture).balances[0].settled, 120_000_000);
    let admin = aiwork_core::Principal {
        user_id: "admin".into(),
        key_id: "admin_session:test".into(),
        scopes: BTreeSet::from(["admin:*".into()]),
    };
    let key = fixture.store.list_api_keys_as_admin(&admin, None).unwrap()
        .into_iter().find(|key| key.id == fixture.key_id).unwrap();
    assert!(key.billing_blocked, "over-balance actual settlement must freeze this key");
}

#[tokio::test]
async fn one_shot_unknown_receipt_keeps_the_full_hold_and_does_not_settle() {
    let fixture = fixture("one-shot-unknown-receipt");
    fixture.bridge.set_quote_unavailable(true);
    fixture.store.set_video_billing_control(aiwork_core::VideoBillingControlInput::diagnostic(
        &fixture.key_id, &"0".repeat(64), "本地下一请求一次性验收",
    )).unwrap();
    assert_eq!(post_video(&fixture).await.status(), StatusCode::ACCEPTED);
    fixture.bridge.set_status(json!({"task":{"id":"video-test","status":"completed"}}));

    assert_eq!(poll_video(&fixture).await.status(), StatusCode::OK);
    let job = fixture.state.jobs.lock().unwrap()["video-test"].clone();
    assert_eq!(job.billing_state, "reconcile_required");
    assert_eq!(job.error_code.as_deref(), Some("billing_receipt_unresolved"));
    assert_eq!(quota(&fixture).balances[0].held, 100_000_000);
    assert_eq!(quota(&fixture).balances[0].settled, 0);
    assert_eq!(fixture.bridge.request_count_prefix(&format!(
        "/internal/bridge/requests/{}/billing/finalize", job.request_id
    )), 1);
    assert_eq!(post_video(&fixture).await.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(fixture.bridge.request_count("/v1/videos/generations"), 1);
}

#[tokio::test]
async fn verified_receipt_commits_actual_credits_once() {
    let fixture = fixture("verified");
    assert_eq!(post_video(&fixture).await.status(), StatusCode::ACCEPTED);
    fixture.bridge.set_receipt("final", Some("1.000000"), "credits", Some("video-test"));
    fixture.bridge.set_status(json!({
        "task": {"id":"video-test","status":"completed",
        "billing":{"status":"verified","actual_credits":"1.000000","unit":"credits","task_ref":"video-test"}}
    }));
    assert_eq!(poll_video(&fixture).await.status(), StatusCode::OK);
    assert_eq!(poll_video(&fixture).await.status(), StatusCode::OK);
    let usage = quota(&fixture);
    assert_eq!(usage.balances[0].held, 0);
    assert_eq!(usage.balances[0].settled, 1_000_000);
    assert_eq!(fixture.state.jobs.lock().unwrap()["video-test"].billing_state, "settled");
    assert_eq!(fixture.bridge.request_count("/v1/videos/video-test"), 2);
}

#[tokio::test]
async fn concurrent_verified_polls_commit_only_once() {
    let fixture = fixture("concurrent");
    assert_eq!(post_video(&fixture).await.status(), StatusCode::ACCEPTED);
    fixture.bridge.set_receipt("final", Some("1.000000"), "credits", Some("video-test"));
    fixture.bridge.set_status(json!({
        "task":{"id":"video-test","status":"completed",
        "billing":{"status":"verified","actual_credits":"1.000000","unit":"credits","task_ref":"video-test"}}
    }));
    let (first, second) = tokio::join!(poll_video(&fixture), poll_video(&fixture));
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(second.status(), StatusCode::OK);
    let usage = quota(&fixture);
    assert_eq!(usage.balances[0].held, 0);
    assert_eq!(usage.balances[0].settled, 1_000_000);
    assert_eq!(fixture.state.jobs.lock().unwrap()["video-test"].billing_state, "settled");
}

#[tokio::test]
async fn completed_without_verified_receipt_is_reconciliation_required() {
    let fixture = fixture("unverified");
    assert_eq!(post_video(&fixture).await.status(), StatusCode::ACCEPTED);
    fixture.bridge.set_status(json!({"task":{"id":"video-test","status":"completed"}}));
    assert_eq!(poll_video(&fixture).await.status(), StatusCode::OK);
    let job = fixture.state.jobs.lock().unwrap()["video-test"].clone();
    assert_eq!(job.status, "reconcile_required");
    assert_eq!(job.billing_state, "reconcile_required");
    assert_eq!(quota(&fixture).balances[0].held, 20_000_000);
    assert_eq!(quota(&fixture).balances[0].settled, 0);
}

#[tokio::test]
async fn accepted_upstream_failure_without_verified_receipt_stays_held_for_reconciliation() {
    let fixture = fixture("failed");
    assert_eq!(post_video(&fixture).await.status(), StatusCode::ACCEPTED);
    fixture.bridge.set_status(json!({"task":{"id":"video-test","status":"failed"}}));
    assert_eq!(poll_video(&fixture).await.status(), StatusCode::OK);
    let job = fixture.state.jobs.lock().unwrap()["video-test"].clone();
    assert_eq!(job.status, "reconcile_required");
    assert_eq!(job.billing_state, "reconcile_required");
    assert_eq!(quota(&fixture).balances[0].held, 20_000_000);
    assert_eq!(quota(&fixture).balances[0].settled, 0);
}

#[tokio::test]
async fn fractional_credit_receipt_settles_exactly_in_microcredits() {
    let fixture = fixture("fractional");
    assert_eq!(post_video(&fixture).await.status(), StatusCode::ACCEPTED);
    fixture.bridge.set_receipt("final", Some("12.500000"), "credits", Some("video-test"));
    fixture.bridge.set_status(json!({
        "task":{"id":"video-test","status":"completed",
        "billing":{"status":"verified","actual_credits":"12.500000","unit":"credits","task_ref":"video-test"}}
    }));
    assert_eq!(poll_video(&fixture).await.status(), StatusCode::OK);
    let job = fixture.state.jobs.lock().unwrap()["video-test"].clone();
    assert_eq!(job.status, "completed");
    assert_eq!(job.billing_state, "settled");
    assert_eq!(job.actual_credits.as_deref(), Some("12.500000"));
    assert_eq!(quota(&fixture).balances[0].held, 0);
    assert_eq!(quota(&fixture).balances[0].settled, 12_500_000);
}

#[tokio::test]
async fn over_quote_receipt_debits_actual_and_releases_the_hold() {
    let fixture = fixture("over-bound");
    assert_eq!(post_video(&fixture).await.status(), StatusCode::ACCEPTED);
    fixture.bridge.set_receipt("final", Some("21.000000"), "credits", Some("video-test"));
    fixture.bridge.set_status(json!({
        "task":{"id":"video-test","status":"completed",
        "billing":{"status":"verified","actual_credits":"2.000000","unit":"credits","task_ref":"video-test"}}
    }));
    assert_eq!(poll_video(&fixture).await.status(), StatusCode::OK);
    let job = fixture.state.jobs.lock().unwrap()["video-test"].clone();
    assert_eq!(job.billing_state, "settled");
    assert_eq!(job.actual_credits.as_deref(), Some("21.000000"));
    assert_eq!(quota(&fixture).balances[0].held, 0);
    assert_eq!(quota(&fixture).balances[0].settled, 21_000_000);
}

#[tokio::test]
async fn wrong_task_reference_is_not_accepted_as_a_receipt() {
    let fixture = fixture("wrong-receipt");
    assert_eq!(post_video(&fixture).await.status(), StatusCode::ACCEPTED);
    fixture.bridge.set_receipt("final", Some("1.000000"), "credits", Some("other-task"));
    fixture.bridge.set_status(json!({
        "task":{"id":"video-test","status":"completed",
        "billing":{"status":"verified","actual_credits":"1.000000","unit":"points","task_ref":"other-task"}}
    }));
    assert_eq!(poll_video(&fixture).await.status(), StatusCode::OK);
    let job = fixture.state.jobs.lock().unwrap()["video-test"].clone();
    assert_eq!(job.billing_state, "reconcile_required");
    assert_eq!(job.error_code.as_deref(), Some("billing_task_ref_mismatch"));
    assert_eq!(quota(&fixture).balances[0].held, 20_000_000);
    assert_eq!(quota(&fixture).balances[0].settled, 0);
}

#[tokio::test]
async fn wrong_receipt_unit_is_not_accepted_as_a_final_credit_receipt() {
    let fixture = fixture("wrong-unit");
    assert_eq!(post_video(&fixture).await.status(), StatusCode::ACCEPTED);
    fixture.bridge.set_receipt("final", Some("1.000000"), "points", Some("video-test"));
    fixture.bridge.set_status(json!({"task":{"id":"video-test","status":"completed"}}));
    assert_eq!(poll_video(&fixture).await.status(), StatusCode::OK);
    let job = fixture.state.jobs.lock().unwrap()["video-test"].clone();
    assert_eq!(job.billing_state, "reconcile_required");
    assert_eq!(job.error_code.as_deref(), Some("billing_receipt_unresolved"));
    assert_eq!(quota(&fixture).balances[0].held, 20_000_000);
    assert_eq!(quota(&fixture).balances[0].settled, 0);
}

#[tokio::test]
async fn pre_accept_rejection_releases_the_hold() {
    let fixture = fixture("rejected");
    fixture.bridge.set_submit_status(400);
    fixture.bridge.set_receipt("failed_no_charge", Some("0.000000"), "credits", Some("video-test"));
    assert_eq!(post_video(&fixture).await.status(), StatusCode::BAD_REQUEST);
    let usage = quota(&fixture);
    assert_eq!(usage.balances[0].held, 0);
    assert_eq!(usage.balances[0].settled, 0);
}

#[tokio::test]
async fn diagnostic_mode_rejects_a_non_claimed_key_before_reservation() {
    let fixture = fixture("diagnostic");
    fixture.store.set_video_billing_control(aiwork_core::VideoBillingControlInput::diagnostic(
        &fixture.key_id, &"a".repeat(64), "only one diagnostic request",
    )).unwrap();
    let response = post_video(&fixture).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(quota(&fixture).balances[0].held, 0);
}

#[tokio::test]
async fn accepted_job_is_restored_from_disk_after_router_restart() {
    let fixture = fixture("restart");
    assert_eq!(post_video(&fixture).await.status(), StatusCode::ACCEPTED);
    let config = RouterConfig::defaults(fixture.dir.path().to_path_buf());
    let reopened = StarlinkRouterState::open_with_key_vault(
        config,
        BridgeClient::from_transport("http://bridge", "bridge-secret", fixture.bridge.clone()),
        KeyVault::for_test(),
    ).unwrap();
    let job = reopened.jobs.lock().unwrap()["video-test"].clone();
    assert_eq!(job.billing_state, "held");
    assert_eq!(job.reservation_id.is_some(), true);
    assert_eq!(job.upstream_id.as_deref(), Some("video-test"));
}
