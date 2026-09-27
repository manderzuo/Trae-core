//! Encrypted business checkpoints, never billing facts or API credentials.
use base64::{engine::general_purpose::URL_SAFE_NO_PAD,Engine as _};
use rusqlite::{params,Connection,OptionalExtension,TransactionBehavior};
use crate::{CoreStore,CoreError,Principal};

pub struct BudgetContinuation {
    pub request_id:String,
    pub api_key_id:String,
    pub fingerprint:String,
    pub key_version:u32,
    pub ciphertext:Vec<u8>,
    /// Current authorization, not the scopes that happened to exist at submit.
    pub principal:Option<Principal>,
}
impl BudgetContinuation {
    pub fn encryption_context(&self)->String {
        format!("budget-continuation-v1:{}:{}:{}",self.request_id,self.api_key_id,self.fingerprint)
    }
}
impl CoreStore {
    /// Bounded rotation; crypto runs outside the SQLite lock. Call again while
    /// remaining > 0, and keep prior vault keys until every record is rewrapped.
    pub fn rewrap_budget_continuations_as_admin<F>(&self,principal:&Principal,target:u32,mut rewrap:F)->Result<(usize,usize),CoreError>
    where F:FnMut(&str,u32,&[u8])->Result<(Vec<u8>,u32),CoreError> {
        if target==0 {return Err(CoreError::ApiKeyEncryptionUnavailable);}
        self.authorize_admin_principal(principal)?;
        let ids={
            let connection=self.connection.lock().expect("core store mutex poisoned");
            let mut stmt=connection.prepare("SELECT request_id FROM budget_continuations WHERE key_version!=?1 ORDER BY request_id LIMIT 16")?;
            let ids=stmt.query_map([target],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;ids
        };
        let mut count=0;
        for id in ids {
            let Some(saved)=self.budget_continuation(&id)? else {return Err(CoreError::IdempotencyConflict);};
            if saved.key_version==target {continue;}
            let (ciphertext,version)=rewrap(&saved.encryption_context(),saved.key_version,&saved.ciphertext)?;
            if version!=target || !(28..=8*1024*1024+4096).contains(&ciphertext.len()) {return Err(CoreError::ApiKeyEncryptionUnavailable);}
            let mut connection=self.connection.lock().expect("core store mutex poisoned");
            let tx=connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            Self::authorize_admin_principal_in_transaction(&tx,principal)?;
            let changed=tx.execute("UPDATE budget_continuations SET ciphertext=?1,key_version=?2 WHERE request_id=?3 AND key_version=?4 AND ciphertext=?5",
                params![ciphertext,version,id,saved.key_version,saved.ciphertext])?;
            if changed!=1 {return Err(CoreError::IdempotencyConflict);}
            Self::insert_audit_event(&tx,&principal.user_id,"budget_continuation.vault_rewrap","budget_continuation",&id,
                serde_json::json!({"key_version":target}),chrono::Utc::now().timestamp_millis())?;
            tx.commit()?;count+=1;
        }
        let connection=self.connection.lock().expect("core store mutex poisoned");
        let remaining:i64=connection.query_row("SELECT COUNT(*) FROM budget_continuations WHERE key_version!=?1",[target],|r|r.get(0))?;
        Ok((count,remaining as usize))
    }

    pub fn save_budget_continuation(&self,principal:&Principal,request:&str,body:&serde_json::Value,key_version:u32,ciphertext:&[u8])->Result<bool,CoreError> {
        if key_version==0 || !(28..=8*1024*1024+4096).contains(&ciphertext.len()) || serde_json::to_vec(body)?.len()>8*1024*1024 {
            return Err(CoreError::Validation {field:"budget_continuation".into(),reason:"invalid encrypted checkpoint size or version".into()});
        }
        let mut connection=self.connection.lock().expect("core store mutex poisoned");
        let tx=connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let owner:Option<(String,String,Vec<u8>,String,String)>=tx.query_row(
            "SELECT r.api_key_id,r.user_id,r.request_hash,r.endpoint,r.model FROM requests r
             JOIN api_keys k ON k.id=r.api_key_id JOIN users u ON u.id=r.user_id
             WHERE r.id=?1 AND k.status='active' AND u.status='active'",
            [request],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
        let Some((key,user,hash,endpoint,model))=owner else {return Err(CoreError::InvalidRequestIdentity {user_id:principal.user_id.clone(),api_key_id:principal.key_id.clone()});};
        if key!=principal.key_id || user!=principal.user_id || endpoint!="videos" || model!="seedance"
            || hash!=crate::requests::request_hash(&endpoint,&model,body).to_vec() {return Err(CoreError::IdempotencyConflict);}
        let existing:Option<Vec<u8>>=tx.query_row("SELECT request_hash FROM budget_continuations WHERE request_id=?1",[request],|r|r.get(0)).optional()?;
        if let Some(existing)=existing {
            if existing!=hash {return Err(CoreError::IdempotencyConflict);}
            return Ok(false);
        }
        tx.execute("INSERT INTO budget_continuations(request_id,request_hash,key_version,ciphertext,created_at_ms) VALUES (?1,?2,?3,?4,?5)",
            params![request,hash,key_version,ciphertext,chrono::Utc::now().timestamp_millis()])?;
        tx.commit()?;Ok(true)
    }

    pub fn budget_continuation(&self,request:&str)->Result<Option<BudgetContinuation>,CoreError> {
        let connection=self.connection.lock().expect("core store mutex poisoned");
        continuation_in_connection(&connection,request)
    }

    pub fn pending_budget_continuations_after(&self,after:&str,limit:usize)->Result<Vec<BudgetContinuation>,CoreError> {
        if !(1..=100).contains(&limit) {return Err(CoreError::Validation {field:"continuation.limit".into(),reason:"must be 1..100".into()});}
        let connection=self.connection.lock().expect("core store mutex poisoned");
        let mut stmt=connection.prepare("SELECT c.request_id FROM budget_continuations c
            JOIN budget_operations o ON o.parent_request_id=c.request_id
            WHERE c.request_id>?1 AND o.execution_state IN ('ready','running','unknown')
            AND EXISTS(SELECT 1 FROM budget_steps s WHERE s.operation_id=o.operation_id AND s.kind='assist' AND s.execution_state='succeeded')
            AND NOT EXISTS(SELECT 1 FROM budget_steps s WHERE s.operation_id=o.operation_id AND s.kind='video')
            ORDER BY c.request_id LIMIT ?2")?;
        let ids=stmt.query_map(params![after,limit as i64],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        ids.into_iter().map(|id|continuation_in_connection(&connection,&id)?.ok_or(CoreError::RequestNotFound {request_id:id})).collect()
    }
}
fn continuation_in_connection(connection:&Connection,request:&str)->Result<Option<BudgetContinuation>,CoreError> {
    let row=connection.query_row("SELECT r.api_key_id,r.user_id,r.request_hash,c.request_hash,c.key_version,c.ciphertext,k.scopes_json,k.status,u.status
        FROM budget_continuations c JOIN requests r ON r.id=c.request_id JOIN api_keys k ON k.id=r.api_key_id JOIN users u ON u.id=r.user_id WHERE c.request_id=?1",
        [request],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,Vec<u8>>(2)?,r.get::<_,Vec<u8>>(3)?,r.get::<_,u32>(4)?,r.get::<_,Vec<u8>>(5)?,r.get::<_,String>(6)?,r.get::<_,String>(7)?,r.get::<_,String>(8)?))).optional()?;
    let Some((key,user,hash,checkpoint_hash,version,ciphertext,scopes,key_status,user_status))=row else {return Ok(None);};
    if hash.len()!=32 || hash!=checkpoint_hash {return Err(CoreError::IdempotencyConflict);}
    let principal=if key_status=="active" && user_status=="active" {Some(Principal {key_id:key.clone(),user_id:user,scopes:serde_json::from_str(&scopes)?})} else {None};
    Ok(Some(BudgetContinuation {request_id:request.into(),api_key_id:key,fingerprint:URL_SAFE_NO_PAD.encode(hash),key_version:version,ciphertext,principal}))
}
