use chrono::Utc;
use rusqlite::{params, OptionalExtension, TransactionBehavior};

use crate::{BudgetExecutionState, BudgetMutation, CoreError, CoreStore};

/// Trusted server-side observation input. This type deliberately has no
/// serde deserialization implementation and is not exposed as a public API.
#[allow(dead_code)] // The trusted Core execution observer is connected in Task4.
pub(crate) struct LegacyExecutionTerminalInput {
    pub request_id: String,
    pub api_key_id: String,
    pub task_ref: String,
    pub terminal_state: BudgetExecutionState,
    pub source_ref: String,
    pub observed_at_ms: i64,
}

impl CoreStore {
    /// Persist a trusted terminal observation for a V1 video task without
    /// changing any request, reservation, receipt, or financial state.
    #[allow(dead_code)] // The trusted Core execution observer is connected in Task4.
    pub(crate) fn record_legacy_execution_terminal(
        &self,
        input: LegacyExecutionTerminalInput,
    ) -> Result<BudgetMutation, CoreError> {
        if input.request_id.trim().is_empty()
            || input.api_key_id.trim().is_empty()
            || input.task_ref.trim().is_empty()
            || input.task_ref.len() > 256
            || input.task_ref.chars().any(char::is_control)
            || input.source_ref.trim().is_empty()
            || input.source_ref.len() > 512
            || input.source_ref.chars().any(char::is_control)
            || !matches!(input.terminal_state, BudgetExecutionState::Succeeded | BudgetExecutionState::Failed | BudgetExecutionState::Canceled)
            || input.observed_at_ms <= 0
        {
            return Err(CoreError::BillingReceiptInvalid {
                reason: "legacy terminal observation is incomplete or invalid".into(),
            });
        }
        let now = Utc::now().timestamp_millis();
        if input.observed_at_ms > now {
            return Err(CoreError::BillingReceiptInvalid {
                reason: "legacy terminal observation time is in the future".into(),
            });
        }
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let request: Option<(String, String, bool, bool)> = transaction.query_row(
            "SELECT request.api_key_id, request.endpoint,
                    EXISTS(SELECT 1 FROM budget_operations operation WHERE operation.parent_request_id = request.id)
                      OR EXISTS(SELECT 1 FROM budget_steps step WHERE step.request_id = request.id),
                    EXISTS(SELECT 1 FROM request_relations relation
                           WHERE relation.child_request_id = request.id
                             AND relation.relationship_kind = 'seedance_assist')
             FROM requests request WHERE request.id = ?1",
            [&input.request_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        ).optional()?;
        let Some((request_key_id, endpoint, has_v2_operation, is_assist_child)) = request else {
            return Err(CoreError::RequestNotFound { request_id: input.request_id });
        };
        if request_key_id != input.api_key_id {
            return Err(CoreError::InvalidRequestIdentity { user_id: String::new(), api_key_id: input.api_key_id });
        }
        if endpoint != "videos" || has_v2_operation || is_assist_child {
            return Err(CoreError::BillingReceiptInvalid {
                reason: "legacy execution evidence requires a V1 video parent".into(),
            });
        }

        if let Some((stored_key, stored_task, stored_terminal)) = transaction.query_row(
            "SELECT api_key_id, task_ref, terminal_state FROM legacy_execution_evidence WHERE request_id = ?1",
            [&input.request_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?)),
        ).optional()? {
            if stored_key == input.api_key_id
                && stored_task == input.task_ref
                && stored_terminal == input.terminal_state.as_str()
            {
                transaction.commit()?;
                return Ok(BudgetMutation::Duplicate);
            }
            return Err(CoreError::IdempotencyConflict);
        }

        let bound_task: Option<String> = transaction.query_row(
            "SELECT step.task_ref FROM controlled_billing_steps step
             JOIN controlled_billing_operations operation ON operation.operation_id = step.operation_id
             WHERE operation.parent_request_id = ?1 AND operation.api_key_id = ?2
               AND step.request_id = ?1 AND step.kind = 'video'
               AND step.task_ref IS NOT NULL AND length(trim(step.task_ref)) > 0",
            params![&input.request_id, &input.api_key_id],
            |row| row.get(0),
        ).optional()?;
        if bound_task.as_deref() != Some(input.task_ref.as_str()) {
            return Err(CoreError::BillingReceiptInvalid {
                reason: "legacy execution task is not bound to a trusted V1 video step".into(),
            });
        }
        transaction.execute(
            "INSERT INTO legacy_execution_evidence
             (request_id, api_key_id, task_ref, terminal_state, source_ref, observed_at_ms, recorded_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![input.request_id, input.api_key_id, input.task_ref, input.terminal_state.as_str(),
                input.source_ref, input.observed_at_ms, now],
        )?;
        transaction.commit()?;
        Ok(BudgetMutation::Applied)
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, fs, path::PathBuf};

    use rusqlite::Connection;
    use serde_json::json;

    use crate::{
        BeginRequest, BeginRequestInput, BudgetExecutionState, ControlledStepKind, CoreStore,
        CreditAmount, KeyQuotaGrant, NewUser, QuotaGrant, UserRole, UpstreamCreditSnapshot,
    };

    use super::*;

    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
    }

    fn fixture() -> (Fixture, CoreStore, String, crate::Principal) {
        let root = std::env::temp_dir().join(format!("legacy-terminal-{}", rand::random::<u64>()));
        fs::create_dir_all(&root).unwrap();
        let store = CoreStore::open(&root).unwrap();
        store.migrate().unwrap();
        store.create_user(NewUser { id: "admin".into(), name: "Admin".into(), role: UserRole::Admin }, "bootstrap").unwrap();
        store.create_user(NewUser { id: "user".into(), name: "User".into(), role: UserRole::User }, "admin").unwrap();
        let admin_key = store.issue_api_key("admin", "admin", BTreeSet::from(["admin:*".into()]), "bootstrap").unwrap();
        let admin = store.authenticate_api_key(&admin_key.plaintext).unwrap();
        let key = store.issue_api_key_as_admin_with_max_concurrency(
            "user", "video", BTreeSet::from(["video:submit".into(), "chat:invoke".into()]), 3, &admin,
        ).unwrap();
        store.key_quota_grant_as_admin(&admin, KeyQuotaGrant {
            api_key_id: key.id.clone(), resource_kind: "credits".into(), amount: 100_000_000,
            actor_user_id: "admin".into(), reason: "legacy evidence test".into(),
        }).unwrap();
        store.quota_pool_grant_as_admin(&admin, QuotaGrant {
            user_id: "user".into(), resource_kind: "credits".into(), amount: 100_000_000,
            actor_user_id: "admin".into(), reason: "legacy evidence pool".into(),
        }).unwrap();
        (Fixture(root), store, key.id, admin)
    }

    fn video_request(store: &CoreStore, key_id: &str, idempotency: &str) -> String {
        match store.begin_billed_request(BeginRequestInput {
            user_id: "user".into(), api_key_id: key_id.into(), protocol: "openai".into(), endpoint: "videos".into(),
            model: "seedance-fast".into(), idempotency_key: idempotency.into(), body: json!({"model":"seedance-fast","prompt":"kite"}),
        }).unwrap() {
            BeginRequest::Created(request) => request.id,
            other => panic!("expected new video request, got {other:?}"),
        }
    }

    fn submitted_controlled_video(store: &CoreStore, key_id: &str, idempotency: &str) -> String {
        let parent = video_request(store, key_id, idempotency);
        store.begin_controlled_operation(&parent, UpstreamCreditSnapshot {
            total: CreditAmount::parse("1000", "credits").unwrap(),
            updated_at_ms: chrono::Utc::now().timestamp_millis(),
        }).unwrap();
        store.mark_controlled_step_dispatched(&parent, &parent, ControlledStepKind::Video).unwrap();
        parent
    }

    fn input(request_id: &str, api_key_id: &str, task_ref: &str, terminal_state: BudgetExecutionState, source_ref: &str, observed_at_ms: i64) -> LegacyExecutionTerminalInput {
        LegacyExecutionTerminalInput {
            request_id: request_id.into(), api_key_id: api_key_id.into(), task_ref: task_ref.into(),
            terminal_state, source_ref: source_ref.into(), observed_at_ms,
        }
    }

    #[test]
    fn trusted_v1_terminal_is_immutable_and_releases_both_count_paths_without_financial_changes() {
        let (fixture, store, key_id, admin) = fixture();
        let parent = submitted_controlled_video(&store, &key_id, "legacy-terminal-parent");
        let observed_at = chrono::Utc::now().timestamp_millis() - 10_000;
        assert!(store.record_legacy_execution_terminal(input(&parent, &key_id, "not-yet-bound", BudgetExecutionState::Succeeded, "server-observation-1", observed_at)).is_err());
        store.bind_controlled_video_task(&parent, "task-bound").unwrap();
        assert_eq!(store.active_execution_count_for_key(&key_id).unwrap(), 1);
        assert!(store.record_legacy_execution_terminal(input(&parent, &key_id, "different-bound-task", BudgetExecutionState::Succeeded, "server-observation-wrong-task", observed_at)).is_err());
        let database = fixture.0.join("data").join(crate::CORE_DB_FILE);
        let before: (String, String, i64, String, String, i64) = Connection::open(&database).unwrap().query_row(
            "SELECT request.state,reservation.state,reservation.amount,step.state,operation.state,
                    (SELECT COUNT(*) FROM quota_ledger WHERE request_id=?1)
             FROM requests request JOIN controlled_billing_operations operation ON operation.parent_request_id=request.id
             JOIN controlled_billing_steps step ON step.operation_id=operation.operation_id
             JOIN quota_reservations reservation ON reservation.id=operation.hold_reservation_id
             WHERE request.id=?1",
            [&parent], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?)),
        ).unwrap();
        assert_eq!(store.record_legacy_execution_terminal(input(&parent, &key_id, "task-bound", BudgetExecutionState::Succeeded, "server-observation-1", observed_at)).unwrap(), crate::BudgetMutation::Applied);
        assert_eq!(store.record_legacy_execution_terminal(input(&parent, &key_id, "task-bound", BudgetExecutionState::Succeeded, "later-observation", observed_at + 1_000)).unwrap(), crate::BudgetMutation::Duplicate);
        assert!(store.record_legacy_execution_terminal(input(&parent, &key_id, "task-bound", BudgetExecutionState::Failed, "contradictory-observation", observed_at + 2_000)).is_err());
        assert_eq!(store.active_execution_count_for_key(&key_id).unwrap(), 0);
        let later = video_request(&store, &key_id, "legacy-terminal-later-admission");
        let connection = Connection::open(&database).unwrap();
        let admission_count = CoreStore::active_execution_count_for_budget_admission_in_connection(&connection, &key_id, &later).unwrap();
        let admission_count_from_shared_counter = CoreStore::active_execution_count_in_connection(&connection, &key_id, Some(&later)).unwrap();
        let after: (String, String, i64, String, String, i64, i64) = connection.query_row(
            "SELECT request.state,reservation.state,reservation.amount,step.state,operation.state,
                    (SELECT COUNT(*) FROM quota_ledger WHERE request_id=?1),
                    (SELECT observed_at_ms FROM legacy_execution_evidence WHERE request_id=?1)
             FROM requests request JOIN controlled_billing_operations operation ON operation.parent_request_id=request.id
             JOIN controlled_billing_steps step ON step.operation_id=operation.operation_id
             JOIN quota_reservations reservation ON reservation.id=operation.hold_reservation_id
             WHERE request.id=?1",
            [&parent], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?)),
        ).unwrap();
        assert_eq!(admission_count, 0);
        assert_eq!(admission_count_from_shared_counter, admission_count);
        assert_eq!((&after.0,&after.1,after.2,&after.3,&after.4,after.5), (&before.0,&before.1,before.2,&before.3,&before.4,before.5));
        assert_eq!(after.6, observed_at);
        let admin_key = store.list_api_keys_as_admin(&admin, Some("user")).unwrap().into_iter().find(|view| view.id == key_id).unwrap();
        assert_eq!(admin_key.current_concurrency, 1);
        let held = store.key_quota_balance_as_admin(&admin, &key_id, "credits").unwrap();
        assert_eq!(held.held, before.2);
    }

    #[test]
    fn trusted_v1_terminal_rejects_wrong_key_and_assist_child() {
        let (_fixture, store, key_id, _) = fixture();
        let parent = submitted_controlled_video(&store, &key_id, "legacy-terminal-wrong-key");
        store.bind_controlled_video_task(&parent, "task-bound").unwrap();
        assert!(store.record_legacy_execution_terminal(input(&parent, "other-key", "task-bound", BudgetExecutionState::Succeeded, "server-observation", chrono::Utc::now().timestamp_millis())).is_err());
        let assist_parent = video_request(&store, &key_id, "legacy-terminal-assist-parent");
        let child = store.begin_budget_assist_request(&assist_parent, BeginRequestInput {
            user_id: "user".into(), api_key_id: key_id.clone(), protocol: "openai".into(), endpoint: "chat".into(),
            model: "assist-model".into(), idempotency_key: "legacy-terminal-child".into(),
            body: json!({"model":"assist-model","messages":[{"role":"user","content":"describe"}]}),
        }).unwrap();
        let child_id = match child { BeginRequest::Created(request) => request.id, other => panic!("expected child, got {other:?}") };
        assert!(store.record_legacy_execution_terminal(input(&child_id, &key_id, "task-bound", BudgetExecutionState::Succeeded, "server-observation", chrono::Utc::now().timestamp_millis())).is_err());
    }
}
