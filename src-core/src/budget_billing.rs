use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

use crate::{
    canonical_json_hash, BeginRequest, BeginRequestInput, BillingReceipt, CoreError, CoreStore,
    BillingReceiptStatus, CreditAmount, QuotaReserve, RequestResult, RequestState,
    ReservationState, ReserveResult,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetStepKind {
    Assist,
    Video,
    Chat,
}

impl BudgetStepKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Assist => "assist",
            Self::Video => "video",
            Self::Chat => "chat",
        }
    }

    fn from_db(value: &str) -> Option<Self> {
        match value {
            "assist" => Some(Self::Assist),
            "video" => Some(Self::Video),
            "chat" => Some(Self::Chat),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetAuthorization {
    pub budget_id: String,
    pub parent_request_id: String,
    pub request_id: String,
    pub core_key_id: String,
    pub request_fingerprint: String,
    pub endpoint: String,
    pub model: String,
    pub account_ref: String,
    pub bridge_instance_id: String,
    pub profile_fingerprint: String,
    pub policy_version: String,
    pub hold_credits: CreditAmount,
    pub expires_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetStepInput {
    pub kind: BudgetStepKind,
    pub authorization: BudgetAuthorization,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetExecutionState {
    Ready,
    Running,
    Unknown,
    Succeeded,
    Failed,
    Canceled,
}

impl BudgetExecutionState {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Unknown => "unknown",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
        }
    }

    fn from_db(value: &str) -> Option<Self> {
        match value {
            "ready" => Some(Self::Ready),
            "running" => Some(Self::Running),
            "unknown" => Some(Self::Unknown),
            "succeeded" => Some(Self::Succeeded),
            "failed" => Some(Self::Failed),
            "canceled" => Some(Self::Canceled),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetFinancialState {
    Held,
    Unknown,
    Settled,
    Released,
    Conflict,
}

impl BudgetFinancialState {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Held => "held",
            Self::Unknown => "unknown",
            Self::Settled => "settled",
            Self::Released => "released",
            Self::Conflict => "conflict",
        }
    }

    fn from_db(value: &str) -> Option<Self> {
        match value {
            "held" => Some(Self::Held),
            "unknown" => Some(Self::Unknown),
            "settled" => Some(Self::Settled),
            "released" => Some(Self::Released),
            "conflict" => Some(Self::Conflict),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BudgetMutation {
    Applied,
    Duplicate,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BudgetStepView {
    pub operation_id: String,
    pub parent_request_id: String,
    pub request_id: String,
    pub kind: BudgetStepKind,
    pub budget_id: String,
    pub core_key_id: String,
    pub request_fingerprint: String,
    pub endpoint: String,
    pub model: String,
    pub account_ref: String,
    pub bridge_instance_id: String,
    pub profile_fingerprint: String,
    pub policy_version: String,
    pub hold_credits: CreditAmount,
    pub expires_at_ms: i64,
    pub reservation_id: String,
    pub dispatch_attempted: bool,
    pub execution_state: BudgetExecutionState,
    pub financial_state: BudgetFinancialState,
    pub actual_credits: Option<CreditAmount>,
    pub task_ref: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BudgetOperationView {
    pub operation_id: String,
    pub parent_request_id: String,
    pub api_key_id: String,
    pub execution_state: BudgetExecutionState,
    pub steps: Vec<BudgetStepView>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BudgetReceiptInput {
    pub budget_id: String,
    pub account_ref: String,
    pub bridge_instance_id: String,
    pub receipt: BillingReceipt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BudgetReceiptResult {
    Settled {
        actual_credits: CreditAmount,
        released_microcredits: i64,
        debt: bool,
    },
    Pending,
    Duplicate,
    Conflict,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BudgetReceiptConflict {
    pub request_id: String,
    pub budget_id: String,
    pub account_ref: String,
    pub bridge_instance_id: String,
    pub source_ref: String,
    pub evidence_hash: String,
    pub observed_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BudgetStepRecoveryView {
    pub step: BudgetStepView,
}

struct BudgetReceiptContext {
    kind: String,
    budget_id: String,
    core_key_id: String,
    request_fingerprint: String,
    endpoint: String,
    model: String,
    account_ref: String,
    bridge_instance_id: String,
    hold_microcredits: i64,
    reservation_id: String,
    dispatch_attempted: i64,
    financial_state: String,
    task_ref: Option<String>,
    operation_key_id: String,
    user_id: String,
    request_key_id: String,
    request_endpoint: String,
    request_model: String,
    request_hash: Vec<u8>,
}

struct BudgetReceiptConflictContext {
    budget_id: String,
    account_ref: String,
    bridge_instance_id: String,
    core_key_id: String,
    hold_microcredits: i64,
    reservation_id: String,
    dispatch_attempted: i64,
    operation_key_id: String,
    user_id: String,
    request_key_id: String,
    request_fingerprint: String,
    request_hash: Vec<u8>,
}

impl CoreStore {
    pub fn begin_budget_assist_request(
        &self,
        parent_request_id: &str,
        input: BeginRequestInput,
    ) -> Result<BeginRequest, CoreError> {
        if parent_request_id.trim().is_empty() {
            return Err(CoreError::InvalidConfiguration {
                key: "request_relation.parent_request_id".into(),
                value: "must not be empty".into(),
            });
        }
        if input.endpoint != "chat" {
            return Err(CoreError::BillingQuoteMismatch {
                request_id: parent_request_id.into(),
            });
        }
        self.begin_request_internal(input, false, false, Some(parent_request_id), true)
    }

    pub fn abort_budget_preparation(
        &self,
        parent_request_id: &str,
        reason: &str,
    ) -> Result<BudgetMutation, CoreError> {
        if parent_request_id.trim().is_empty() || reason.trim().is_empty() {
            return Err(CoreError::InvalidConfiguration {
                key: "budget_preparations.abort".into(),
                value: "parent request and reason must not be empty".into(),
            });
        }
        let now = Utc::now().timestamp_millis();
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let preparation: Option<(String, String, Option<String>)> = transaction
            .query_row(
                "SELECT child_request_id, state, abort_reason FROM budget_preparations
                 WHERE parent_request_id = ?1",
                [parent_request_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((child_request_id, state, abort_reason)) = preparation else {
            return Err(CoreError::RequestNotFound {
                request_id: parent_request_id.into(),
            });
        };
        if state == "aborted" {
            if abort_reason.as_deref() == Some(reason) {
                return Ok(BudgetMutation::Duplicate);
            }
            return Err(CoreError::IdempotencyConflict);
        }
        if state != "open"
            || Self::budget_preparation_has_execution_or_billing_evidence(
                &transaction,
                parent_request_id,
                &child_request_id,
            )?
        {
            return Err(CoreError::IdempotencyConflict);
        }

        Self::fail_budget_preparation_request(&transaction, parent_request_id, now)?;
        Self::fail_budget_preparation_request(&transaction, &child_request_id, now)?;
        let changed = transaction.execute(
            "UPDATE budget_preparations SET state = 'aborted', abort_reason = ?1, updated_at_ms = ?2
             WHERE parent_request_id = ?3 AND child_request_id = ?4 AND state = 'open'",
            params![reason, now, parent_request_id, &child_request_id],
        )?;
        if changed != 1 {
            return Err(CoreError::IdempotencyConflict);
        }
        Self::insert_audit_event(
            &transaction,
            "system",
            "request.budget_preparation_aborted",
            "request",
            parent_request_id,
            serde_json::json!({"child_request_id": child_request_id, "reason": reason}),
            now,
        )?;
        transaction.commit()?;
        Ok(BudgetMutation::Applied)
    }

    pub fn recover_abandoned_budget_preparations_before(
        &self,
        startup_cutoff_ms: i64,
    ) -> Result<Vec<String>, CoreError> {
        let now = Utc::now().timestamp_millis();
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let candidates = {
            let mut statement = transaction.prepare(
                "SELECT parent_request_id, child_request_id FROM budget_preparations
                 WHERE state = 'open' AND created_at_ms < ?1
                 ORDER BY created_at_ms, parent_request_id",
            )?;
            let rows = statement.query_map([startup_cutoff_ms], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let mut recovered = Vec::new();
        for (parent_request_id, child_request_id) in candidates {
            if Self::budget_preparation_has_execution_or_billing_evidence(
                &transaction,
                &parent_request_id,
                &child_request_id,
            )? {
                continue;
            }
            Self::fail_budget_preparation_request(&transaction, &parent_request_id, now)?;
            Self::fail_budget_preparation_request(&transaction, &child_request_id, now)?;
            let changed = transaction.execute(
                "UPDATE budget_preparations SET state = 'aborted',
                   abort_reason = 'abandoned_budget_preparation_recovered', updated_at_ms = ?1
                 WHERE parent_request_id = ?2 AND child_request_id = ?3 AND state = 'open'
                   AND created_at_ms < ?4",
                params![now, &parent_request_id, &child_request_id, startup_cutoff_ms],
            )?;
            if changed == 1 {
                Self::insert_audit_event(
                    &transaction,
                    "system",
                    "request.budget_preparation_recovered",
                    "request",
                    &parent_request_id,
                    serde_json::json!({"child_request_id": child_request_id}),
                    now,
                )?;
                recovered.push(parent_request_id);
            }
        }
        transaction.commit()?;
        Ok(recovered)
    }

    pub fn mark_budget_step_dispatched(&self, request_id: &str) -> Result<BudgetMutation, CoreError> {
        let now = Utc::now().timestamp_millis();
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let step: Option<(String, String, i64, String, i64, String)> = transaction
            .query_row(
                "SELECT step.operation_id, step.core_key_id, step.expires_at_ms,
                        step.execution_state, step.dispatch_attempted, step.financial_state
                 FROM budget_steps step WHERE step.request_id = ?1",
                [request_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
            )
            .optional()?;
        let Some((operation_id, api_key_id, expires_at_ms, execution_state, attempted, financial_state)) = step else {
            return Err(CoreError::RequestNotFound {
                request_id: request_id.into(),
            });
        };
        if attempted != 0 {
            if matches!(execution_state.as_str(), "running" | "unknown") {
                return Ok(BudgetMutation::Duplicate);
            }
            return Err(CoreError::IdempotencyConflict);
        }
        if execution_state != BudgetExecutionState::Ready.as_str()
            || financial_state != BudgetFinancialState::Held.as_str()
        {
            return Err(CoreError::IdempotencyConflict);
        }
        if expires_at_ms <= now {
            return Err(CoreError::BillingQuoteExpired {
                request_id: request_id.into(),
            });
        }
        let operation_state: String = transaction.query_row(
            "SELECT execution_state FROM budget_operations WHERE operation_id = ?1",
            [&operation_id],
            |row| row.get(0),
        )?;
        if !matches!(operation_state.as_str(), "ready" | "running") {
            return Err(CoreError::IdempotencyConflict);
        }
        let active_key: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM api_keys WHERE id = ?1 AND status = 'active')",
            [&api_key_id],
            |row| row.get(0),
        )?;
        if !active_key {
            return Err(CoreError::InvalidRequestIdentity {
                user_id: String::new(),
                api_key_id,
            });
        }
        let blocked: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM api_key_billing_blocks WHERE key_id = ?1)
             OR EXISTS(SELECT 1 FROM budget_steps WHERE core_key_id = ?1 AND financial_state = 'conflict')",
            [&api_key_id],
            |row| row.get(0),
        )?;
        if blocked {
            return Err(CoreError::ApiKeyBillingBlocked { api_key_id });
        }
        let reservation = Self::reservation_by_request(&transaction, request_id)?.ok_or_else(|| {
            CoreError::ReservationNotFound {
                reservation_id: request_id.into(),
            }
        })?;
        if let Some(account_id) = reservation.key_budget_account_id.as_deref() {
            Self::ensure_budget_account_ready_in_transaction(&transaction, account_id)?;
        }
        if let Some(account_id) = reservation.user_cap_account_id.as_deref() {
            Self::ensure_budget_account_ready_in_transaction(&transaction, account_id)?;
        }
        let key_balance = reservation
            .key_budget_account_id
            .as_deref()
            .map(|id| Self::budget_balance_in_transaction(&transaction, id))
            .transpose()?;
        let user_balance = reservation
            .user_cap_account_id
            .as_deref()
            .map(|id| Self::budget_balance_in_transaction(&transaction, id))
            .transpose()?;
        let available = key_balance
            .as_ref()
            .map(|balance| balance.available)
            .into_iter()
            .chain(user_balance.as_ref().map(|balance| balance.available))
            .min()
            .unwrap_or(0);
        if available < 0 {
            return Err(CoreError::QuotaInsufficient {
                available,
                required: reservation.amount,
            });
        }

        Self::advance_request_to_dispatched(&transaction, request_id, now)?;
        let changed = transaction.execute(
            "UPDATE budget_steps SET dispatch_attempted = 1, execution_state = 'running', updated_at_ms = ?1
             WHERE request_id = ?2 AND dispatch_attempted = 0 AND execution_state = 'ready'
               AND financial_state = 'held' AND expires_at_ms > ?1",
            params![now, request_id],
        )?;
        if changed != 1 {
            return Err(CoreError::IdempotencyConflict);
        }
        let changed = transaction.execute(
            "UPDATE budget_operations SET execution_state = 'running', updated_at_ms = ?1
             WHERE operation_id = ?2 AND execution_state IN ('ready','running')",
            params![now, &operation_id],
        )?;
        if changed != 1 {
            return Err(CoreError::IdempotencyConflict);
        }
        transaction.commit()?;
        Ok(BudgetMutation::Applied)
    }

    pub fn release_unattempted_budget_step(
        &self,
        request_id: &str,
        reason: &str,
    ) -> Result<BudgetMutation, CoreError> {
        if request_id.trim().is_empty() || reason.trim().is_empty() {
            return Err(CoreError::Validation {
                field: "budget_step.release_reason".into(),
                reason: "request and non-empty reason are required".into(),
            });
        }
        let reason = reason.trim();
        let now = Utc::now().timestamp_millis();
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let step: Option<(String, i64, String, String, Option<String>, String)> = transaction
            .query_row(
                "SELECT step.kind, step.dispatch_attempted, step.execution_state,
                        step.financial_state, step.release_reason, step.reservation_id
                 FROM budget_steps step WHERE step.request_id = ?1",
                [request_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
            )
            .optional()?;
        let Some((kind, attempted, execution_state, financial_state, stored_reason, reservation_id)) = step else {
            return Err(CoreError::RequestNotFound {
                request_id: request_id.into(),
            });
        };
        if financial_state == BudgetFinancialState::Released.as_str()
            && attempted == 0
            && matches!(
                execution_state.as_str(),
                "failed" | "canceled"
            )
            && stored_reason.as_deref() == Some(reason)
        {
            transaction.commit()?;
            return Ok(BudgetMutation::Duplicate);
        }
        if attempted != 0
            || !matches!(
                execution_state.as_str(),
                "ready" | "failed" | "canceled"
            )
            || financial_state != BudgetFinancialState::Held.as_str()
        {
            return Err(CoreError::IdempotencyConflict);
        }
        let reservation = Self::reservation_by_id(&transaction, &reservation_id)?.ok_or_else(|| {
            CoreError::ReservationNotFound {
                reservation_id: reservation_id.clone(),
            }
        })?;
        if reservation.request_id != request_id || reservation.state != ReservationState::Held {
            return Err(CoreError::ReservationSettlementConflict {
                reservation_id: reservation.id,
            });
        }
        let released_execution_state = if execution_state == BudgetExecutionState::Ready.as_str() {
            BudgetExecutionState::Canceled.as_str()
        } else {
            execution_state.as_str()
        };
        if kind == BudgetStepKind::Assist.as_str()
            && execution_state == BudgetExecutionState::Ready.as_str()
        {
            Self::sync_assist_request_execution(
                &transaction,
                request_id,
                BudgetExecutionState::Canceled,
                now,
            )?;
        }
        Self::set_reservation_state(&transaction, &reservation.id, ReservationState::Released, now)?;
        let event_group_id = reservation.event_group_id.as_deref().ok_or_else(|| {
            CoreError::InvalidConfiguration {
                key: "quota_reservations.event_group_id".into(),
                value: reservation.id.clone(),
            }
        })?;
        Self::insert_reservation_budget_event(
            &transaction,
            &reservation,
            "release",
            reservation.amount,
            reservation.amount,
            Some(reason),
            now,
            event_group_id,
        )?;
        let changed = transaction.execute(
            "UPDATE budget_steps SET financial_state = 'released', execution_state = ?5,
                   release_reason = ?1, updated_at_ms = ?2
             WHERE request_id = ?3 AND dispatch_attempted = 0
               AND execution_state = ?4 AND financial_state = 'held'",
            params![reason, now, request_id, execution_state, released_execution_state],
        )?;
        if changed != 1 {
            return Err(CoreError::IdempotencyConflict);
        }
        transaction.commit()?;
        Ok(BudgetMutation::Applied)
    }

    pub fn add_budget_step(
        &self,
        parent_request_id: &str,
        input: BudgetStepInput,
    ) -> Result<BudgetOperationView, CoreError> {
        if parent_request_id.trim().is_empty()
            || input.authorization.parent_request_id != parent_request_id
        {
            return Err(CoreError::BillingQuoteMismatch {
                request_id: input.authorization.request_id,
            });
        }
        let now = Utc::now().timestamp_millis();
        validate_budget_authorization(&input, now, true)?;
        let authorization_hash = canonical_json_hash(&serde_json::to_value(&input.authorization)?).to_vec();
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let operation: Option<(String, String, String)> = transaction
            .query_row(
                "SELECT operation_id, api_key_id, execution_state FROM budget_operations
                 WHERE parent_request_id = ?1",
                [parent_request_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((operation_id, api_key_id, operation_state)) = operation else {
            return Err(CoreError::RequestNotFound {
                request_id: parent_request_id.into(),
            });
        };
        let existing_step: Option<(String, String, Vec<u8>)> = transaction
            .query_row(
                "SELECT request_id, budget_id, authorization_hash FROM budget_steps
                 WHERE operation_id = ?1 AND kind = ?2",
                params![&operation_id, input.kind.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((request_id, budget_id, stored_hash)) = existing_step {
            if request_id == input.authorization.request_id
                && budget_id == input.authorization.budget_id
                && stored_hash == authorization_hash
            {
                let view = Self::budget_operation_in_connection(&transaction, parent_request_id)?
                    .ok_or_else(|| CoreError::RequestNotFound {
                        request_id: parent_request_id.into(),
                    })?;
                transaction.commit()?;
                return Ok(view);
            }
            return Err(CoreError::IdempotencyConflict);
        }
        if !matches!(operation_state.as_str(), "ready" | "running" | "unknown")
            || input.kind == BudgetStepKind::Assist
        {
            return Err(CoreError::IdempotencyConflict);
        }
        if input.authorization.expires_at_ms <= now {
            return Err(CoreError::BillingQuoteExpired {
                request_id: input.authorization.request_id,
            });
        }
        if input.authorization.core_key_id != api_key_id {
            return Err(CoreError::InvalidRequestIdentity {
                user_id: String::new(),
                api_key_id: input.authorization.core_key_id,
            });
        }
        if input.kind == BudgetStepKind::Video {
            let assist_execution: Option<String> = transaction
                .query_row(
                    "SELECT execution_state FROM budget_steps WHERE operation_id = ?1 AND kind = 'assist'",
                    [&operation_id],
                    |row| row.get(0),
                )
                .optional()?;
            if assist_execution.as_deref().is_some_and(|state| {
                !matches!(state, "succeeded" | "failed" | "canceled")
            }) {
                return Err(CoreError::IdempotencyConflict);
            }
        }
        let parent: (String, String, String, String, Vec<u8>, String) = transaction
            .query_row(
                "SELECT user_id, api_key_id, endpoint, model, request_hash, state
                 FROM requests WHERE id = ?1",
                [parent_request_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
            )
            .optional()?
            .ok_or_else(|| CoreError::RequestNotFound {
                request_id: parent_request_id.into(),
            })?;
        if parent.1 != api_key_id || parent.5 != RequestState::Reserved.as_str() {
            return Err(CoreError::BillingQuoteMismatch {
                request_id: parent_request_id.into(),
            });
        }
        let expected_endpoint = match input.kind {
            BudgetStepKind::Video => "videos",
            BudgetStepKind::Chat => "chat",
            BudgetStepKind::Assist => unreachable!(),
        };
        if input.authorization.request_id != parent_request_id
            || parent.2 != expected_endpoint
            || input.authorization.endpoint != parent.2
            || input.authorization.model != parent.3
            || URL_SAFE_NO_PAD.encode(parent.4) != input.authorization.request_fingerprint
        {
            return Err(CoreError::BillingQuoteMismatch {
                request_id: input.authorization.request_id,
            });
        }
        let active_key: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM api_keys WHERE id = ?1 AND user_id = ?2 AND status = 'active')",
            params![&api_key_id, &parent.0],
            |row| row.get(0),
        )?;
        if !active_key {
            return Err(CoreError::InvalidRequestIdentity {
                user_id: parent.0,
                api_key_id,
            });
        }
        let blocked: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM api_key_billing_blocks WHERE key_id = ?1)
             OR EXISTS(SELECT 1 FROM budget_steps WHERE core_key_id = ?1 AND financial_state = 'conflict')",
            [&api_key_id],
            |row| row.get(0),
        )?;
        if blocked {
            return Err(CoreError::ApiKeyBillingBlocked { api_key_id });
        }
        let overlap: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM billing_quotes WHERE request_id = ?1)
             OR EXISTS(SELECT 1 FROM quota_reservations WHERE request_id = ?1)
             OR EXISTS(SELECT 1 FROM controlled_billing_operations WHERE parent_request_id = ?1)
             OR EXISTS(SELECT 1 FROM controlled_billing_steps step
               JOIN controlled_billing_operations operation USING(operation_id)
               WHERE operation.parent_request_id = ?1 OR step.request_id = ?1)
             OR EXISTS(SELECT 1 FROM budget_steps WHERE request_id = ?1)",
            [&input.authorization.request_id],
            |row| row.get(0),
        )?;
        if overlap {
            return Err(CoreError::IdempotencyConflict);
        }
        let hold_microcredits = input.authorization.hold_credits.as_microcredits();
        let ttl_ms = input.authorization.expires_at_ms.checked_sub(now).ok_or(CoreError::InvalidQuotaAmount)?;
        let reservation = match Self::reserve_dual_in_transaction(
            &transaction,
            &QuotaReserve {
                user_id: parent.0,
                request_id: input.authorization.request_id.clone(),
                resource_kind: "credits".into(),
                amount: hold_microcredits,
                ttl_ms,
            },
            &api_key_id,
            now,
            input.authorization.expires_at_ms,
        )? {
            ReserveResult::Created(reservation) => reservation,
            ReserveResult::Existing(_) => {
                return Err(CoreError::ReservationRequestConflict {
                    request_id: input.authorization.request_id,
                });
            }
            ReserveResult::Insufficient { available } => {
                return Err(CoreError::QuotaInsufficient {
                    available,
                    required: hold_microcredits,
                });
            }
        };
        transaction.execute(
            "INSERT INTO budget_steps
             (request_id, operation_id, kind, budget_id, core_key_id, request_fingerprint,
              endpoint, model, account_ref, bridge_instance_id, profile_fingerprint, policy_version,
              authorization_hash, hold_microcredits, expires_at_ms, reservation_id,
              dispatch_attempted, execution_state, financial_state, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, 0, 'ready', 'held', ?17, ?17)",
            params![
                &input.authorization.request_id,
                &operation_id,
                input.kind.as_str(),
                &input.authorization.budget_id,
                &input.authorization.core_key_id,
                &input.authorization.request_fingerprint,
                &input.authorization.endpoint,
                &input.authorization.model,
                &input.authorization.account_ref,
                &input.authorization.bridge_instance_id,
                &input.authorization.profile_fingerprint,
                &input.authorization.policy_version,
                authorization_hash,
                hold_microcredits,
                input.authorization.expires_at_ms,
                &reservation.id,
                now,
            ],
        )?;
        transaction.execute(
            "UPDATE budget_operations SET updated_at_ms = ?1 WHERE operation_id = ?2",
            params![now, &operation_id],
        )?;
        let view = Self::budget_operation_in_connection(&transaction, parent_request_id)?
            .ok_or_else(|| CoreError::RequestNotFound {
                request_id: parent_request_id.into(),
            })?;
        transaction.commit()?;
        Ok(view)
    }

    pub fn bind_budget_video_task(
        &self,
        request_id: &str,
        task_ref: &str,
    ) -> Result<BudgetMutation, CoreError> {
        if task_ref.trim().is_empty() {
            return Err(CoreError::BillingQuoteMismatch {
                request_id: request_id.into(),
            });
        }
        let now = Utc::now().timestamp_millis();
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let step: Option<(String, i64, Option<String>, String)> = transaction
            .query_row(
                "SELECT kind, dispatch_attempted, task_ref, execution_state
                 FROM budget_steps WHERE request_id = ?1",
                [request_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((kind, attempted, existing_task_ref, execution_state)) = step else {
            return Err(CoreError::RequestNotFound {
                request_id: request_id.into(),
            });
        };
        if kind != BudgetStepKind::Video.as_str() || attempted == 0 {
            return Err(CoreError::BillingQuoteMismatch {
                request_id: request_id.into(),
            });
        }
        if let Some(existing) = existing_task_ref {
            if existing == task_ref {
                transaction.commit()?;
                return Ok(BudgetMutation::Duplicate);
            }
            return Err(CoreError::IdempotencyConflict);
        }
        if matches!(execution_state.as_str(), "failed" | "canceled") {
            return Err(CoreError::IdempotencyConflict);
        }
        let changed = transaction.execute(
            "UPDATE budget_steps SET task_ref = ?1, updated_at_ms = ?2
             WHERE request_id = ?3 AND kind = 'video' AND dispatch_attempted = 1 AND task_ref IS NULL",
            params![task_ref, now, request_id],
        )?;
        if changed != 1 {
            return Err(CoreError::IdempotencyConflict);
        }
        transaction.commit()?;
        Ok(BudgetMutation::Applied)
    }

    pub fn mark_budget_step_execution(
        &self,
        request_id: &str,
        next_state: BudgetExecutionState,
    ) -> Result<BudgetMutation, CoreError> {
        if !matches!(next_state, BudgetExecutionState::Running | BudgetExecutionState::Unknown | BudgetExecutionState::Succeeded | BudgetExecutionState::Failed | BudgetExecutionState::Canceled) {
            return Err(CoreError::IdempotencyConflict);
        }
        let now = Utc::now().timestamp_millis();
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let step: Option<(String, String, i64, String, Option<String>)> = transaction
            .query_row(
                "SELECT operation_id, kind, dispatch_attempted, execution_state, task_ref
                 FROM budget_steps WHERE request_id = ?1",
                [request_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .optional()?;
        let Some((operation_id, kind, attempted, current_value, task_ref)) = step else {
            return Err(CoreError::RequestNotFound {
                request_id: request_id.into(),
            });
        };
        let current_state = BudgetExecutionState::from_db(&current_value).ok_or_else(|| {
            CoreError::InvalidConfiguration {
                key: "budget_steps.execution_state".into(),
                value: current_value,
            }
        })?;
        if current_state == next_state {
            transaction.commit()?;
            return Ok(BudgetMutation::Duplicate);
        }
        if matches!(current_state, BudgetExecutionState::Succeeded | BudgetExecutionState::Failed | BudgetExecutionState::Canceled)
            || (current_state == BudgetExecutionState::Ready
                && (attempted != 0
                    || !matches!(next_state, BudgetExecutionState::Failed | BudgetExecutionState::Canceled)))
            || (matches!(current_state, BudgetExecutionState::Running | BudgetExecutionState::Unknown)
                && attempted == 0)
        {
            return Err(CoreError::IdempotencyConflict);
        }
        if kind == BudgetStepKind::Video.as_str()
            && next_state == BudgetExecutionState::Succeeded
            && task_ref.as_deref().is_none_or(str::is_empty)
        {
            return Err(CoreError::BillingQuoteMismatch {
                request_id: request_id.into(),
            });
        }
        if kind == BudgetStepKind::Assist.as_str()
            && matches!(next_state, BudgetExecutionState::Succeeded | BudgetExecutionState::Failed | BudgetExecutionState::Canceled)
        {
            Self::sync_assist_request_execution(&transaction, request_id, next_state, now)?;
        }
        let changed = transaction.execute(
            "UPDATE budget_steps SET execution_state = ?1, updated_at_ms = ?2
             WHERE request_id = ?3 AND execution_state = ?4",
            params![next_state.as_str(), now, request_id, current_state.as_str()],
        )?;
        if changed != 1 {
            return Err(CoreError::IdempotencyConflict);
        }
        let operation_changed = transaction.execute(
            "UPDATE budget_operations SET execution_state = ?1, updated_at_ms = ?2
             WHERE operation_id = ?3 AND execution_state IN ('ready','running','unknown')",
            params![
                if next_state == BudgetExecutionState::Unknown { "unknown" } else { "running" },
                now,
                &operation_id,
            ],
        )?;
        if operation_changed != 1 {
            return Err(CoreError::IdempotencyConflict);
        }
        transaction.commit()?;
        Ok(BudgetMutation::Applied)
    }

    pub fn finish_budget_execution(
        &self,
        parent_request_id: &str,
        terminal: BudgetExecutionState,
    ) -> Result<BudgetMutation, CoreError> {
        if !matches!(terminal, BudgetExecutionState::Succeeded | BudgetExecutionState::Failed | BudgetExecutionState::Canceled) {
            return Err(CoreError::IdempotencyConflict);
        }
        let now = Utc::now().timestamp_millis();
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let operation: Option<(String, String)> = transaction
            .query_row(
                "SELECT operation_id, execution_state FROM budget_operations WHERE parent_request_id = ?1",
                [parent_request_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((operation_id, execution_value)) = operation else {
            return Err(CoreError::RequestNotFound {
                request_id: parent_request_id.into(),
            });
        };
        let current_operation = BudgetExecutionState::from_db(&execution_value).ok_or_else(|| {
            CoreError::InvalidConfiguration {
                key: "budget_operations.execution_state".into(),
                value: execution_value,
            }
        })?;
        if current_operation == terminal {
            transaction.commit()?;
            return Ok(BudgetMutation::Duplicate);
        }
        if matches!(current_operation, BudgetExecutionState::Succeeded | BudgetExecutionState::Failed | BudgetExecutionState::Canceled) {
            return Err(CoreError::IdempotencyConflict);
        }
        let (step_count, unfinished_count): (i64, i64) = transaction.query_row(
            "SELECT COUNT(*), SUM(CASE WHEN execution_state NOT IN ('succeeded','failed','canceled') THEN 1 ELSE 0 END)
             FROM budget_steps WHERE operation_id = ?1",
            [&operation_id],
            |row| Ok((row.get(0)?, row.get::<_, Option<i64>>(1)?.unwrap_or(0))),
        )?;
        if step_count == 0 || unfinished_count != 0 {
            return Err(CoreError::IdempotencyConflict);
        }
        Self::finish_parent_request_execution(&transaction, parent_request_id, terminal, now)?;
        let changed = transaction.execute(
            "UPDATE budget_operations SET execution_state = ?1, updated_at_ms = ?2
             WHERE operation_id = ?3 AND execution_state = ?4",
            params![terminal.as_str(), now, &operation_id, current_operation.as_str()],
        )?;
        if changed != 1 {
            return Err(CoreError::IdempotencyConflict);
        }
        transaction.commit()?;
        Ok(BudgetMutation::Applied)
    }

    pub fn mark_budget_receipt_conflict(
        &self,
        input: BudgetReceiptConflict,
    ) -> Result<BudgetMutation, CoreError> {
        if input.request_id.trim().is_empty()
            || input.budget_id.trim().is_empty()
            || input.account_ref.trim().is_empty()
            || input.bridge_instance_id.trim().is_empty()
            || input.source_ref.trim().is_empty()
            || input.evidence_hash.trim().is_empty()
            || input.observed_at_ms <= 0
        {
            return Err(CoreError::BillingReceiptInvalid {
                reason: "conflict request, owner, source, evidence hash, and observed time are required".into(),
            });
        }
        let now = Utc::now().timestamp_millis();
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let context: Option<BudgetReceiptConflictContext> = transaction
            .query_row(
                "SELECT step.budget_id, step.account_ref, step.bridge_instance_id,
                        step.core_key_id, step.hold_microcredits, step.reservation_id,
                        step.dispatch_attempted, operation.api_key_id, request.user_id,
                        request.api_key_id, step.request_fingerprint, request.request_hash
                 FROM budget_steps step
                 JOIN budget_operations operation ON operation.operation_id = step.operation_id
                 JOIN requests request ON request.id = step.request_id
                 WHERE step.request_id = ?1",
                [&input.request_id],
                |row| {
                    Ok(BudgetReceiptConflictContext {
                        budget_id: row.get(0)?,
                        account_ref: row.get(1)?,
                        bridge_instance_id: row.get(2)?,
                        core_key_id: row.get(3)?,
                        hold_microcredits: row.get(4)?,
                        reservation_id: row.get(5)?,
                        dispatch_attempted: row.get(6)?,
                        operation_key_id: row.get(7)?,
                        user_id: row.get(8)?,
                        request_key_id: row.get(9)?,
                        request_fingerprint: row.get(10)?,
                        request_hash: row.get(11)?,
                    })
                },
            )
            .optional()?;
        let Some(context) = context else {
            return Err(CoreError::BillingQuoteMismatch {
                request_id: input.request_id,
            });
        };
        if input.budget_id != context.budget_id
            || input.account_ref != context.account_ref
            || input.bridge_instance_id != context.bridge_instance_id
            || context.core_key_id != context.operation_key_id
            || context.core_key_id != context.request_key_id
            || URL_SAFE_NO_PAD.encode(&context.request_hash) != context.request_fingerprint
            || context.dispatch_attempted == 0
        {
            return Err(CoreError::BillingReceiptInvalid {
                reason: "conflict evidence does not match the attempted request owner".into(),
            });
        }
        let reservation = Self::reservation_by_id(&transaction, &context.reservation_id)?
            .ok_or_else(|| CoreError::ReservationNotFound {
                reservation_id: context.reservation_id.clone(),
            })?;
        if reservation.request_id != input.request_id
            || reservation.user_id != context.user_id
            || reservation.api_key_id.as_deref() != Some(context.core_key_id.as_str())
            || reservation.amount != context.hold_microcredits
            || reservation.resource_kind != "credits"
        {
            return Err(CoreError::BillingReceiptInvalid {
                reason: "conflict reservation ownership mismatch".into(),
            });
        }
        let existing: Option<String> = transaction
            .query_row(
                "SELECT record_kind FROM budget_receipt_evidence
                 WHERE request_id = ?1 AND evidence_hash = ?2",
                params![&input.request_id, &input.evidence_hash],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(record_kind) = existing {
            if record_kind == "conflict" {
                transaction.commit()?;
                return Ok(BudgetMutation::Duplicate);
            }
            return Err(CoreError::IdempotencyConflict);
        }
        transaction.execute(
            "INSERT INTO budget_receipt_evidence
             (evidence_id, request_id, budget_id, account_ref, bridge_instance_id,
              record_kind, receipt_status, actual_microcredits, unit, source_ref,
              evidence_hash, task_ref, observed_at_ms, conflict_reason, recorded_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, 'conflict', NULL, NULL, NULL, ?6, ?7, NULL, ?8, ?9, ?10)",
            params![
                format!("{}:{}", input.request_id, input.evidence_hash),
                &input.request_id,
                &input.budget_id,
                &input.account_ref,
                &input.bridge_instance_id,
                &input.source_ref,
                &input.evidence_hash,
                input.observed_at_ms,
                "upstream_receipt_conflict",
                now,
            ],
        )?;
        transaction.execute(
            "UPDATE budget_steps SET financial_state = 'conflict', updated_at_ms = ?1
             WHERE request_id = ?2 AND financial_state <> 'released'",
            params![now, &input.request_id],
        )?;
        transaction.execute(
            "INSERT OR IGNORE INTO api_key_billing_blocks
             (key_id, request_id, reason, quote_max_credits, actual_credits,
              excess_credits, source_ref, blocked_at_ms)
             VALUES (?1, ?2, 'receipt_conflict', ?3, 0, 0, ?4, ?5)",
            params![
                &context.core_key_id,
                &input.request_id,
                context.hold_microcredits,
                &input.source_ref,
                now,
            ],
        )?;
        transaction.commit()?;
        Ok(BudgetMutation::Applied)
    }

    pub fn apply_budget_receipt(
        &self,
        input: BudgetReceiptInput,
    ) -> Result<BudgetReceiptResult, CoreError> {
        let receipt = &input.receipt;
        if receipt.request_id.trim().is_empty()
            || receipt.source_ref.trim().is_empty()
            || receipt.observed_at_ms <= 0
            || receipt.unit != "credits"
        {
            return Err(CoreError::BillingReceiptInvalid {
                reason: "request, source, observed time, and credits unit are required".into(),
            });
        }
        let reported_actual = receipt.actual_credits.map(CreditAmount::as_microcredits);
        if reported_actual.is_some_and(|actual| actual < 0)
            || (receipt.status == BillingReceiptStatus::Final && reported_actual.is_none())
            || (receipt.status == BillingReceiptStatus::FailedNoCharge
                && reported_actual.is_some_and(|actual| actual != 0))
        {
            return Err(CoreError::BillingReceiptInvalid {
                reason: "final requires a nonnegative amount; failed_no_charge requires zero".into(),
            });
        }
        let now = Utc::now().timestamp_millis();
        let evidence_value = serde_json::json!({
            "request_id": receipt.request_id,
            "budget_id": input.budget_id,
            "account_ref": input.account_ref,
            "bridge_instance_id": input.bridge_instance_id,
            "status": receipt.status.as_str(),
            "actual_microcredits": reported_actual,
            "unit": receipt.unit,
            "source_ref": receipt.source_ref,
            "task_ref": receipt.task_ref,
        });
        let evidence_hash = URL_SAFE_NO_PAD.encode(canonical_json_hash(&evidence_value));
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let context: Option<BudgetReceiptContext> = transaction
            .query_row(
                "SELECT step.kind, step.budget_id, step.core_key_id,
                        step.request_fingerprint, step.endpoint, step.model, step.account_ref,
                        step.bridge_instance_id, step.hold_microcredits, step.reservation_id,
                        step.dispatch_attempted, step.financial_state,
                        step.task_ref, operation.api_key_id, request.user_id, request.api_key_id,
                        request.endpoint, request.model, request.request_hash
                 FROM budget_steps step
                 JOIN budget_operations operation ON operation.operation_id = step.operation_id
                 JOIN requests request ON request.id = step.request_id
                 WHERE step.request_id = ?1",
                [&receipt.request_id],
                |row| {
                    Ok(BudgetReceiptContext {
                        kind: row.get(0)?,
                        budget_id: row.get(1)?,
                        core_key_id: row.get(2)?,
                        request_fingerprint: row.get(3)?,
                        endpoint: row.get(4)?,
                        model: row.get(5)?,
                        account_ref: row.get(6)?,
                        bridge_instance_id: row.get(7)?,
                        hold_microcredits: row.get(8)?,
                        reservation_id: row.get(9)?,
                        dispatch_attempted: row.get(10)?,
                        financial_state: row.get(11)?,
                        task_ref: row.get(12)?,
                        operation_key_id: row.get(13)?,
                        user_id: row.get(14)?,
                        request_key_id: row.get(15)?,
                        request_endpoint: row.get(16)?,
                        request_model: row.get(17)?,
                        request_hash: row.get(18)?,
                    })
                },
            )
            .optional()?;
        let Some(context) = context else {
            return Err(CoreError::BillingQuoteMismatch {
                request_id: receipt.request_id.clone(),
            });
        };
        if input.budget_id != context.budget_id
            || input.account_ref != context.account_ref
            || input.bridge_instance_id != context.bridge_instance_id
            || context.core_key_id != context.operation_key_id
            || context.request_key_id != context.core_key_id
            || context.request_endpoint != context.endpoint
            || context.request_model != context.model
            || URL_SAFE_NO_PAD.encode(&context.request_hash) != context.request_fingerprint
        {
            return Err(CoreError::BillingReceiptInvalid {
                reason: "receipt budget, account, instance, request, or Key ownership mismatch".into(),
            });
        }
        let reservation = Self::reservation_by_id(&transaction, &context.reservation_id)?.ok_or_else(|| {
            CoreError::ReservationNotFound {
                reservation_id: context.reservation_id.clone(),
            }
        })?;
        if reservation.request_id != receipt.request_id
            || reservation.user_id != context.user_id
            || reservation.api_key_id.as_deref() != Some(context.core_key_id.as_str())
            || reservation.amount != context.hold_microcredits
            || reservation.resource_kind != "credits"
        {
            return Err(CoreError::BillingReceiptInvalid {
                reason: "receipt reservation ownership mismatch".into(),
            });
        }
        if context.dispatch_attempted == 0 {
            return Err(CoreError::BillingReceiptInvalid {
                reason: "receipt requires a recorded dispatch attempt".into(),
            });
        }
        let is_final = matches!(receipt.status, BillingReceiptStatus::Final | BillingReceiptStatus::FailedNoCharge);
        if context.kind == BudgetStepKind::Video.as_str()
            && receipt.status == BillingReceiptStatus::Final
            && context.task_ref.is_none()
        {
            return Err(CoreError::BillingReceiptInvalid {
                reason: "video task binding is not yet available for final receipt verification".into(),
            });
        }
        let existing_evidence: Option<(
            String,
            Option<String>,
            Option<i64>,
            Option<String>,
            String,
            Option<String>,
            Option<String>,
            String,
        )> = transaction
            .query_row(
                "SELECT record_kind, receipt_status, actual_microcredits, unit,
                        source_ref, task_ref, conflict_reason, evidence_id
                 FROM budget_receipt_evidence WHERE request_id = ?1 AND evidence_hash = ?2",
                params![&receipt.request_id, &evidence_hash],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                    ))
                },
            )
            .optional()?;
        let duplicate = existing_evidence.is_some_and(
            |(record_kind, status, actual, unit, source_ref, task_ref, conflict_reason, evidence_id)| {
                let receipt_semantics_match = evidence_id == format!("{}:{evidence_hash}", receipt.request_id)
                    && status.as_deref() == Some(receipt.status.as_str())
                    && actual == reported_actual
                    && unit.as_deref() == Some(receipt.unit.as_str())
                    && source_ref.as_str() == receipt.source_ref.as_str()
                    && task_ref.as_deref() == receipt.task_ref.as_deref();
                match record_kind.as_str() {
                    "receipt" => receipt_semantics_match && conflict_reason.is_none(),
                    "conflict" => {
                        receipt_semantics_match
                            && status.is_some()
                            && unit.as_deref() == Some("credits")
                            && conflict_reason.as_deref().is_some_and(|reason| !reason.trim().is_empty())
                    }
                    _ => false,
                }
            },
        );
        if duplicate {
            transaction.commit()?;
            return Ok(BudgetReceiptResult::Duplicate);
        }
        let video_task_conflict = if context.kind == BudgetStepKind::Video.as_str() {
            match receipt.status {
                BillingReceiptStatus::Final => receipt.task_ref != context.task_ref,
                BillingReceiptStatus::FailedNoCharge => {
                    context.task_ref.is_some() || receipt.task_ref.is_some()
                }
                _ => false,
            }
        } else {
            false
        };
        if video_task_conflict {
            let reason = if receipt.status == BillingReceiptStatus::FailedNoCharge {
                "failed_no_charge conflicts with a video task reference"
            } else {
                "video final receipt is not bound to the stored task"
            };
            Self::record_budget_receipt_conflict_in_transaction(
                &transaction,
                &context,
                &input,
                &evidence_hash,
                reported_actual,
                reason,
                now,
            )?;
            transaction.commit()?;
            return Ok(BudgetReceiptResult::Conflict);
        }
        let existing_settlement: Option<(i64, String, String, Option<String>, Option<String>)> = transaction
            .query_row(
                "SELECT settlement.actual_microcredits, settlement.source_ref,
                        settlement.evidence_hash, settlement.task_ref, evidence.receipt_status
                 FROM budget_settlements settlement
                 LEFT JOIN budget_receipt_evidence evidence
                   ON evidence.request_id = settlement.request_id
                  AND evidence.evidence_hash = settlement.evidence_hash
                  AND evidence.record_kind = 'receipt'
                 WHERE settlement.request_id = ?1",
                [&receipt.request_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .optional()?;
        if let Some((settled_actual, settled_source, settled_hash, settled_task, settled_status)) = existing_settlement {
            if matches!(receipt.status, BillingReceiptStatus::Final | BillingReceiptStatus::FailedNoCharge) {
                let incoming_actual = if receipt.status == BillingReceiptStatus::FailedNoCharge {
                    0
                } else {
                    reported_actual.unwrap_or(0)
                };
                let same_final = incoming_actual == settled_actual
                    && receipt.source_ref == settled_source
                    && receipt.task_ref == settled_task
                    && evidence_hash == settled_hash
                    && settled_status.as_deref() == Some(receipt.status.as_str());
                if same_final {
                    Self::insert_budget_receipt_evidence(
                        &transaction,
                        &input,
                        &evidence_hash,
                        "receipt",
                        reported_actual,
                        None,
                        now,
                    )?;
                    transaction.commit()?;
                    return Ok(BudgetReceiptResult::Duplicate);
                }
                Self::record_budget_receipt_conflict_in_transaction(
                    &transaction,
                    &context,
                    &input,
                    &evidence_hash,
                    reported_actual,
                    "conflicting final receipt after settlement",
                    now,
                )?;
                transaction.commit()?;
                return Ok(BudgetReceiptResult::Conflict);
            }
            Self::insert_budget_receipt_evidence(
                &transaction,
                &input,
                &evidence_hash,
                "receipt",
                reported_actual,
                None,
                now,
            )?;
            transaction.commit()?;
            return Ok(BudgetReceiptResult::Duplicate);
        }
        if context.financial_state == BudgetFinancialState::Conflict.as_str() {
            return Ok(BudgetReceiptResult::Conflict);
        }
        if !is_final {
            Self::insert_budget_receipt_evidence(
                &transaction,
                &input,
                &evidence_hash,
                "receipt",
                reported_actual,
                None,
                now,
            )?;
            if matches!(context.financial_state.as_str(), "held" | "unknown")
                && matches!(reservation.state, ReservationState::Held | ReservationState::Unknown)
            {
                Self::set_reservation_state(&transaction, &reservation.id, ReservationState::Unknown, now)?;
                transaction.execute(
                    "UPDATE budget_steps SET financial_state = 'unknown', updated_at_ms = ?1
                     WHERE request_id = ?2 AND financial_state IN ('held','unknown')",
                    params![now, &receipt.request_id],
                )?;
            }
            transaction.commit()?;
            return Ok(if matches!(context.financial_state.as_str(), "released" | "settled") {
                BudgetReceiptResult::Duplicate
            } else {
                BudgetReceiptResult::Pending
            });
        }
        if !matches!(context.financial_state.as_str(), "held" | "unknown")
            || !matches!(reservation.state, ReservationState::Held | ReservationState::Unknown)
        {
            return Err(CoreError::ReservationSettlementConflict {
                reservation_id: reservation.id,
            });
        }
        let actual_microcredits = if receipt.status == BillingReceiptStatus::FailedNoCharge {
            0
        } else {
            reported_actual.ok_or_else(|| CoreError::BillingReceiptInvalid {
                reason: "final receipt has no actual amount".into(),
            })?
        };
        let actual_credits = CreditAmount::try_from_microcredits(actual_microcredits)
            .ok_or(CoreError::InvalidQuotaAmount)?;
        let released_microcredits = context
            .hold_microcredits
            .checked_sub(actual_microcredits)
            .ok_or(CoreError::InvalidQuotaAmount)?;
        Self::insert_budget_receipt_evidence(
            &transaction,
            &input,
            &evidence_hash,
            "receipt",
            Some(actual_microcredits),
            None,
            now,
        )?;
        Self::set_reservation_state(&transaction, &reservation.id, ReservationState::Committed, now)?;
        Self::insert_reservation_budget_event(
            &transaction,
            &reservation,
            "commit",
            actual_microcredits,
            released_microcredits,
            None,
            now,
            reservation.event_group_id.as_deref().ok_or_else(|| CoreError::InvalidConfiguration {
                key: "quota_reservations.event_group_id".into(),
                value: reservation.id.clone(),
            })?,
        )?;
        let changed = transaction.execute(
            "UPDATE budget_steps SET financial_state = 'settled', actual_microcredits = ?1, updated_at_ms = ?2
             WHERE request_id = ?3 AND financial_state IN ('held','unknown') AND dispatch_attempted = 1",
            params![actual_microcredits, now, &receipt.request_id],
        )?;
        if changed != 1 {
            return Err(CoreError::IdempotencyConflict);
        }
        let key_balance = reservation
            .key_budget_account_id
            .as_deref()
            .map(|id| Self::budget_balance_in_transaction(&transaction, id))
            .transpose()?;
        let user_balance = reservation
            .user_cap_account_id
            .as_deref()
            .map(|id| Self::budget_balance_in_transaction(&transaction, id))
            .transpose()?;
        let debt = key_balance.as_ref().is_some_and(|balance| balance.available < 0)
            || user_balance.as_ref().is_some_and(|balance| balance.available < 0);
        transaction.execute(
            "INSERT INTO budget_settlements
             (request_id, reservation_id, budget_id, account_ref, bridge_instance_id,
              source_ref, evidence_hash, task_ref, hold_microcredits, actual_microcredits,
              released_microcredits, debt, settled_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                &receipt.request_id,
                &reservation.id,
                &input.budget_id,
                &input.account_ref,
                &input.bridge_instance_id,
                &receipt.source_ref,
                &evidence_hash,
                receipt.task_ref.as_deref(),
                context.hold_microcredits,
                actual_microcredits,
                released_microcredits,
                i64::from(debt),
                now,
            ],
        )?;
        transaction.commit()?;
        Ok(BudgetReceiptResult::Settled {
            actual_credits,
            released_microcredits,
            debt,
        })
    }

    fn insert_budget_receipt_evidence(
        transaction: &Connection,
        input: &BudgetReceiptInput,
        evidence_hash: &str,
        record_kind: &str,
        actual_microcredits: Option<i64>,
        conflict_reason: Option<&str>,
        now: i64,
    ) -> Result<(), CoreError> {
        transaction.execute(
            "INSERT OR IGNORE INTO budget_receipt_evidence
             (evidence_id, request_id, budget_id, account_ref, bridge_instance_id,
              record_kind, receipt_status, actual_microcredits, unit, source_ref,
              evidence_hash, task_ref, observed_at_ms, conflict_reason, recorded_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                format!("{}:{evidence_hash}", input.receipt.request_id),
                &input.receipt.request_id,
                &input.budget_id,
                &input.account_ref,
                &input.bridge_instance_id,
                record_kind,
                input.receipt.status.as_str(),
                actual_microcredits,
                &input.receipt.unit,
                &input.receipt.source_ref,
                evidence_hash,
                input.receipt.task_ref.as_deref(),
                input.receipt.observed_at_ms,
                conflict_reason,
                now,
            ],
        )?;
        Ok(())
    }

    fn record_budget_receipt_conflict_in_transaction(
        transaction: &Connection,
        context: &BudgetReceiptContext,
        input: &BudgetReceiptInput,
        evidence_hash: &str,
        actual_microcredits: Option<i64>,
        reason: &str,
        now: i64,
    ) -> Result<(), CoreError> {
        Self::insert_budget_receipt_evidence(
            transaction,
            input,
            evidence_hash,
            "conflict",
            actual_microcredits,
            Some(reason),
            now,
        )?;
        transaction.execute(
            "UPDATE budget_steps SET financial_state = 'conflict', updated_at_ms = ?1
             WHERE request_id = ?2 AND financial_state <> 'released'",
            params![now, &input.receipt.request_id],
        )?;
        transaction.execute(
            "INSERT OR IGNORE INTO api_key_billing_blocks
             (key_id, request_id, reason, quote_max_credits, actual_credits,
              excess_credits, source_ref, blocked_at_ms)
             VALUES (?1, ?2, 'receipt_conflict', ?3, ?4, 0, ?5, ?6)",
            params![
                &context.core_key_id,
                &input.receipt.request_id,
                context.hold_microcredits,
                actual_microcredits.unwrap_or(0),
                &input.receipt.source_ref,
                now,
            ],
        )?;
        Ok(())
    }

    fn finish_parent_request_execution(
        connection: &Connection,
        parent_request_id: &str,
        terminal: BudgetExecutionState,
        now: i64,
    ) -> Result<(), CoreError> {
        let state_value: String = connection
            .query_row("SELECT state FROM requests WHERE id = ?1", [parent_request_id], |row| row.get(0))
            .optional()?
            .ok_or_else(|| CoreError::RequestNotFound {
                request_id: parent_request_id.into(),
            })?;
        let mut state = RequestState::from_db(&state_value).ok_or_else(|| CoreError::InvalidConfiguration {
            key: "requests.state".into(),
            value: state_value,
        })?;
        let target = match terminal {
            BudgetExecutionState::Succeeded => RequestState::Succeeded,
            BudgetExecutionState::Failed => RequestState::Failed,
            BudgetExecutionState::Canceled => RequestState::Canceled,
            _ => return Err(CoreError::IdempotencyConflict),
        };
        if state == target {
            return Ok(());
        }
        if matches!(state, RequestState::Succeeded | RequestState::Failed | RequestState::Canceled | RequestState::Settled) {
            return Err(CoreError::IdempotencyConflict);
        }
        if state == RequestState::Received {
            Self::transition_request_on_connection(connection, parent_request_id, state, RequestState::Validating, None, now)?;
            state = RequestState::Validating;
        }
        if terminal == BudgetExecutionState::Succeeded {
            if state == RequestState::Validating {
                Self::transition_request_on_connection(connection, parent_request_id, state, RequestState::Reserved, None, now)?;
                state = RequestState::Reserved;
            }
            if state == RequestState::Reserved {
                Self::transition_request_on_connection(connection, parent_request_id, state, RequestState::Queued, None, now)?;
                state = RequestState::Queued;
            }
            if state == RequestState::Queued {
                Self::transition_request_on_connection(connection, parent_request_id, state, RequestState::Dispatched, None, now)?;
                state = RequestState::Dispatched;
            }
            if state == RequestState::Dispatched {
                Self::transition_request_on_connection(connection, parent_request_id, state, RequestState::Completing, None, now)?;
                state = RequestState::Completing;
            }
        } else if terminal == BudgetExecutionState::Canceled
            && matches!(state, RequestState::Validating | RequestState::Reserved | RequestState::Queued | RequestState::Dispatched | RequestState::Completing)
        {
            if state == RequestState::Validating {
                Self::transition_request_on_connection(connection, parent_request_id, state, RequestState::Reserved, None, now)?;
                state = RequestState::Reserved;
            }
            Self::transition_request_on_connection(connection, parent_request_id, state, RequestState::CancelRequested, None, now)?;
            state = RequestState::CancelRequested;
        } else if terminal == BudgetExecutionState::Failed && state == RequestState::CancelRequested {
            Self::transition_request_on_connection(connection, parent_request_id, state, RequestState::Unknown, None, now)?;
            state = RequestState::Unknown;
        }
        Self::transition_request_on_connection(connection, parent_request_id, state, target, None, now)
    }

    fn sync_assist_request_execution(
        connection: &Connection,
        request_id: &str,
        execution_state: BudgetExecutionState,
        now: i64,
    ) -> Result<(), CoreError> {
        let state_value: String = connection
            .query_row("SELECT state FROM requests WHERE id = ?1", [request_id], |row| row.get(0))
            .optional()?
            .ok_or_else(|| CoreError::RequestNotFound {
                request_id: request_id.into(),
            })?;
        let mut state = RequestState::from_db(&state_value).ok_or_else(|| CoreError::InvalidConfiguration {
            key: "requests.state".into(),
            value: state_value,
        })?;
        if state == RequestState::Received {
            Self::transition_request_on_connection(connection, request_id, state, RequestState::Validating, None, now)?;
            state = RequestState::Validating;
        }
        if state == RequestState::Validating && execution_state != BudgetExecutionState::Failed {
            Self::transition_request_on_connection(connection, request_id, state, RequestState::Reserved, None, now)?;
            state = RequestState::Reserved;
        }
        if state == RequestState::Reserved && execution_state == BudgetExecutionState::Canceled {
            Self::transition_request_on_connection(connection, request_id, state, RequestState::CancelRequested, None, now)?;
            state = RequestState::CancelRequested;
        }
        if state == RequestState::Queued && execution_state == BudgetExecutionState::Canceled {
            Self::transition_request_on_connection(connection, request_id, state, RequestState::CancelRequested, None, now)?;
            state = RequestState::CancelRequested;
        }
        let target = match execution_state {
            BudgetExecutionState::Unknown => RequestState::Unknown,
            BudgetExecutionState::Succeeded => RequestState::Succeeded,
            BudgetExecutionState::Failed => RequestState::Failed,
            BudgetExecutionState::Canceled => RequestState::Canceled,
            _ => return Err(CoreError::IdempotencyConflict),
        };
        if execution_state == BudgetExecutionState::Succeeded && state == RequestState::Dispatched {
            Self::transition_request_on_connection(connection, request_id, state, RequestState::Completing, None, now)?;
            state = RequestState::Completing;
        }
        if execution_state == BudgetExecutionState::Canceled
            && matches!(state, RequestState::Reserved | RequestState::Queued | RequestState::Dispatched | RequestState::Completing)
        {
            Self::transition_request_on_connection(connection, request_id, state, RequestState::CancelRequested, None, now)?;
            state = RequestState::CancelRequested;
        }
        if state == target {
            return Ok(());
        }
        Self::transition_request_on_connection(connection, request_id, state, target, None, now)
    }

    fn advance_request_to_dispatched(
        connection: &Connection,
        request_id: &str,
        now: i64,
    ) -> Result<(), CoreError> {
        let state_value: String = connection
            .query_row("SELECT state FROM requests WHERE id = ?1", [request_id], |row| row.get(0))
            .optional()?
            .ok_or_else(|| CoreError::RequestNotFound {
                request_id: request_id.into(),
            })?;
        let mut state = RequestState::from_db(&state_value).ok_or_else(|| CoreError::InvalidConfiguration {
            key: "requests.state".into(),
            value: state_value,
        })?;
        if state == RequestState::Received {
            Self::transition_request_on_connection(connection, request_id, state, RequestState::Validating, None, now)?;
            state = RequestState::Validating;
        }
        if state == RequestState::Validating {
            Self::transition_request_on_connection(connection, request_id, state, RequestState::Reserved, None, now)?;
            state = RequestState::Reserved;
        }
        if state == RequestState::Reserved {
            Self::transition_request_on_connection(connection, request_id, state, RequestState::Queued, None, now)?;
            state = RequestState::Queued;
        }
        if state != RequestState::Queued {
            return Err(CoreError::InvalidTransition {
                request_id: request_id.into(),
                expected: state,
                next: RequestState::Dispatched,
            });
        }
        Self::transition_request_on_connection(connection, request_id, state, RequestState::Dispatched, None, now)
    }

    fn budget_preparation_has_execution_or_billing_evidence(
        connection: &Connection,
        parent_request_id: &str,
        child_request_id: &str,
    ) -> Result<bool, CoreError> {
        connection
            .query_row(
                "SELECT
                   (SELECT COUNT(*) FROM requests parent JOIN requests child ON child.id = ?2
                    WHERE parent.id = ?1 AND parent.endpoint = 'videos' AND child.endpoint = 'chat'
                      AND parent.user_id = child.user_id AND parent.api_key_id = child.api_key_id
                      AND parent.state IN ('received','validating')
                      AND child.state IN ('received','validating')) = 0
                   OR EXISTS(SELECT 1 FROM quota_reservations WHERE request_id IN (?1, ?2))
                   OR EXISTS(SELECT 1 FROM billing_quotes WHERE request_id IN (?1, ?2))
                   OR EXISTS(SELECT 1 FROM jobs WHERE request_id IN (?1, ?2))
                   OR EXISTS(SELECT 1 FROM upstream_leases WHERE request_id IN (?1, ?2))
                   OR EXISTS(SELECT 1 FROM controlled_billing_operations WHERE parent_request_id = ?1)
                   OR EXISTS(SELECT 1 FROM controlled_billing_steps step
                     JOIN controlled_billing_operations operation USING(operation_id)
                     WHERE operation.parent_request_id = ?1 OR step.request_id IN (?1, ?2))
                   OR EXISTS(SELECT 1 FROM budget_operations WHERE parent_request_id = ?1)
                   OR EXISTS(SELECT 1 FROM budget_steps
                     WHERE request_id IN (?1, ?2) AND dispatch_attempted = 1)",
                params![parent_request_id, child_request_id],
                |row| row.get(0),
            )
            .map_err(CoreError::from)
    }

    fn fail_budget_preparation_request(
        connection: &Connection,
        request_id: &str,
        now: i64,
    ) -> Result<(), CoreError> {
        let state: String = connection
            .query_row("SELECT state FROM requests WHERE id = ?1", [request_id], |row| row.get(0))
            .optional()?
            .ok_or_else(|| CoreError::RequestNotFound {
                request_id: request_id.into(),
            })?;
        let mut state = RequestState::from_db(&state).ok_or_else(|| CoreError::InvalidConfiguration {
            key: "requests.state".into(),
            value: state,
        })?;
        if state == RequestState::Received {
            Self::transition_request_on_connection(
                connection,
                request_id,
                RequestState::Received,
                RequestState::Validating,
                None,
                now,
            )?;
            state = RequestState::Validating;
        }
        if state != RequestState::Validating {
            return Err(CoreError::IdempotencyConflict);
        }
        Self::transition_request_on_connection(
            connection,
            request_id,
            RequestState::Validating,
            RequestState::Failed,
            Some(RequestResult {
                status: None,
                error_code: Some("budget_preparation_aborted".into()),
            }),
            now,
        )
    }

    pub fn begin_budget_operation(
        &self,
        parent_request_id: &str,
        input: BudgetStepInput,
    ) -> Result<BudgetOperationView, CoreError> {
        if parent_request_id.trim().is_empty()
            || input.authorization.parent_request_id != parent_request_id
        {
            return Err(CoreError::BillingQuoteMismatch {
                request_id: input.authorization.request_id,
            });
        }
        let now = Utc::now().timestamp_millis();
        validate_budget_authorization(&input, now, true)?;
        let expected_endpoint = match input.kind {
            BudgetStepKind::Chat => Some("chat"),
            BudgetStepKind::Video => Some("videos"),
            BudgetStepKind::Assist => None,
        };
        if expected_endpoint.is_some_and(|endpoint| input.authorization.endpoint != endpoint) {
            return Err(CoreError::BillingQuoteMismatch {
                request_id: input.authorization.request_id,
            });
        }
        let authorization_hash = canonical_json_hash(&serde_json::to_value(&input.authorization)?).to_vec();

        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some(existing) = Self::budget_operation_in_connection(&transaction, parent_request_id)? {
            let stored_hash: Option<Vec<u8>> = transaction
                .query_row(
                    "SELECT authorization_hash FROM budget_steps
                     WHERE operation_id = ?1 AND request_id = ?2 AND kind = ?3 AND budget_id = ?4",
                    params![
                        &existing.operation_id,
                        &input.authorization.request_id,
                        input.kind.as_str(),
                        &input.authorization.budget_id,
                    ],
                    |row| row.get(0),
                )
                .optional()?;
            if stored_hash.as_deref() == Some(authorization_hash.as_slice()) {
                transaction.commit()?;
                return Ok(existing);
            }
            return Err(CoreError::IdempotencyConflict);
        }

        if input.authorization.expires_at_ms <= now {
            return Err(CoreError::BillingQuoteExpired {
                request_id: input.authorization.request_id,
            });
        }

        let preparation: Option<(String, String)> = transaction
            .query_row(
                "SELECT child_request_id, state FROM budget_preparations WHERE parent_request_id = ?1",
                [parent_request_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        match (preparation.as_ref(), input.kind) {
            (Some((child, state)), BudgetStepKind::Assist)
                if child == &input.authorization.request_id && state == "open" => {}
            (Some(_), _) => {
                return Err(CoreError::IdempotencyConflict);
            }
            (None, BudgetStepKind::Assist) => {
                return Err(CoreError::BillingQuoteMismatch {
                    request_id: input.authorization.request_id,
                });
            }
            (None, _) => {}
        }

        let (user_id, api_key_id, parent_state): (String, String, String) = transaction
            .query_row(
                "SELECT user_id, api_key_id, state FROM requests WHERE id = ?1",
                [parent_request_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
            .ok_or_else(|| CoreError::RequestNotFound {
                request_id: parent_request_id.into(),
            })?;
        if parent_state != RequestState::Received.as_str() {
            return Err(CoreError::BillingQuoteMismatch {
                request_id: parent_request_id.into(),
            });
        }
        validate_budget_request_identity(&transaction, parent_request_id, &input, &user_id, &api_key_id)?;

        let active: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM api_keys WHERE id = ?1 AND user_id = ?2 AND status = 'active')",
            params![&api_key_id, &user_id],
            |row| row.get(0),
        )?;
        if !active {
            return Err(CoreError::InvalidRequestIdentity { user_id, api_key_id });
        }
        let blocked: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM api_key_billing_blocks WHERE key_id = ?1)
             OR EXISTS(SELECT 1 FROM budget_steps WHERE core_key_id = ?1 AND financial_state = 'conflict')",
            [&api_key_id],
            |row| row.get(0),
        )?;
        if blocked {
            return Err(CoreError::ApiKeyBillingBlocked { api_key_id });
        }

        let legacy_overlap: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM billing_quotes WHERE request_id IN (?1, ?2))
             OR EXISTS(SELECT 1 FROM quota_reservations WHERE request_id IN (?1, ?2))
             OR EXISTS(SELECT 1 FROM controlled_billing_operations WHERE parent_request_id = ?1)
             OR EXISTS(
               SELECT 1 FROM controlled_billing_steps step
               JOIN controlled_billing_operations operation USING(operation_id)
               WHERE operation.parent_request_id = ?1 OR step.request_id = ?2
             )",
            params![parent_request_id, &input.authorization.request_id],
            |row| row.get(0),
        )?;
        if legacy_overlap {
            return Err(CoreError::IdempotencyConflict);
        }

        let max_concurrency: i64 = transaction.query_row(
            "SELECT max_concurrency FROM api_keys WHERE id = ?1 AND user_id = ?2 AND status = 'active'",
            params![&api_key_id, &user_id],
            |row| row.get(0),
        )?;
        let active_concurrency = Self::active_execution_count_for_budget_admission_in_connection(
            &transaction,
            &api_key_id,
            parent_request_id,
        )?;
        if active_concurrency >= max_concurrency {
            return Err(CoreError::KeyConcurrencyExceeded {
                api_key_id,
                active_concurrency,
                max_concurrency,
            });
        }

        let hold_microcredits = input.authorization.hold_credits.as_microcredits();
        let ttl_ms = input.authorization.expires_at_ms.checked_sub(now).ok_or(CoreError::InvalidQuotaAmount)?;
        let reservation = match Self::reserve_dual_in_transaction(
            &transaction,
            &QuotaReserve {
                user_id,
                request_id: input.authorization.request_id.clone(),
                resource_kind: "credits".into(),
                amount: hold_microcredits,
                ttl_ms,
            },
            &api_key_id,
            now,
            input.authorization.expires_at_ms,
        )? {
            ReserveResult::Created(reservation) => reservation,
            ReserveResult::Existing(_) => {
                return Err(CoreError::ReservationRequestConflict {
                    request_id: input.authorization.request_id,
                });
            }
            ReserveResult::Insufficient { available } => {
                return Err(CoreError::QuotaInsufficient {
                    available,
                    required: hold_microcredits,
                });
            }
        };

        let operation_id = Self::new_id("budget-operation");
        transaction.execute(
            "INSERT INTO budget_operations
             (operation_id, parent_request_id, api_key_id, execution_state, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, 'ready', ?4, ?4)",
            params![&operation_id, parent_request_id, &api_key_id, now],
        )?;
        transaction.execute(
            "INSERT INTO budget_steps
             (request_id, operation_id, kind, budget_id, core_key_id, request_fingerprint,
              endpoint, model, account_ref, bridge_instance_id, profile_fingerprint, policy_version,
              authorization_hash, hold_microcredits, expires_at_ms, reservation_id,
              dispatch_attempted, execution_state, financial_state, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, 0, 'ready', 'held', ?17, ?17)",
            params![
                &input.authorization.request_id,
                &operation_id,
                input.kind.as_str(),
                &input.authorization.budget_id,
                &input.authorization.core_key_id,
                &input.authorization.request_fingerprint,
                &input.authorization.endpoint,
                &input.authorization.model,
                &input.authorization.account_ref,
                &input.authorization.bridge_instance_id,
                &input.authorization.profile_fingerprint,
                &input.authorization.policy_version,
                authorization_hash,
                hold_microcredits,
                input.authorization.expires_at_ms,
                &reservation.id,
                now,
            ],
        )?;

        Self::transition_request_on_connection(
            &transaction,
            parent_request_id,
            RequestState::Received,
            RequestState::Validating,
            None,
            now,
        )?;
        Self::transition_request_on_connection(
            &transaction,
            parent_request_id,
            RequestState::Validating,
            RequestState::Reserved,
            None,
            now,
        )?;
        if let Some((_, "open")) = preparation.as_ref().map(|(child, state)| (child, state.as_str())) {
            let updated = transaction.execute(
                "UPDATE budget_preparations SET state = 'admitted', updated_at_ms = ?1
                 WHERE parent_request_id = ?2 AND child_request_id = ?3 AND state = 'open'",
                params![now, parent_request_id, &input.authorization.request_id],
            )?;
            if updated != 1 {
                return Err(CoreError::IdempotencyConflict);
            }
        }
        transaction.commit()?;
        drop(connection);

        let connection = self.connection.lock().expect("core store mutex poisoned");
        Self::budget_operation_in_connection(&connection, parent_request_id)?.ok_or_else(|| {
            CoreError::InvalidConfiguration {
                key: "budget_operations.parent_request_id".into(),
                value: parent_request_id.into(),
            }
        })
    }

    pub fn budget_operation(
        &self,
        parent_request_id: &str,
    ) -> Result<Option<BudgetOperationView>, CoreError> {
        let connection = self.connection.lock().expect("core store mutex poisoned");
        Self::budget_operation_in_connection(&connection, parent_request_id)
    }

    pub fn pending_budget_steps(&self, limit: usize) -> Result<Vec<BudgetStepRecoveryView>, CoreError> {
        if !(1..=100).contains(&limit) {
            return Err(CoreError::Validation {
                field: "pending_budget_steps.limit".into(),
                reason: "must be between 1 and 100".into(),
            });
        }
        let connection = self.connection.lock().expect("core store mutex poisoned");
        let steps: Vec<(String, String)> = {
            let mut statement = connection.prepare(
                "SELECT step.request_id, operation.parent_request_id
                 FROM budget_steps step
                 JOIN budget_operations operation ON operation.operation_id = step.operation_id
                 WHERE step.financial_state IN ('held','unknown','conflict')
                    OR step.execution_state IN ('ready','running','unknown')
                 ORDER BY step.updated_at_ms, step.request_id LIMIT ?1",
            )?;
            let rows = statement
                .query_map([limit as i64], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        let mut recovery = Vec::with_capacity(steps.len());
        for (request_id, parent_request_id) in steps {
            let operation = Self::budget_operation_in_connection(&connection, &parent_request_id)?
                .ok_or_else(|| CoreError::InvalidConfiguration {
                    key: "budget_operations.parent_request_id".into(),
                    value: parent_request_id,
                })?;
            let step = operation
                .steps
                .into_iter()
                .find(|step| step.request_id == request_id)
                .ok_or_else(|| CoreError::InvalidConfiguration {
                    key: "budget_steps.request_id".into(),
                    value: request_id,
                })?;
            recovery.push(BudgetStepRecoveryView { step });
        }
        Ok(recovery)
    }

    pub fn active_execution_count_for_key(&self, api_key_id: &str) -> Result<i64, CoreError> {
        let connection = self.connection.lock().expect("core store mutex poisoned");
        Self::active_execution_count_in_connection(&connection, api_key_id, None)
    }

    pub(crate) fn active_execution_count_in_connection(
        connection: &Connection,
        api_key_id: &str,
        exclude_request_id: Option<&str>,
    ) -> Result<i64, CoreError> {
        connection
            .query_row(
                "SELECT
                   (SELECT COUNT(*) FROM budget_operations
                    WHERE api_key_id = ?1 AND execution_state IN ('ready','running','unknown'))
                   +
                   (SELECT COUNT(*) FROM requests request
                    WHERE request.api_key_id = ?1
                      AND request.state IN ('received','validating','reserved','queued','dispatched','completing','cancel_requested','unknown')
                      AND (?2 IS NULL OR request.id <> ?2)
                      AND NOT EXISTS (
                        SELECT 1 FROM request_relations relation
                        WHERE relation.child_request_id = request.id
                          AND relation.relationship_kind = 'seedance_assist'
                      )
                      AND NOT EXISTS (
                        SELECT 1 FROM budget_operations operation
                        WHERE operation.parent_request_id = request.id
                      ))",
                params![api_key_id, exclude_request_id],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub(crate) fn active_execution_count_for_budget_admission_in_connection(
        connection: &Connection,
        api_key_id: &str,
        request_id: &str,
    ) -> Result<i64, CoreError> {
        let received_at_ms: i64 = connection
            .query_row(
                "SELECT created_at_ms FROM requests WHERE id = ?1 AND api_key_id = ?2",
                params![request_id, api_key_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| CoreError::RequestNotFound {
                request_id: request_id.into(),
            })?;
        let assist_parent_request_id: Option<String> = connection
            .query_row(
                "SELECT parent_request_id FROM request_relations
                 WHERE child_request_id = ?1 AND relationship_kind = 'seedance_assist'",
                [request_id],
                |row| row.get(0),
            )
            .optional()?;
        let budget_operations_exist: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'budget_operations')",
            [],
            |row| row.get(0),
        )?;
        let active_budget_operations = if budget_operations_exist {
            connection.query_row(
                "SELECT COUNT(*) FROM budget_operations
                 WHERE api_key_id = ?1 AND execution_state IN ('ready','running','unknown')
                   AND (?2 IS NULL OR parent_request_id <> ?2)",
                params![api_key_id, assist_parent_request_id],
                |row| row.get::<_, i64>(0),
            )?
        } else {
            0
        };
        let legacy_requests_sql = if budget_operations_exist {
            "SELECT COUNT(*) FROM requests request
             WHERE request.api_key_id = ?1
               AND request.state IN ('received','validating','reserved','queued','dispatched','completing','cancel_requested','unknown')
               AND request.id <> ?2
               AND (?3 IS NULL OR request.id <> ?3)
               AND (request.state <> 'received'
                 OR request.created_at_ms < ?4
                 OR (request.created_at_ms = ?4 AND request.id < ?2))
               AND NOT EXISTS (
                 SELECT 1 FROM request_relations relation
                 WHERE relation.child_request_id = request.id
                   AND relation.relationship_kind = 'seedance_assist'
               )
               AND NOT EXISTS (
                 SELECT 1 FROM budget_operations operation
                 WHERE operation.parent_request_id = request.id
               )"
        } else {
            "SELECT COUNT(*) FROM requests request
             WHERE request.api_key_id = ?1
               AND request.state IN ('received','validating','reserved','queued','dispatched','completing','cancel_requested','unknown')
               AND request.id <> ?2
               AND (?3 IS NULL OR request.id <> ?3)
               AND (request.state <> 'received'
                 OR request.created_at_ms < ?4
                 OR (request.created_at_ms = ?4 AND request.id < ?2))
               AND NOT EXISTS (
                 SELECT 1 FROM request_relations relation
                 WHERE relation.child_request_id = request.id
                   AND relation.relationship_kind = 'seedance_assist'
               )"
        };
        let active_legacy_requests: i64 = connection.query_row(
            legacy_requests_sql,
            params![api_key_id, request_id, assist_parent_request_id, received_at_ms],
            |row| row.get(0),
        )?;
        Ok(active_budget_operations + active_legacy_requests)
    }

    fn budget_operation_in_connection(
        connection: &Connection,
        parent_request_id: &str,
    ) -> Result<Option<BudgetOperationView>, CoreError> {
        let operation: Option<(String, String, String, String)> = connection
            .query_row(
                "SELECT operation_id, parent_request_id, api_key_id, execution_state
                 FROM budget_operations WHERE parent_request_id = ?1",
                [parent_request_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((operation_id, parent_request_id, api_key_id, execution_value)) = operation else {
            return Ok(None);
        };
        let execution_state = BudgetExecutionState::from_db(&execution_value).ok_or_else(|| {
            CoreError::InvalidConfiguration {
                key: "budget_operations.execution_state".into(),
                value: execution_value,
            }
        })?;
        let mut statement = connection.prepare(
            "SELECT request_id, kind, budget_id, core_key_id, request_fingerprint, endpoint,
                    model, account_ref, bridge_instance_id, profile_fingerprint, policy_version,
                    hold_microcredits, expires_at_ms, reservation_id, dispatch_attempted,
                    execution_state, financial_state, actual_microcredits, task_ref
             FROM budget_steps WHERE operation_id = ?1 ORDER BY created_at_ms, request_id",
        )?;
        let steps = statement
            .query_map([&operation_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, i64>(11)?,
                    row.get::<_, i64>(12)?,
                    row.get::<_, String>(13)?,
                    row.get::<_, i64>(14)?,
                    row.get::<_, String>(15)?,
                    row.get::<_, String>(16)?,
                    row.get::<_, Option<i64>>(17)?,
                    row.get::<_, Option<String>>(18)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut views = Vec::with_capacity(steps.len());
        for (
            request_id,
            kind,
            budget_id,
            core_key_id,
            request_fingerprint,
            endpoint,
            model,
            account_ref,
            bridge_instance_id,
            profile_fingerprint,
            policy_version,
            hold_microcredits,
            expires_at_ms,
            reservation_id,
            dispatch_attempted,
            execution_value,
            financial_value,
            actual_microcredits,
            task_ref,
        ) in steps
        {
            views.push(BudgetStepView {
                operation_id: operation_id.clone(),
                parent_request_id: parent_request_id.clone(),
                request_id,
                kind: BudgetStepKind::from_db(&kind).ok_or_else(|| CoreError::InvalidConfiguration {
                    key: "budget_steps.kind".into(),
                    value: kind,
                })?,
                budget_id,
                core_key_id,
                request_fingerprint,
                endpoint,
                model,
                account_ref,
                bridge_instance_id,
                profile_fingerprint,
                policy_version,
                hold_credits: CreditAmount::try_from_microcredits(hold_microcredits)
                    .ok_or(CoreError::InvalidQuotaAmount)?,
                expires_at_ms,
                reservation_id,
                dispatch_attempted: dispatch_attempted != 0,
                execution_state: BudgetExecutionState::from_db(&execution_value).ok_or_else(|| {
                    CoreError::InvalidConfiguration {
                        key: "budget_steps.execution_state".into(),
                        value: execution_value,
                    }
                })?,
                financial_state: BudgetFinancialState::from_db(&financial_value).ok_or_else(|| {
                    CoreError::InvalidConfiguration {
                        key: "budget_steps.financial_state".into(),
                        value: financial_value,
                    }
                })?,
                actual_credits: actual_microcredits
                    .map(|value| CreditAmount::try_from_microcredits(value).ok_or(CoreError::InvalidQuotaAmount))
                    .transpose()?,
                task_ref,
            });
        }
        Ok(Some(BudgetOperationView {
            operation_id,
            parent_request_id,
            api_key_id,
            execution_state,
            steps: views,
        }))
    }
}

fn validate_budget_authorization(
    input: &BudgetStepInput,
    now_ms: i64,
    allow_expired: bool,
) -> Result<(), CoreError> {
    let authorization = &input.authorization;
    for value in [
        authorization.budget_id.as_str(),
        authorization.parent_request_id.as_str(),
        authorization.request_id.as_str(),
        authorization.core_key_id.as_str(),
        authorization.request_fingerprint.as_str(),
        authorization.endpoint.as_str(),
        authorization.model.as_str(),
        authorization.account_ref.as_str(),
        authorization.bridge_instance_id.as_str(),
        authorization.profile_fingerprint.as_str(),
        authorization.policy_version.as_str(),
    ] {
        if value.trim().is_empty() {
            return Err(CoreError::BillingQuoteMismatch {
                request_id: authorization.request_id.clone(),
            });
        }
    }
    if authorization.hold_credits.as_microcredits() <= 0 {
        return Err(CoreError::InvalidQuotaAmount);
    }
    if !allow_expired && authorization.expires_at_ms <= now_ms {
        return Err(CoreError::BillingQuoteExpired {
            request_id: authorization.request_id.clone(),
        });
    }
    Ok(())
}

fn validate_budget_request_identity(
    transaction: &rusqlite::Transaction<'_>,
    parent_request_id: &str,
    input: &BudgetStepInput,
    parent_user_id: &str,
    parent_api_key_id: &str,
) -> Result<(), CoreError> {
    let authorization = &input.authorization;
    if authorization.core_key_id != parent_api_key_id {
        return Err(CoreError::InvalidRequestIdentity {
            user_id: parent_user_id.into(),
            api_key_id: authorization.core_key_id.clone(),
        });
    }
    if input.kind != BudgetStepKind::Assist && authorization.request_id != parent_request_id {
        return Err(CoreError::BillingQuoteMismatch {
            request_id: authorization.request_id.clone(),
        });
    }
    let (user_id, api_key_id, endpoint, model, request_hash, state): (
        String,
        String,
        String,
        String,
        Vec<u8>,
        String,
    ) = transaction
        .query_row(
            "SELECT user_id, api_key_id, endpoint, model, request_hash, state FROM requests WHERE id = ?1",
            [&authorization.request_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
        )
        .optional()?
        .ok_or_else(|| CoreError::RequestNotFound {
            request_id: authorization.request_id.clone(),
        })?;
    if user_id != parent_user_id || api_key_id != parent_api_key_id {
        return Err(CoreError::InvalidRequestIdentity {
            user_id,
            api_key_id,
        });
    }
    if state != RequestState::Received.as_str()
        || endpoint != authorization.endpoint
        || model != authorization.model
        || URL_SAFE_NO_PAD.encode(request_hash) != authorization.request_fingerprint
    {
        return Err(CoreError::BillingQuoteMismatch {
            request_id: authorization.request_id.clone(),
        });
    }
    match input.kind {
        BudgetStepKind::Assist => {
            let valid_relation: bool = transaction.query_row(
                "SELECT EXISTS(
                   SELECT 1 FROM request_relations relation
                   JOIN requests parent ON parent.id = relation.parent_request_id
                   JOIN requests child ON child.id = relation.child_request_id
                   WHERE relation.parent_request_id = ?1 AND relation.child_request_id = ?2
                     AND relation.relationship_kind = 'seedance_assist'
                     AND parent.user_id = child.user_id AND parent.api_key_id = child.api_key_id
                     AND parent.endpoint = 'videos' AND child.endpoint = 'chat'
                 )",
                params![parent_request_id, &authorization.request_id],
                |row| row.get(0),
            )?;
            if !valid_relation {
                return Err(CoreError::BillingQuoteMismatch {
                    request_id: authorization.request_id.clone(),
                });
            }
        }
        BudgetStepKind::Video if endpoint != "videos" => {
            return Err(CoreError::BillingQuoteMismatch {
                request_id: authorization.request_id.clone(),
            });
        }
        _ => {}
    }
    Ok(())
}
