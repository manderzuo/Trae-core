//! Slot ownership is independent of execution truth and accounting. Released
//! identities remain queryable, but can never authorize another paid dispatch.
use crate::{BudgetMutation, CoreError, CoreStore};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

/// Trusted operator evidence, not accepted from ordinary request bodies. The
/// operator must observe the original native worker and global execution state.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReviewedExecutionSlotRelease {
    pub request_id: String,
    pub core_key_id: String,
    pub budget_id: String,
    pub account_ref: String,
    pub bridge_instance_id: String,
    pub task_ref: String,
    pub worker_absent_since_ms: i64,
    pub observed_at_ms: i64,
    pub review_ref: String,
}

impl CoreStore {
    pub(crate) fn execution_slot_released_in_connection(db: &Connection, parent: &str) -> Result<bool, CoreError> {
        Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM budget_execution_slot_releases WHERE parent_request_id=?1)", [parent], |r|r.get(0))?)
    }

    /// Both this method and add/dispatch hold an IMMEDIATE transaction. An
    /// attention decision cannot race a new video step into an uncounted slot.
    pub fn release_attention_execution_slots(&self, limit: usize) -> Result<usize, CoreError> {
        if !(1..=100).contains(&limit) { return Err(CoreError::Validation {field:"execution_slot.limit".into(), reason:"must be 1..100".into()}); }
        let mut db=self.connection.lock().expect("core store mutex poisoned");
        let tx=db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ids={
            let mut query=tx.prepare("SELECT o.parent_request_id FROM budget_operations o
                JOIN budget_continuation_retries r ON r.request_id=o.parent_request_id
                WHERE r.state='attention' AND o.execution_state IN ('ready','running','unknown')
                  AND EXISTS(SELECT 1 FROM budget_steps s WHERE s.operation_id=o.operation_id AND s.kind='assist')
                  AND NOT EXISTS(SELECT 1 FROM budget_steps s WHERE s.operation_id=o.operation_id AND (s.kind='video' OR s.execution_state NOT IN ('succeeded','failed','canceled')))
                  AND NOT EXISTS(SELECT 1 FROM budget_execution_slot_releases e WHERE e.parent_request_id=o.parent_request_id)
                ORDER BY o.parent_request_id LIMIT ?1")?;
            let ids=query.query_map([limit as i64],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;ids
        };
        let now=chrono::Utc::now().timestamp_millis();
        for id in &ids {
            tx.execute("INSERT INTO budget_execution_slot_releases(parent_request_id,reason,proof_json,released_at_ms) VALUES (?1,'continuation_attention','{}',?2)",params![id,now])?;
            Self::insert_audit_event(&tx,"system","budget.execution_slot_released","request",id,serde_json::json!({"reason":"continuation_attention"}),now)?;
        }
        tx.commit()?;Ok(ids.len())
    }

    /// Reviewed worker absence only retires the slot, never the unknown result.
    /// Keep the original identity for deduplication and late read-only recovery.
    pub fn release_reviewed_unknown_execution_slot(&self, proof: &ReviewedExecutionSlotRelease) -> Result<BudgetMutation, CoreError> {
        let invalid=||CoreError::Validation {field:"execution_slot.review".into(),reason:"worker review or request identity does not match".into()};
        let now=chrono::Utc::now().timestamp_millis();
        if [&proof.request_id,&proof.core_key_id,&proof.budget_id,&proof.account_ref,&proof.bridge_instance_id,&proof.task_ref,&proof.review_ref].iter().any(|v|v.trim().is_empty() || v.len()>256)
            || proof.worker_absent_since_ms<=0 || proof.worker_absent_since_ms>proof.observed_at_ms.saturating_sub(1_800_000)
            || proof.observed_at_ms>now+60_000 || proof.observed_at_ms<now-300_000 { return Err(invalid()); }
        let mut db=self.connection.lock().expect("core store mutex poisoned");
        let tx=db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let serialized=serde_json::to_string(proof)?;
        let prior:Option<String>=tx.query_row("SELECT proof_json FROM budget_execution_slot_releases WHERE parent_request_id=?1",[&proof.request_id],|r|r.get(0)).optional()?;
        if let Some(prior)=prior { return if prior==serialized {Ok(BudgetMutation::Duplicate)} else {Err(CoreError::IdempotencyConflict)}; }
        let eligible:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM budget_operations o
            JOIN budget_steps s ON s.operation_id=o.operation_id
            WHERE o.parent_request_id=?1 AND o.api_key_id=?2 AND o.execution_state='unknown' AND o.created_at_ms<=?7
              AND s.request_id=?1 AND s.core_key_id=?2 AND s.budget_id=?3 AND s.account_ref=?4 AND s.bridge_instance_id=?5 AND s.task_ref=?6
              AND s.kind='video' AND s.dispatch_attempted=1 AND s.execution_state='unknown' AND s.financial_state='settled'
              AND NOT EXISTS(SELECT 1 FROM budget_steps other WHERE other.operation_id=o.operation_id AND (other.financial_state<>'settled' OR (other.request_id<>?1 AND other.execution_state NOT IN ('succeeded','failed','canceled')))))",
            params![proof.request_id,proof.core_key_id,proof.budget_id,proof.account_ref,proof.bridge_instance_id,proof.task_ref,now-86_400_000],|r|r.get(0))?;
        if !eligible {return Err(invalid());}
        tx.execute("INSERT INTO budget_execution_slot_releases(parent_request_id,reason,proof_json,released_at_ms) VALUES (?1,'reviewed_worker_absence',?2,?3)",params![proof.request_id,serialized,now])?;
        Self::insert_audit_event(&tx,"operator","budget.execution_slot_released","request",&proof.request_id,serde_json::json!({"reason":"reviewed_worker_absence","review_ref":proof.review_ref}),now)?;
        tx.commit()?;Ok(BudgetMutation::Applied)
    }
}
