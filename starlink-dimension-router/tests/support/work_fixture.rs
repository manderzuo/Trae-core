pub(crate) use aiwork_core::{CoreStore, KeyQuotaGrant, NewUser, Principal, UserRole};
pub(crate) use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
pub(crate) use serde_json::{json, Value};
pub(crate) use starlink_dimension_router::{
    bridge_client::{BridgeClient, BridgeResponse, BridgeTransport},
    config::RouterConfig,
    state::StarlinkRouterState,
    work_context,
};
pub(crate) use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};
pub(crate) use tower::ServiceExt;
pub(crate) struct Bridge {
    pub(crate) claims: Mutex<BTreeMap<String, Value>>,
    pub(crate) video_sends: AtomicUsize,
    pub(crate) assist_sends: AtomicUsize,
    pub(crate) unknown: std::sync::atomic::AtomicBool,
    pub(crate) tail_capability: std::sync::atomic::AtomicBool,
    pub(crate) frame_failure: std::sync::atomic::AtomicBool,
    pub(crate) frame_reads: AtomicUsize,
    pub(crate) billing_final: std::sync::atomic::AtomicBool,
    pub(crate) assist_prepare_failure: std::sync::atomic::AtomicBool,
    pub(crate) assist_prepare_panic: std::sync::atomic::AtomicBool,
    pub(crate) helper_decision: Mutex<Option<Value>>,
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
        if url.ends_with("/budgets/prepare") && input["step_kind"]=="assist" && self.assist_prepare_panic.load(Ordering::SeqCst) {panic!("simulated preparation worker interruption");}
        if url.ends_with("/budgets/prepare") && input["step_kind"]=="assist" && self.assist_prepare_failure.load(Ordering::SeqCst) {
            return Ok(BridgeResponse {status:503,headers:BTreeMap::new(),body:serde_json::to_vec(&json!({"error":{"code":"budget_policy_unconfigured"}})).unwrap()});
        }
        if url.contains("/last-frame?") {
            if self.frame_failure.load(Ordering::SeqCst) {
                return Err("frame unavailable".into());
            }
            self.frame_reads.fetch_add(1, Ordering::SeqCst);
            let claims = self.claims.lock().unwrap();
            let (id, c) = claims
                .iter()
                .find(|(id, _)| url.contains(&format!("/requests/{id}/")))
                .ok_or("missing video claim")?;
            use base64::Engine;
            use sha2::{Digest, Sha256};
            let body=base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aVl8AAAAASUVORK5CYII=").unwrap();
            return Ok(BridgeResponse {
                status: 200,
                headers: BTreeMap::from([
                    ("content-type".into(), "image/png".into()),
                    ("content-length".into(), body.len().to_string()),
                    ("x-aiwork-frame-width".into(), "1".into()),
                    ("x-aiwork-frame-height".into(), "1".into()),
                    ("x-aiwork-frame-timestamp-ms".into(), "875".into()),
                    (
                        "x-aiwork-source-sha256".into(),
                        format!("{:x}", Sha256::digest(format!("source-{id}"))),
                    ),
                    (
                        "x-aiwork-frame-sha256".into(),
                        format!("{:x}", Sha256::digest(&body)),
                    ),
                    ("x-aiwork-request-id".into(), id.clone()),
                    ("x-aiwork-budget-id".into(), format!("b-{id}")),
                    (
                        "x-aiwork-core-key-id".into(),
                        c["core_key_id"].as_str().unwrap().into(),
                    ),
                    ("x-aiwork-account-ref".into(), "exclusive-account".into()),
                    ("x-aiwork-bridge-instance-id".into(), "fixture".into()),
                ]),
                body,
            });
        }
        let value = if url.ends_with("/video-capabilities") {
            json!({"tail_reference":self.tail_capability.load(Ordering::SeqCst),"native_first_frame":false,"native_video_extend":false,"contract_version":"tail-reference-v1","evidence_digest":"ab".repeat(32)})
        } else if url.ends_with("/key-registry") {
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
                        json!({"action":if payload["requested_action"]=="continue"{"continue"}else if parent&&!create{"revise"}else{"create"},"effective_prompt":prompt,"spec_patch":{},"reference_policy":"inherit","clarification":null})
                    };
                    let decision=self.helper_decision.lock().unwrap().clone().unwrap_or(decision);
                    json!({"choices":[{"message":{"content":decision.to_string()},"finish_reason":"stop"}]})
                };
            } else if url.contains("/billing?") {
                if self.billing_final.load(Ordering::SeqCst) {
                    let receipt = json!({"request_id":id,"status":"final","actual_credits":"1.250000","unit":"credits","source_ref":format!("fixture-final-{id}"),"task_ref":if c["step_kind"]=="video"{json!(format!("video-{id}"))}else{Value::Null},"observed_at_ms":chrono::Utc::now().timestamp_millis()});
                    v["status"] = json!("final");
                    v["receipt"] = receipt.clone();
                    v["event"] = json!({"wire_version":2,"generation":"fixture","sequence":1,"event_id":format!("event-{id}"),"request_id":id,"core_key_id":c["core_key_id"],"budget_id":format!("b-{id}"),"account_ref":"exclusive-account","bridge_instance_id":"fixture","kind":"final","receipt":receipt,"conflict":null,"confirmation_policy":"post-terminal-session-observation-v1","evidence_hash":format!("hash-{id}")});
                } else {
                    v["status"] = json!("pending");
                    v["event"] = Value::Null;
                    v["receipt"] = Value::Null;
                }
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
pub(crate) struct Fixture {
    pub(crate) state: Arc<StarlinkRouterState>,
    pub(crate) app: axum::Router,
    pub(crate) bridge: Arc<Bridge>,
    pub(crate) key: String,
    pub(crate) owner: Principal,
    pub(crate) dir: PathBuf,
}
impl Fixture {
    pub(crate) fn new() -> Self {
        Self::with_continuation(false)
    }
    pub(crate) fn with_continuation(enabled: bool) -> Self {
        Self::with_gray_keys(enabled, None)
    }
    pub(crate) fn with_gray_keys(enabled: bool, gray_keys: Option<Vec<String>>) -> Self {
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
            tail_capability: std::sync::atomic::AtomicBool::new(true),
            frame_failure: std::sync::atomic::AtomicBool::new(false),
            frame_reads: AtomicUsize::new(0),
            billing_final: std::sync::atomic::AtomicBool::new(false),
            assist_prepare_failure: std::sync::atomic::AtomicBool::new(false),
            assist_prepare_panic: std::sync::atomic::AtomicBool::new(false),
            helper_decision: Mutex::new(None),
        });
        let mut config = RouterConfig::defaults(dir.clone());
        config.budget_billing_v2 = true;
        config.work_context_enabled = true;
        if let Some(keys) = gray_keys {
            let mut value = serde_json::to_value(&config).unwrap();
            value["work_context_key_ids"] = json!(keys);
            config = serde_json::from_value(value).unwrap();
        }
        config.continuation_enabled = enabled;
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
    pub(crate) async fn response(
        &self,
        key: &str,
        id: &str,
        body: &Value,
    ) -> axum::response::Response {
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
    pub(crate) async fn chat(&self, id: &str, body: &Value) -> Value {
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
pub(crate) fn create() -> Value {
    json!({"model":"seedance","duration":5,"resolution":"480p","ratio":"9:16","messages":[{"role":"user","content":"生成橘猫散步的视频"}]})
}
pub(crate) fn revise(result: &Value, text: &str) -> Value {
    json!({"model":"seedance","messages":[result["choices"][0]["message"],{"role":"user","content":text}]})
}
