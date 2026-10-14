//! Reservation release is not a zero-fee receipt. Keep recovery alive for late bills.
use base64::{engine::general_purpose::URL_SAFE_NO_PAD,Engine};
use rusqlite::{params,Connection,OptionalExtension,TransactionBehavior};
use serde::{Deserialize,Serialize};
use crate::{CoreStore,CoreError,BudgetMutation,ReservationState,canonical_json_hash};

#[derive(Clone,Debug,Serialize,Deserialize,PartialEq,Eq)]
#[serde(deny_unknown_fields)]
pub struct BudgetHoldRelease {
    pub wire_version:u8,pub request_id:String,pub budget_id:String,pub core_key_id:String,
    pub account_ref:String,pub bridge_instance_id:String,pub session_ref:String,
    pub hold_microcredits:i64,pub policy:String,pub review_ref:Option<String>,
    pub started_at_ms:i64,pub finished_at_ms:i64,pub query_start_ms:i64,pub query_end_ms:i64,
    pub first_read_at_ms:i64,pub last_read_at_ms:i64,pub observed_at_ms:i64,pub source_hash:String,
}
impl BudgetHoldRelease {
    pub fn validate(&self)->bool {
        self.wire_version==2 && [&self.request_id,&self.budget_id,&self.core_key_id,&self.account_ref,&self.bridge_instance_id,&self.source_hash]
            .iter().all(|s|!s.trim().is_empty() && s.len()<=256)
            && self.session_ref==format!("core-v2-{}",self.budget_id) && self.hold_microcredits>0
            && self.started_at_ms>0 && self.finished_at_ms>=self.started_at_ms
            && self.query_start_ms>0 && self.query_start_ms<=self.started_at_ms
            && self.first_read_at_ms.saturating_sub(self.finished_at_ms)>=900_000
            && self.last_read_at_ms.saturating_sub(self.first_read_at_ms)>=60_000
            && self.observed_at_ms>=self.last_read_at_ms && self.query_end_ms>=self.last_read_at_ms
            && match self.policy.as_str() {
                "failed-absence-reservation-release-v1"=>self.review_ref.is_none(),
                "reviewed-absence-reservation-release-v1"=>self.review_ref.as_ref().is_some_and(|s|!s.trim().is_empty() && s.len()<=256),
                _=>false,
            }
    }
    pub fn evidence_hash(&self)->String {URL_SAFE_NO_PAD.encode(canonical_json_hash(&serde_json::to_value(self).expect("release proof serializable")))}
}
fn invalid()->CoreError {CoreError::BillingReceiptInvalid {reason:"hold release evidence or identity mismatch".into()}}
impl CoreStore {
    pub(crate) fn hold_release_in_connection(db:&Connection,request:&str)->Result<Option<BudgetHoldRelease>,CoreError> {
        let row:Option<(String,String)>=db.query_row("SELECT proof_json,evidence_hash FROM budget_hold_releases WHERE request_id=?1",[request],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        row.map(|(json,hash)| {
            let proof:BudgetHoldRelease=serde_json::from_str(&json).map_err(|_|invalid())?;
            if !proof.validate() || proof.request_id!=request || proof.evidence_hash()!=hash {return Err(invalid());}Ok(proof)
        }).transpose()
    }
    pub fn release_budget_hold(&self,proof:&BudgetHoldRelease)->Result<BudgetMutation,CoreError> {
        let now=chrono::Utc::now().timestamp_millis();
        if !proof.validate() || proof.observed_at_ms>now+300_000 {return Err(invalid());}
        let mut db=self.connection.lock().expect("core store mutex poisoned");
        let tx=db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row:(String,String,String,String,String,i64,i64,String,String,String)=tx.query_row(
            "SELECT budget_id,core_key_id,account_ref,bridge_instance_id,reservation_id,hold_microcredits,dispatch_attempted,execution_state,financial_state,request_id FROM budget_steps WHERE request_id=?1",
            [&proof.request_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?,r.get(9)?)))?;
        if (row.0.as_str(),row.1.as_str(),row.2.as_str(),row.3.as_str(),row.5,row.6)!=(proof.budget_id.as_str(),proof.core_key_id.as_str(),proof.account_ref.as_str(),proof.bridge_instance_id.as_str(),proof.hold_microcredits,1) {return Err(invalid());}
        if let Some(old)=Self::hold_release_in_connection(&tx,&proof.request_id)? {
            return if old==*proof {Ok(BudgetMutation::Duplicate)}else{Err(CoreError::IdempotencyConflict)};
        }
        // The real receipt event may overtake the release observation. Once
        // settled there is no remaining hold; never credit it a second time.
        if row.8=="settled" {return Ok(BudgetMutation::Duplicate);}
        if !matches!(row.8.as_str(),"held"|"unknown") || !(row.7=="failed" || (row.7=="unknown" && proof.review_ref.is_some())) {return Err(invalid());}
        let reservation=Self::reservation_by_id(&tx,&row.4)?.ok_or_else(||invalid())?;
        if reservation.request_id!=proof.request_id || reservation.api_key_id.as_deref()!=Some(proof.core_key_id.as_str()) || reservation.amount!=proof.hold_microcredits
            || reservation.resource_kind!="credits" || !matches!(reservation.state,ReservationState::Held|ReservationState::Unknown) {return Err(invalid());}
        let conflicting:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM budget_settlements WHERE request_id=?1) OR EXISTS(SELECT 1 FROM budget_receipt_evidence WHERE request_id=?1 AND (record_kind='conflict' OR receipt_status IN ('final','failed_no_charge')))",[&proof.request_id],|r|r.get(0))?;
        if conflicting {return Err(invalid());}
        tx.execute("INSERT INTO budget_hold_releases(request_id,budget_id,evidence_hash,proof_json,released_at_ms) VALUES (?1,?2,?3,?4,?5)",params![proof.request_id,proof.budget_id,proof.evidence_hash(),serde_json::to_string(proof).map_err(|_|invalid())?,now])?;
        Self::set_reservation_state(&tx,&reservation.id,ReservationState::Released,now)?;
        Self::insert_reservation_budget_event(&tx,&reservation,"release",reservation.amount,reservation.amount,Some(&proof.policy),now,reservation.event_group_id.as_deref().ok_or_else(invalid)?)?;
        // Unknown means the final bill remains open. The separate reservation
        // state removes the hold without inventing an upstream zero settlement.
        tx.execute("UPDATE budget_steps SET financial_state='unknown',updated_at_ms=?1 WHERE request_id=?2",params![now,proof.request_id])?;
        tx.commit()?;Ok(BudgetMutation::Applied)
    }
}
