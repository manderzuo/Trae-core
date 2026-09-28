use aiwork_core::{
    BeginRequest, BeginRequestInput, CoreStore, EncryptedWorkSnapshot, NewUser, Principal,
    UserRole, VideoWorkSnapshot, WorkAction,
};
use axum::http::HeaderMap;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use starlink_dimension_router::{
    assets::ParsedAssetUpload,
    bridge_client::BridgeClient,
    config::RouterConfig,
    key_vault::KeyVault,
    state::StarlinkRouterState,
    work_context::{self, WorkResolution},
    work_media,
};
use std::{collections::BTreeSet, fs, path::PathBuf, sync::Arc};
struct Fixture {
    state: Arc<StarlinkRouterState>,
    owner: Principal,
    other: Principal,
    dir: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("work-context-{}", rand::random::<u64>()));
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
        let a = store
            .issue_api_key(
                "admin",
                "A",
                BTreeSet::from(["admin:*".into()]),
                "bootstrap",
            )
            .unwrap();
        let b = store
            .issue_api_key(
                "admin",
                "B",
                BTreeSet::from(["admin:*".into()]),
                "bootstrap",
            )
            .unwrap();
        let mut config = RouterConfig::defaults(dir.clone());
        config.work_context_enabled = true;
        let state = StarlinkRouterState::for_test(
            store,
            BridgeClient::new("http://127.0.0.1:1", "unused"),
            config,
        );
        Self {
            state,
            owner: Principal {
                user_id: "admin".into(),
                key_id: a.id,
                scopes: BTreeSet::from(["admin:*".into()]),
            },
            other: Principal {
                user_id: "admin".into(),
                key_id: b.id,
                scopes: BTreeSet::from(["admin:*".into()]),
            },
            dir,
        }
    }
    fn version(&self, label: &str, media: Vec<String>) -> (String, String, String) {
        let work = self
            .state
            .store
            .create_video_work(&self.owner, label)
            .unwrap();
        let request = match self
            .state
            .store
            .begin_billed_request(BeginRequestInput {
                user_id: self.owner.user_id.clone(),
                api_key_id: self.owner.key_id.clone(),
                protocol: "openai".into(),
                endpoint: "videos".into(),
                model: "seedance".into(),
                idempotency_key: label.into(),
                body: json!({"prompt":label}),
            })
            .unwrap()
        {
            BeginRequest::Created(h) => h.id,
            other => panic!("{other:?}"),
        };
        let snapshot = VideoWorkSnapshot {
            effective_prompt: "橘猫散步".into(),
            duration: 5,
            resolution: "480p".into(),
            ratio: "9:16".into(),
            watermark: false,
            user_media_ids: media,
            tail_frame_media_id: None,
            parent_version_id: None,
            source_request_id: None,
            reference_mode: "user_reference".into(),
            summary: String::new(),
            dispatch_body: None,
        };
        let raw = serde_json::to_string(&snapshot).unwrap();
        let encrypted = self
            .state
            .key_vault
            .encrypt(
                &aiwork_core::work_snapshot_context(&self.owner.key_id, &work.work_id, &request),
                &raw,
            )
            .unwrap();
        let v = self
            .state
            .store
            .bind_work_version(
                &self.owner,
                &work.work_id,
                None,
                &request,
                WorkAction::Create,
                &EncryptedWorkSnapshot {
                    key_version: encrypted.key_version,
                    ciphertext: encrypted.ciphertext,
                    snapshot_sha256: format!("{:x}", Sha256::digest(raw.as_bytes())),
                },
            )
            .unwrap()
            .version;
        let handle = work_context::issue_handle(
            &self.state,
            &self.owner,
            &work.work_id,
            Some(&v.version_id),
        )
        .unwrap();
        (work.work_id, v.version_id, handle)
    }
    fn resolve(&self, body: &Value) -> Result<WorkResolution, String> {
        work_context::resolve(&self.state, &self.owner, &HeaderMap::new(), body)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}
fn body(handle: &str) -> Value {
    json!({"model":"seedance","messages":[{"role":"assistant","content":format!("视频已完成。[AIWORK_WORK:{handle}]")},{"role":"user","content":"改成夜景，其他不变"}]})
}

#[test]
fn history_marker_resumes_exact_version() {
    let f = Fixture::new();
    let (w, v, h) = f.version("one", vec![]);
    f.version("other-chat", vec![]);
    match f.resolve(&body(&h)).unwrap() {
        WorkResolution::Existing {
            work,
            base_version: Some(base),
        } => {
            assert_eq!(work.work_id, w);
            assert_eq!(base.version_id, v);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn explicit_owned_context_handle_resolves_and_conflicts_clarify() {
    let f = Fixture::new();
    let (work, version, handle) = f.version("handle-body", vec![]);
    let body = json!({"work_context":{"context_handle":handle},"messages":[{"role":"user","content":"改为夜景"}]});
    let resolved = work_context::resolve(&f.state, &f.owner, &HeaderMap::new(), &body).unwrap();
    assert!(
        matches!(resolved,WorkResolution::Existing{work:w,base_version:Some(v)} if w.work_id==work && v.version_id==version)
    );
    assert!(work_context::resolve(&f.state, &f.other, &HeaderMap::new(), &body).is_err());
    let (other, other_version, _) = f.version("other-parent", vec![]);
    let conflicting = json!({"work_context":{"work_id":other,"base_version_id":other_version,"context_handle":handle}});
    assert!(matches!(
        work_context::resolve(&f.state, &f.owner, &HeaderMap::new(), &conflicting).unwrap(),
        WorkResolution::Clarify { .. }
    ));
}
#[test]
fn same_key_new_chat_has_no_global_fallback() {
    let f = Fixture::new();
    f.version("old", vec![]);
    assert!(matches!(
        f.resolve(&json!({"messages":[{"role":"user","content":"生成新的海滩视频"}]}))
            .unwrap(),
        WorkResolution::New
    ));
}
#[test]
fn foreign_key_or_tampered_handle_is_rejected() {
    let f = Fixture::new();
    let (w, v, h) = f.version("owned", vec![]);
    assert!(work_context::resolve(&f.state, &f.other, &HeaderMap::new(), &body(&h)).is_err());
    assert!(f.resolve(&body(&"x".repeat(43))).is_err());
    assert!(work_context::resolve(
        &f.state,
        &f.other,
        &HeaderMap::new(),
        &json!({"work_context":{"work_id":w,"base_version_id":v}})
    )
    .is_err());
}
#[test]
fn conflicting_parent_markers_clarify() {
    let f = Fixture::new();
    let (w, v, h) = f.version("first", vec![]);
    let (_, _, other) = f.version("second", vec![]);
    let mut b = body(&h);
    b["messages"][0]["content"] = json!(format!("[AIWORK_WORK:{h}] [AIWORK_WORK:{other}]"));
    assert!(matches!(
        f.resolve(&b).unwrap(),
        WorkResolution::Clarify { .. }
    ));
    let mut b = body(&other);
    b["work_context"] = json!({"work_id":w,"base_version_id":v});
    assert!(matches!(
        f.resolve(&b).unwrap(),
        WorkResolution::Clarify { .. }
    ));
}
#[test]
fn history_without_marker_does_not_guess() {
    let f = Fixture::new();
    f.version("old", vec![]);
    assert!(matches!(f.resolve(&json!({"messages":[{"role":"assistant","content":"视频已生成"},{"role":"user","content":"把刚才那段改一下"}]})).unwrap(),WorkResolution::Clarify {..}));
}
#[test]
fn reference_tool_resume_preserves_work() {
    let f = Fixture::new();
    let w = f
        .state
        .store
        .create_video_work(&f.owner, "pending-upload")
        .unwrap();
    let h = work_context::issue_handle(&f.state, &f.owner, &w.work_id, None).unwrap();
    let mut reply = json!({"choices":[{"message":{"role":"assistant","content":"正在上传图片","tool_calls":[{"id":"upload1"}]}}]});
    work_context::decorate_reply(&mut reply, &h, &json!({"work_id":w.work_id}));
    let b = json!({"messages":[reply["choices"][0]["message"],{"role":"tool","tool_call_id":"upload1","content":"uploaded"},{"role":"user","content":"开始生成"}]});
    match f.resolve(&b).unwrap() {
        WorkResolution::Existing {
            work,
            base_version: None,
        } => assert_eq!(work.work_id, w.work_id),
        other => panic!("{other:?}"),
    }
}
#[test]
fn download_receipt_does_not_create_work() {
    let f = Fixture::new();
    let (w, v, h) = f.version("done", vec![]);
    let mut b = body(&h);
    b["messages"][1] = json!({"role":"tool","content":"downloaded"});
    f.resolve(&b).unwrap();
    assert_eq!(f.state.store.work_versions(&f.owner, &w).unwrap().len(), 1);
    assert!(f
        .state
        .store
        .owned_work_version(&f.owner, &v)
        .unwrap()
        .is_some());
}
#[test]
fn handle_stable_after_retry_and_restart() {
    let f = Fixture::new();
    let (w, v, h) = f.version("stable", vec![]);
    assert_eq!(
        work_context::issue_handle(&f.state, &f.owner, &w, Some(&v)).unwrap(),
        h
    );
    let store = Arc::new(CoreStore::open(&f.dir).unwrap());
    store.migrate().unwrap();
    let s = StarlinkRouterState::for_test_with_key_vault(
        store,
        BridgeClient::new("http://127.0.0.1:1", "unused"),
        f.state.config.clone(),
        KeyVault::for_test(),
    );
    assert_eq!(
        work_context::issue_handle(&s, &f.owner, &w, Some(&v)).unwrap(),
        h
    );
}
#[test]
fn work_reference_restored_before_missing_reference_check() {
    let f = Fixture::new();
    let work = f
        .state
        .store
        .create_video_work(&f.owner, "media-work")
        .unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    let m = work_media::pin(
        &f.state,
        &f.owner,
        &work.work_id,
        &ParsedAssetUpload {
            filename: "cat.png".into(),
            declared_mime: Some("image/png".into()),
            bytes: b"\x89PNG\r\n\x1a\nfixture".to_vec(),
        },
        now,
    )
    .unwrap();
    let (w, v, h) = f.version("media-work", vec![m.media_id]);
    assert_eq!(work.work_id, w);
    let mut b = body(&h);
    b["messages"][1]["content"] = json!("使用参考图把刚才的视频改成夜景");
    work_context::recover_references(
        &f.state,
        &f.owner,
        &HeaderMap::new(),
        &mut b,
        now + 31 * 60 * 1000,
    )
    .unwrap();
    assert_eq!(b["image_asset_ids"].as_array().unwrap().len(), 1);
    assert!(f
        .state
        .store
        .owned_work_version(&f.owner, &v)
        .unwrap()
        .is_some());
    assert!(!b.to_string().contains("AIWORK_WORK:"));
}

#[test]
fn verified_client_conversation_is_key_and_namespace_scoped() {
    let f = Fixture::new();
    let b = json!({"client_namespace":"trae-cn","conversation_id":"chat-123","messages":[{"role":"user","content":"把刚才的视频改成夜景"}]});
    let association = work_context::client_association(&f.owner, &HeaderMap::new(), &b)
        .unwrap()
        .unwrap();
    let (w, v, _) = f.version(&association, vec![]);
    match f.resolve(&b).unwrap() {
        WorkResolution::Existing {
            work,
            base_version: Some(base),
        } => {
            assert_eq!(work.work_id, w);
            assert_eq!(base.version_id, v);
        }
        r => panic!("{r:?}"),
    }
    assert!(matches!(
        work_context::resolve(&f.state, &f.other, &HeaderMap::new(), &b).unwrap(),
        WorkResolution::Clarify { .. }
    ));
    let mut another = b.clone();
    another["client_namespace"] = json!("dsh");
    assert!(matches!(
        f.resolve(&another).unwrap(),
        WorkResolution::Clarify { .. }
    ));
    assert!(!association.contains("chat-123"));
}

#[test]
fn sse_preserves_metadata_and_history_marker() {
    let f = Fixture::new();
    let (w, v, h) = f.version("sse", vec![]);
    let mut reply = json!({"id":"chatcmpl-example","model":"seedance","choices":[{"message":{"role":"assistant","content":"视频已生成"},"finish_reason":"stop"}]});
    work_context::decorate_reply(&mut reply, &h, &json!({"work_id":w,"base_version_id":v}));
    let bytes = starlink_dimension_router::video_delivery::sse_completion(&reply);
    let line = std::str::from_utf8(&bytes)
        .unwrap()
        .lines()
        .find(|s| s.starts_with("data: {"))
        .unwrap();
    let chunk: Value = serde_json::from_str(&line[6..]).unwrap();
    assert_eq!(chunk["work_context"]["work_id"], w);
    let b = json!({"messages":[{"role":"assistant","content":chunk["choices"][0]["delta"]["content"]},{"role":"user","content":"动作慢一些"}]});
    assert!(
        matches!(f.resolve(&b).unwrap(),WorkResolution::Existing{base_version:Some(base),..} if base.version_id==v)
    );
}
