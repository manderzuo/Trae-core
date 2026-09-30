use crate::{CoreError, CoreStore, Principal};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VideoWork {
    pub work_id: String,
    pub owner_key_id: String,
    pub owner_user_id: String,
    pub conversation_ref: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub deleted_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkAction {
    Create,
    Revise,
    Continue,
}
impl WorkAction {
    fn name(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Revise => "revise",
            Self::Continue => "continue",
        }
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkVersionState {
    Preparing,
    Running,
    Completed,
    Failed,
    Unknown,
}
impl WorkVersionState {
    fn name(self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }
    fn permits(self, next: Self) -> bool {
        self == next
            || match self {
                Self::Preparing => matches!(next, Self::Running | Self::Failed | Self::Unknown),
                Self::Running | Self::Unknown => matches!(
                    next,
                    Self::Running | Self::Completed | Self::Failed | Self::Unknown
                ),
                _ => false,
            }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EncryptedWorkSnapshot {
    pub key_version: u32,
    pub ciphertext: Vec<u8>,
    pub snapshot_sha256: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VideoWorkSnapshot {
    pub effective_prompt: String,
    pub duration: i64,
    pub resolution: String,
    pub ratio: String,
    pub watermark: bool,
    pub user_media_ids: Vec<String>,
    pub tail_frame_media_id: Option<String>,
    /// Full parent MP4 pinned by Core, never accepted from client input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation_video_media_id: Option<String>,
    pub parent_version_id: Option<String>,
    pub source_request_id: Option<String>,
    pub reference_mode: String,
    pub summary: String,
    /// Internal immutable, already-materialized upstream input. It is stored
    /// only inside the authenticated encrypted version snapshot.
    #[serde(default)]
    pub dispatch_body: Option<serde_json::Value>,
}
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct VideoWorkVersion {
    pub version_id: String,
    pub work_id: String,
    pub parent_version_id: Option<String>,
    pub operation_request_id: String,
    pub ordinal: i64,
    pub action: WorkAction,
    pub state: WorkVersionState,
    #[serde(skip_serializing)]
    pub sealed_snapshot: EncryptedWorkSnapshot,
    pub context_handle_sha256: Option<String>,
    pub tail_frame_media_id: Option<String>,
    pub frame_state: String,
    pub frame_error: Option<String>,
    #[serde(skip_serializing)]
    pub sealed_frame: Option<EncryptedWorkSnapshot>,
    pub delivery_state: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub deleted_at_ms: Option<i64>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkMutation {
    pub version: VideoWorkVersion,
    pub created: bool,
}
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WorkMediaRef {
    pub media_id: String,
    pub work_id: String,
    pub owner_key_id: String,
    pub kind: String,
    pub content_sha256: String,
    #[serde(skip_serializing)]
    pub encrypted_storage_ref: Vec<u8>,
    pub key_version: u32,
    pub size_bytes: i64,
    pub expires_at_ms: i64,
    pub state: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkHandleRecord {
    pub context_handle_sha256: String,
    pub work_id: String,
    pub version_id: Option<String>,
    pub key_version: u32,
    pub encrypted_handle: Vec<u8>,
}
#[derive(Debug, Clone)]
pub struct WorkSecretRef {
    pub kind: String,
    pub id: String,
    pub context: String,
    pub key_version: u32,
    pub ciphertext: Vec<u8>,
}
pub fn work_snapshot_context(owner: &str, work: &str, request: &str) -> String {
    format!("work-snapshot:{owner}:{work}:{request}")
}
pub fn work_handle_context(owner: &str, work: &str, version: Option<&str>) -> String {
    format!(
        "work-handle:{owner}:{work}:{}",
        version.unwrap_or("pending")
    )
}
pub fn work_frame_context(owner: &str, work: &str, version: &str) -> String {
    format!("work-frame:{owner}:{work}:{version}")
}

fn invalid(reason: &str) -> CoreError {
    CoreError::Validation {
        field: "video_work".into(),
        reason: reason.into(),
    }
}
fn valid_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}
fn valid_sealed(version: u32, cipher: &[u8]) -> bool {
    version > 0 && cipher.len() > 28 && cipher.len() <= 128 * 1024
}
fn valid_request(con: &Connection, p: &Principal, id: &str) -> Result<(), CoreError> {
    let valid: bool = con.query_row(
        "SELECT EXISTS(SELECT 1 FROM requests WHERE id=?1 AND api_key_id=?2 AND user_id=?3)",
        params![id, p.key_id, p.user_id],
        |r| r.get(0),
    )?;
    if valid {
        Ok(())
    } else {
        Err(invalid("request is unavailable"))
    }
}
fn parse_enum<T: serde::de::DeserializeOwned>(value: String, index: usize) -> rusqlite::Result<T> {
    serde_json::from_value(serde_json::Value::String(value)).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, Box::new(e))
    })
}
fn read_version(r: &rusqlite::Row<'_>) -> rusqlite::Result<VideoWorkVersion> {
    Ok(VideoWorkVersion {
        version_id: r.get(0)?,
        work_id: r.get(1)?,
        parent_version_id: r.get(2)?,
        operation_request_id: r.get(3)?,
        ordinal: r.get(4)?,
        action: parse_enum(r.get(5)?, 5)?,
        state: parse_enum(r.get(6)?, 6)?,
        sealed_snapshot: EncryptedWorkSnapshot {
            key_version: r.get(7)?,
            ciphertext: r.get(8)?,
            snapshot_sha256: r.get(9)?,
        },
        context_handle_sha256: r.get(10)?,
        tail_frame_media_id: r.get(11)?,
        frame_state: r.get(12)?,
        frame_error: r.get(13)?,
        delivery_state: r.get(14)?,
        created_at_ms: r.get(15)?,
        updated_at_ms: r.get(16)?,
        deleted_at_ms: r.get(17)?,
        sealed_frame: match r.get::<_, Option<u32>>(18)? {
            Some(key_version) => Some(EncryptedWorkSnapshot {
                key_version,
                ciphertext: r.get(19)?,
                snapshot_sha256: r.get(20)?,
            }),
            None => None,
        },
    })
}
const VERSION_COLUMNS:&str="v.version_id,v.work_id,v.parent_version_id,v.operation_request_id,v.ordinal,v.action,v.state,v.key_version,v.encrypted_snapshot,v.snapshot_sha256,v.context_handle_sha256,v.tail_frame_media_id,v.frame_state,v.frame_error,v.delivery_state,v.created_at_ms,v.updated_at_ms,v.deleted_at_ms,v.frame_key_version,v.encrypted_frame_metadata,v.frame_metadata_sha256";
fn version_by(
    con: &Connection,
    p: &Principal,
    field: &str,
    value: &str,
) -> Result<Option<VideoWorkVersion>, CoreError> {
    Ok(con.query_row(&format!("SELECT {VERSION_COLUMNS} FROM video_work_versions v JOIN video_works w ON w.work_id=v.work_id WHERE v.{field}=?1 AND w.owner_key_id=?2 AND w.owner_user_id=?3 AND w.deleted_at_ms IS NULL AND v.deleted_at_ms IS NULL"),params![value,p.key_id,p.user_id],read_version).optional()?)
}
fn read_media(r: &rusqlite::Row<'_>) -> rusqlite::Result<WorkMediaRef> {
    Ok(WorkMediaRef {
        media_id: r.get(0)?,
        work_id: r.get(1)?,
        owner_key_id: r.get(2)?,
        kind: r.get(3)?,
        content_sha256: r.get(4)?,
        encrypted_storage_ref: r.get(5)?,
        key_version: r.get(6)?,
        size_bytes: r.get(7)?,
        expires_at_ms: r.get(8)?,
        state: r.get(9)?,
    })
}
const MEDIA_COLUMNS:&str="m.media_id,m.work_id,m.owner_key_id,m.kind,m.content_sha256,m.encrypted_storage_ref,m.key_version,m.size_bytes,m.expires_at_ms,m.state";
fn owned_media(
    con: &Connection,
    p: &Principal,
    id: &str,
) -> Result<Option<WorkMediaRef>, CoreError> {
    Ok(con.query_row(&format!("SELECT {MEDIA_COLUMNS} FROM video_work_media m JOIN video_works w ON w.work_id=m.work_id WHERE m.media_id=?1 AND m.owner_key_id=?2 AND w.owner_key_id=?2 AND w.owner_user_id=?3 AND w.deleted_at_ms IS NULL AND m.state='active'"),params![id,p.key_id,p.user_id],read_media).optional()?)
}
fn read_handle(r: &rusqlite::Row<'_>) -> rusqlite::Result<WorkHandleRecord> {
    Ok(WorkHandleRecord {
        context_handle_sha256: r.get(0)?,
        work_id: r.get(1)?,
        version_id: r.get(2)?,
        key_version: r.get(3)?,
        encrypted_handle: r.get(4)?,
    })
}

pub(crate) fn validate_owner(con: &Connection, principal: &Principal) -> Result<(), CoreError> {
    let valid:bool=con.query_row("SELECT EXISTS(SELECT 1 FROM api_keys k JOIN users u ON u.id=k.user_id WHERE k.id=?1 AND k.user_id=?2 AND k.status='active' AND u.status='active')",params![principal.key_id,principal.user_id],|r|r.get(0))?;
    if !valid {
        return Err(CoreError::InvalidRequestIdentity {
            user_id: principal.user_id.clone(),
            api_key_id: principal.key_id.clone(),
        });
    }
    Ok(())
}

fn read_work(row: &rusqlite::Row<'_>) -> rusqlite::Result<VideoWork> {
    Ok(VideoWork {
        work_id: row.get(0)?,
        owner_key_id: row.get(1)?,
        owner_user_id: row.get(2)?,
        conversation_ref: row.get(3)?,
        created_at_ms: row.get(4)?,
        updated_at_ms: row.get(5)?,
        deleted_at_ms: row.get(6)?,
    })
}
pub(crate) fn owned_work(
    con: &Connection,
    p: &Principal,
    id: &str,
) -> Result<Option<VideoWork>, CoreError> {
    Ok(con.query_row("SELECT work_id,owner_key_id,owner_user_id,conversation_ref,created_at_ms,updated_at_ms,deleted_at_ms FROM video_works WHERE work_id=?1 AND owner_key_id=?2 AND owner_user_id=?3 AND deleted_at_ms IS NULL",params![id,p.key_id,p.user_id],read_work).optional()?)
}

impl CoreStore {
    pub fn create_video_work(
        &self,
        p: &Principal,
        conversation_ref: &str,
    ) -> Result<VideoWork, CoreError> {
        if conversation_ref.is_empty()
            || conversation_ref.len() > 256
            || conversation_ref.chars().any(char::is_control)
        {
            return Err(CoreError::Validation {
                field: "conversation_ref".into(),
                reason: "must be a bounded opaque association".into(),
            });
        }
        let mut con = self.connection.lock().expect("core store mutex poisoned");
        let tx = con.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_owner(&tx, p)?;
        let id = Self::new_id("work");
        let now = chrono::Utc::now().timestamp_millis();
        tx.execute("INSERT INTO video_works(work_id,owner_key_id,owner_user_id,conversation_ref,created_at_ms,updated_at_ms) VALUES (?1,?2,?3,?4,?5,?5) ON CONFLICT(owner_key_id,conversation_ref) DO NOTHING",params![id,p.key_id,p.user_id,conversation_ref,now])?;
        let work=tx.query_row("SELECT work_id,owner_key_id,owner_user_id,conversation_ref,created_at_ms,updated_at_ms,deleted_at_ms FROM video_works WHERE owner_key_id=?1 AND owner_user_id=?2 AND conversation_ref=?3 AND deleted_at_ms IS NULL",params![p.key_id,p.user_id,conversation_ref],read_work)?;
        tx.commit()?;
        Ok(work)
    }
    pub fn owned_video_work(
        &self,
        p: &Principal,
        id: &str,
    ) -> Result<Option<VideoWork>, CoreError> {
        let con = self.connection.lock().expect("core store mutex poisoned");
        validate_owner(&con, p)?;
        owned_work(&con, p, id)
    }

    pub fn owned_work_for_conversation(
        &self,
        p: &Principal,
        association: &str,
    ) -> Result<Option<VideoWork>, CoreError> {
        let con = self.connection.lock().expect("core store mutex poisoned");
        validate_owner(&con, p)?;
        Ok(con.query_row("SELECT work_id,owner_key_id,owner_user_id,conversation_ref,created_at_ms,updated_at_ms,deleted_at_ms FROM video_works WHERE owner_key_id=?1 AND owner_user_id=?2 AND conversation_ref=?3 AND deleted_at_ms IS NULL",params![p.key_id,p.user_id,association],read_work).optional()?)
    }

    pub fn bind_work_version(
        &self,
        p: &Principal,
        work: &str,
        parent: Option<&str>,
        request: &str,
        action: WorkAction,
        sealed: &EncryptedWorkSnapshot,
    ) -> Result<WorkMutation, CoreError> {
        if !valid_sealed(sealed.key_version, &sealed.ciphertext)
            || !valid_hash(&sealed.snapshot_sha256)
        {
            return Err(invalid("invalid sealed snapshot"));
        }
        let mut con = self.connection.lock().expect("core store mutex poisoned");
        let tx = con.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_owner(&tx, p)?;
        valid_request(&tx, p, request)?;
        if owned_work(&tx, p, work)?.is_none() {
            return Err(invalid("work is unavailable"));
        }
        if let Some(old) = version_by(&tx, p, "operation_request_id", request)? {
            if old.work_id != work
                || old.parent_version_id.as_deref() != parent
                || old.action != action
                || old.sealed_snapshot.snapshot_sha256 != sealed.snapshot_sha256
            {
                return Err(CoreError::IdempotencyConflict);
            }
            tx.commit()?;
            return Ok(WorkMutation {
                version: old,
                created: false,
            });
        }
        if let Some(parent) = parent {
            if version_by(&tx, p, "version_id", parent)?.map_or(true, |v| v.work_id != work) {
                return Err(invalid("parent is unavailable"));
            }
        } else if action != WorkAction::Create {
            return Err(invalid("revision/continuation requires an exact parent"));
        }
        let id = Self::new_id("version");
        let now = chrono::Utc::now().timestamp_millis();
        let ordinal: i64 = tx.query_row(
            "SELECT COALESCE(MAX(ordinal),0)+1 FROM video_work_versions WHERE work_id=?1",
            [work],
            |r| r.get(0),
        )?;
        tx.execute("INSERT INTO video_work_versions(version_id,work_id,parent_version_id,operation_request_id,ordinal,action,state,key_version,encrypted_snapshot,snapshot_sha256,created_at_ms,updated_at_ms) VALUES (?1,?2,?3,?4,?5,?6,'preparing',?7,?8,?9,?10,?10)",params![id,work,parent,request,ordinal,action.name(),sealed.key_version,sealed.ciphertext,sealed.snapshot_sha256,now])?;
        tx.execute(
            "UPDATE video_works SET updated_at_ms=?2 WHERE work_id=?1",
            params![work, now],
        )?;
        let version = version_by(&tx, p, "version_id", &id)?
            .ok_or_else(|| invalid("version write failed"))?;
        tx.commit()?;
        Ok(WorkMutation {
            version,
            created: true,
        })
    }
    pub fn owned_work_version(
        &self,
        p: &Principal,
        id: &str,
    ) -> Result<Option<VideoWorkVersion>, CoreError> {
        let con = self.connection.lock().expect("core store mutex poisoned");
        validate_owner(&con, p)?;
        version_by(&con, p, "version_id", id)
    }
    pub fn work_version_for_request(
        &self,
        p: &Principal,
        id: &str,
    ) -> Result<Option<VideoWorkVersion>, CoreError> {
        let con = self.connection.lock().expect("core store mutex poisoned");
        validate_owner(&con, p)?;
        version_by(&con, p, "operation_request_id", id)
    }
    pub fn revoke_work_version(&self, p: &Principal, id: &str) -> Result<(), CoreError> {
        let mut con = self.connection.lock().expect("core store mutex poisoned");
        let tx = con.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_owner(&tx, p)?;
        let v = version_by(&tx, p, "version_id", id)?
            .ok_or_else(|| invalid("version is unavailable"))?;
        if !matches!(
            v.state,
            WorkVersionState::Completed | WorkVersionState::Failed
        ) {
            return Err(invalid("active or unknown version cannot be revoked"));
        }
        tx.execute(
            "UPDATE video_work_versions SET deleted_at_ms=?2 WHERE version_id=?1",
            params![id, chrono::Utc::now().timestamp_millis()],
        )?;
        // Retain immutable input, billing binding, child branches, and shared
        // media. Revocation only removes this version as an eligible context.
        tx.commit()?;
        Ok(())
    }
    /// Records a validated client receipt for this exact owned request, not
    /// the latest work version. Legacy requests without a work are a no-op.
    pub fn set_work_delivery_state(
        &self,
        p: &Principal,
        request: &str,
        state: &str,
    ) -> Result<(), CoreError> {
        if !matches!(state, "saved" | "download_failed") {
            return Err(invalid("invalid delivery state"));
        }
        let mut con = self.connection.lock().expect("core store mutex poisoned");
        let tx = con.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_owner(&tx, p)?;
        if let Some(v) = version_by(&tx, p, "operation_request_id", request)? {
            if v.state != WorkVersionState::Completed {
                return Err(invalid("source version is incomplete"));
            }
            // A delayed failure or repeated receipt cannot undo a confirmed
            // save, nor repeatedly change the version's modification time.
            if v.delivery_state != "saved" && v.delivery_state != state {
                tx.execute(
                    "UPDATE video_work_versions SET delivery_state=?2,updated_at_ms=?3 WHERE version_id=?1",
                    params![v.version_id, state, chrono::Utc::now().timestamp_millis()],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }
    pub fn set_work_frame(
        &self,
        p: &Principal,
        version: &str,
        state: &str,
        media: Option<&str>,
        sealed: Option<&EncryptedWorkSnapshot>,
        error: Option<&str>,
    ) -> Result<(), CoreError> {
        if !matches!(state, "running" | "ready" | "failed")
            || error.is_some_and(|s| {
                s.len() > 128 || !s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            })
        {
            return Err(invalid("invalid frame state"));
        }
        let mut con = self.connection.lock().expect("core store mutex poisoned");
        let tx = con.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_owner(&tx, p)?;
        let v = version_by(&tx, p, "version_id", version)?
            .ok_or_else(|| invalid("version is unavailable"))?;
        if v.state != WorkVersionState::Completed {
            return Err(invalid("source version is incomplete"));
        }
        if state == "ready" {
            let id = media.ok_or_else(|| invalid("frame media missing"))?;
            let proof = sealed.ok_or_else(|| invalid("frame evidence missing"))?;
            if !valid_sealed(proof.key_version, &proof.ciphertext)
                || proof.snapshot_sha256.len() != 64
                || !proof.snapshot_sha256.bytes().all(|b| b.is_ascii_hexdigit())
            {
                return Err(invalid("frame evidence invalid"));
            }
            let m = owned_media(&tx, p, id)?
                .filter(|m| m.work_id == v.work_id && m.kind == "tail_frame")
                .ok_or_else(|| invalid("frame media unavailable"))?;
            if v.frame_state == "ready" {
                if v.tail_frame_media_id.as_deref() != Some(id)
                    || v.sealed_frame
                        .as_ref()
                        .is_none_or(|s| s.snapshot_sha256 != proof.snapshot_sha256)
                {
                    return Err(CoreError::IdempotencyConflict);
                }
                return Ok(());
            }
            tx.execute("UPDATE video_work_versions SET frame_state='ready',frame_error=NULL,tail_frame_media_id=?2,frame_key_version=?3,encrypted_frame_metadata=?4,frame_metadata_sha256=?5,updated_at_ms=?6 WHERE version_id=?1",params![version,m.media_id,proof.key_version,proof.ciphertext,proof.snapshot_sha256,chrono::Utc::now().timestamp_millis()])?;
        } else if v.frame_state != "ready" {
            tx.execute("UPDATE video_work_versions SET frame_state=?2,frame_error=?3,updated_at_ms=?4 WHERE version_id=?1",params![version,state,error,chrono::Utc::now().timestamp_millis()])?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn work_versions_needing_frames(
        &self,
        limit: usize,
    ) -> Result<Vec<(Principal, VideoWorkVersion)>, CoreError> {
        let con = self.connection.lock().expect("core store mutex poisoned");
        let mut s=con.prepare(&format!("SELECT {VERSION_COLUMNS},w.owner_user_id,w.owner_key_id,k.scopes_json FROM video_work_versions v JOIN video_works w ON w.work_id=v.work_id JOIN api_keys k ON k.id=w.owner_key_id JOIN users u ON u.id=w.owner_user_id WHERE (v.state='completed' OR (v.state IN ('preparing','running','unknown') AND EXISTS(SELECT 1 FROM budget_steps b WHERE b.request_id=v.operation_request_id AND b.core_key_id=w.owner_key_id AND b.kind='video' AND b.execution_state='succeeded' AND b.task_ref IS NOT NULL))) AND v.frame_state IN ('pending','running') AND w.deleted_at_ms IS NULL AND v.deleted_at_ms IS NULL AND k.status='active' AND u.status='active' ORDER BY v.updated_at_ms LIMIT ?1"))?;
        let rows = s
            .query_map([limit.min(32) as i64], |r| {
                Ok((
                    Principal {
                        user_id: r.get(21)?,
                        key_id: r.get(22)?,
                        scopes: serde_json::from_str(&r.get::<_, String>(23)?)
                            .map_err(|_| rusqlite::Error::InvalidQuery)?,
                    },
                    read_version(r)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
    pub fn work_versions(
        &self,
        p: &Principal,
        work: &str,
    ) -> Result<Vec<VideoWorkVersion>, CoreError> {
        let con = self.connection.lock().expect("core store mutex poisoned");
        validate_owner(&con, p)?;
        if owned_work(&con, p, work)?.is_none() {
            return Err(invalid("work is unavailable"));
        }
        let mut s=con.prepare(&format!("SELECT {VERSION_COLUMNS} FROM video_work_versions v WHERE work_id=?1 AND deleted_at_ms IS NULL ORDER BY ordinal"))?;
        let rows = s
            .query_map([work], read_version)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
    pub fn set_work_version_state(
        &self,
        p: &Principal,
        request: &str,
        next: WorkVersionState,
    ) -> Result<WorkMutation, CoreError> {
        let mut con = self.connection.lock().expect("core store mutex poisoned");
        let tx = con.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_owner(&tx, p)?;
        let old = version_by(&tx, p, "operation_request_id", request)?
            .ok_or_else(|| invalid("version is unavailable"))?;
        if !old.state.permits(next) {
            return Err(invalid("invalid version state transition"));
        }
        if old.state != next {
            tx.execute("UPDATE video_work_versions SET state=?2,updated_at_ms=?3 WHERE operation_request_id=?1",params![request,next.name(),chrono::Utc::now().timestamp_millis()])?;
        }
        let version = version_by(&tx, p, "operation_request_id", request)?.unwrap();
        tx.commit()?;
        Ok(WorkMutation {
            version,
            created: false,
        })
    }

    pub fn save_work_handle(
        &self,
        p: &Principal,
        work: &str,
        version: Option<&str>,
        hash: &str,
        key_version: u32,
        cipher: &[u8],
    ) -> Result<WorkHandleRecord, CoreError> {
        if !valid_hash(hash) || !valid_sealed(key_version, cipher) {
            return Err(invalid("invalid encrypted handle"));
        }
        let mut con = self.connection.lock().expect("core store mutex poisoned");
        let tx = con.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_owner(&tx, p)?;
        if owned_work(&tx, p, work)?.is_none() {
            return Err(invalid("work is unavailable"));
        }
        if let Some(version) = version {
            if version_by(&tx, p, "version_id", version)?.map_or(true, |v| v.work_id != work) {
                return Err(invalid("version is unavailable"));
            }
        }
        let existing=tx.query_row("SELECT context_handle_sha256,work_id,version_id,key_version,encrypted_handle FROM video_work_contexts WHERE work_id=?1 AND version_id IS ?2",params![work,version],read_handle).optional()?;
        if let Some(existing) = existing {
            tx.commit()?;
            return Ok(existing);
        }
        tx.execute(
            "INSERT INTO video_work_contexts VALUES (?1,?2,?3,?4,?5)",
            params![hash, work, version, key_version, cipher],
        )?;
        if let Some(version) = version {
            tx.execute(
                "UPDATE video_work_versions SET context_handle_sha256=?2 WHERE version_id=?1",
                params![version, hash],
            )?;
        }
        tx.commit()?;
        Ok(WorkHandleRecord {
            context_handle_sha256: hash.into(),
            work_id: work.into(),
            version_id: version.map(str::to_owned),
            key_version,
            encrypted_handle: cipher.into(),
        })
    }
    pub fn work_handle_for_version(
        &self,
        p: &Principal,
        work: &str,
        version: Option<&str>,
    ) -> Result<Option<WorkHandleRecord>, CoreError> {
        let con = self.connection.lock().expect("core store mutex poisoned");
        validate_owner(&con, p)?;
        if owned_work(&con, p, work)?.is_none() {
            return Ok(None);
        }
        Ok(con.query_row("SELECT context_handle_sha256,work_id,version_id,key_version,encrypted_handle FROM video_work_contexts WHERE work_id=?1 AND version_id IS ?2",params![work,version],read_handle).optional()?)
    }
    pub fn owned_work_handle(
        &self,
        p: &Principal,
        hash: &str,
    ) -> Result<Option<WorkHandleRecord>, CoreError> {
        let con = self.connection.lock().expect("core store mutex poisoned");
        validate_owner(&con, p)?;
        let record=con.query_row("SELECT context_handle_sha256,work_id,version_id,key_version,encrypted_handle FROM video_work_contexts WHERE context_handle_sha256=?1",[hash],read_handle).optional()?;
        let Some(record) = record else {
            return Ok(None);
        };
        if owned_work(&con, p, &record.work_id)?.is_none() {
            return Ok(None);
        }
        if let Some(ref id) = record.version_id {
            if version_by(&con, p, "version_id", id)?.is_none() {
                return Ok(None);
            }
        }
        Ok(Some(record))
    }

    pub fn reserve_work_media(
        &self,
        p: &Principal,
        m: &WorkMediaRef,
        key_limit: i64,
        global_limit: i64,
    ) -> Result<WorkMediaRef, CoreError> {
        if m.owner_key_id != p.key_id
            || m.media_id.is_empty()
            || !valid_hash(&m.content_sha256)
            || !valid_sealed(m.key_version, &m.encrypted_storage_ref)
            || m.size_bytes <= 0
            || m.expires_at_ms <= 0
            || !["image", "video", "tail_frame"].contains(&m.kind.as_str())
        {
            return Err(invalid("invalid private media"));
        }
        let mut con = self.connection.lock().expect("core store mutex poisoned");
        let tx = con.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_owner(&tx, p)?;
        if owned_work(&tx, p, &m.work_id)?.is_none() {
            return Err(invalid("work is unavailable"));
        }
        let old=tx.query_row(&format!("SELECT {MEDIA_COLUMNS} FROM video_work_media m WHERE m.work_id=?1 AND m.kind=?2 AND m.content_sha256=?3"),params![m.work_id,m.kind,m.content_sha256],read_media).optional()?;
        if let Some(ref old) = old {
            if old.state == "active" {
                tx.commit()?;
                return Ok(old.clone());
            }
            if old.state != "deleted" {
                return Err(invalid("private media is not ready"));
            }
        }
        let global: i64 = tx.query_row(
            "SELECT COALESCE(SUM(size_bytes),0) FROM video_work_media WHERE state!='deleted'",
            [],
            |r| r.get(0),
        )?;
        let used:i64=tx.query_row("SELECT COALESCE(SUM(size_bytes),0) FROM video_work_media WHERE owner_key_id=?1 AND state!='deleted'",[&p.key_id],|r|r.get(0))?;
        if m.size_bytes > key_limit.saturating_sub(used)
            || m.size_bytes > global_limit.saturating_sub(global)
        {
            return Err(invalid("work media capacity exceeded"));
        }
        let mut out = m.clone();
        out.state = "preparing".into();
        if let Some(old) = old {
            // Preserve referenced media IDs; accept a freshly uploaded copy only
            // after the old ciphertext was positively cleaned up.
            out.media_id = old.media_id;
            // Caller encrypts with media ID in AAD, so a recycled ID requires
            // a fresh caller round with that exact ID, never silently swapping.
            if out.media_id != m.media_id {
                return Err(invalid("reuse deleted media id for replacement"));
            }
            tx.execute("UPDATE video_work_media SET encrypted_storage_ref=?2,key_version=?3,size_bytes=?4,expires_at_ms=?5,state='preparing' WHERE media_id=?1 AND state='deleted'",params![out.media_id,out.encrypted_storage_ref,out.key_version,out.size_bytes,out.expires_at_ms])?;
        } else {
            tx.execute("INSERT INTO video_work_media(media_id,work_id,owner_key_id,kind,content_sha256,encrypted_storage_ref,key_version,size_bytes,expires_at_ms,state) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,'preparing')",params![m.media_id,m.work_id,p.key_id,m.kind,m.content_sha256,m.encrypted_storage_ref,m.key_version,m.size_bytes,m.expires_at_ms])?;
        }
        tx.commit()?;
        Ok(out)
    }
    pub fn activate_work_media(&self, p: &Principal, id: &str) -> Result<(), CoreError> {
        let con = self.connection.lock().expect("core store mutex poisoned");
        validate_owner(&con, p)?;
        if con.execute("UPDATE video_work_media SET state='active' WHERE media_id=?1 AND owner_key_id=?2 AND state='preparing'",params![id,p.key_id])?!=1 {return Err(invalid("private media cannot be activated"));}
        Ok(())
    }
    pub fn owned_work_media(
        &self,
        p: &Principal,
        id: &str,
    ) -> Result<Option<WorkMediaRef>, CoreError> {
        let con = self.connection.lock().expect("core store mutex poisoned");
        validate_owner(&con, p)?;
        owned_media(&con, p, id)
    }
    pub fn acquire_work_media_lease(
        &self,
        p: &Principal,
        id: &str,
        request: &str,
    ) -> Result<(), CoreError> {
        let mut con = self.connection.lock().expect("core store mutex poisoned");
        let tx = con.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_owner(&tx, p)?;
        valid_request(&tx, p, request)?;
        if owned_media(&tx, p, id)?.is_none() {
            return Err(invalid("private media unavailable"));
        }
        let raw: String = tx.query_row(
            "SELECT leases_json FROM video_work_media WHERE media_id=?1",
            [id],
            |r| r.get(0),
        )?;
        let mut leases: Vec<String> = serde_json::from_str(&raw)?;
        if !leases.iter().any(|l| l == request) {
            leases.push(request.into());
        }
        tx.execute(
            "UPDATE video_work_media SET leases_json=?2 WHERE media_id=?1",
            params![id, serde_json::to_string(&leases)?],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn release_work_media_leases(&self, p: &Principal, request: &str) -> Result<(), CoreError> {
        let mut con = self.connection.lock().expect("core store mutex poisoned");
        let tx = con.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_owner(&tx, p)?;
        valid_request(&tx, p, request)?;
        let rows = {
            let mut s=tx.prepare("SELECT media_id,leases_json FROM video_work_media WHERE owner_key_id=?1 AND leases_json!='[]'")?;
            let rows = s
                .query_map([&p.key_id], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        for (id, raw) in rows {
            let mut leases: Vec<String> = serde_json::from_str(&raw)?;
            leases.retain(|s| s != request);
            tx.execute(
                "UPDATE video_work_media SET leases_json=?2 WHERE media_id=?1",
                params![id, serde_json::to_string(&leases)?],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn claim_expired_work_media(&self, now: i64) -> Result<Vec<WorkMediaRef>, CoreError> {
        let mut con = self.connection.lock().expect("core store mutex poisoned");
        let tx = con.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let rows = {
            let mut s=tx.prepare(&format!("SELECT {MEDIA_COLUMNS} FROM video_work_media m WHERE m.expires_at_ms<=?1 AND m.leases_json='[]' AND m.state IN ('preparing','active','deleting') LIMIT 100"))?;
            let rows = s
                .query_map([now], read_media)?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        for m in &rows {
            tx.execute(
                "UPDATE video_work_media SET state='deleting' WHERE media_id=?1",
                [&m.media_id],
            )?;
        }
        tx.commit()?;
        Ok(rows)
    }
    pub fn finish_work_media_cleanup(&self, id: &str) -> Result<(), CoreError> {
        let con = self.connection.lock().expect("core store mutex poisoned");
        con.execute("UPDATE video_work_media SET state='deleted' WHERE media_id=?1 AND state='deleting' AND leases_json='[]'",[id])?;
        Ok(())
    }
    pub fn work_media_records(&self) -> Result<Vec<WorkMediaRef>, CoreError> {
        let con = self.connection.lock().expect("core store mutex poisoned");
        let mut s = con.prepare(&format!("SELECT {MEDIA_COLUMNS} FROM video_work_media m"))?;
        let out = s
            .query_map([], read_media)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(out)
    }
    pub fn leased_work_media_ids(&self) -> Result<Vec<String>, CoreError> {
        let con = self.connection.lock().expect("core store mutex poisoned");
        let mut s = con.prepare("SELECT media_id FROM video_work_media WHERE leases_json!='[]'")?;
        let out = s
            .query_map([], |r| r.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(out)
    }
    pub fn set_preparing_work_media_ready(&self, id: &str, ready: bool) -> Result<(), CoreError> {
        let con = self.connection.lock().expect("core store mutex poisoned");
        con.execute(
            "UPDATE video_work_media SET state=?2 WHERE media_id=?1 AND state='preparing'",
            params![id, if ready { "active" } else { "deleted" }],
        )?;
        Ok(())
    }
    pub fn replace_work_media_crypto(
        &self,
        old: &WorkMediaRef,
        new_version: u32,
        new_ref: &[u8],
    ) -> Result<bool, CoreError> {
        if !valid_sealed(new_version, new_ref) {
            return Err(invalid("invalid media crypto replacement"));
        }
        let con = self.connection.lock().expect("core store mutex poisoned");
        Ok(con.execute("UPDATE video_work_media SET key_version=?4,encrypted_storage_ref=?5 WHERE media_id=?1 AND key_version=?2 AND encrypted_storage_ref=?3",params![old.media_id,old.key_version,old.encrypted_storage_ref,new_version,new_ref])?==1)
    }
    pub fn work_secret_records(&self) -> Result<Vec<WorkSecretRef>, CoreError> {
        let con = self.connection.lock().expect("core store mutex poisoned");
        let mut out = Vec::new();
        let mut s=con.prepare("SELECT v.version_id,w.owner_key_id,v.work_id,v.operation_request_id,v.key_version,v.encrypted_snapshot FROM video_work_versions v JOIN video_works w ON w.work_id=v.work_id")?;
        let rows = s
            .query_map([], |r| {
                Ok(WorkSecretRef {
                    kind: "snapshot".into(),
                    id: r.get(0)?,
                    context: work_snapshot_context(
                        &r.get::<_, String>(1)?,
                        &r.get::<_, String>(2)?,
                        &r.get::<_, String>(3)?,
                    ),
                    key_version: r.get(4)?,
                    ciphertext: r.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        out.extend(rows);
        let mut s=con.prepare("SELECT c.context_handle_sha256,w.owner_key_id,c.work_id,c.version_id,c.key_version,c.encrypted_handle FROM video_work_contexts c JOIN video_works w ON w.work_id=c.work_id")?;
        let rows = s
            .query_map([], |r| {
                Ok(WorkSecretRef {
                    kind: "handle".into(),
                    id: r.get(0)?,
                    context: work_handle_context(
                        &r.get::<_, String>(1)?,
                        &r.get::<_, String>(2)?,
                        r.get::<_, Option<String>>(3)?.as_deref(),
                    ),
                    key_version: r.get(4)?,
                    ciphertext: r.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        out.extend(rows);
        let mut s=con.prepare("SELECT v.version_id,w.owner_key_id,v.work_id,v.frame_key_version,v.encrypted_frame_metadata FROM video_work_versions v JOIN video_works w ON w.work_id=v.work_id WHERE v.frame_key_version IS NOT NULL")?;
        out.extend(
            s.query_map([], |r| {
                Ok(WorkSecretRef {
                    kind: "frame".into(),
                    id: r.get(0)?,
                    context: work_frame_context(
                        &r.get::<_, String>(1)?,
                        &r.get::<_, String>(2)?,
                        &r.get::<_, String>(0)?,
                    ),
                    key_version: r.get(3)?,
                    ciphertext: r.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?,
        );
        Ok(out)
    }
    pub fn replace_work_secret_crypto(
        &self,
        old: &WorkSecretRef,
        version: u32,
        cipher: &[u8],
    ) -> Result<bool, CoreError> {
        if !valid_sealed(version, cipher) {
            return Err(invalid("invalid work crypto replacement"));
        }
        let sql=match old.kind.as_str() {"snapshot"=>"UPDATE video_work_versions SET key_version=?4,encrypted_snapshot=?5 WHERE version_id=?1 AND key_version=?2 AND encrypted_snapshot=?3","frame"=>"UPDATE video_work_versions SET frame_key_version=?4,encrypted_frame_metadata=?5 WHERE version_id=?1 AND frame_key_version=?2 AND encrypted_frame_metadata=?3","handle"=>"UPDATE video_work_contexts SET key_version=?4,encrypted_handle=?5 WHERE context_handle_sha256=?1 AND key_version=?2 AND encrypted_handle=?3",_=>return Err(invalid("unknown work secret kind"))};
        let con = self.connection.lock().expect("core store mutex poisoned");
        Ok(con.execute(
            sql,
            params![old.id, old.key_version, old.ciphertext, version, cipher],
        )? == 1)
    }
}
