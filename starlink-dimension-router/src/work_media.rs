//! Owned durable encrypted media, separate from 30-minute public upload assets.
use crate::{
    assets::{self, ParsedAssetUpload},
    state::StarlinkRouterState,
};
use aiwork_core::{CoreAsset, Principal, WorkMediaRef};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::PathBuf,
};

#[derive(Serialize, Deserialize)]
struct StorageRef {
    file: String,
    filename: String,
    mime: String,
}
fn aad(m: &WorkMediaRef, kind: &str) -> String {
    format!(
        "work-media:{}:{}:{}:{kind}",
        m.owner_key_id, m.work_id, m.media_id
    )
}
fn root(state: &StarlinkRouterState) -> PathBuf {
    state.config.data_dir.join("data/work-media")
}
fn path(state: &StarlinkRouterState, name: &str) -> Result<PathBuf, String> {
    if name.len() > 160
        || !name.ends_with(".bin")
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        || name.contains("..")
    {
        return Err("work_media_invalid_storage_ref".into());
    }
    let dir = root(state);
    fs::create_dir_all(&dir).map_err(|_| "work_media_storage_unavailable")?;
    let meta = fs::symlink_metadata(&dir).map_err(|_| "work_media_storage_unavailable")?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err("work_media_invalid_directory".into());
    }
    let result = dir.join(name);
    if let Ok(meta) = fs::symlink_metadata(&result) {
        if !meta.is_file() || meta.file_type().is_symlink() {
            return Err("work_media_invalid_file".into());
        }
    }
    Ok(result)
}
fn reference(state: &StarlinkRouterState, m: &WorkMediaRef) -> Result<StorageRef, String> {
    let raw = state
        .key_vault
        .decrypt(&aad(m, "ref"), m.key_version, &m.encrypted_storage_ref)
        .map_err(|_| "work_media_decryption_failed")?;
    serde_json::from_str(&raw).map_err(|_| "work_media_invalid_storage_ref".into())
}
fn read_cipher(
    state: &StarlinkRouterState,
    m: &WorkMediaRef,
    r: &StorageRef,
) -> Result<Vec<u8>, String> {
    let file = fs::File::open(path(state, &r.file)?).map_err(|_| "work_media_missing")?;
    if file.metadata().map_err(|_| "work_media_missing")?.len() != m.size_bytes as u64
        || m.size_bytes > assets::MAX_ASSET_BYTES as i64 + 28
    {
        return Err("work_media_size_mismatch".into());
    }
    let mut bytes = Vec::new();
    file.take(assets::MAX_ASSET_BYTES as u64 + 29)
        .read_to_end(&mut bytes)
        .map_err(|_| "work_media_missing")?;
    Ok(bytes)
}
fn read_plain(
    state: &StarlinkRouterState,
    m: &WorkMediaRef,
    r: &StorageRef,
) -> Result<Vec<u8>, String> {
    let bytes = state
        .key_vault
        .decrypt_bytes(
            &aad(m, "content"),
            m.key_version,
            &read_cipher(state, m, r)?,
        )
        .map_err(|_| "work_media_decryption_failed")?;
    if format!("{:x}", Sha256::digest(&bytes)) != m.content_sha256
        || assets::detect_format(&bytes).map(|p| p.0) != Some(r.mime.as_str())
    {
        return Err("work_media_digest_mismatch".into());
    }
    Ok(bytes)
}
fn publish(state: &StarlinkRouterState, name: &str, bytes: &[u8]) -> Result<(), String> {
    let final_path = path(state, name)?;
    let tmp = final_path.with_extension("partial");
    let result = (|| {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|_| "work_media_publish_failed")?;
        f.write_all(bytes)
            .map_err(|_| "work_media_publish_failed")?;
        f.sync_all().map_err(|_| "work_media_publish_failed")?;
        fs::rename(&tmp, &final_path).map_err(|_| "work_media_publish_failed")
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result.map_err(str::to_owned)
}
pub fn pin(
    state: &StarlinkRouterState,
    p: &Principal,
    work: &str,
    asset: &ParsedAssetUpload,
    now: i64,
) -> Result<WorkMediaRef, String> {
    pin_kind(state, p, work, asset, now, None)
}
pub(crate) fn pin_kind(
    state: &StarlinkRouterState,
    p: &Principal,
    work: &str,
    asset: &ParsedAssetUpload,
    now: i64,
    kind: Option<&str>,
) -> Result<WorkMediaRef, String> {
    let _guard = state
        .work_media_lock
        .lock()
        .map_err(|_| "work_media_lock_unavailable")?;
    assets::validate_filename(&asset.filename).map_err(|_| "work_media_invalid_filename")?;
    if asset.bytes.is_empty() || asset.bytes.len() > assets::MAX_ASSET_BYTES || now <= 0 {
        return Err("work_media_invalid_size".into());
    }
    let (mime, _) = assets::detect_format(&asset.bytes).ok_or("work_media_invalid_format")?;
    if asset.declared_mime.as_deref().map_or(false, |v| v != mime) {
        return Err("work_media_invalid_mime".into());
    }
    let digest = format!("{:x}", Sha256::digest(&asset.bytes));
    let kind = kind.unwrap_or(if mime.starts_with("image/") {
        "image"
    } else {
        "video"
    });
    if kind == "tail_frame" && mime != "image/png" {
        return Err("work_media_invalid_tail_frame".into());
    }
    let existing = state
        .store
        .work_media_records()
        .map_err(|_| "work_media_store_unavailable")?
        .into_iter()
        .find(|m| {
            m.work_id == work
                && m.owner_key_id == p.key_id
                && m.kind == kind
                && m.content_sha256 == digest
        });
    if let Some(ref old) = existing {
        if old.state == "active" && old.expires_at_ms > now {
            let current = state
                .store
                .owned_work_media(p, &old.media_id)
                .map_err(|_| "work_media_unavailable")?
                .ok_or("work_media_unavailable")?;
            read_plain(state, &current, &reference(state, &current)?)?;
            return Ok(current);
        }
        if old.state != "deleted" {
            return Err("work_media_expired_or_unavailable".into());
        }
    }
    let id = existing
        .map(|m| m.media_id)
        .unwrap_or_else(|| format!("media_{:032x}", rand::random::<u128>()));
    let mut m = WorkMediaRef {
        media_id: id,
        work_id: work.into(),
        owner_key_id: p.key_id.clone(),
        kind: kind.into(),
        content_sha256: digest,
        encrypted_storage_ref: Vec::new(),
        key_version: state.key_vault.active_version(),
        size_bytes: 0,
        expires_at_ms: now
            .checked_add(state.config.work_media_retention_ms)
            .ok_or("work_media_invalid_retention")?,
        state: "preparing".into(),
    };
    let cipher = state
        .key_vault
        .encrypt_bytes(&aad(&m, "content"), &asset.bytes)
        .map_err(|_| "work_media_encryption_failed")?;
    m.size_bytes = cipher.ciphertext.len() as i64;
    let r = StorageRef {
        file: format!("{}-{:032x}.bin", m.media_id, rand::random::<u128>()),
        filename: asset.filename.clone(),
        mime: mime.into(),
    };
    m.encrypted_storage_ref = state
        .key_vault
        .encrypt(
            &aad(&m, "ref"),
            &serde_json::to_string(&r).map_err(|_| "work_media_invalid_metadata")?,
        )
        .map_err(|_| "work_media_encryption_failed")?
        .ciphertext;
    let reserved = state
        .store
        .reserve_work_media(
            p,
            &m,
            state.config.work_media_key_limit_bytes,
            state.config.work_media_global_limit_bytes,
        )
        .map_err(|e| e.to_string())?;
    if reserved.media_id != m.media_id {
        return Ok(reserved);
    }
    if let Err(e) = publish(state, &r.file, &cipher.ciphertext) {
        let _ = state
            .store
            .set_preparing_work_media_ready(&m.media_id, false);
        return Err(e);
    }
    state
        .store
        .activate_work_media(p, &m.media_id)
        .map_err(|_| "work_media_store_unavailable")?;
    m.state = "active".into();
    Ok(m)
}
pub fn materialize(
    state: &StarlinkRouterState,
    p: &Principal,
    media: &WorkMediaRef,
    now: i64,
) -> Result<CoreAsset, String> {
    let _guard = state
        .work_media_lock
        .lock()
        .map_err(|_| "work_media_lock_unavailable")?;
    let m = state
        .store
        .owned_work_media(p, &media.media_id)
        .map_err(|_| "work_media_unavailable")?
        .ok_or("work_media_unavailable")?;
    if m.expires_at_ms <= now {
        return Err("work_media_expired".into());
    }
    let r = reference(state, &m)?;
    let bytes = read_plain(state, &m, &r)?;
    let parsed = ParsedAssetUpload {
        filename: r.filename,
        declared_mime: Some(r.mime),
        bytes,
    };
    let stored =
        assets::write_asset(&state.config.data_dir, p, parsed).map_err(|e| e.to_string())?;
    assets::persist_asset(&state.store, p, &stored).map_err(|e| e.to_string())
}
pub fn cleanup(state: &StarlinkRouterState, now: i64) -> Result<usize, String> {
    let _guard = state
        .work_media_lock
        .lock()
        .map_err(|_| "work_media_lock_unavailable")?;
    let mut removed = 0;
    for m in state
        .store
        .claim_expired_work_media(now)
        .map_err(|e| e.to_string())?
    {
        let r = reference(state, &m)?;
        let target = path(state, &r.file)?;
        match fs::remove_file(target) {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("work_media_cleanup_failed".into()),
        }
        state
            .store
            .finish_work_media_cleanup(&m.media_id)
            .map_err(|e| e.to_string())?;
    }
    removed += cleanup_orphans(state, now)?;
    Ok(removed)
}
fn cleanup_orphans(state: &StarlinkRouterState, now: i64) -> Result<usize, String> {
    use std::collections::HashSet;
    let records = state
        .store
        .work_media_records()
        .map_err(|e| e.to_string())?;
    let mut keep = HashSet::new();
    for m in records.iter().filter(|m| m.state != "deleted") {
        keep.insert(reference(state, m)?.file);
    }
    let leased = state
        .store
        .leased_work_media_ids()
        .map_err(|e| e.to_string())?;
    let dir = root(state);
    if !dir.exists() {
        return Ok(0);
    }
    let root_meta = fs::symlink_metadata(&dir).map_err(|_| "work_media_cleanup_failed")?;
    if !root_meta.is_dir() || root_meta.file_type().is_symlink() {
        return Err("work_media_invalid_directory".into());
    }
    let mut removed = 0;
    for entry in fs::read_dir(&dir).map_err(|_| "work_media_cleanup_failed")? {
        let entry = entry.map_err(|_| "work_media_cleanup_failed")?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let stem = name
            .strip_suffix(".bin")
            .or_else(|| name.strip_suffix(".partial"))
            .unwrap_or("");
        let Some(stem) = stem.strip_prefix("media_") else {
            continue;
        };
        let Some((id, nonce)) = stem.split_once('-') else {
            continue;
        };
        if id.len() != 32
            || nonce.len() != 32
            || !id
                .bytes()
                .chain(nonce.bytes())
                .all(|b| b.is_ascii_hexdigit())
            || keep.contains(&name)
            || leased.iter().any(|id| name.starts_with(&format!("{id}-")))
        {
            continue;
        }
        let meta = fs::symlink_metadata(entry.path()).map_err(|_| "work_media_cleanup_failed")?;
        if !meta.is_file() || meta.file_type().is_symlink() {
            continue;
        }
        let modified = meta
            .modified()
            .map_err(|_| "work_media_cleanup_failed")?
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| "work_media_cleanup_failed")?
            .as_millis();
        if modified > now.saturating_sub(3600 * 1000).max(0) as u128 {
            continue;
        }
        fs::remove_file(entry.path()).map_err(|_| "work_media_cleanup_failed")?;
        removed += 1;
    }
    Ok(removed)
}
pub fn recover(state: &StarlinkRouterState) -> Result<usize, String> {
    let _guard = state
        .work_media_lock
        .lock()
        .map_err(|_| "work_media_lock_unavailable")?;
    let mut count = 0;
    for m in state
        .store
        .work_media_records()
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|m| m.state == "preparing")
    {
        // Missing key material or unauthenticated ciphertext is not evidence
        // that a paid job's reference may be discarded. Keep its reservation.
        let r = reference(state, &m)?;
        let ready = match read_plain(state, &m, &r) {
            Ok(_) => true,
            Err(e) if e == "work_media_missing" => false,
            Err(e) => return Err(e),
        };
        state
            .store
            .set_preparing_work_media_ready(&m.media_id, ready)
            .map_err(|e| e.to_string())?;
        count += 1;
    }
    Ok(count)
}
pub fn rotate(state: &StarlinkRouterState) -> Result<usize, String> {
    let _guard = state
        .work_media_lock
        .lock()
        .map_err(|_| "work_media_lock_unavailable")?;
    let mut changed = 0;
    for old in state
        .store
        .work_secret_records()
        .map_err(|e| e.to_string())?
    {
        if old.key_version == state.key_vault.active_version() {
            continue;
        }
        let encrypted = state
            .key_vault
            .reencrypt(&old.context, old.key_version, &old.ciphertext)
            .map_err(|_| "work_crypto_rotation_failed")?;
        if state
            .store
            .replace_work_secret_crypto(&old, encrypted.key_version, &encrypted.ciphertext)
            .map_err(|e| e.to_string())?
        {
            changed += 1;
        }
    }
    for m in state
        .store
        .work_media_records()
        .map_err(|e| e.to_string())?
    {
        if m.key_version == state.key_vault.active_version() {
            continue;
        }
        let mut r = reference(state, &m)?;
        if m.state != "deleted" {
            let plain = read_plain(state, &m, &r)?;
            let encrypted = state
                .key_vault
                .encrypt_bytes(&aad(&m, "content"), &plain)
                .map_err(|_| "work_media_rotation_failed")?;
            r.file = format!("{}-{:032x}.bin", m.media_id, rand::random::<u128>());
            publish(state, &r.file, &encrypted.ciphertext)?;
        }
        let new_ref = state
            .key_vault
            .encrypt(
                &aad(&m, "ref"),
                &serde_json::to_string(&r).map_err(|_| "work_media_invalid_metadata")?,
            )
            .map_err(|_| "work_media_rotation_failed")?;
        if state
            .store
            .replace_work_media_crypto(&m, new_ref.key_version, &new_ref.ciphertext)
            .map_err(|e| e.to_string())?
        {
            changed += 1;
        }
    }
    Ok(changed)
}
