//! Key-bound explicit/history context. Never a Key-wide "last video" lookup.
use crate::state::StarlinkRouterState;
use aiwork_core::{Principal, VideoWork, VideoWorkSnapshot, VideoWorkVersion};
use axum::http::HeaderMap;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::{rngs::OsRng, RngCore};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
const PREFIX: &str = "[AIWORK_WORK:";

#[derive(Debug, Clone)]
pub enum WorkResolution {
    New,
    Existing {
        work: VideoWork,
        base_version: Option<VideoWorkVersion>,
    },
    Clarify {
        text: String,
    },
}
fn clarify() -> WorkResolution {
    WorkResolution::Clarify {
        text: "请明确要修改或续写哪个视频版本；本次未提交视频。".into(),
    }
}
fn unavailable() -> String {
    "work_context_unavailable".into()
}
fn text(message: &Value) -> String {
    match &message["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|p| p["type"] == "text")
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}
fn latest_user(body: &Value) -> String {
    body["messages"]
        .as_array()
        .and_then(|m| m.iter().rev().find(|m| m["role"] == "user"))
        .map(text)
        .unwrap_or_default()
}
fn needs_parent(body: &Value) -> bool {
    matches!(
        body["action"].as_str(),
        Some("revise" | "continue" | "status" | "download")
    ) || [
        "刚才",
        "上一段",
        "上一版",
        "上一个视频",
        "继续生成",
        "接着生成",
        "不满意",
    ]
    .iter()
    .any(|w| latest_user(body).contains(w))
}
fn existing(
    state: &StarlinkRouterState,
    p: &Principal,
    work: &str,
    version: Option<&str>,
) -> Result<WorkResolution, String> {
    let work = state
        .store
        .owned_video_work(p, work)
        .map_err(|_| unavailable())?
        .ok_or_else(unavailable)?;
    let base_version = version
        .map(|id| {
            state
                .store
                .owned_work_version(p, id)
                .map_err(|_| unavailable())?
                .filter(|v| v.work_id == work.work_id)
                .ok_or_else(unavailable)
        })
        .transpose()?;
    Ok(WorkResolution::Existing { work, base_version })
}
fn from_handle(
    state: &StarlinkRouterState,
    p: &Principal,
    handle: &str,
) -> Result<WorkResolution, String> {
    if handle.len() != 43
        || URL_SAFE_NO_PAD
            .decode(handle)
            .ok()
            .map_or(true, |b| b.len() != 32)
    {
        return Err(unavailable());
    }
    let hash = format!("{:x}", Sha256::digest(handle.as_bytes()));
    let h = state
        .store
        .owned_work_handle(p, &hash)
        .map_err(|_| unavailable())?
        .ok_or_else(unavailable)?;
    let raw = state
        .key_vault
        .decrypt(
            &aiwork_core::work_handle_context(&p.key_id, &h.work_id, h.version_id.as_deref()),
            h.key_version,
            &h.encrypted_handle,
        )
        .map_err(|_| unavailable())?;
    if raw != handle {
        return Err(unavailable());
    }
    existing(state, p, &h.work_id, h.version_id.as_deref())
}
fn identity(r: &WorkResolution) -> Option<(&str, Option<&str>)> {
    match r {
        WorkResolution::Existing { work, base_version } => Some((
            &work.work_id,
            base_version.as_ref().map(|v| v.version_id.as_str()),
        )),
        _ => None,
    }
}
fn recent_handles(body: &Value) -> Result<Vec<String>, String> {
    let Some(messages) = body["messages"].as_array() else {
        return Ok(Vec::new());
    };
    for m in messages.iter().rev().filter(|m| m["role"] == "assistant") {
        let s = text(m);
        let mut tail = s.as_str();
        let mut found = Vec::new();
        while let Some((_, rest)) = tail.split_once(PREFIX) {
            let (token, next) = rest.split_once(']').ok_or_else(unavailable)?;
            if token.len() > 64 || found.len() >= 8 {
                return Err(unavailable());
            }
            if !found.iter().any(|s| s == token) {
                found.push(token.into());
            }
            tail = next;
        }
        if !found.is_empty() {
            return Ok(found);
        }
    }
    Ok(Vec::new())
}

/// Client identifiers only locate a server-established association. They are
/// never an authorization token, nor a Key-wide most-recent-version pointer.
pub fn client_association(
    p: &Principal,
    headers: &HeaderMap,
    body: &Value,
) -> Result<Option<String>, String> {
    let namespace = body["client_namespace"].as_str().or_else(|| {
        headers
            .get("x-client-namespace")
            .and_then(|v| v.to_str().ok())
    });
    let id = body["conversation_id"]
        .as_str()
        .or_else(|| {
            body.pointer("/work_context/conversation_id")
                .and_then(Value::as_str)
        })
        .or_else(|| {
            headers
                .get("x-conversation-id")
                .and_then(|v| v.to_str().ok())
        });
    let (Some(namespace), Some(id)) = (namespace, id) else {
        return Ok(None);
    };
    if namespace.is_empty()
        || namespace.len() > 128
        || id.is_empty()
        || id.len() > 256
        || namespace.chars().any(char::is_control)
        || id.chars().any(char::is_control)
    {
        return Err(unavailable());
    }
    Ok(Some(format!(
        "client-v1:{:x}",
        Sha256::digest(
            serde_json::to_vec(&json!([p.key_id, namespace, id])).map_err(|_| unavailable())?
        )
    )))
}

pub fn resolve(
    state: &StarlinkRouterState,
    p: &Principal,
    headers: &HeaderMap,
    body: &Value,
) -> Result<WorkResolution, String> {
    if !state.config.work_context_enabled {
        return Ok(WorkResolution::New);
    }
    // Explicit independent creation must never inherit references or a marker.
    if body["action"] == "create" {
        return Ok(WorkResolution::New);
    }
    let explicit = match body.get("work_context").filter(|v| !v.is_null()) {
        Some(c) => {
            if !c.is_object() {
                return Err(unavailable());
            }
            let handle = c
                .get("context_handle")
                .filter(|v| !v.is_null())
                .map(|v| {
                    v.as_str()
                        .ok_or_else(unavailable)
                        .and_then(|h| from_handle(state, p, h))
                })
                .transpose()?;
            let version = match c.get("base_version_id").filter(|v| !v.is_null()) {
                Some(v) => Some(
                    v.as_str()
                        .filter(|s| !s.is_empty() && s.len() <= 128)
                        .ok_or_else(unavailable)?,
                ),
                None => None,
            };
            if let Some(id) = c["work_id"]
                .as_str()
                .filter(|s| !s.is_empty() && s.len() <= 128)
            {
                let resolved = existing(state, p, id, version)?;
                if handle
                    .as_ref()
                    .is_some_and(|h| identity(h) != identity(&resolved))
                {
                    return Ok(clarify());
                }
                Some(resolved)
            } else {
                if c.get("work_id").is_some_and(|v| !v.is_null()) || version.is_some() {
                    return Err(unavailable());
                }
                Some(handle.ok_or_else(unavailable)?)
            }
        }
        None => None,
    };
    let handles = recent_handles(body)?;
    if handles.len() > 1 {
        for h in &handles {
            from_handle(state, p, h)?;
        }
        return Ok(clarify());
    }
    let history = handles
        .first()
        .map(|h| from_handle(state, p, h))
        .transpose()?;
    if let (Some(explicit), Some(history)) = (&explicit, &history) {
        if identity(explicit) != identity(history) {
            return Ok(clarify());
        }
    }
    let mapped = client_association(p, headers, body)?
        .map(|association| {
            state
                .store
                .owned_work_for_conversation(p, &association)
                .map_err(|_| unavailable())
        })
        .transpose()?
        .flatten();
    if let Some(work) = mapped {
        if explicit.is_none() && history.is_none() {
            let versions = state
                .store
                .work_versions(p, &work.work_id)
                .map_err(|_| unavailable())?;
            let leaves: Vec<_> = versions
                .iter()
                .filter(|v| {
                    !versions
                        .iter()
                        .any(|c| c.parent_version_id.as_deref() == Some(v.version_id.as_str()))
                })
                .collect();
            if leaves.len() > 1 {
                return Ok(clarify());
            }
            return Ok(WorkResolution::Existing {
                work,
                base_version: leaves.first().map(|v| (*v).clone()),
            });
        }
    }
    if let Some(r) = explicit.or(history) {
        return Ok(r);
    }
    Ok(if needs_parent(body) {
        clarify()
    } else {
        WorkResolution::New
    })
}

pub fn decorate_owned_request(
    state: &StarlinkRouterState,
    p: &Principal,
    request: &str,
    reply: &mut Value,
) -> Result<(), String> {
    if !state.config.work_context_enabled {
        return Ok(());
    }
    if let Some(v) = state
        .store
        .work_version_for_request(p, request)
        .map_err(|_| unavailable())?
    {
        let h = issue_handle(state, p, &v.work_id, Some(&v.version_id))?;
        let snapshot = read_snapshot(state, p, &v)?;
        if snapshot.reference_mode == "tail_reference" {
            if let Some(choices) = reply["choices"].as_array_mut() {
                for choice in choices {
                    if let Some(text) = choice["message"]["content"].as_str().map(str::to_owned) {
                        let notice="本段使用上一版本尾帧作近似参考，生成独立新片段；不是原生视频延长或严格首帧锁定。";
                        if !text.contains(notice) {
                            choice["message"]["content"] = json!(format!("{text}\n\n{notice}"));
                        }
                    }
                }
            }
        }
        decorate_reply(
            reply,
            &h,
            &json!({"work_id":v.work_id,"base_version_id":v.version_id,"request_id":request,"parent_version_id":v.parent_version_id,"reference_mode":snapshot.reference_mode}),
        );
    }
    Ok(())
}

pub fn issue_handle(
    state: &StarlinkRouterState,
    p: &Principal,
    work: &str,
    version: Option<&str>,
) -> Result<String, String> {
    if let Some(h) = state
        .store
        .work_handle_for_version(p, work, version)
        .map_err(|_| unavailable())?
    {
        let raw = state
            .key_vault
            .decrypt(
                &aiwork_core::work_handle_context(&p.key_id, work, version),
                h.key_version,
                &h.encrypted_handle,
            )
            .map_err(|_| unavailable())?;
        if format!("{:x}", Sha256::digest(raw.as_bytes())) != h.context_handle_sha256 {
            return Err(unavailable());
        }
        return Ok(raw);
    }
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let raw = URL_SAFE_NO_PAD.encode(bytes);
    let hash = format!("{:x}", Sha256::digest(raw.as_bytes()));
    let encrypted = state
        .key_vault
        .encrypt(
            &aiwork_core::work_handle_context(&p.key_id, work, version),
            &raw,
        )
        .map_err(|_| unavailable())?;
    let saved = state
        .store
        .save_work_handle(
            p,
            work,
            version,
            &hash,
            encrypted.key_version,
            &encrypted.ciphertext,
        )
        .map_err(|_| unavailable())?;
    // Concurrent issuers return the winning stable handle, not their candidate.
    state
        .key_vault
        .decrypt(
            &aiwork_core::work_handle_context(&p.key_id, work, version),
            saved.key_version,
            &saved.encrypted_handle,
        )
        .map_err(|_| unavailable())
}
pub fn decorate_reply(reply: &mut Value, handle: &str, context: &Value) {
    reply["work_context"] = context.clone();
    if let Some(choices) = reply["choices"].as_array_mut() {
        for choice in choices {
            if let Some(m) = choice["message"].as_object_mut() {
                let marker = format!("{PREFIX}{handle}]");
                let content = m.entry("content").or_insert(json!(""));
                match content {
                    Value::String(s) => {
                        if !s.contains(&marker) {
                            if !s.is_empty() {
                                s.push_str("\n\n");
                            }
                            s.push_str(&marker);
                        }
                    }
                    Value::Null => *content = json!(marker),
                    _ => {}
                }
            }
        }
    }
}
pub fn strip_markers(s: &str) -> String {
    let mut out = String::new();
    let mut tail = s;
    while let Some((head, rest)) = tail.split_once(PREFIX) {
        out.push_str(head);
        if let Some((_, next)) = rest.split_once(']') {
            tail = next;
        } else {
            tail = "";
            break;
        }
    }
    out.push_str(tail);
    out
}
pub fn strip_body_markers(body: &mut Value) {
    if let Some(messages) = body["messages"].as_array_mut() {
        for m in messages {
            match &mut m["content"] {
                Value::String(s) => *s = strip_markers(s),
                Value::Array(parts) => {
                    for p in parts {
                        if let Some(s) = p["text"].as_str() {
                            p["text"] = json!(strip_markers(s));
                        }
                    }
                }
                _ => {}
            }
        }
    }
}
pub fn read_snapshot(
    state: &StarlinkRouterState,
    p: &Principal,
    v: &VideoWorkVersion,
) -> Result<VideoWorkSnapshot, String> {
    let current = state
        .store
        .owned_work_version(p, &v.version_id)
        .map_err(|_| unavailable())?
        .ok_or_else(unavailable)?;
    let sealed = &current.sealed_snapshot;
    let raw = zeroize::Zeroizing::new(
        state
            .key_vault
            .decrypt(
                &aiwork_core::work_snapshot_context(&p.key_id, &v.work_id, &v.operation_request_id),
                sealed.key_version,
                &sealed.ciphertext,
            )
            .map_err(|_| "work_snapshot_unavailable")?,
    );
    if format!("{:x}", Sha256::digest(raw.as_bytes())) != sealed.snapshot_sha256 {
        return Err("work_snapshot_invalid".into());
    }
    serde_json::from_str(&raw).map_err(|_| "work_snapshot_invalid".into())
}
pub fn recover_references(
    state: &StarlinkRouterState,
    p: &Principal,
    headers: &HeaderMap,
    body: &mut Value,
    now: i64,
) -> Result<WorkResolution, String> {
    let resolution = resolve(state, p, headers, body)?;
    if let WorkResolution::Existing { work, base_version } = &resolution {
        body["work_context"] = json!({"work_id":work.work_id,"base_version_id":base_version.as_ref().map(|v|&v.version_id)});
        let latest = body["messages"]
            .as_array()
            .and_then(|m| m.iter().rev().find(|m| m["role"] == "user"));
        let fresh_inline = latest
            .and_then(|m| m["content"].as_array())
            .is_some_and(|ps| ps.iter().any(|p| p["type"] == "image_url"));
        let fresh_refs = [
            "image_asset_ids",
            "video_asset_ids",
            "image_urls",
            "video_urls",
        ]
        .iter()
        .any(|f| body[*f].as_array().is_some_and(|a| !a.is_empty()));
        let clear = ["不用参考图", "取消参考图", "去掉参考图", "不使用参考图"]
            .iter()
            .any(|s| latest_user(body).contains(s));
        if !fresh_inline && !fresh_refs && !clear {
            if let Some(base) = base_version {
                let snapshot = read_snapshot(state, p, base)?;
                let mut media = snapshot.user_media_ids;
                if let Some(tail) = snapshot.tail_frame_media_id {
                    if !media.contains(&tail) {
                        media.push(tail);
                    }
                }
                if media.len() > 10 {
                    return Err("reference_image_limit".into());
                }
                let mut images = Vec::new();
                let mut videos = Vec::new();
                for id in media {
                    let m = state
                        .store
                        .owned_work_media(p, &id)
                        .map_err(|_| "work_media_unavailable")?
                        .filter(|m| m.work_id == work.work_id)
                        .ok_or("work_media_unavailable")?;
                    let asset = crate::work_media::materialize(state, p, &m, now)?;
                    if asset.mime_type.starts_with("image/") {
                        images.push(asset.id);
                    } else {
                        videos.push(asset.id);
                    }
                }
                if !images.is_empty() {
                    body["image_asset_ids"] = json!(images);
                }
                if !videos.is_empty() {
                    body["video_asset_ids"] = json!(videos);
                }
            }
        }
    }
    strip_body_markers(body);
    Ok(resolution)
}
