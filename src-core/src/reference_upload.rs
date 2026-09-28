//! Unbilled, short-lived client attachment handoffs. No Key-global image cache.
use crate::{CoreError, CoreStore, Principal};
use rusqlite::{params, OptionalExtension};

pub struct ReferenceUpload {
    pub key_id: String,
    pub key_version: u32,
    pub ciphertext: Vec<u8>,
    pub expires_at_ms: i64,
    pub asset_ids: Vec<Option<String>>,
}

fn invalid() -> CoreError {
    CoreError::InvalidConfiguration { key: "reference_upload".into(), value: "invalid_or_expired".into() }
}

impl CoreStore {
    pub fn prune_reference_uploads(&self, now_ms: i64) -> Result<usize, CoreError> {
        let conn=self.connection.lock().expect("core store mutex poisoned");
        Ok(conn.execute("DELETE FROM reference_uploads WHERE id IN (SELECT id FROM reference_uploads WHERE expires_at_ms<=?1 LIMIT 128)",[now_ms])?)
    }

    pub fn save_reference_upload(&self, principal: &Principal, id: &str, count: usize, expires: i64,
        version: u32, ciphertext: &[u8], dedupe_hash: &[u8;32], request_hash: &[u8;32], explicit_retry: bool) -> Result<Option<String>, CoreError> {
        let now = chrono::Utc::now().timestamp_millis();
        if !(1..=10).contains(&count) || expires <= now || expires > now + 30*60*1000
            || ciphertext.len() > 8*1024*1024+4096 { return Err(invalid()); }
        let mut conn = self.connection.lock().expect("core store mutex poisoned");
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // Bounded retention independent of video/billing records.
        tx.execute("DELETE FROM reference_uploads WHERE id IN (SELECT id FROM reference_uploads WHERE expires_at_ms<=?1 LIMIT 128)", [now])?;
        let existing: Option<(String,Vec<u8>)> = tx.query_row("SELECT id,request_hash FROM reference_uploads
            WHERE api_key_id=?1 AND dedupe_hash=?2 AND expires_at_ms>?3 AND created_at_ms>=?4 ORDER BY created_at_ms DESC LIMIT 1",
            params![principal.key_id,dedupe_hash,now,now-if explicit_retry {30*60*1000} else {2*60*1000}],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        if let Some((id,hash))=existing {return Ok((hash==request_hash).then_some(id));}
        let pending: i64 = tx.query_row("SELECT COUNT(*) FROM reference_uploads WHERE api_key_id=?1 AND expires_at_ms>?2",
            params![principal.key_id, now], |r|r.get(0))?;
        if pending >= 64 { return Err(invalid()); }
        tx.execute("INSERT INTO reference_uploads(id,api_key_id,key_version,ciphertext,expires_at_ms,asset_ids_json,created_at_ms,dedupe_hash,request_hash) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![id,principal.key_id,version,ciphertext,expires,serde_json::to_string(&vec![Option::<String>::None;count])?,now,dedupe_hash,request_hash])?;
        tx.commit()?;
        Ok(Some(id.into()))
    }

    pub fn reference_upload(&self, id: &str) -> Result<Option<ReferenceUpload>, CoreError> {
        let conn = self.connection.lock().expect("core store mutex poisoned");
        let row = conn.query_row("SELECT api_key_id,key_version,ciphertext,expires_at_ms,asset_ids_json FROM reference_uploads WHERE id=?1 AND expires_at_ms>?2",
            params![id,chrono::Utc::now().timestamp_millis()], |r| Ok((r.get::<_,String>(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get::<_,String>(4)?))).optional()?;
        row.map(|(key_id,key_version,ciphertext,expires_at_ms,assets)|Ok(ReferenceUpload {
            key_id,key_version,ciphertext,expires_at_ms,asset_ids:serde_json::from_str(&assets)?
        })).transpose()
    }

    /// Returns the winning asset ID; a retry with different bytes is rejected.
    pub fn bind_reference_upload_asset(&self, principal: &Principal, id: &str, index: usize, asset: &str) -> Result<String,CoreError> {
        let mut conn = self.connection.lock().expect("core store mutex poisoned");
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let raw: Option<String> = tx.query_row("SELECT h.asset_ids_json FROM reference_uploads h JOIN api_keys k ON k.id=h.api_key_id JOIN users u ON u.id=k.user_id
            WHERE h.id=?1 AND h.api_key_id=?2 AND k.user_id=?3 AND k.status='active' AND u.status='active' AND h.expires_at_ms>?4",
            params![id,principal.key_id,principal.user_id,chrono::Utc::now().timestamp_millis()], |r|r.get(0)).optional()?;
        let mut ids: Vec<Option<String>> = serde_json::from_str(&raw.ok_or_else(invalid)?)?;
        if index >= ids.len() { return Err(invalid()); }
        let sha: Option<String> = tx.query_row("SELECT sha256 FROM assets WHERE id=?1 AND user_id=?2 AND state='active' AND expires_at_ms>?3 AND mime_type LIKE 'image/%'",
            params![asset,principal.user_id,chrono::Utc::now().timestamp_millis()],|r|r.get(0)).optional()?;
        let sha = sha.ok_or_else(invalid)?;
        if let Some(existing) = &ids[index] {
            let old: String = tx.query_row("SELECT sha256 FROM assets WHERE id=?1",[existing],|r|r.get(0))?;
            return if old==sha {Ok(existing.clone())} else {Err(invalid())};
        }
        ids[index]=Some(asset.into());
        tx.execute("UPDATE reference_uploads SET asset_ids_json=?1 WHERE id=?2",params![serde_json::to_string(&ids)?,id])?;
        tx.commit()?;
        Ok(asset.into())
    }

    pub fn active_principal_for_key(&self, key: &str) -> Result<Option<Principal>, CoreError> {
        let conn = self.connection.lock().expect("core store mutex poisoned");
        let row = conn.query_row("SELECT k.user_id,k.scopes_json FROM api_keys k JOIN users u ON u.id=k.user_id
            WHERE k.id=?1 AND k.status='active' AND k.deleted_at_ms IS NULL AND u.status='active'",[key],
            |r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).optional()?;
        row.map(|(user_id,scopes)|Ok(Principal {user_id,key_id:key.into(),scopes:serde_json::from_str(&scopes)?})).transpose()
    }
}
