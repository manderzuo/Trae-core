use chrono::Utc;
use rusqlite::{params, OptionalExtension, TransactionBehavior};

use crate::{
    BillingReceipt, BillingReceiptStatus, CoreError, CoreStore, CreditAmount, QuotaReserve,
    RequestResult, RequestState, ReservationState, ReserveResult, UpstreamCreditSnapshot,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlledStepKind {
    Assist,
    Video,
}

impl ControlledStepKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Assist => "assist",
            Self::Video => "video",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlledStepResult {
    Verified,
    Held,
    Duplicate,
    Conflict,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlledOperation {
    pub operation_id: String,
    pub api_key_id: String,
    pub held: CreditAmount,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlledSettlement {
    pub api_key_id: String,
    pub actual_credits: CreditAmount,
    pub over_authorized_hold: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlledRecoverableStep {
    pub parent_request_id: String,
    pub user_id: String,
    pub api_key_id: String,
    pub operation_id: String,
    pub hold_id: String,
    pub request_id: String,
    pub kind: ControlledStepKind,
    pub state: String,
    pub task_ref: Option<String>,
}

impl CoreStore {
    /// Startup-only inventory. The cutoff must be captured before this process
    /// accepts new requests, so an in-flight helper cannot race settlement.
    pub fn recoverable_undispatched_operations_before(&self, startup_cutoff_ms: i64) -> Result<Vec<String>, CoreError> {
        let connection = self.connection.lock().expect("core store mutex poisoned");
        let mut statement = connection.prepare(
            "SELECT operation.parent_request_id FROM controlled_billing_operations operation
             WHERE operation.created_at_ms < ?1 AND operation.state IN ('held','submitted','unknown')
             AND NOT EXISTS (SELECT 1 FROM controlled_billing_steps step
                             WHERE step.operation_id = operation.operation_id AND step.kind = 'video')
             AND NOT EXISTS (SELECT 1 FROM controlled_billing_steps step
                             WHERE step.operation_id = operation.operation_id AND step.state != 'verified')
             ORDER BY operation.created_at_ms LIMIT 100",
        )?;
        let rows = statement.query_map([startup_cutoff_ms], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
    pub fn active_controlled_operation_for_key(&self, api_key_id: &str) -> Result<bool, CoreError> {
        let connection = self.connection.lock().expect("core store mutex poisoned");
        connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM controlled_billing_operations
             WHERE api_key_id = ?1 AND state IN ('held','submitted','unknown'))",
                [api_key_id],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub fn controlled_operation_summaries(
        &self,
        limit: usize,
    ) -> Result<Vec<serde_json::Value>, CoreError> {
        let connection = self.connection.lock().expect("core store mutex poisoned");
        let mut statement = connection.prepare(
            "SELECT operation_id, parent_request_id, api_key_id, held_microcredits,
                    actual_microcredits, state, created_at_ms, updated_at_ms
             FROM controlled_billing_operations ORDER BY created_at_ms DESC LIMIT ?1",
        )?;
        let operations = statement
            .query_map([limit.min(100) as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let mut result = Vec::with_capacity(operations.len());
        for (operation_id, parent_request_id, api_key_id, held, actual, state, created, updated) in
            operations
        {
            let mut step_statement = connection.prepare(
                "SELECT request_id, kind, state, actual_microcredits, task_ref
                 FROM controlled_billing_steps WHERE operation_id = ?1 ORDER BY kind",
            )?;
            let steps = step_statement.query_map([&operation_id], |row| {
                Ok(serde_json::json!({
                    "request_id":row.get::<_, String>(0)?, "kind":row.get::<_, String>(1)?,
                    "state":row.get::<_, String>(2)?, "actual_microcredits":row.get::<_, Option<i64>>(3)?,
                    "task_ref":row.get::<_, Option<String>>(4)?,
                }))
            })?.collect::<Result<Vec<_>, _>>()?;
            result.push(serde_json::json!({
                "operation_id":operation_id, "parent_request_id":parent_request_id,
                "api_key_id":api_key_id, "held_microcredits":held,
                "actual_microcredits":actual, "state":state,
                "created_at_ms":created, "updated_at_ms":updated,
                "steps":steps,
            }));
        }
        Ok(result)
    }

    pub fn controlled_operation_exists(&self, parent_request_id: &str) -> Result<bool, CoreError> {
        let connection = self.connection.lock().expect("core store mutex poisoned");
        connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM controlled_billing_operations WHERE parent_request_id = ?1)",
            [parent_request_id], |row| row.get(0),
        ).map_err(Into::into)
    }

    /// Query-only recovery inventory. Active operations remain held until
    /// verified receipts or explicit no-charge evidence permit settlement.
    pub fn recoverable_controlled_steps(
        &self,
        limit: usize,
    ) -> Result<Vec<ControlledRecoverableStep>, CoreError> {
        let connection = self.connection.lock().expect("core store mutex poisoned");
        let mut statement = connection.prepare(
            "SELECT operation.parent_request_id, step.request_id, step.kind, step.state, step.task_ref,
                    parent.user_id, operation.api_key_id, operation.operation_id, operation.hold_reservation_id
             FROM controlled_billing_steps step
             JOIN controlled_billing_operations operation ON operation.operation_id = step.operation_id
             JOIN requests parent ON parent.id = operation.parent_request_id
             WHERE operation.state IN ('held','submitted','unknown')
             ORDER BY operation.created_at_ms, step.kind LIMIT ?1",
        )?;
        let rows = statement.query_map([limit.min(1000) as i64], |row| {
            let kind: String = row.get(2)?;
            let kind = match kind.as_str() {
                "assist" => ControlledStepKind::Assist,
                "video" => ControlledStepKind::Video,
                _ => {
                    return Err(rusqlite::Error::InvalidColumnType(
                        2,
                        "kind".into(),
                        rusqlite::types::Type::Text,
                    ))
                }
            };
            Ok(ControlledRecoverableStep {
                parent_request_id: row.get(0)?,
                user_id: row.get(5)?,
                api_key_id: row.get(6)?,
                operation_id: row.get(7)?,
                hold_id: row.get(8)?,
                request_id: row.get(1)?,
                kind,
                state: row.get(3)?,
                task_ref: row.get(4)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}

impl CoreStore {
    /// Bind AI Work's already accepted task to the dispatched video step.
    /// Rebinding to a different task is a hard conflict, never an overwrite.
    pub fn bind_controlled_video_task(&self, parent_request_id: &str, task_ref: &str) -> Result<(), CoreError> {
        if task_ref.trim().is_empty() || task_ref.len() > 256 || task_ref.chars().any(char::is_control) {
            return Err(CoreError::BillingReceiptInvalid { reason: "controlled video task reference is invalid".into() });
        }
        let connection = self.connection.lock().expect("core store mutex poisoned");
        let updated = connection.execute(
            "UPDATE controlled_billing_steps SET task_ref = ?2
             WHERE request_id = ?1 AND kind = 'video' AND state IN ('pending','unknown')
             AND (task_ref IS NULL OR task_ref = ?2)
             AND operation_id = (SELECT operation_id FROM controlled_billing_operations
                                 WHERE parent_request_id = ?1 AND state IN ('submitted','unknown'))",
            params![parent_request_id, task_ref],
        )?;
        if updated != 1 {
            return Err(CoreError::BillingReceiptInvalid { reason: "controlled video task reference conflicts with the operation".into() });
        }
        Ok(())
    }
    /// One transaction creates the only charge authorization for the whole
    /// assistant-plus-video operation. It is not an upstream price quote.
    pub fn begin_controlled_operation(
        &self,
        parent_request_id: &str,
        snapshot: UpstreamCreditSnapshot,
    ) -> Result<ControlledOperation, CoreError> {
        let now = Utc::now().timestamp_millis();
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (user_id, api_key_id, state): (String, String, String) = transaction
            .query_row(
                "SELECT user_id, api_key_id, state FROM requests WHERE id = ?1",
                [parent_request_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
            .ok_or_else(|| CoreError::RequestNotFound {
                request_id: parent_request_id.into(),
            })?;

        let prior: Option<(String, i64)> = transaction.query_row(
            "SELECT operation_id, held_microcredits FROM controlled_billing_operations WHERE parent_request_id = ?1",
            [parent_request_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?;
        if let Some((operation_id, held)) = prior {
            transaction.commit()?;
            return Ok(ControlledOperation {
                operation_id,
                api_key_id,
                held: CreditAmount::try_from_microcredits(held)
                    .ok_or(CoreError::InvalidQuotaAmount)?,
            });
        }

        Self::ensure_fresh_upstream_credit_snapshot(&snapshot, now)?;
        Self::upstream_credit_capacity_in_transaction(&transaction, &snapshot)?;
        if state != RequestState::Received.as_str() {
            return Err(CoreError::BillingQuoteMismatch {
                request_id: parent_request_id.into(),
            });
        }
        let active: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM api_keys WHERE id = ?1 AND user_id = ?2 AND status = 'active')",
            params![&api_key_id, &user_id], |row| row.get(0),
        )?;
        if !active {
            return Err(CoreError::InvalidRequestIdentity {
                user_id,
                api_key_id,
            });
        }
        let blocked: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM api_key_billing_blocks WHERE key_id = ?1)",
            [&api_key_id],
            |row| row.get(0),
        )?;
        let already_running: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM controlled_billing_operations
             WHERE api_key_id = ?1 AND state IN ('held','submitted','unknown'))",
            [&api_key_id],
            |row| row.get(0),
        )?;
        if blocked || already_running {
            return Err(CoreError::ApiKeyBillingBlocked { api_key_id });
        }

        let key_account_id: String = transaction.query_row(
            "SELECT id FROM quota_budget_accounts
             WHERE scope = 'key' AND api_key_id = ?1 AND user_id = ?2 AND resource_kind = 'credits'",
            params![&api_key_id, &user_id], |row| row.get(0),
        ).optional()?.ok_or_else(|| CoreError::KeyQuotaNotConfigured {
            api_key_id: api_key_id.clone(), resource_kind: "credits".into(),
        })?;
        let available =
            Self::budget_balance_in_transaction(&transaction, &key_account_id)?.available;
        if available <= 0 {
            return Err(CoreError::QuotaInsufficient {
                available,
                required: 1,
            });
        }
        Self::transition_request_on_connection(
            &transaction,
            parent_request_id,
            RequestState::Received,
            RequestState::Validating,
            None,
            now,
        )?;
        let reserve = QuotaReserve {
            user_id,
            request_id: parent_request_id.into(),
            resource_kind: "credits".into(),
            amount: available,
            ttl_ms: i64::MAX - now,
        };
        let reservation = match Self::reserve_dual_in_transaction(
            &transaction,
            &reserve,
            &api_key_id,
            now,
            i64::MAX,
        )? {
            ReserveResult::Created(reservation) => reservation,
            ReserveResult::Insufficient { available } => {
                return Err(CoreError::QuotaInsufficient {
                    available,
                    required: reserve.amount,
                });
            }
            ReserveResult::Existing(_) => {
                return Err(CoreError::ReservationRequestConflict {
                    request_id: parent_request_id.into(),
                });
            }
        };
        let operation_id = Self::new_id("controlled-operation");
        transaction.execute(
            "INSERT INTO controlled_billing_operations
             (operation_id, parent_request_id, api_key_id, hold_reservation_id,
              held_microcredits, state, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, 'held', ?6, ?6)",
            params![
                &operation_id,
                parent_request_id,
                &api_key_id,
                &reservation.id,
                available,
                now
            ],
        )?;
        Self::transition_request_on_connection(
            &transaction,
            parent_request_id,
            RequestState::Validating,
            RequestState::Reserved,
            None,
            now,
        )?;
        transaction.commit()?;
        Ok(ControlledOperation {
            operation_id,
            api_key_id,
            held: CreditAmount::try_from_microcredits(available)
                .ok_or(CoreError::InvalidQuotaAmount)?,
        })
    }
}

impl CoreStore {
    /// Commits the sum of verified step receipts against the single parent
    /// hold. A missing or unknown step can never be treated as zero cost.
    pub fn finish_controlled_operation(
        &self,
        parent_request_id: &str,
        video_outcome: Option<bool>,
    ) -> Result<ControlledSettlement, CoreError> {
        let video_submitted = video_outcome.is_some();
        let now = Utc::now().timestamp_millis();
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (operation_id, api_key_id, hold_id, held, actual, state): (
            String,
            String,
            String,
            i64,
            Option<i64>,
            String,
        ) = transaction
            .query_row(
                "SELECT operation_id, api_key_id, hold_reservation_id, held_microcredits,
                    actual_microcredits, state
             FROM controlled_billing_operations WHERE parent_request_id = ?1",
                [parent_request_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| CoreError::BillingReceiptInvalid {
                reason: "controlled operation is missing".into(),
            })?;
        if state == "settled" {
            let actual = actual.ok_or_else(|| CoreError::BillingReceiptInvalid {
                reason: "settled controlled operation lacks actual credits".into(),
            })?;
            transaction.commit()?;
            return Ok(ControlledSettlement {
                api_key_id,
                actual_credits: CreditAmount::try_from_microcredits(actual)
                    .ok_or(CoreError::InvalidQuotaAmount)?,
                over_authorized_hold: actual > held,
            });
        }
        if state == "released" {
            return Err(CoreError::BillingReceiptInvalid {
                reason: "controlled operation was released".into(),
            });
        }
        // A linked child without a dispatched step cannot have reached the
        // upstream: mark_controlled_step_dispatched precedes every send.
        let has_assist: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM controlled_billing_steps
             WHERE operation_id = ?1 AND kind = 'assist')",
            [&operation_id],
            |row| row.get(0),
        )?;
        let mut statement = transaction.prepare(
            "SELECT step.kind, step.state, step.actual_microcredits, receipt.status
             FROM controlled_billing_steps step
             LEFT JOIN billing_receipts receipt
               ON receipt.request_id = step.request_id AND receipt.receipt_hash = step.receipt_hash
             WHERE step.operation_id = ?1",
        )?;
        let steps = statement
            .query_map([&operation_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);

        let mut assist_verified = false;
        let mut video_verified = false;
        let mut video_failed_no_charge = false;
        let mut total = 0_i64;
        for (kind, step_state, amount, receipt_status) in steps {
            if step_state != "verified"
                || !matches!(
                    receipt_status.as_deref(),
                    Some("final" | "failed_no_charge")
                )
            {
                return Err(CoreError::BillingReceiptInvalid {
                    reason: "controlled step receipt is still unknown or conflicted".into(),
                });
            }
            total = total
                .checked_add(amount.ok_or_else(|| CoreError::BillingReceiptInvalid {
                    reason: "verified controlled step lacks actual credits".into(),
                })?)
                .ok_or(CoreError::InvalidQuotaAmount)?;
            match kind.as_str() {
                "assist" => assist_verified = true,
                "video" => {
                    video_verified = true;
                    video_failed_no_charge = receipt_status.as_deref() == Some("failed_no_charge");
                }
                _ => {
                    return Err(CoreError::BillingReceiptInvalid {
                        reason: "unknown controlled step kind".into(),
                    })
                }
            }
        }
        if (has_assist && !assist_verified)
            || (video_submitted && !video_verified)
            || (!video_submitted && video_verified)
        {
            return Err(CoreError::BillingReceiptInvalid {
                reason:
                    "controlled operation lacks a verified terminal receipt for each submitted step"
                        .into(),
            });
        }
        let reservation = Self::reservation_by_id(&transaction, &hold_id)?.ok_or_else(|| {
            CoreError::ReservationNotFound {
                reservation_id: hold_id.clone(),
            }
        })?;
        if reservation.request_id != parent_request_id
            || reservation.api_key_id.as_deref() != Some(api_key_id.as_str())
            || reservation.amount != held
            || reservation.resource_kind != "credits"
            || !matches!(
                reservation.state,
                ReservationState::Held | ReservationState::Unknown
            )
            || reservation.key_budget_account_id.is_none()
        {
            return Err(CoreError::BillingReceiptInvalid {
                reason: "controlled operation and Key hold do not match".into(),
            });
        }
        Self::set_reservation_state(&transaction, &hold_id, ReservationState::Committed, now)?;
        Self::insert_reservation_budget_event(
            &transaction,
            &reservation,
            "commit",
            total,
            held.checked_sub(total)
                .ok_or(CoreError::InvalidQuotaAmount)?,
            None,
            now,
            reservation.event_group_id.as_deref().ok_or_else(|| {
                CoreError::InvalidConfiguration {
                    key: "quota_reservations.event_group_id".into(),
                    value: hold_id.clone(),
                }
            })?,
        )?;
        transaction.execute(
            "UPDATE controlled_billing_operations
             SET state = 'settled', actual_microcredits = ?2, updated_at_ms = ?3
             WHERE operation_id = ?1",
            params![&operation_id, total, now],
        )?;
        let over_authorized_hold = total > held;
        if over_authorized_hold {
            transaction.execute(
                "INSERT OR IGNORE INTO api_key_billing_blocks
                 (key_id, request_id, reason, quote_max_credits, actual_credits,
                  excess_credits, source_ref, blocked_at_ms)
                 VALUES (?1, ?2, 'over_authorized_hold', ?3, ?4, ?5, ?6, ?7)",
                params![
                    &api_key_id,
                    parent_request_id,
                    held,
                    total,
                    total - held,
                    format!("controlled-operation:{operation_id}"),
                    now
                ],
            )?;
        }
        let request_state: String = transaction.query_row(
            "SELECT state FROM requests WHERE id = ?1",
            [parent_request_id],
            |row| row.get(0),
        )?;
        let request_state = RequestState::from_db(&request_state).ok_or_else(|| {
            CoreError::InvalidConfiguration {
                key: "requests.state".into(),
                value: request_state,
            }
        })?;
        Self::settle_request_state(
            &transaction,
            parent_request_id,
            request_state,
            if video_outcome == Some(true) && !video_failed_no_charge {
                RequestState::Succeeded
            } else {
                RequestState::Failed
            },
            (video_outcome == Some(false)).then(|| RequestResult {
                status: Some(502),
                error_code: Some("video_generation_failed".into()),
            }),
            now,
        )?;
        transaction.commit()?;
        Ok(ControlledSettlement {
            api_key_id,
            actual_credits: CreditAmount::try_from_microcredits(total)
                .ok_or(CoreError::InvalidQuotaAmount)?,
            over_authorized_hold,
        })
    }
}

impl CoreStore {
    /// Persists a request-scoped observation without consuming the operation
    /// hold. Only verified terminal steps contribute to final settlement.
    pub fn mark_controlled_step_dispatched(
        &self,
        parent_request_id: &str,
        step_request_id: &str,
        kind: ControlledStepKind,
    ) -> Result<(), CoreError> {
        let now = Utc::now().timestamp_millis();
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (operation_id, api_key_id, state): (String, String, String) = transaction.query_row(
            "SELECT operation_id, api_key_id, state FROM controlled_billing_operations WHERE parent_request_id = ?1",
            [parent_request_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional()?.ok_or_else(|| CoreError::BillingReceiptInvalid {
            reason: "controlled operation is missing".into(),
        })?;
        if !matches!(state.as_str(), "held" | "submitted") {
            return Err(CoreError::BillingReceiptInvalid {
                reason: "controlled operation cannot dispatch another step".into(),
            });
        }
        let valid_step: bool = if kind == ControlledStepKind::Video {
            step_request_id == parent_request_id
        } else {
            transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM request_relations WHERE parent_request_id = ?1
                 AND child_request_id = ?2 AND relationship_kind = 'seedance_assist')",
                params![parent_request_id, step_request_id],
                |row| row.get(0),
            )?
        };
        let step_key: String = transaction
            .query_row(
                "SELECT api_key_id FROM requests WHERE id = ?1",
                [step_request_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| CoreError::RequestNotFound {
                request_id: step_request_id.into(),
            })?;
        if !valid_step || step_key != api_key_id {
            return Err(CoreError::BillingReceiptInvalid {
                reason: "controlled step identity does not match its parent Key".into(),
            });
        }
        transaction.execute(
            "INSERT INTO controlled_billing_steps (request_id, operation_id, kind, state)
             VALUES (?1, ?2, ?3, 'pending')",
            params![step_request_id, operation_id, kind.as_str()],
        )?;
        transaction.execute(
            "UPDATE controlled_billing_operations SET state = 'submitted', updated_at_ms = ?2 WHERE operation_id = ?1",
            params![operation_id, now],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn record_controlled_step(
        &self,
        parent_request_id: &str,
        step_request_id: &str,
        kind: ControlledStepKind,
        receipt: BillingReceipt,
    ) -> Result<ControlledStepResult, CoreError> {
        if receipt.request_id != step_request_id
            || receipt.unit != "credits"
            || receipt.source_ref.trim().is_empty()
            || receipt.observed_at_ms <= 0
        {
            return Err(CoreError::BillingReceiptInvalid {
                reason: "controlled receipt identity, unit or source is invalid".into(),
            });
        }
        if kind == ControlledStepKind::Video
            && receipt.status == BillingReceiptStatus::Final
            && !receipt
                .task_ref
                .as_deref()
                .is_some_and(|task| !task.trim().is_empty())
        {
            return Err(CoreError::BillingReceiptInvalid {
                reason: "final video receipt requires a task reference".into(),
            });
        }
        let actual = match receipt.status {
            BillingReceiptStatus::Final => Some(
                receipt
                    .actual_credits
                    .ok_or_else(|| CoreError::BillingReceiptInvalid {
                        reason: "final receipt lacks actual credits".into(),
                    })?
                    .as_microcredits(),
            ),
            BillingReceiptStatus::FailedNoCharge => {
                if receipt
                    .actual_credits
                    .is_some_and(|value| value.as_microcredits() != 0)
                {
                    return Err(CoreError::BillingReceiptInvalid {
                        reason: "no-charge receipt contains nonzero credits".into(),
                    });
                }
                Some(0)
            }
            _ => None,
        };
        let hash = crate::canonical_json_hash(&serde_json::to_value(&receipt)?).to_vec();
        let now = Utc::now().timestamp_millis();
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (operation_id, api_key_id, hold_id, held, operation_state): (
            String,
            String,
            String,
            i64,
            String,
        ) = transaction
            .query_row(
                "SELECT operation_id, api_key_id, hold_reservation_id, held_microcredits, state
             FROM controlled_billing_operations WHERE parent_request_id = ?1",
                [parent_request_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| CoreError::BillingReceiptInvalid {
                reason: "controlled operation is missing".into(),
            })?;
        let step_key: String = transaction
            .query_row(
                "SELECT api_key_id FROM requests WHERE id = ?1",
                [step_request_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| CoreError::RequestNotFound {
                request_id: step_request_id.into(),
            })?;
        let relation_matches = if kind == ControlledStepKind::Video {
            step_request_id == parent_request_id
        } else {
            transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM request_relations
                 WHERE parent_request_id = ?1 AND child_request_id = ?2 AND relationship_kind = 'seedance_assist')",
                params![parent_request_id, step_request_id], |row| row.get::<_, bool>(0),
            )?
        };
        if step_key != api_key_id || !relation_matches {
            return Err(CoreError::BillingReceiptInvalid {
                reason: "controlled step is not owned by the operation Key and request relation"
                    .into(),
            });
        }

        let previous: Option<(Option<Vec<u8>>, String, Option<i64>)> = transaction.query_row(
            "SELECT receipt_hash, state, actual_microcredits FROM controlled_billing_steps WHERE request_id = ?1",
            [step_request_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional()?;
        if let Some((previous_hash, previous_state, _)) = previous.as_ref() {
            if previous_hash.as_deref() == Some(hash.as_slice()) {
                transaction.commit()?;
                return Ok(if previous_state == "verified" {
                    ControlledStepResult::Duplicate
                } else if previous_state == "conflict" {
                    ControlledStepResult::Conflict
                } else {
                    ControlledStepResult::Held
                });
            }
        }
        if operation_state == "released" {
            return Err(CoreError::BillingReceiptInvalid {
                reason: "controlled operation was released".into(),
            });
        }
        let prior_verified = previous
            .as_ref()
            .is_some_and(|(_, state, _)| state == "verified")
            || operation_state == "settled";
        if prior_verified {
            Self::insert_billing_receipt(&transaction, &receipt, &hash, "conflict", actual, now)?;
            transaction.execute(
                "INSERT OR IGNORE INTO api_key_billing_blocks
                 (key_id, request_id, reason, quote_max_credits, actual_credits,
                  excess_credits, source_ref, blocked_at_ms)
                 VALUES (?1, ?2, 'receipt_conflict', ?3, ?4, 0, ?5, ?6)",
                params![
                    &api_key_id,
                    parent_request_id,
                    held,
                    actual.unwrap_or(0),
                    &receipt.source_ref,
                    now
                ],
            )?;
            transaction.execute(
                "UPDATE controlled_billing_steps SET state = 'conflict' WHERE request_id = ?1",
                [step_request_id],
            )?;
            if operation_state != "settled" {
                transaction.execute(
                    "UPDATE controlled_billing_operations SET state = 'unknown', updated_at_ms = ?2 WHERE operation_id = ?1",
                    params![&operation_id, now],
                )?;
                Self::set_reservation_state(
                    &transaction,
                    &hold_id,
                    ReservationState::Unknown,
                    now,
                )?;
            }
            transaction.commit()?;
            return Ok(ControlledStepResult::Conflict);
        }

        let normalized_state = if actual.is_some() {
            "verified"
        } else {
            "unknown"
        };
        Self::insert_billing_receipt(
            &transaction,
            &receipt,
            &hash,
            receipt.status.as_str(),
            actual,
            now,
        )?;
        transaction.execute(
            "INSERT INTO controlled_billing_steps
             (request_id, operation_id, kind, receipt_hash, actual_microcredits, task_ref, state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(request_id) DO UPDATE SET
               receipt_hash = excluded.receipt_hash,
               actual_microcredits = excluded.actual_microcredits,
               task_ref = excluded.task_ref,
               state = excluded.state",
            params![
                step_request_id,
                &operation_id,
                kind.as_str(),
                &hash,
                actual,
                receipt.task_ref.as_deref(),
                normalized_state
            ],
        )?;
        if actual.is_none() {
            transaction.execute(
                "UPDATE controlled_billing_operations SET state = 'unknown', updated_at_ms = ?2 WHERE operation_id = ?1",
                params![&operation_id, now],
            )?;
            Self::set_reservation_state(&transaction, &hold_id, ReservationState::Unknown, now)?;
        }
        transaction.commit()?;
        Ok(if actual.is_some() {
            ControlledStepResult::Verified
        } else {
            ControlledStepResult::Held
        })
    }
}
