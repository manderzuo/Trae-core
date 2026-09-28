//! Continuation never resubmits its parent, nor substitutes missing frame input.
use crate::{
    state::StarlinkRouterState,
    work_context,
    work_planner::{self, WorkDecision, WorkIntent},
};
use aiwork_core::{
    EncryptedWorkSnapshot, Principal, VideoWorkSnapshot, VideoWorkVersion, WorkMediaRef,
    WorkVersionState,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex, OnceLock};
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ContinuationMode {
    Auto,
    TailReference,
    NativeFirstFrame,
    NativeVideoExtend,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrameEvidence {
    request_id: String,
    budget_id: String,
    task_ref: String,
    source_sha256: String,
    frame_sha256: String,
    width: u32,
    height: u32,
    timestamp_ms: i64,
}
static OWNERS: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
struct Owner(String);
impl Drop for Owner {
    fn drop(&mut self) {
        OWNERS
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
    }
}
fn try_acquire(state: &StarlinkRouterState, v: &VideoWorkVersion) -> Result<Option<Owner>, String> {
    let key = format!("{}:{}", state.config.data_dir.display(), v.version_id);
    let mut set = OWNERS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if set.contains(&key) {
        return Ok(None);
    }
    if set.len() >= 10 {
        return Err("frame_extractor_busy".into());
    }
    set.insert(key.clone());
    Ok(Some(Owner(key)))
}
async fn acquire(state: &StarlinkRouterState, v: &VideoWorkVersion) -> Result<Owner, String> {
    let started = std::time::Instant::now();
    loop {
        if let Some(owner) = try_acquire(state, v)? {
            return Ok(owner);
        }
        if started.elapsed() > std::time::Duration::from_secs(65) {
            return Err("frame_extractor_busy".into());
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}
fn saved(
    state: &StarlinkRouterState,
    p: &Principal,
    v: &VideoWorkVersion,
    step: &aiwork_core::BudgetStepView,
) -> Result<Option<WorkMediaRef>, String> {
    let Some(id) = v.tail_frame_media_id.as_deref() else {
        return Ok(None);
    };
    let media = state
        .store
        .owned_work_media(p, id)
        .map_err(|_| "work_media_unavailable")?
        .filter(|m| {
            m.work_id == v.work_id
                && m.kind == "tail_frame"
                && m.expires_at_ms > chrono::Utc::now().timestamp_millis()
        })
        .ok_or("work_media_unavailable")?;
    let sealed = v.sealed_frame.as_ref().ok_or("frame_identity_invalid")?;
    let raw = zeroize::Zeroizing::new(
        state
            .key_vault
            .decrypt(
                &aiwork_core::work_frame_context(&p.key_id, &v.work_id, &v.version_id),
                sealed.key_version,
                &sealed.ciphertext,
            )
            .map_err(|_| "frame_identity_invalid")?,
    );
    if format!("{:x}", Sha256::digest(raw.as_bytes())) != sealed.snapshot_sha256 {
        return Err("frame_identity_invalid".into());
    }
    let proof: FrameEvidence = serde_json::from_str(&raw).map_err(|_| "frame_identity_invalid")?;
    if proof.request_id != v.operation_request_id
        || proof.budget_id != step.budget_id
        || Some(&proof.task_ref) != step.task_ref.as_ref()
        || proof.frame_sha256 != media.content_sha256
    {
        return Err("frame_identity_invalid".into());
    }
    crate::work_media::materialize(state, p, &media, chrono::Utc::now().timestamp_millis())?;
    Ok(Some(media))
}
pub async fn warm_tail_frame(
    state: Arc<StarlinkRouterState>,
    p: Principal,
    version: VideoWorkVersion,
) -> Result<WorkMediaRef, String> {
    if !state.config.work_context_enabled || !state.config.continuation_enabled {
        return Err("continuation_disabled".into());
    }
    aiwork_core::require_scope(&p, "videos:read").map_err(|_| "insufficient_scope")?;
    let owner = acquire(&state, &version).await?;
    warm_owned(state, p, version, owner).await
}
async fn warm_owned(
    state: Arc<StarlinkRouterState>,
    p: Principal,
    version: VideoWorkVersion,
    owner: Owner,
) -> Result<WorkMediaRef, String> {
    let s = state.clone();
    tokio::task::spawn_blocking(move || {
        // Keep ownership inside the blocking worker even if its async caller is cancelled.
        let _owner = owner;
        let mut v = s
            .store
            .owned_work_version(&p, &version.version_id)
            .map_err(|_| "work_parent_unavailable")?
            .filter(|v| {
                v.work_id == version.work_id
                    && v.operation_request_id == version.operation_request_id
            })
            .ok_or("work_parent_unavailable")?;
        let step = crate::budget_flow::owned_video_step(&s, &p, &v.operation_request_id)?
            .filter(|step| step.execution_state == aiwork_core::BudgetExecutionState::Succeeded)
            .ok_or("frame_result_not_ready")?;
        if v.state != WorkVersionState::Completed {
            if v.state == WorkVersionState::Preparing {
                s.store
                    .set_work_version_state(&p, &v.operation_request_id, WorkVersionState::Running)
                    .map_err(|_| "work_parent_unavailable")?;
            }
            v = s
                .store
                .set_work_version_state(&p, &v.operation_request_id, WorkVersionState::Completed)
                .map_err(|_| "work_parent_unavailable")?
                .version;
            s.store
                .release_work_media_leases(&p, &v.operation_request_id)
                .map_err(|_| "work_media_unavailable")?;
        }
        if let Some(media) = saved(&s, &p, &v, &step)? {
            return Ok(media);
        }
        s.store
            .set_work_frame(&p, &v.version_id, "running", None, None, None)
            .map_err(|_| "frame_identity_invalid")?;
        let result = (|| {
            let frame = s.bridge_client().last_frame(&step)?;
            let proof = FrameEvidence {
                request_id: v.operation_request_id.clone(),
                budget_id: step.budget_id.clone(),
                task_ref: step.task_ref.clone().ok_or("frame_result_not_ready")?,
                source_sha256: frame.source_sha256,
                frame_sha256: frame.frame_sha256,
                width: frame.width,
                height: frame.height,
                timestamp_ms: frame.timestamp_ms,
            };
            let asset = crate::assets::ParsedAssetUpload {
                filename: "tail-frame.png".into(),
                declared_mime: Some("image/png".into()),
                bytes: frame.bytes,
            };
            let media = crate::work_media::pin_kind(
                &s,
                &p,
                &v.work_id,
                &asset,
                chrono::Utc::now().timestamp_millis(),
                Some("tail_frame"),
            )?;
            let raw = zeroize::Zeroizing::new(
                serde_json::to_string(&proof).map_err(|_| "frame_identity_invalid")?,
            );
            let sealed = s
                .key_vault
                .encrypt(
                    &aiwork_core::work_frame_context(&p.key_id, &v.work_id, &v.version_id),
                    &raw,
                )
                .map_err(|_| "frame_identity_invalid")?;
            s.store
                .set_work_frame(
                    &p,
                    &v.version_id,
                    "ready",
                    Some(&media.media_id),
                    Some(&EncryptedWorkSnapshot {
                        key_version: sealed.key_version,
                        ciphertext: sealed.ciphertext,
                        snapshot_sha256: format!("{:x}", Sha256::digest(raw.as_bytes())),
                    }),
                    None,
                )
                .map_err(|_| "frame_identity_invalid")?;
            Ok(media)
        })();
        if result.is_err() {
            let _ = s.store.set_work_frame(
                &p,
                &v.version_id,
                "failed",
                None,
                None,
                Some("frame_extraction_unavailable"),
            );
        }
        result
    })
    .await
    .map_err(|_| "frame_extraction_unavailable")?
}
pub async fn prepare_continuation(
    state: Arc<StarlinkRouterState>,
    p: Principal,
    base: VideoWorkVersion,
    input: Value,
) -> Result<VideoWorkSnapshot, String> {
    if !state.config.continuation_enabled {
        return Err("continuation_disabled".into());
    }
    let requested = input
        .get("continuation_mode")
        .filter(|v| !v.is_null())
        .map(|v| serde_json::from_value::<ContinuationMode>(v.clone()))
        .transpose()
        .map_err(|_| "continuation_mode_unsupported")?
        .unwrap_or(ContinuationMode::Auto);
    let s = state.clone();
    let caps = tokio::task::spawn_blocking(move || {
        s.bridge_client()
            .json_request("GET", "/internal/bridge/v2/video-capabilities", &[], None)
    })
    .await
    .map_err(|_| "continuation_capabilities_unavailable")??;
    // Native fields are not yet proven for this provider. Never borrow another
    // provider's first_frame/video_extend mapping, even if a bit is mis-set.
    let verified = caps["contract_version"] == "tail-reference-v1"
        && caps["evidence_digest"]
            .as_str()
            .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()));
    if !(verified && caps["tail_reference"] == true
        || pending_tail_allowed(
            &state.config,
            &p.key_id,
            &caps,
            requested,
            chrono::Utc::now().timestamp_millis(),
        ))
        || matches!(
            requested,
            ContinuationMode::NativeFirstFrame | ContinuationMode::NativeVideoExtend
        )
    {
        return Err("continuation_mode_unsupported".into());
    }
    let snapshot = work_context::read_snapshot(&state, &p, &base)?;
    let decision = WorkDecision {
        action: WorkIntent::Continue,
        effective_prompt: input["prompt"].as_str().map(str::to_owned),
        spec_patch: json!({}),
        reference_policy: "inherit".into(),
        clarification: None,
    };
    let mut next = work_planner::merge_snapshot(Some(&snapshot), &decision, &input)?;
    let frame = warm_tail_frame(state, p, base.clone()).await?;
    next.tail_frame_media_id = Some(frame.media_id);
    next.reference_mode = "tail_reference".into();
    next.parent_version_id = Some(base.version_id);
    Ok(next)
}
fn pending_tail_allowed(
    config: &crate::config::RouterConfig,
    key: &str,
    caps: &Value,
    mode: ContinuationMode,
    now: i64,
) -> bool {
    // No fabricated verified evidence: this exception expires and applies to one of
    // a small explicit allowlist, ordinary image-reference mapping only, never native.
    matches!(
        mode,
        ContinuationMode::Auto | ContinuationMode::TailReference
    ) && caps["contract_version"] == "not_verified"
        && caps["tail_reference"] == false
        && config.continuation_test_key_ids.len() <= 8
        && config
            .continuation_test_key_ids
            .iter()
            .any(|id| id == key && id.starts_with("key_"))
        && config.continuation_test_expires_at_ms > now
        && config.continuation_test_expires_at_ms <= now.saturating_add(24 * 3600 * 1000)
}
pub(crate) fn schedule(
    state: &Arc<StarlinkRouterState>,
    p: &Principal,
    version: &VideoWorkVersion,
) {
    if !state.config.work_context_enabled
        || !state.config.continuation_enabled
        || version.frame_state == "ready"
        || version.frame_state == "failed"
        || aiwork_core::require_scope(p, "videos:read").is_err()
    {
        return;
    }
    // Background ticks skip an already owned version; only an explicit continuation waits.
    let Ok(Some(owner)) = try_acquire(state, version) else {
        return;
    };
    let state = Arc::downgrade(state);
    let p = p.clone();
    let v = version.clone();
    tokio::spawn(async move {
        if let Some(state) = state.upgrade() {
            let _ = warm_owned(state, p, v, owner).await;
        }
    });
}
pub(crate) fn spawn(state: &Arc<StarlinkRouterState>) {
    if !state.config.work_context_enabled || !state.config.continuation_enabled {
        return;
    }
    let weak = Arc::downgrade(state);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(10));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let Some(s) = weak.upgrade() else { break };
            let store = s.store.clone();
            let rows =
                tokio::task::spawn_blocking(move || store.work_versions_needing_frames(8)).await;
            if let Ok(Ok(rows)) = rows {
                for (p, v) in rows {
                    schedule(&s, &p, &v);
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn pending_tail_exception_is_key_bound_expiring_and_never_native() {
        let now = chrono::Utc::now().timestamp_millis();
        let mut config = crate::config::RouterConfig::defaults("unused".into());
        config.continuation_test_key_ids = vec!["key_fixture".into()];
        config.continuation_test_expires_at_ms = now + 60000;
        let caps = serde_json::json!({"contract_version":"not_verified","tail_reference":false});
        assert!(super::pending_tail_allowed(
            &config,
            "key_fixture",
            &caps,
            super::ContinuationMode::Auto,
            now
        ));
        assert!(!super::pending_tail_allowed(
            &config,
            "key_other",
            &caps,
            super::ContinuationMode::Auto,
            now
        ));
        assert!(!super::pending_tail_allowed(
            &config,
            "key_fixture",
            &caps,
            super::ContinuationMode::NativeFirstFrame,
            now
        ));
        assert!(!super::pending_tail_allowed(
            &config,
            "key_fixture",
            &caps,
            super::ContinuationMode::Auto,
            now + 60001
        ));
        assert!(!super::pending_tail_allowed(
            &config,
            "key_fixture",
            &serde_json::json!({"contract_version":"foreign_contract"}),
            super::ContinuationMode::Auto,
            now
        ));
    }
}
