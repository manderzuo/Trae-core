use aiwork_core::{CoreStore, KeyQuotaGrant, NewUser, Principal, UserRole};
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use serde_json::{json, Value};
use starlink_dimension_router::{
    bridge_client::{BridgeClient, BridgeResponse, BridgeTransport},
    config::RouterConfig,
    state::StarlinkRouterState,
    work_context,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};
use tower::ServiceExt;
struct Bridge {
    claims: Mutex<BTreeMap<String, Value>>,
    video_sends: AtomicUsize,
    assist_sends: AtomicUsize,
    unknown: std::sync::atomic::AtomicBool,
}
impl BridgeTransport for Bridge {
    fn send(
        &self,
        _: &str,
        url: &str,
        headers: &BTreeMap<String, String>,
        raw: &[u8],
    ) -> Result<BridgeResponse, String> {
        assert_eq!(
            headers.get("authorization").map(String::as_str),
            Some("Bearer bridge-only")
        );
        let input: Value = serde_json::from_slice(raw).unwrap_or(Value::Null);
        let value = if url.ends_with("/key-registry") {
            json!({"applied":true})
        } else if url.ends_with("/v1/assets") {
            json!({"id":"bridge-owned-image"})
        } else if url.ends_with("/budgets/prepare") {
            let id = input["request_id"].as_str().unwrap();
            self.claims.lock().unwrap().insert(id.into(), input.clone());
            json!({"wire_version":2,"dispatch_token":"fixture","evidence_level":"policy_only","prepared_at_ms":chrono::Utc::now().timestamp_millis(),"revision":1,
                "authorization":{"budget_id":format!("b-{id}"),"parent_request_id":input["parent_request_id"],"request_id":id,"core_key_id":input["core_key_id"],"request_fingerprint":input["request_fingerprint"],"endpoint":input["endpoint"],"model":input["model"],"account_ref":"exclusive-account","bridge_instance_id":"fixture","profile_fingerprint":"profile","policy_version":"fixture","hold_credits":if input["step_kind"]=="assist" {"2"}else{"40"},"expires_at_ms":chrono::Utc::now().timestamp_millis()+60000}})
        } else if url.ends_with("/budgets/dispatch") {
            let kind = self.claims.lock().unwrap()
                [input["authorization"]["request_id"].as_str().unwrap()]["step_kind"]
                .clone();
            if kind == "video" {
                self.video_sends.fetch_add(1, Ordering::SeqCst);
            } else {
                self.assist_sends.fetch_add(1, Ordering::SeqCst);
            }
            json!({"wire_version":2,"status":"accepted","budget_id":input["authorization"]["budget_id"],"request_id":input["authorization"]["request_id"]})
        } else if url.ends_with("/budgets/cancel") {
            json!({"wire_version":2,"status":"canceled"})
        } else {
            let claims = self.claims.lock().unwrap();
            let (id, c) = claims
                .iter()
                .find(|(id, _)| url.contains(&format!("/requests/{id}/")))
                .ok_or("missing claim")?;
            let mut v = json!({"wire_version":2,"budget_id":format!("b-{id}"),"request_id":id,"core_key_id":c["core_key_id"],"account_ref":"exclusive-account","bridge_instance_id":"fixture"});
            let unknown = c["step_kind"] == "video" && self.unknown.load(Ordering::SeqCst);
            if url.contains("/execution?") {
                v["status"] = json!(if unknown { "unknown" } else { "succeeded" });
                v["execution"] = json!({"budget_id":format!("b-{id}"),"request_id":id,"core_key_id":c["core_key_id"],"account_ref":"exclusive-account","bridge_instance_id":"fixture","step_kind":c["step_kind"],"state":if unknown{"unknown"}else{"succeeded"},"task_ref":if c["step_kind"]=="video"{json!(format!("video-{id}"))}else{Value::Null},"finished_at_ms":if unknown{Value::Null}else{json!(chrono::Utc::now().timestamp_millis())},"result_available":!unknown});
            } else if url.contains("/result?") {
                v["status"] = json!(if unknown { "not_ready" } else { "ready" });
                v["result"] = if unknown {
                    Value::Null
                } else if c["step_kind"] == "video" {
                    json!({"id":format!("video-{id}"),"status":"completed"})
                } else {
                    let payload: Value =
                        serde_json::from_str(c["body"]["messages"][1]["content"].as_str().unwrap())
                            .unwrap_or(Value::Null);
                    assert_eq!(c["body"]["model"], "glm-5.3-flash");
                    assert_eq!(c["body"]["max_tokens"], 1024);
                    let parent = !payload["parent"].is_null();
                    let create = payload["requested_action"] == "create"
                        || payload["current"]
                            .as_str()
                            .is_some_and(|s| s.contains("独立生成"));
                    let prompt = if parent && !create {
                        "橘猫在夜色中放慢脚步"
                    } else {
                        "橘猫在草地上散步"
                    };
                    let decision = if payload.is_null() {
                        json!({"intent":"video","prompt":prompt})
                    } else {
                        json!({"action":if parent&&!create{"revise"}else{"create"},"effective_prompt":prompt,"spec_patch":{},"reference_policy":"inherit","clarification":null})
                    };
                    json!({"choices":[{"message":{"content":decision.to_string()},"finish_reason":"stop"}]})
                };
            } else if url.contains("/billing?") {
                v["status"] = json!("pending");
                v["event"] = Value::Null;
                v["receipt"] = Value::Null;
            } else {
                return Err("unexpected route".into());
            }
            v
        };
        Ok(BridgeResponse {
            status: 200,
            headers: BTreeMap::new(),
            body: serde_json::to_vec(&value).unwrap(),
        })
    }
}
struct Fixture {
    state: Arc<StarlinkRouterState>,
    app: axum::Router,
    bridge: Arc<Bridge>,
    key: String,
    owner: Principal,
    dir: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("work-execution-{}", rand::random::<u64>()));
        let store = Arc::new(CoreStore::open(&dir).unwrap());
        store.migrate().unwrap();
        store
            .create_user(
                NewUser {
                    id: "admin".into(),
                    name: "Admin".into(),
                    role: UserRole::Admin,
                },
                "bootstrap",
            )
            .unwrap();
        let k = store
            .issue_api_key(
                "admin",
                "test",
                BTreeSet::from(["admin:*".into()]),
                "bootstrap",
            )
            .unwrap();
        let owner = store.authenticate_api_key(&k.plaintext).unwrap();
        store
            .key_quota_grant_as_admin(
                &owner,
                KeyQuotaGrant {
                    api_key_id: k.id,
                    resource_kind: "credits".into(),
                    amount: 500000000,
                    actor_user_id: "admin".into(),
                    reason: "fixture".into(),
                },
            )
            .unwrap();
        store
            .set_video_billing_control(aiwork_core::VideoBillingControlInput {
                mode: aiwork_core::VideoBillingMode::Active,
                reason: "fixture".into(),
                diagnostic_key_id: None,
                diagnostic_request_hash: None,
            })
            .unwrap();
        let bridge = Arc::new(Bridge {
            claims: Mutex::new(BTreeMap::new()),
            video_sends: AtomicUsize::new(0),
            assist_sends: AtomicUsize::new(0),
            unknown: std::sync::atomic::AtomicBool::new(false),
        });
        let mut config = RouterConfig::defaults(dir.clone());
        config.budget_billing_v2 = true;
        config.work_context_enabled = true;
        config.seedance_assistant_model = "glm-5.3-flash".into();
        config.public_base_url = "https://api.example.test".into();
        let state = StarlinkRouterState::for_test(
            store,
            BridgeClient::from_transport("http://bridge", "bridge-only", bridge.clone()),
            config,
        );
        let app = starlink_dimension_router::server::build_router(state.clone());
        Self {
            state,
            app,
            bridge,
            key: k.plaintext,
            owner,
            dir,
        }
    }
    async fn response(&self, key: &str, id: &str, body: &Value) -> axum::response::Response {
        self.app
            .clone()
            .oneshot(
                Request::post("/v1/chat/completions")
                    .header("authorization", format!("Bearer {key}"))
                    .header("idempotency-key", id)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }
    async fn chat(&self, id: &str, body: &Value) -> Value {
        let r = self.response(&self.key, id, body).await;
        let status = r.status();
        let bytes = to_bytes(r.into_body(), 256 * 1024).await.unwrap();
        let value: Value = if bytes.starts_with(b"data: ") {
            std::str::from_utf8(&bytes)
                .unwrap()
                .lines()
                .filter_map(|s| s.strip_prefix("data: "))
                .filter_map(|s| serde_json::from_str::<Value>(s).ok())
                .last()
                .unwrap()
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        assert_eq!(status, StatusCode::OK, "{value}");
        assert!(value["error"].is_null(), "{value}");
        value
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
fn create() -> Value {
    json!({"model":"seedance","duration":5,"resolution":"480p","ratio":"9:16","messages":[{"role":"user","content":"生成橘猫散步的视频"}]})
}
fn revise(result: &Value, text: &str) -> Value {
    json!({"model":"seedance","messages":[result["choices"][0]["message"],{"role":"user","content":text}]})
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
