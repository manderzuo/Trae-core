use std::{
    collections::BTreeSet,
    fs,
    path::PathBuf,
    sync::{Arc, Barrier},
    thread,
    time::Instant,
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use aiwork_core::{
    BeginRequest, BeginRequestInput, BillingQuote, BillingReceipt, BillingReceiptResult,
    BillingReceiptStatus, BillingReservationResult, CoreStore, CreditAmount, KeyQuotaGrant,
    NewUser, UserRole, CORE_DB_FILE,
};
use rusqlite::{params, Connection};
use serde_json::json;

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "core-budget-{label}-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0) {
            eprintln!("failed to remove test fixture {}: {error}", self.0.display());
        }
    }
}

fn dispatch_current_budget_step(
    store: &CoreStore,
    request_id: &str,
) -> Result<aiwork_core::BudgetMutation, aiwork_core::CoreError> {
    let budget_id = store
        .pending_budget_steps(100)?
        .into_iter()
        .find(|view| view.step.request_id == request_id)
        .map(|view| view.step.budget_id)
        .ok_or_else(|| aiwork_core::CoreError::RequestNotFound {
            request_id: request_id.into(),
        })?;
    store.mark_budget_step_dispatched(request_id, &budget_id)
}

#[derive(Debug, PartialEq, Eq)]
struct LegacyBillingSnapshot {
    reservations: Vec<(String, String, i64)>,
    ledger_events: Vec<(String, String, i64, i64)>,
    receipt_count: i64,
    settlements: Vec<(String, i64)>,
}

fn legacy_billing_snapshot(database: &std::path::Path, request_ids: [&str; 3]) -> LegacyBillingSnapshot {
    let connection = Connection::open(database).unwrap();
    let mut statement = connection
        .prepare(
            "SELECT request_id, state, amount FROM quota_reservations
             WHERE request_id IN (?1, ?2, ?3) ORDER BY request_id",
        )
        .unwrap();
    let reservations = statement
        .query_map(params![request_ids[0], request_ids[1], request_ids[2]], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    drop(statement);

    let mut statement = connection
        .prepare(
            "SELECT request_id, event_kind, amount, delta FROM quota_ledger
             WHERE request_id IN (?1, ?2, ?3) ORDER BY request_id, event_kind, entry_id",
        )
        .unwrap();
    let ledger_events = statement
        .query_map(params![request_ids[0], request_ids[1], request_ids[2]], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    drop(statement);

    let receipt_count = connection
        .query_row(
            "SELECT COUNT(*) FROM billing_receipts WHERE request_id IN (?1, ?2, ?3)",
            params![request_ids[0], request_ids[1], request_ids[2]],
            |row| row.get(0),
        )
        .unwrap();
    let mut statement = connection
        .prepare(
            "SELECT request_id, actual_credits FROM billing_settlements
             WHERE request_id IN (?1, ?2, ?3) ORDER BY request_id",
        )
        .unwrap();
    let settlements = statement
        .query_map(params![request_ids[0], request_ids[1], request_ids[2]], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    LegacyBillingSnapshot {
        reservations,
        ledger_events,
        receipt_count,
        settlements,
    }
}

fn create_legacy_credit_request(store: &CoreStore, key_id: &str, idempotency_key: &str) -> String {
    let request = match store
        .begin_billed_request(BeginRequestInput {
            user_id: "budget-user".into(),
            api_key_id: key_id.into(),
            protocol: "openai".into(),
            endpoint: "/v1/chat/completions".into(),
            model: "legacy-text-model".into(),
            idempotency_key: idempotency_key.into(),
            body: json!({"model":"legacy-text-model","messages":[{"role":"user","content":"hello"}]}),
        })
        .unwrap()
    {
        BeginRequest::Created(request) => request,
        other => panic!("expected a new v1 request, got {other:?}"),
    };
    let quote = BillingQuote {
        request_id: request.id.clone(),
        quote_id: format!("quote-{idempotency_key}"),
        request_fingerprint: store.request_fingerprint_for_billing(&request.id).unwrap(),
        endpoint: request.endpoint,
        model: request.model,
        max_credits: CreditAmount::parse("1", "credits").unwrap(),
        unit: "credits".into(),
        expires_at_ms: chrono::Utc::now().timestamp_millis() + 60_000,
        source_ref: format!("legacy-quote-{idempotency_key}"),
    };
    assert!(matches!(
        store.reserve_credit_quote(quote).unwrap(),
        BillingReservationResult::Created { .. }
    ));
    request.id
}

fn legacy_receipt(
    request_id: &str,
    status: BillingReceiptStatus,
    actual_credits: Option<CreditAmount>,
    source_ref: &str,
) -> BillingReceipt {
    BillingReceipt {
        request_id: request_id.into(),
        status,
        actual_credits,
        unit: "credits".into(),
        source_ref: source_ref.into(),
        task_ref: None,
        observed_at_ms: chrono::Utc::now().timestamp_millis(),
    }
}

fn budget_fixture(label: &str, concurrency: i64, credits: i64) -> (TestDirectory, CoreStore, String, aiwork_core::Principal) {
    let directory = TestDirectory::new(label);
    let store = CoreStore::open(&directory.0).unwrap();
    store.migrate().unwrap();
    store
        .create_user(
            NewUser { id: "budget-admin".into(), name: "Admin".into(), role: UserRole::Admin },
            "bootstrap",
        )
        .unwrap();
    store
        .create_user(
            NewUser { id: "budget-user".into(), name: "User".into(), role: UserRole::User },
            "budget-admin",
        )
        .unwrap();
    let admin_key = store
        .issue_api_key("budget-admin", "admin", BTreeSet::from(["admin:*".into()]), "bootstrap")
        .unwrap();
    let admin = store.authenticate_api_key(&admin_key.plaintext).unwrap();
    let user_key = store
        .issue_api_key_as_admin_with_max_concurrency(
            "budget-user", "budget", BTreeSet::from(["video:submit".into(), "chat:invoke".into()]), concurrency, &admin,
        )
        .unwrap();
    store
        .key_quota_grant_as_admin(
            &admin,
            KeyQuotaGrant {
                api_key_id: user_key.id.clone(),
                resource_kind: "credits".into(),
                amount: credits,
                actor_user_id: "budget-admin".into(),
                reason: "budget billing test fixture".into(),
            },
        )
        .unwrap();
    (directory, store, user_key.id, admin)
}

#[test]
fn pending_budget_recovery_pages_do_not_starve_later_unresolved_keys() {
    let (_dir,store,key,_admin)=budget_fixture("recovery-page",3,100_000_000);
    for index in 0..3 {
        let request=begin_video_parent(&store,&key,&format!("page-{index}"));
        store.begin_budget_operation(&request,video_budget_step(&store,&key,&request,&format!("budget-{index}"))).unwrap();
    }
    let first=store.pending_budget_steps_after(1,"").unwrap().remove(0).step.request_id;
    let second=store.pending_budget_steps_after(1,&first).unwrap().remove(0).step.request_id;
    assert!(second>first,"an unresolved first page must not starve later requests");
    let third=store.pending_budget_steps_after(1,&second).unwrap().remove(0).step.request_id;
    assert!(third>second);
    assert!(store.pending_budget_steps_after(1,&third).unwrap().is_empty());
}

fn begin_video_parent(store: &CoreStore, key_id: &str, idempotency_key: &str) -> String {
    match store
        .begin_billed_request(BeginRequestInput {
            user_id: "budget-user".into(),
            api_key_id: key_id.into(),
            protocol: "openai".into(),
            endpoint: "videos".into(),
            model: "seedance-fast".into(),
            idempotency_key: idempotency_key.into(),
            body: json!({"model":"seedance-fast","prompt":"a red kite"}),
        })
        .unwrap()
    {
        BeginRequest::Created(request) => request.id,
        other => panic!("expected a new video parent, got {other:?}"),
    }
}

fn begin_chat_parent(store: &CoreStore, key_id: &str, idempotency_key: &str) -> String {
    match store
        .begin_billed_request(BeginRequestInput {
            user_id: "budget-user".into(),
            api_key_id: key_id.into(),
            protocol: "openai".into(),
            endpoint: "chat".into(),
            model: "chat-model".into(),
            idempotency_key: idempotency_key.into(),
            body: json!({"model":"chat-model","messages":[{"role":"user","content":"hello"}]}),
        })
        .unwrap()
    {
        BeginRequest::Created(request) => request.id,
        other => panic!("expected a new chat parent, got {other:?}"),
    }
}

fn budget_assist_input(key_id: &str, idempotency_key: &str) -> BeginRequestInput {
    BeginRequestInput {
        user_id: "budget-user".into(),
        api_key_id: key_id.into(),
        protocol: "openai".into(),
        endpoint: "chat".into(),
        model: "assist-model".into(),
        idempotency_key: idempotency_key.into(),
        body: json!({"model":"assist-model","messages":[{"role":"user","content":"describe"}]}),
    }
}

fn video_budget_step(
    store: &CoreStore,
    key_id: &str,
    parent_request_id: &str,
    budget_id: &str,
) -> aiwork_core::BudgetStepInput {
    aiwork_core::BudgetStepInput {
        kind: aiwork_core::BudgetStepKind::Video,
        authorization: aiwork_core::BudgetAuthorization {
            budget_id: budget_id.into(),
            parent_request_id: parent_request_id.into(),
            request_id: parent_request_id.into(),
            core_key_id: key_id.into(),
            request_fingerprint: store.request_fingerprint_for_billing(parent_request_id).unwrap(),
            endpoint: "videos".into(),
            model: "seedance-fast".into(),
            account_ref: "account-test".into(),
            bridge_instance_id: "bridge-test".into(),
            profile_fingerprint: "profile-hash".into(),
            policy_version: "policy-v1".into(),
            hold_credits: CreditAmount::parse("2", "credits").unwrap(),
            expires_at_ms: chrono::Utc::now().timestamp_millis() + 60_000,
        },
    }
}

fn chat_budget_step(
    store: &CoreStore,
    key_id: &str,
    parent_request_id: &str,
    budget_id: &str,
) -> aiwork_core::BudgetStepInput {
    aiwork_core::BudgetStepInput {
        kind: aiwork_core::BudgetStepKind::Chat,
        authorization: aiwork_core::BudgetAuthorization {
            budget_id: budget_id.into(),
            parent_request_id: parent_request_id.into(),
            request_id: parent_request_id.into(),
            core_key_id: key_id.into(),
            request_fingerprint: store.request_fingerprint_for_billing(parent_request_id).unwrap(),
            endpoint: "chat".into(),
            model: "chat-model".into(),
            account_ref: "account-test".into(),
            bridge_instance_id: "bridge-test".into(),
            profile_fingerprint: "profile-hash".into(),
            policy_version: "policy-v1".into(),
            hold_credits: CreditAmount::parse("1", "credits").unwrap(),
            expires_at_ms: chrono::Utc::now().timestamp_millis() + 60_000,
        },
    }
}

fn assist_budget_step(
    store: &CoreStore,
    key_id: &str,
    parent_request_id: &str,
    child_request_id: &str,
    budget_id: &str,
) -> aiwork_core::BudgetStepInput {
    aiwork_core::BudgetStepInput {
        kind: aiwork_core::BudgetStepKind::Assist,
        authorization: aiwork_core::BudgetAuthorization {
            budget_id: budget_id.into(),
            parent_request_id: parent_request_id.into(),
            request_id: child_request_id.into(),
            core_key_id: key_id.into(),
            request_fingerprint: store.request_fingerprint_for_billing(child_request_id).unwrap(),
            endpoint: "chat".into(),
            model: "assist-model".into(),
            account_ref: "account-test".into(),
            bridge_instance_id: "bridge-test".into(),
            profile_fingerprint: "profile-hash".into(),
            policy_version: "policy-v1".into(),
            hold_credits: CreditAmount::parse("1", "credits").unwrap(),
            expires_at_ms: chrono::Utc::now().timestamp_millis() + 60_000,
        },
    }
}

fn final_budget_receipt(request_id: &str, budget_id: &str, actual: &str) -> aiwork_core::BudgetReceiptInput {
    aiwork_core::BudgetReceiptInput {
        budget_id: budget_id.into(),
        account_ref: "account-test".into(),
        bridge_instance_id: "bridge-test".into(),
        receipt: BillingReceipt {
            request_id: request_id.into(),
            status: BillingReceiptStatus::Final,
            actual_credits: Some(CreditAmount::parse(actual, "credits").unwrap()),
            unit: "credits".into(),
            source_ref: "bridge-receipt-final-1".into(),
            task_ref: Some("task-receipt-video-1".into()),
            observed_at_ms: chrono::Utc::now().timestamp_millis(),
        },
    }
}

#[test]
fn partial_assist_and_video_settlements_conserve_exact_credit_balances() {
    let (directory, store, key_id, admin) =
        budget_fixture("partial-assist-video-conservation", 2, 1_000_000_000);
    store
        .quota_pool_grant_as_admin(
            &admin,
            aiwork_core::QuotaGrant {
                user_id: "budget-user".into(),
                resource_kind: "credits".into(),
                amount: 1_000_000_000,
                actor_user_id: "budget-admin".into(),
                reason: "partial settlement fixture".into(),
            },
        )
        .unwrap();
    let parent_request_id = begin_video_parent(&store, &key_id, "partial-assist-video-parent");
    let helper = match store
        .begin_budget_assist_request(
            &parent_request_id,
            budget_assist_input(&key_id, "partial-assist-video-helper"),
        )
        .unwrap()
    {
        BeginRequest::Created(request) => request,
        other => panic!("expected a new helper request, got {other:?}"),
    };
    let mut assist = assist_budget_step(
        &store,
        &key_id,
        &parent_request_id,
        &helper.id,
        "partial-assist-budget",
    );
    assist.authorization.hold_credits = CreditAmount::parse("2", "credits").unwrap();
    store.begin_budget_operation(&parent_request_id, assist).unwrap();
    let assist_held = store.key_quota_balance_as_admin(&admin, &key_id, "credits").unwrap();
    assert_eq!((assist_held.available, assist_held.held, assist_held.settled),
        (998_000_000, 2_000_000, 0));

    dispatch_current_budget_step(&store, &helper.id).unwrap();
    store
        .mark_budget_step_execution(&helper.id, aiwork_core::BudgetExecutionState::Succeeded)
        .unwrap();
    let mut video = video_budget_step(&store, &key_id, &parent_request_id, "partial-video-budget");
    video.authorization.hold_credits = CreditAmount::parse("275", "credits").unwrap();
    store.add_budget_step(&parent_request_id, video).unwrap();
    let combined_held = store.key_quota_balance_as_admin(&admin, &key_id, "credits").unwrap();
    assert_eq!((combined_held.available, combined_held.held, combined_held.settled),
        (723_000_000, 277_000_000, 0));

    let mut assist_receipt = final_budget_receipt(&helper.id, "partial-assist-budget", "0.0776");
    assist_receipt.receipt.source_ref = "partial-assist-source".into();
    assist_receipt.receipt.task_ref = None;
    assert!(matches!(
        store.apply_budget_receipt(assist_receipt).unwrap(),
        aiwork_core::BudgetReceiptResult::Settled { released_microcredits: 1_922_400, debt: false, .. }
    ));
    let after_assist = store.key_quota_balance_as_admin(&admin, &key_id, "credits").unwrap();
    assert_eq!((after_assist.available, after_assist.held, after_assist.settled),
        (724_922_400, 275_000_000, 77_600));

    dispatch_current_budget_step(&store, &parent_request_id).unwrap();
    store.bind_budget_video_task(&parent_request_id, "partial-video-task").unwrap();
    store
        .mark_budget_step_execution(&parent_request_id, aiwork_core::BudgetExecutionState::Succeeded)
        .unwrap();
    let mut video_receipt = final_budget_receipt(&parent_request_id, "partial-video-budget", "240.7592");
    video_receipt.receipt.source_ref = "partial-video-source".into();
    video_receipt.receipt.task_ref = Some("partial-video-task".into());
    assert!(matches!(
        store.apply_budget_receipt(video_receipt).unwrap(),
        aiwork_core::BudgetReceiptResult::Settled { released_microcredits: 34_240_800, debt: false, .. }
    ));
    store
        .finish_budget_execution(&parent_request_id, aiwork_core::BudgetExecutionState::Succeeded)
        .unwrap();
    let final_key = store.key_quota_balance_as_admin(&admin, &key_id, "credits").unwrap();
    let final_pool = store.quota_pool_balance_as_admin(&admin, "budget-user", "credits").unwrap();
    assert_eq!((final_key.available, final_key.held, final_key.settled),
        (759_163_200, 0, 240_836_800));
    assert_eq!((final_pool.available, final_pool.held, final_pool.settled),
        (759_163_200, 0, 240_836_800));
    assert_eq!(store.budget_operation(&parent_request_id).unwrap().unwrap().steps.len(), 2);
    assert!(directory.0.join("data").join(CORE_DB_FILE).exists());
}

#[test]
fn unknown_finance_does_not_keep_a_finished_budget_operation_concurrency_slot() {
    let (directory, store, key_id, admin) =
        budget_fixture("unknown-finance-releases-execution-slot", 2, 1_000_000_000);
    store
        .quota_pool_grant_as_admin(
            &admin,
            aiwork_core::QuotaGrant {
                user_id: "budget-user".into(),
                resource_kind: "credits".into(),
                amount: 1_000_000_000,
                actor_user_id: "budget-admin".into(),
                reason: "unknown finance fixture".into(),
            },
        )
        .unwrap();
    let parent_request_id = begin_video_parent(&store, &key_id, "unknown-finance-parent");
    let helper = match store
        .begin_budget_assist_request(
            &parent_request_id,
            budget_assist_input(&key_id, "unknown-finance-helper"),
        )
        .unwrap()
    {
        BeginRequest::Created(request) => request,
        other => panic!("expected a new helper request, got {other:?}"),
    };
    let mut assist = assist_budget_step(
        &store,
        &key_id,
        &parent_request_id,
        &helper.id,
        "unknown-finance-assist-budget",
    );
    assist.authorization.hold_credits = CreditAmount::parse("2", "credits").unwrap();
    store.begin_budget_operation(&parent_request_id, assist).unwrap();
    dispatch_current_budget_step(&store, &helper.id).unwrap();
    store
        .mark_budget_step_execution(&helper.id, aiwork_core::BudgetExecutionState::Succeeded)
        .unwrap();
    let mut assist_unknown = final_budget_receipt(&helper.id, "unknown-finance-assist-budget", "0");
    assist_unknown.receipt.status = BillingReceiptStatus::Unknown;
    assist_unknown.receipt.actual_credits = None;
    assist_unknown.receipt.task_ref = None;
    assist_unknown.receipt.source_ref = "unknown-finance-assist-source".into();
    assert_eq!(store.apply_budget_receipt(assist_unknown).unwrap(), aiwork_core::BudgetReceiptResult::Pending);

    let mut video = video_budget_step(&store, &key_id, &parent_request_id, "unknown-finance-video-budget");
    video.authorization.hold_credits = CreditAmount::parse("275", "credits").unwrap();
    store.add_budget_step(&parent_request_id, video).unwrap();
    dispatch_current_budget_step(&store, &parent_request_id).unwrap();
    store.bind_budget_video_task(&parent_request_id, "unknown-finance-video-task").unwrap();
    store
        .mark_budget_step_execution(&parent_request_id, aiwork_core::BudgetExecutionState::Succeeded)
        .unwrap();
    let mut assist_final = final_budget_receipt(&helper.id, "unknown-finance-assist-budget", "0.0776");
    assist_final.receipt.task_ref = None;
    assist_final.receipt.source_ref = "unknown-finance-assist-final".into();
    assert!(matches!(
        store.apply_budget_receipt(assist_final).unwrap(),
        aiwork_core::BudgetReceiptResult::Settled { released_microcredits: 1_922_400, .. }
    ));
    let mut video_unknown = final_budget_receipt(&parent_request_id, "unknown-finance-video-budget", "0");
    video_unknown.receipt.status = BillingReceiptStatus::Unknown;
    video_unknown.receipt.actual_credits = None;
    video_unknown.receipt.task_ref = None;
    video_unknown.receipt.source_ref = "unknown-finance-video-source".into();
    assert_eq!(store.apply_budget_receipt(video_unknown).unwrap(), aiwork_core::BudgetReceiptResult::Pending);

    assert_eq!(store.active_execution_count_for_key(&key_id).unwrap(), 1);
    assert_eq!(
        store.finish_budget_execution(&parent_request_id, aiwork_core::BudgetExecutionState::Succeeded).unwrap(),
        aiwork_core::BudgetMutation::Applied
    );
    assert_eq!(store.active_execution_count_for_key(&key_id).unwrap(), 0);
    let finished = store.budget_operation(&parent_request_id).unwrap().unwrap();
    assert_eq!(finished.execution_state, aiwork_core::BudgetExecutionState::Succeeded);
    assert_eq!(finished.steps.iter().find(|step| step.kind == aiwork_core::BudgetStepKind::Assist).unwrap().financial_state,
        aiwork_core::BudgetFinancialState::Settled);
    assert_eq!(finished.steps.iter().find(|step| step.kind == aiwork_core::BudgetStepKind::Video).unwrap().financial_state,
        aiwork_core::BudgetFinancialState::Unknown);
    let held = store.key_quota_balance_as_admin(&admin, &key_id, "credits").unwrap();
    assert_eq!((held.available, held.held), (724_922_400, 275_000_000));

    let next_parent = begin_video_parent(&store, &key_id, "unknown-finance-next-parent");
    let mut next_video = video_budget_step(&store, &key_id, &next_parent, "unknown-finance-next-budget");
    next_video.authorization.hold_credits = CreditAmount::parse("1", "credits").unwrap();
    store.begin_budget_operation(&next_parent, next_video).unwrap();
    assert_eq!(store.active_execution_count_for_key(&key_id).unwrap(), 1);
    let after_next = store.key_quota_balance_as_admin(&admin, &key_id, "credits").unwrap();
    assert_eq!((after_next.available, after_next.held), (723_922_400, 276_000_000));
    assert!(directory.0.join("data").join(CORE_DB_FILE).exists());
}

#[test]
fn legacy_quote_counts_running_v2_parent_after_request_cancel_requested() {
    let (directory, store, key_id, _) = budget_fixture("quoted-v2-cancel-requested-slot", 1, 100_000_000);
    let v2_parent = begin_video_parent(&store, &key_id, "quoted-v2-cancel-requested-parent");
    let mut step = video_budget_step(&store, &key_id, &v2_parent, "quoted-v2-cancel-requested-budget");
    step.authorization.hold_credits = CreditAmount::parse("2", "credits").unwrap();
    store.begin_budget_operation(&v2_parent, step).unwrap();
    dispatch_current_budget_step(&store, &v2_parent).unwrap();
    assert_eq!(store.request_state(&v2_parent).unwrap(), aiwork_core::RequestState::Dispatched);
    store
        .transition_request(
            &v2_parent,
            aiwork_core::RequestState::Dispatched,
            aiwork_core::RequestState::CancelRequested,
            None,
        )
        .unwrap();
    assert_eq!(store.request_state(&v2_parent).unwrap(), aiwork_core::RequestState::CancelRequested);
    assert_eq!(
        store.budget_operation(&v2_parent).unwrap().unwrap().execution_state,
        aiwork_core::BudgetExecutionState::Running
    );

    let quoted_request = match store
        .begin_billed_request(BeginRequestInput {
            user_id: "budget-user".into(),
            api_key_id: key_id.clone(),
            protocol: "openai".into(),
            endpoint: "chat".into(),
            model: "legacy-chat".into(),
            idempotency_key: "quoted-v2-cancel-requested-legacy".into(),
            body: json!({"model":"legacy-chat","messages":[]}),
        })
        .unwrap()
    {
        BeginRequest::Created(request) => request,
        other => panic!("expected a fresh legacy quote candidate, got {other:?}"),
    };
    let quote = BillingQuote {
        request_id: quoted_request.id.clone(),
        quote_id: "quoted-v2-cancel-requested-quote".into(),
        request_fingerprint: store.request_fingerprint_for_billing(&quoted_request.id).unwrap(),
        endpoint: quoted_request.endpoint,
        model: quoted_request.model,
        max_credits: CreditAmount::parse("1", "credits").unwrap(),
        unit: "credits".into(),
        expires_at_ms: chrono::Utc::now().timestamp_millis() + 60_000,
        source_ref: "quoted-v2-cancel-requested-source".into(),
    };
    assert!(matches!(
        store.reserve_credit_quote(quote),
        Err(aiwork_core::CoreError::KeyConcurrencyExceeded {
            active_concurrency: 1,
            max_concurrency: 1,
            ..
        })
    ));
    assert_eq!(store.request_state(&quoted_request.id).unwrap(), aiwork_core::RequestState::Received);
    assert!(store.reservation_for_request(&quoted_request.id).unwrap().is_none());
    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    let quote_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM billing_quotes WHERE request_id = ?1",
            [&quoted_request.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(quote_count, 0);
}

#[test]
fn same_key_concurrent_admissions_respect_max_two_and_unknown_finance_frees_slot() {
    let (directory, store, key_id, _) = budget_fixture("same-key-max-two-concurrency", 2, 10_000_000);
    let candidates = (0..3)
        .map(|index| {
            let parent = begin_video_parent(&store, &key_id, &format!("same-key-max-two-{index}"));
            let mut step = video_budget_step(
                &store,
                &key_id,
                &parent,
                &format!("same-key-max-two-budget-{index}"),
            );
            step.authorization.hold_credits = CreditAmount::parse("1", "credits").unwrap();
            (parent, step)
        })
        .collect::<Vec<_>>();

    let db_root = Arc::new(directory.0.clone());
    let admission_barrier = Arc::new(Barrier::new(candidates.len() + 1));
    let workers = candidates
        .iter()
        .map(|(parent, step)| {
            let db_root = Arc::clone(&db_root);
            let barrier = Arc::clone(&admission_barrier);
            let parent = parent.clone();
            let step = step.clone();
            thread::spawn(move || {
                let worker_store = CoreStore::open(db_root.as_path()).unwrap();
                barrier.wait();
                (parent.clone(), step.clone(), worker_store.begin_budget_operation(&parent, step))
            })
        })
        .collect::<Vec<_>>();
    admission_barrier.wait();

    let mut admitted = Vec::new();
    let mut rejected = Vec::new();
    for worker in workers {
        let (parent, step, result) = worker.join().unwrap();
        match result {
            Ok(_) => admitted.push((parent, step)),
            Err(aiwork_core::CoreError::KeyConcurrencyExceeded {
                active_concurrency: 2,
                max_concurrency: 2,
                ..
            }) => rejected.push((parent, step)),
            Err(error) => panic!("unexpected admission result: {error:?}"),
        }
    }
    assert_eq!(admitted.len(), 2, "max_concurrency=2 must admit exactly two concurrent requests");
    assert_eq!(rejected.len(), 1, "the third concurrent request must be rejected at the limit");
    assert!(store.reservation_for_request(&rejected[0].0).unwrap().is_none());

    // A terminal execution with unresolved finances retains its reservation,
    // but finishing its parent frees the execution slot for the rejected next request.
    let (finished_parent, finished_step) = admitted.remove(0);
    dispatch_current_budget_step(&store, &finished_parent).unwrap();
    store.bind_budget_video_task(&finished_parent, "same-key-max-two-finished-task").unwrap();
    store
        .mark_budget_step_execution(&finished_parent, aiwork_core::BudgetExecutionState::Succeeded)
        .unwrap();
    let mut unknown = final_budget_receipt(
        &finished_parent,
        &finished_step.authorization.budget_id,
        "0",
    );
    unknown.receipt.status = BillingReceiptStatus::Unknown;
    unknown.receipt.actual_credits = None;
    unknown.receipt.task_ref = None;
    unknown.receipt.source_ref = "same-key-max-two-unknown-finance".into();
    assert_eq!(store.apply_budget_receipt(unknown).unwrap(), aiwork_core::BudgetReceiptResult::Pending);
    assert_eq!(
        store.finish_budget_execution(&finished_parent, aiwork_core::BudgetExecutionState::Succeeded).unwrap(),
        aiwork_core::BudgetMutation::Applied
    );
    let finished = store.budget_operation(&finished_parent).unwrap().unwrap();
    assert_eq!(finished.execution_state, aiwork_core::BudgetExecutionState::Succeeded);
    assert_eq!(finished.steps[0].financial_state, aiwork_core::BudgetFinancialState::Unknown);
    assert_eq!(finished.steps[0].execution_state, aiwork_core::BudgetExecutionState::Succeeded);

    let (next_parent, next_step) = rejected.remove(0);
    store.begin_budget_operation(&next_parent, next_step).unwrap();
    assert_eq!(store.active_execution_count_for_key(&key_id).unwrap(), 2);
    assert!(directory.0.join("data").join(CORE_DB_FILE).exists());
}

#[test]
fn legacy_preflight_counts_v2_parent_once_and_admits_only_up_to_shared_limit() {
    let (directory, store, key_id, admin) = budget_fixture("mixed-preflight-concurrency", 2, 1_000_000_000);
    let parent = begin_video_parent(&store, &key_id, "mixed-preflight-v2-parent");
    let helper = match store
        .begin_budget_assist_request(
            &parent,
            budget_assist_input(&key_id, "mixed-preflight-v2-helper"),
        )
        .unwrap()
    {
        BeginRequest::Created(request) => request,
        other => panic!("expected a new helper request, got {other:?}"),
    };
    store
        .begin_budget_operation(
            &parent,
            assist_budget_step(&store, &key_id, &parent, &helper.id, "mixed-preflight-v2-budget"),
        )
        .unwrap();
    assert_eq!(store.active_execution_count_for_key(&key_id).unwrap(), 1);
    let admin_keys = store.list_api_keys_as_admin(&admin, Some("budget-user")).unwrap();
    assert_eq!(admin_keys.iter().find(|view| view.id == key_id).unwrap().current_concurrency, 1);

    let preflight = |idempotency_key: &str| aiwork_core::PreflightReserveInput {
        request: BeginRequestInput {
            user_id: "budget-user".into(),
            api_key_id: key_id.clone(),
            protocol: "openai".into(),
            endpoint: "chat".into(),
            model: "legacy-chat".into(),
            idempotency_key: idempotency_key.into(),
            body: json!({"model":"legacy-chat","messages":[]}),
        },
        resource_kind: "credits".into(),
        amount: 1_000_000,
        ttl_ms: 60_000,
    };
    assert!(matches!(
        store.preflight_reserve(preflight("mixed-preflight-first")),
        Ok(aiwork_core::PreflightReserveResult::Created { .. })
    ));
    assert_eq!(store.active_execution_count_for_key(&key_id).unwrap(), 2);
    let admin_keys = store.list_api_keys_as_admin(&admin, Some("budget-user")).unwrap();
    assert_eq!(admin_keys.iter().find(|view| view.id == key_id).unwrap().current_concurrency, 2);
    assert!(matches!(
        store.preflight_reserve(preflight("mixed-preflight-third")),
        Err(aiwork_core::CoreError::KeyConcurrencyExceeded { active_concurrency: 2, max_concurrency: 2, .. })
    ));
    assert_eq!(store.active_execution_count_for_key(&key_id).unwrap(), 2);
    assert!(directory.0.join("data").join(CORE_DB_FILE).exists());
}

#[test]
fn controlled_admission_counts_v2_and_legacy_execution_slots_together() {
    let (directory, store, key_id, _) = budget_fixture("mixed-controlled-concurrency", 2, 1_000_000_000);
    let v2_parent = begin_video_parent(&store, &key_id, "mixed-controlled-v2-parent");
    store
        .begin_budget_operation(
            &v2_parent,
            video_budget_step(&store, &key_id, &v2_parent, "mixed-controlled-v2-budget"),
        )
        .unwrap();
    let legacy_parent = begin_video_parent(&store, &key_id, "mixed-controlled-legacy-parent");
    assert_eq!(store.active_execution_count_for_key(&key_id).unwrap(), 2);
    let controlled_parent = begin_video_parent(&store, &key_id, "mixed-controlled-new-parent");
    let snapshot = aiwork_core::UpstreamCreditSnapshot {
        total: CreditAmount::parse("1000", "credits").unwrap(),
        updated_at_ms: chrono::Utc::now().timestamp_millis(),
    };
    let before_attempt = store.active_execution_count_for_key(&key_id).unwrap();
    assert_eq!(before_attempt, 3); // v2, legacy, and this already-created Received request
    assert!(matches!(
        store.begin_controlled_operation(&controlled_parent, snapshot),
        Err(aiwork_core::CoreError::KeyConcurrencyExceeded { active_concurrency: 2, max_concurrency: 2, .. })
    ));
    assert_eq!(store.active_execution_count_for_key(&key_id).unwrap(), before_attempt);
    assert_eq!(store.request_state(&controlled_parent).unwrap(), aiwork_core::RequestState::Received);
    assert!(store.reservation_for_request(&controlled_parent).unwrap().is_none());

    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    let controlled: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM controlled_billing_operations WHERE parent_request_id = ?1",
            [&controlled_parent],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(controlled, 0);
    assert!(!legacy_parent.is_empty());
}

#[test]
fn ten_keys_each_settle_three_out_of_order_steps_without_cross_key_leaks() {
    let (directory, store, first_key, admin) =
        budget_fixture("ten-key-three-step-isolation", 3, 100_000_000);
    let mut keys = vec![first_key];
    for index in 1..10 {
        let key = store
            .issue_api_key_as_admin_with_max_concurrency(
                "budget-user",
                &format!("ten-key-{index}"),
                BTreeSet::from(["video:submit".into(), "chat:invoke".into()]),
                3,
                &admin,
            )
            .unwrap();
        store
            .key_quota_grant_as_admin(
                &admin,
                KeyQuotaGrant {
                    api_key_id: key.id.clone(),
                    resource_kind: "credits".into(),
                    amount: 100_000_000,
                    actor_user_id: "budget-admin".into(),
                    reason: "ten-key isolation fixture".into(),
                },
            )
            .unwrap();
        keys.push(key.id);
    }

    let mut cases = Vec::new();
    for (key_index, key_id) in keys.iter().enumerate() {
        for step_index in 0..3 {
            let parent = begin_video_parent(
                &store,
                key_id,
                &format!("ten-key-{key_index}-parent-{step_index}"),
            );
            let budget_id = format!("ten-key-{key_index}-budget-{step_index}");
            let mut input = video_budget_step(&store, key_id, &parent, &budget_id);
            input.authorization.hold_credits = CreditAmount::parse("20", "credits").unwrap();
            cases.push((key_index, step_index, parent, input));
        }
    }

    let db_root = Arc::new(directory.0.clone());
    let reserve_barrier = Arc::new(Barrier::new(cases.len() + 1));
    let reserve_workers = cases
        .iter()
        .map(|(_, _, parent_request_id, input)| {
            let db_root = Arc::clone(&db_root);
            let barrier = Arc::clone(&reserve_barrier);
            let parent_request_id = parent_request_id.clone();
            let input = input.clone();
            thread::spawn(move || {
                let worker_store = CoreStore::open(db_root.as_path()).unwrap();
                barrier.wait();
                worker_store.begin_budget_operation(&parent_request_id, input)
            })
        })
        .collect::<Vec<_>>();
    reserve_barrier.wait();
    for result in reserve_workers {
        assert!(result.join().unwrap().is_ok(), "a concurrent per-key reservation was rejected");
    }

    let mut receipts = Vec::with_capacity(cases.len());
    for (key_index, step_index, parent_request_id, input) in &cases {
        dispatch_current_budget_step(&store, parent_request_id).unwrap();
        let task_ref = format!("ten-key-task-{key_index}-{step_index}");
        store.bind_budget_video_task(parent_request_id, &task_ref).unwrap();
        store
            .mark_budget_step_execution(parent_request_id, aiwork_core::BudgetExecutionState::Succeeded)
            .unwrap();
        let mut receipt = final_budget_receipt(
            parent_request_id,
            &input.authorization.budget_id,
            &format!("{}", key_index + 1),
        );
        receipt.receipt.source_ref = format!("ten-key-source-{key_index}-{step_index}");
        receipt.receipt.task_ref = Some(task_ref);
        receipts.push((*key_index, *step_index, parent_request_id.clone(), receipt));
    }

    // For each Key, race its first Final against an identical replay on two
    // independent connections. Settle step ordinals in reverse order while
    // keeping every same-receipt first-write race bounded by a barrier.
    for step_index in (0..3).rev() {
        let round = receipts
            .iter()
            .filter(|(_, receipt_step_index, _, _)| *receipt_step_index == step_index)
            .collect::<Vec<_>>();
        let race_barrier = Arc::new(Barrier::new(round.len() * 2 + 1));
        let mut race_workers = Vec::with_capacity(round.len() * 2);
        for (_, _, _, receipt) in &round {
            for replay in [false, true] {
                let db_root = Arc::clone(&db_root);
                let barrier = Arc::clone(&race_barrier);
                let mut raced_receipt = receipt.clone();
                if replay {
                    raced_receipt.receipt.observed_at_ms += 1;
                }
                race_workers.push(thread::spawn(move || {
                    let worker_store = CoreStore::open(db_root.as_path()).unwrap();
                    barrier.wait();
                    worker_store.apply_budget_receipt(raced_receipt)
                }));
            }
        }
        race_barrier.wait();
        let mut workers = race_workers.into_iter();
        while let Some(first_worker) = workers.next() {
            let second_worker = workers.next().expect("each receipt has two racers");
            let outcomes = [first_worker.join().unwrap().unwrap(), second_worker.join().unwrap().unwrap()];
            assert_eq!(
                outcomes
                    .iter()
                    .filter(|outcome| matches!(outcome, aiwork_core::BudgetReceiptResult::Settled { .. }))
                    .count(),
                1,
                "each first Final race must settle exactly once"
            );
            assert_eq!(
                outcomes
                    .iter()
                    .filter(|outcome| matches!(outcome, aiwork_core::BudgetReceiptResult::Duplicate))
                    .count(),
                1,
                "the competing replay must be Duplicate"
            );
        }
    }
    for (_, _, parent_request_id, _) in &receipts {
        store
            .finish_budget_execution(parent_request_id, aiwork_core::BudgetExecutionState::Succeeded)
            .unwrap();
    }

    for (key_index, key_id) in keys.iter().enumerate() {
        let balance = store.key_quota_balance_as_admin(&admin, key_id, "credits").unwrap();
        let settled = 3 * (key_index as i64 + 1) * 1_000_000;
        assert_eq!((balance.available, balance.held, balance.settled),
            (100_000_000 - settled, 0, settled), "wrong balance for Key {key_index}");
    }

    // A wrong owner tuple is rejected without writing evidence or blocking a Key.
    let mut wrong_budget = receipts[0].3.clone();
    wrong_budget.budget_id = receipts[1].3.budget_id.clone();
    assert!(store.apply_budget_receipt(wrong_budget).is_err());
    let mut wrong_account = receipts[0].3.clone();
    wrong_account.account_ref = "foreign-account".into();
    assert!(store.apply_budget_receipt(wrong_account).is_err());
    let mut wrong_instance = receipts[0].3.clone();
    wrong_instance.bridge_instance_id = "foreign-instance".into();
    assert!(store.apply_budget_receipt(wrong_instance).is_err());

    let key_zero_balance = store.key_quota_balance_as_admin(&admin, &keys[0], "credits").unwrap();
    let mut wrong_task = receipts[0].3.clone();
    wrong_task.receipt.source_ref = "ten-key-cross-owner-task-conflict".into();
    wrong_task.receipt.task_ref = receipts[3].3.receipt.task_ref.clone();
    assert_eq!(store.apply_budget_receipt(wrong_task).unwrap(), aiwork_core::BudgetReceiptResult::Conflict);
    assert_eq!(store.key_quota_balance_as_admin(&admin, &keys[0], "credits").unwrap(), key_zero_balance);

    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    let block_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM api_key_billing_blocks WHERE key_id = ?1", [&keys[0]], |row| row.get(0))
        .unwrap();
    let other_block_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM api_key_billing_blocks WHERE key_id = ?1", [&keys[1]], |row| row.get(0))
        .unwrap();
    assert_eq!((block_count, other_block_count), (1, 0));

    let next_parent = begin_video_parent(&store, &keys[1], "ten-key-other-key-next-parent");
    let mut next_step = video_budget_step(&store, &keys[1], &next_parent, "ten-key-other-key-next-budget");
    next_step.authorization.hold_credits = CreditAmount::parse("1", "credits").unwrap();
    store.begin_budget_operation(&next_parent, next_step).unwrap();
    assert_eq!(store.active_execution_count_for_key(&keys[1]).unwrap(), 1);
}

#[test]
fn negative_key_and_user_cap_balances_block_until_admin_top_up() {
    let (directory, store, first_key, admin) =
        budget_fixture("negative-budget-restoration", 3, 10_000_000);
    let second_key = store
        .issue_api_key_as_admin_with_max_concurrency(
            "budget-user",
            "negative-budget-second-key",
            BTreeSet::from(["video:submit".into(), "chat:invoke".into()]),
            3,
            &admin,
        )
        .unwrap()
        .id;
    store
        .key_quota_grant_as_admin(
            &admin,
            KeyQuotaGrant {
                api_key_id: second_key.clone(),
                resource_kind: "credits".into(),
                amount: 10_000_000,
                actor_user_id: "budget-admin".into(),
                reason: "second-key debt fixture".into(),
            },
        )
        .unwrap();
    store
        .quota_pool_grant_as_admin(
            &admin,
            aiwork_core::QuotaGrant {
                user_id: "budget-user".into(),
                resource_kind: "credits".into(),
                amount: 20_000_000,
                actor_user_id: "budget-admin".into(),
                reason: "user cap debt fixture".into(),
            },
        )
        .unwrap();

    let debt_parent = begin_video_parent(&store, &first_key, "negative-budget-debt-parent");
    store
        .begin_budget_operation(
            &debt_parent,
            video_budget_step(&store, &first_key, &debt_parent, "negative-budget-debt-step"),
        )
        .unwrap();
    dispatch_current_budget_step(&store, &debt_parent).unwrap();
    store.bind_budget_video_task(&debt_parent, "negative-budget-debt-task").unwrap();
    store
        .mark_budget_step_execution(&debt_parent, aiwork_core::BudgetExecutionState::Succeeded)
        .unwrap();
    let mut overspend = final_budget_receipt(&debt_parent, "negative-budget-debt-step", "25");
    overspend.receipt.task_ref = Some("negative-budget-debt-task".into());
    assert!(matches!(
        store.apply_budget_receipt(overspend).unwrap(),
        aiwork_core::BudgetReceiptResult::Settled { debt: true, .. }
    ));
    store
        .finish_budget_execution(&debt_parent, aiwork_core::BudgetExecutionState::Succeeded)
        .unwrap();
    let first_balance = store.key_quota_balance_as_admin(&admin, &first_key, "credits").unwrap();
    let cap_balance = store.quota_pool_balance_as_admin(&admin, "budget-user", "credits").unwrap();
    assert_eq!(first_balance.available, -15_000_000);
    assert_eq!(cap_balance.available, -5_000_000);

    let first_retry = begin_video_parent(&store, &first_key, "negative-budget-first-key-retry");
    let mut first_step = video_budget_step(
        &store,
        &first_key,
        &first_retry,
        "negative-budget-first-key-retry-step",
    );
    first_step.authorization.hold_credits = CreditAmount::parse("1", "credits").unwrap();
    assert!(store.begin_budget_operation(&first_retry, first_step.clone()).is_err());
    assert!(store.reservation_for_request(&first_retry).unwrap().is_none());
    let second_retry = begin_video_parent(&store, &second_key, "negative-budget-cap-retry");
    let mut second_step = video_budget_step(&store, &second_key, &second_retry, "negative-budget-cap-retry-step");
    second_step.authorization.hold_credits = CreditAmount::parse("1", "credits").unwrap();
    assert!(store.begin_budget_operation(&second_retry, second_step.clone()).is_err());
    assert!(store.reservation_for_request(&second_retry).unwrap().is_none());

    store
        .quota_pool_grant_as_admin(
            &admin,
            aiwork_core::QuotaGrant {
                user_id: "budget-user".into(),
                resource_kind: "credits".into(),
                amount: 7_000_000,
                actor_user_id: "budget-admin".into(),
                reason: "restore negative user cap".into(),
            },
        )
        .unwrap();
    assert_eq!(store.quota_pool_balance_as_admin(&admin, "budget-user", "credits").unwrap().available, 2_000_000);
    // The cap is repaired, but the first Key's own negative ledger still blocks it.
    assert!(store.begin_budget_operation(&first_retry, first_step.clone()).is_err());
    assert!(store.reservation_for_request(&first_retry).unwrap().is_none());
    store.begin_budget_operation(&second_retry, second_step).unwrap();
    assert_eq!(store.active_execution_count_for_key(&second_key).unwrap(), 1);

    store
        .key_quota_grant_as_admin(
            &admin,
            KeyQuotaGrant {
                api_key_id: first_key.clone(),
                resource_kind: "credits".into(),
                amount: 16_000_000,
                actor_user_id: "budget-admin".into(),
                reason: "restore negative first key".into(),
            },
        )
        .unwrap();
    assert_eq!(store.key_quota_balance_as_admin(&admin, &first_key, "credits").unwrap().available, 1_000_000);
    store.begin_budget_operation(&first_retry, first_step).unwrap();

    let restored_first = store.key_quota_balance_as_admin(&admin, &first_key, "credits").unwrap();
    assert_eq!((restored_first.available, restored_first.held, restored_first.settled), (0, 1_000_000, 25_000_000));
    let restored_second = store.key_quota_balance_as_admin(&admin, &second_key, "credits").unwrap();
    assert_eq!((restored_second.available, restored_second.held, restored_second.settled), (9_000_000, 1_000_000, 0));
    let restored_cap = store.quota_pool_balance_as_admin(&admin, "budget-user", "credits").unwrap();
    assert_eq!((restored_cap.available, restored_cap.held, restored_cap.settled), (0, 2_000_000, 25_000_000));
    assert!(directory.0.join("data").join(CORE_DB_FILE).exists());
}

#[test]
fn reconcile_v2_valid_states_overrun_and_later_account_versions_without_quarantine() {
    let (directory, store, key_id, admin) = budget_fixture("v2-event-group-valid", 5, 100_000_000);
    store
        .quota_pool_grant_as_admin(
            &admin,
            aiwork_core::QuotaGrant {
                user_id: "budget-user".into(),
                resource_kind: "credits".into(),
                amount: 100_000_000,
                actor_user_id: "budget-admin".into(),
                reason: "v2 event group user cap".into(),
            },
        )
        .unwrap();

    let assist_parent = begin_video_parent(&store, &key_id, "v2-event-group-assist");
    let assist_child = match store
        .begin_budget_assist_request(
            &assist_parent,
            budget_assist_input(&key_id, "v2-event-group-assist-child"),
        )
        .unwrap()
    {
        BeginRequest::Created(request) => request,
        other => panic!("expected a new Assist child, got {other:?}"),
    };
    let mut assist_step = assist_budget_step(
        &store,
        &key_id,
        &assist_parent,
        &assist_child.id,
        "v2-event-group-assist-budget",
    );
    assist_step.authorization.hold_credits = CreditAmount::parse("2", "credits").unwrap();
    store.begin_budget_operation(&assist_parent, assist_step).unwrap();
    store.release_unattempted_budget_step(&assist_child.id, "verified not dispatched").unwrap();
    store
        .finish_budget_execution(&assist_parent, aiwork_core::BudgetExecutionState::Canceled)
        .unwrap();

    let held_parent = begin_video_parent(&store, &key_id, "v2-event-group-held");
    let mut held_step = video_budget_step(&store, &key_id, &held_parent, "v2-event-group-held-budget");
    held_step.authorization.hold_credits = CreditAmount::parse("2", "credits").unwrap();
    store.begin_budget_operation(&held_parent, held_step).unwrap();

    let unknown_parent = begin_video_parent(&store, &key_id, "v2-event-group-unknown");
    let mut unknown_step = video_budget_step(&store, &key_id, &unknown_parent, "v2-event-group-unknown-budget");
    unknown_step.authorization.hold_credits = CreditAmount::parse("2", "credits").unwrap();
    store.begin_budget_operation(&unknown_parent, unknown_step).unwrap();
    dispatch_current_budget_step(&store, &unknown_parent).unwrap();
    store.bind_budget_video_task(&unknown_parent, "v2-event-group-unknown-task").unwrap();
    store
        .mark_budget_step_execution(&unknown_parent, aiwork_core::BudgetExecutionState::Unknown)
        .unwrap();
    let mut unknown = final_budget_receipt(&unknown_parent, "v2-event-group-unknown-budget", "0");
    unknown.receipt.status = BillingReceiptStatus::Unknown;
    unknown.receipt.actual_credits = None;
    unknown.receipt.task_ref = None;
    unknown.receipt.source_ref = "v2-event-group-unknown-source".into();
    assert_eq!(store.apply_budget_receipt(unknown).unwrap(), aiwork_core::BudgetReceiptResult::Pending);

    let released_parent = begin_video_parent(&store, &key_id, "v2-event-group-released");
    let mut released_step = video_budget_step(&store, &key_id, &released_parent, "v2-event-group-released-budget");
    released_step.authorization.hold_credits = CreditAmount::parse("2", "credits").unwrap();
    store.begin_budget_operation(&released_parent, released_step).unwrap();
    store.release_unattempted_budget_step(&released_parent, "verified never dispatched").unwrap();
    store
        .finish_budget_execution(&released_parent, aiwork_core::BudgetExecutionState::Canceled)
        .unwrap();

    let overrun_parent = begin_video_parent(&store, &key_id, "v2-event-group-overrun");
    let mut overrun_step = video_budget_step(&store, &key_id, &overrun_parent, "v2-event-group-overrun-budget");
    overrun_step.authorization.hold_credits = CreditAmount::parse("2", "credits").unwrap();
    store.begin_budget_operation(&overrun_parent, overrun_step).unwrap();
    dispatch_current_budget_step(&store, &overrun_parent).unwrap();
    store.bind_budget_video_task(&overrun_parent, "v2-event-group-overrun-task").unwrap();
    store
        .mark_budget_step_execution(&overrun_parent, aiwork_core::BudgetExecutionState::Succeeded)
        .unwrap();
    let mut overrun = final_budget_receipt(&overrun_parent, "v2-event-group-overrun-budget", "3");
    overrun.receipt.task_ref = Some("v2-event-group-overrun-task".into());
    assert!(matches!(
        store.apply_budget_receipt(overrun).unwrap(),
        aiwork_core::BudgetReceiptResult::Settled { debt: false, .. }
    ));
    store
        .finish_budget_execution(&overrun_parent, aiwork_core::BudgetExecutionState::Succeeded)
        .unwrap();

    let versioned_parent = begin_video_parent(&store, &key_id, "v2-event-group-versioned");
    let mut versioned_step = video_budget_step(&store, &key_id, &versioned_parent, "v2-event-group-versioned-budget");
    versioned_step.authorization.hold_credits = CreditAmount::parse("2", "credits").unwrap();
    store.begin_budget_operation(&versioned_parent, versioned_step).unwrap();
    store
        .key_quota_grant_as_admin(
            &admin,
            KeyQuotaGrant {
                api_key_id: key_id.clone(),
                resource_kind: "credits".into(),
                amount: 1_000_000,
                actor_user_id: "budget-admin".into(),
                reason: "advance key version while v2 hold is live".into(),
            },
        )
        .unwrap();
    store
        .quota_pool_grant_as_admin(
            &admin,
            aiwork_core::QuotaGrant {
                user_id: "budget-user".into(),
                resource_kind: "credits".into(),
                amount: 1_000_000,
                actor_user_id: "budget-admin".into(),
                reason: "advance user-cap version while v2 hold is live".into(),
            },
        )
        .unwrap();
    dispatch_current_budget_step(&store, &versioned_parent).unwrap();
    store.bind_budget_video_task(&versioned_parent, "v2-event-group-versioned-task").unwrap();
    store
        .mark_budget_step_execution(&versioned_parent, aiwork_core::BudgetExecutionState::Succeeded)
        .unwrap();
    let mut versioned_final = final_budget_receipt(&versioned_parent, "v2-event-group-versioned-budget", "1");
    versioned_final.receipt.task_ref = Some("v2-event-group-versioned-task".into());
    assert!(matches!(
        store.apply_budget_receipt(versioned_final).unwrap(),
        aiwork_core::BudgetReceiptResult::Settled { .. }
    ));
    store
        .finish_budget_execution(&versioned_parent, aiwork_core::BudgetExecutionState::Succeeded)
        .unwrap();

    assert_eq!(store.reconcile_quota_event_groups(chrono::Utc::now().timestamp_millis()).unwrap(), 0);

    store
        .mark_budget_step_execution(&unknown_parent, aiwork_core::BudgetExecutionState::Succeeded)
        .unwrap();
    let mut unknown_final = final_budget_receipt(&unknown_parent, "v2-event-group-unknown-budget", "0.5");
    unknown_final.receipt.task_ref = Some("v2-event-group-unknown-task".into());
    assert!(matches!(
        store.apply_budget_receipt(unknown_final).unwrap(),
        aiwork_core::BudgetReceiptResult::Settled { .. }
    ));
    store
        .finish_budget_execution(&unknown_parent, aiwork_core::BudgetExecutionState::Succeeded)
        .unwrap();

    dispatch_current_budget_step(&store, &held_parent).unwrap();
    store.bind_budget_video_task(&held_parent, "v2-event-group-held-task").unwrap();
    store
        .mark_budget_step_execution(&held_parent, aiwork_core::BudgetExecutionState::Succeeded)
        .unwrap();
    let mut held_final = final_budget_receipt(&held_parent, "v2-event-group-held-budget", "0.5");
    held_final.receipt.task_ref = Some("v2-event-group-held-task".into());
    assert!(matches!(
        store.apply_budget_receipt(held_final).unwrap(),
        aiwork_core::BudgetReceiptResult::Settled { .. }
    ));
    store
        .finish_budget_execution(&held_parent, aiwork_core::BudgetExecutionState::Succeeded)
        .unwrap();

    assert_eq!(store.reconcile_quota_event_groups(chrono::Utc::now().timestamp_millis()).unwrap(), 0);
    let next_parent = begin_video_parent(&store, &key_id, "v2-event-group-after-reconcile");
    let mut next_step = video_budget_step(&store, &key_id, &next_parent, "v2-event-group-after-reconcile-budget");
    next_step.authorization.hold_credits = CreditAmount::parse("1", "credits").unwrap();
    store.begin_budget_operation(&next_parent, next_step).unwrap();
    assert!(store.key_quota_balance_as_admin(&admin, &key_id, "credits").unwrap().available > 0);
    assert!(directory.0.join("data").join(CORE_DB_FILE).exists());
}

#[derive(Clone, Copy, Debug)]
enum MissingV2ReservationAccountPointer {
    Key,
    UserCap,
    Both,
}

fn held_v2_reservation_with_missing_pointer(
    label: &str,
    missing: MissingV2ReservationAccountPointer,
) -> (TestDirectory, CoreStore, String, String, String, String, aiwork_core::Principal) {
    let (directory, store, key_id, admin) = budget_fixture(label, 5, 100_000_000);
    store
        .quota_pool_grant_as_admin(
            &admin,
            aiwork_core::QuotaGrant {
                user_id: "budget-user".into(),
                resource_kind: "credits".into(),
                amount: 100_000_000,
                actor_user_id: "budget-admin".into(),
                reason: "missing V2 reservation account pointer fixture".into(),
            },
        )
        .unwrap();
    let request_id = begin_video_parent(&store, &key_id, label);
    let budget_id = format!("{label}-budget");
    store
        .begin_budget_operation(
            &request_id,
            video_budget_step(&store, &key_id, &request_id, &budget_id),
        )
        .unwrap();

    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    let (key_account_id, user_cap_account_id): (String, String) = connection
        .query_row(
            "SELECT key_budget_account_id, user_cap_account_id
             FROM quota_reservations WHERE request_id = ?1",
            [&request_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let sql = match missing {
        MissingV2ReservationAccountPointer::Key => {
            "UPDATE quota_reservations SET key_budget_account_id = NULL WHERE request_id = ?1"
        }
        MissingV2ReservationAccountPointer::UserCap => {
            "UPDATE quota_reservations SET user_cap_account_id = NULL WHERE request_id = ?1"
        }
        MissingV2ReservationAccountPointer::Both => {
            "UPDATE quota_reservations
             SET key_budget_account_id = NULL, user_cap_account_id = NULL WHERE request_id = ?1"
        }
    };
    assert_eq!(connection.execute(sql, [&request_id]).unwrap(), 1);

    (
        directory,
        store,
        request_id,
        budget_id,
        key_account_id,
        user_cap_account_id,
        admin,
    )
}

#[derive(Debug, PartialEq, Eq)]
struct V2ReservationSnapshot {
    request_state: String,
    operation_state: String,
    budget_id: String,
    execution_state: String,
    dispatch_attempted: i64,
    financial_state: String,
    reservation_state: String,
    reservation_amount: i64,
    key_account_id: Option<String>,
    user_cap_account_id: Option<String>,
    event_group_id: Option<String>,
    ledger_events: Vec<(Option<String>, String, i64, i64, Option<i64>)>,
}

fn v2_reservation_snapshot(connection: &Connection, request_id: &str) -> V2ReservationSnapshot {
    let mut snapshot = connection
        .query_row(
            "SELECT request.state, operation.execution_state, step.budget_id,
                    step.execution_state, step.dispatch_attempted, step.financial_state,
                    reservation.state, reservation.amount, reservation.key_budget_account_id,
                    reservation.user_cap_account_id, reservation.event_group_id
             FROM quota_reservations reservation
             JOIN budget_steps step ON step.reservation_id = reservation.id
                                  AND step.request_id = reservation.request_id
             JOIN budget_operations operation ON operation.operation_id = step.operation_id
             JOIN requests request ON request.id = step.request_id
             WHERE reservation.request_id = ?1",
            [request_id],
            |row| {
                Ok(V2ReservationSnapshot {
                    request_state: row.get(0)?,
                    operation_state: row.get(1)?,
                    budget_id: row.get(2)?,
                    execution_state: row.get(3)?,
                    dispatch_attempted: row.get(4)?,
                    financial_state: row.get(5)?,
                    reservation_state: row.get(6)?,
                    reservation_amount: row.get(7)?,
                    key_account_id: row.get(8)?,
                    user_cap_account_id: row.get(9)?,
                    event_group_id: row.get(10)?,
                    ledger_events: Vec::new(),
                })
            },
        )
        .unwrap();
    let mut statement = connection
        .prepare(
            "SELECT event_group_id, event_kind, amount, delta, authorization_revision
             FROM quota_ledger WHERE request_id = ?1 ORDER BY entry_id",
        )
        .unwrap();
    snapshot.ledger_events = statement
        .query_map([request_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    snapshot
}

fn v2_budget_account_snapshot(connection: &Connection, account_id: &str) -> (i64, String) {
    connection
        .query_row(
            "SELECT version, migration_state FROM quota_budget_accounts WHERE id = ?1",
            [account_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
}

type V2ReservationFacts = (String, String, String, i64, Option<String>, Option<String>, Option<String>);
type V2LedgerFacts = (String, Option<String>, String, i64, i64, Option<String>, Option<String>);

fn v2_hold_ledger_facts_snapshot(
    connection: &Connection,
) -> (Vec<V2ReservationFacts>, Vec<V2LedgerFacts>) {
    let mut statement = connection
        .prepare(
            "SELECT id, request_id, state, amount, key_budget_account_id,
                    user_cap_account_id, event_group_id
             FROM quota_reservations ORDER BY id",
        )
        .unwrap();
    let reservations = statement
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    drop(statement);

    let mut statement = connection
        .prepare(
            "SELECT entry_id, request_id, event_kind, amount, delta,
                    budget_account_id, event_group_id
             FROM quota_ledger ORDER BY entry_id",
        )
        .unwrap();
    let ledger = statement
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    (reservations, ledger)
}

fn assert_reconcile_required_key_rejects_budget_admission(
    store: &CoreStore,
    connection: &Connection,
    key_id: &str,
    key_account_id: &str,
    parent_idempotency_key: &str,
    budget_id: &str,
) {
    let parent = begin_video_parent(store, key_id, parent_idempotency_key);
    let financial_facts_before = v2_hold_ledger_facts_snapshot(connection);
    let result = store.begin_budget_operation(
        &parent,
        video_budget_step(store, key_id, &parent, budget_id),
    );
    assert!(
        matches!(
            result,
            Err(aiwork_core::CoreError::QuotaMigrationPending { ref account_id })
                if account_id == key_account_id
        ),
        "admission must fail specifically because this Key account requires reconciliation; got {result:?}",
    );

    let new_reservations: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM quota_reservations WHERE request_id = ?1",
            [&parent],
            |row| row.get(0),
        )
        .unwrap();
    let new_ledger_entries: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM quota_ledger WHERE request_id = ?1",
            [&parent],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(new_reservations, 0, "a rejected request must not acquire a hold");
    assert_eq!(new_ledger_entries, 0, "a rejected request must not write ledger entries");
    assert_eq!(
        v2_hold_ledger_facts_snapshot(connection),
        financial_facts_before,
        "rejecting a new request must not mutate any existing hold or ledger fact",
    );
}

#[test]
fn v2_key_only_hold_remains_valid_after_user_cap_account_is_created() {
    let (directory, store, key_id, admin) = budget_fixture("v2-key-only-then-cap", 5, 100_000_000);
    let request_id = begin_video_parent(&store, &key_id, "v2-key-only-then-cap");
    let budget_id = "v2-key-only-then-cap-budget";
    let mut step = video_budget_step(&store, &key_id, &request_id, budget_id);
    step.authorization.hold_credits = CreditAmount::parse("2", "credits").unwrap();
    store.begin_budget_operation(&request_id, step).unwrap();

    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    let (key_account_id, user_cap_account_id, event_group_id): (String, Option<String>, String) = connection
        .query_row(
            "SELECT key_budget_account_id, user_cap_account_id, event_group_id
             FROM quota_reservations WHERE request_id = ?1",
            [&request_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert!(user_cap_account_id.is_none(), "the hold was created without a user-cap account");
    let reserve_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM quota_ledger
             WHERE event_group_id = ?1 AND event_kind = 'reserve'",
            [&event_group_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(reserve_count, 1, "the immutable hold evidence is Key-only");
    let key_before = v2_budget_account_snapshot(&connection, &key_account_id);

    store
        .quota_pool_grant_as_admin(
            &admin,
            aiwork_core::QuotaGrant {
                user_id: "budget-user".into(),
                resource_kind: "credits".into(),
                amount: 100_000_000,
                actor_user_id: "budget-admin".into(),
                reason: "user-cap created after a Key-only V2 hold".into(),
            },
        )
        .unwrap();
    let later_user_cap_id: String = connection
        .query_row(
            "SELECT id FROM quota_budget_accounts
             WHERE scope = 'user_cap' AND user_id = 'budget-user' AND resource_kind = 'credits'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_ne!(later_user_cap_id, key_account_id);

    assert_eq!(
        store.reconcile_quota_event_groups(chrono::Utc::now().timestamp_millis()).unwrap(),
        0,
        "a later account must not retroactively change the original hold's account set",
    );
    assert_eq!(v2_budget_account_snapshot(&connection, &key_account_id), key_before);
    let user_cap_state: String = connection
        .query_row(
            "SELECT migration_state FROM quota_budget_accounts WHERE id = ?1",
            [&later_user_cap_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(user_cap_state, "ready", "the later account is not part of this hold");

    assert_eq!(
        store.mark_budget_step_dispatched(&request_id, budget_id).unwrap(),
        aiwork_core::BudgetMutation::Applied,
    );
}

#[test]
fn reconcile_v2_double_null_key_account_pointer_quarantines_key_without_rewriting_hold() {
    let (directory, store, key_id, admin) = budget_fixture("v2-double-null-key", 5, 100_000_000);
    let request_id = begin_video_parent(&store, &key_id, "v2-double-null-key");
    let budget_id = "v2-double-null-key-budget";
    let mut step = video_budget_step(&store, &key_id, &request_id, budget_id);
    step.authorization.hold_credits = CreditAmount::parse("2", "credits").unwrap();
    store.begin_budget_operation(&request_id, step).unwrap();

    let database = directory.0.join("data").join(CORE_DB_FILE);
    let connection = Connection::open(&database).unwrap();
    let (reservation_id, key_account_id, event_group_id, user_cap_account_id):
        (String, String, String, Option<String>) = connection
        .query_row(
            "SELECT id, key_budget_account_id, event_group_id, user_cap_account_id
             FROM quota_reservations WHERE request_id = ?1",
            [&request_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert!(user_cap_account_id.is_none(), "the original hold must be Key-only");

    store
        .quota_pool_grant_as_admin(
            &admin,
            aiwork_core::QuotaGrant {
                user_id: "budget-user".into(),
                resource_kind: "credits".into(),
                amount: 100_000_000,
                actor_user_id: "budget-admin".into(),
                reason: "created after the Key-only hold".into(),
            },
        )
        .unwrap();
    let later_user_cap_id: String = connection
        .query_row(
            "SELECT id FROM quota_budget_accounts
             WHERE scope = 'user_cap' AND user_id = 'budget-user' AND resource_kind = 'credits'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        connection
            .execute(
                "UPDATE quota_reservations
                 SET key_budget_account_id = NULL, user_cap_account_id = ?2
                 WHERE id = ?1",
                params![&reservation_id, &later_user_cap_id],
            )
            .unwrap(),
        1,
    );
    assert_eq!(
        connection
            .execute(
                "UPDATE quota_ledger SET budget_account_id = NULL
                 WHERE event_group_id = ?1 AND event_kind = 'reserve'",
                [&event_group_id],
            )
            .unwrap(),
        1,
    );

    let reservation_before = v2_reservation_snapshot(&connection, &request_id);
    let key_before = v2_budget_account_snapshot(&connection, &key_account_id);
    let user_cap_before = v2_budget_account_snapshot(&connection, &later_user_cap_id);
    let ledger_before = {
        let mut statement = connection
            .prepare(
                "SELECT entry_id, event_kind, budget_account_id, amount, delta
                 FROM quota_ledger WHERE event_group_id = ?1 ORDER BY entry_id",
            )
            .unwrap();
        statement
            .query_map([&event_group_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    assert_eq!(key_before.1, "ready");
    assert_eq!(user_cap_before.1, "ready");

    assert_eq!(store.reconcile_quota_event_groups(1_900_000_000_000).unwrap(), 1);

    let key_after = v2_budget_account_snapshot(&connection, &key_account_id);
    assert_eq!(key_after, (key_before.0, "reconcile_required".into()));
    assert_eq!(v2_budget_account_snapshot(&connection, &later_user_cap_id), user_cap_before);
    assert_eq!(v2_reservation_snapshot(&connection, &request_id), reservation_before);
    let ledger_after = {
        let mut statement = connection
            .prepare(
                "SELECT entry_id, event_kind, budget_account_id, amount, delta
                 FROM quota_ledger WHERE event_group_id = ?1 ORDER BY entry_id",
            )
            .unwrap();
        statement
            .query_map([&event_group_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    assert_eq!(ledger_after, ledger_before, "reconciliation must preserve ledger facts");

    assert_reconcile_required_key_rejects_budget_admission(
        &store,
        &connection,
        &key_id,
        &key_account_id,
        "v2-double-null-key-next-admission",
        "v2-double-null-key-next-budget",
    );

    drop(store);
    let reopened = CoreStore::open(&directory.0).unwrap();
    reopened.migrate().unwrap();
    assert_eq!(
        v2_budget_account_snapshot(&connection, &key_account_id),
        (key_before.0, "reconcile_required".into()),
    );
    assert_eq!(reopened.reconcile_quota_event_groups(1_900_000_000_000).unwrap(), 1);
    let audit_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM audit_events
             WHERE action = 'quota.reconcile_required'
               AND target_type = 'quota_reservation' AND target_id = ?1",
            [&reservation_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(audit_count, 1, "repeated scans after reopening are audit-idempotent");

    assert_reconcile_required_key_rejects_budget_admission(
        &reopened,
        &connection,
        &key_id,
        &key_account_id,
        "v2-double-null-key-reopened-admission",
        "v2-double-null-key-reopened-budget",
    );
}

#[test]
fn v27_reconcile_indexes_migrate_and_cover_event_group_and_cursor_plans() {
    let (directory, store, key_id, _admin) = budget_fixture("v27-reconcile-indexes", 5, 100_000_000);
    let request_id = begin_video_parent(&store, &key_id, "v27-reconcile-indexes");
    store
        .begin_budget_operation(
            &request_id,
            video_budget_step(&store, &key_id, &request_id, "v27-reconcile-indexes-budget"),
        )
        .unwrap();

    let database = directory.0.join("data").join(CORE_DB_FILE);
    let connection = Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "DROP INDEX IF EXISTS quota_ledger_by_event_group_entry;
             DROP INDEX IF EXISTS quota_reservations_by_created_id;
             UPDATE schema_meta SET value = '26' WHERE key = 'schema_version';",
        )
        .unwrap();
    let ledger_before: (i64, i64, i64) = connection
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(amount), 0), COALESCE(SUM(delta), 0)
             FROM quota_ledger",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    let reservations_before: (i64, i64) = connection
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(amount), 0) FROM quota_reservations",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();

    store.migrate().unwrap();

    let schema_version: String = connection
        .query_row(
            "SELECT value FROM schema_meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(schema_version, aiwork_core::CURRENT_SCHEMA_VERSION.to_string(), "V26 databases must receive all subsequent migrations explicitly");
    let index_names = {
        let mut statement = connection.prepare("SELECT name FROM sqlite_master WHERE type='index'").unwrap();
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<std::collections::BTreeSet<_>, _>>()
            .unwrap()
    };
    assert!(index_names.contains("quota_ledger_by_event_group_entry"));
    assert!(index_names.contains("quota_reservations_by_created_id"));
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(amount), 0), COALESCE(SUM(delta), 0)
                 FROM quota_ledger",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?)),
            )
            .unwrap(),
        ledger_before,
        "V27 must not rewrite historical ledger facts",
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(amount), 0) FROM quota_reservations",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .unwrap(),
        reservations_before,
        "V27 must not rewrite historical holds",
    );

    let event_group_plan = {
        let mut statement = connection
            .prepare(
                "EXPLAIN QUERY PLAN
                 SELECT entry_id, event_kind FROM quota_ledger
                 WHERE event_group_id = ?1 ORDER BY entry_id",
            )
            .unwrap();
        statement
            .query_map(["v27-plan-probe"], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    assert!(
        event_group_plan
            .iter()
            .any(|detail| detail.contains("quota_ledger_by_event_group_entry")),
        "event-group plan did not use the general index: {event_group_plan:?}",
    );
    let cursor_plan = {
        let mut statement = connection
            .prepare(
                "EXPLAIN QUERY PLAN
                 SELECT reservation.id FROM quota_reservations AS reservation
                 INDEXED BY quota_reservations_by_created_id
                 WHERE (reservation.created_at_ms, reservation.id) > (?1, ?2)
                   AND (reservation.created_at_ms, reservation.id) <= (?3, ?4)
                 ORDER BY reservation.created_at_ms, reservation.id LIMIT ?5",
            )
            .unwrap();
        statement
            .query_map(params![0_i64, "", i64::MAX, "\u{10ffff}", 100_i64], |row| {
                row.get::<_, String>(3)
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    assert!(
        cursor_plan
            .iter()
            .any(|detail| {
                detail.contains("SEARCH reservation USING")
                    && detail.contains("quota_reservations_by_created_id")
            }),
        "stable raw cursor page must seek through its index: {cursor_plan:?}",
    );
    assert!(
        cursor_plan
            .iter()
            .all(|detail| !detail.contains("USE TEMP B-TREE FOR ORDER BY")),
        "stable cursor page must not sort the scanned history: {cursor_plan:?}",
    );
    eprintln!("V27 event-group plan: {event_group_plan:?}");
    eprintln!("V27 raw cursor plan: {cursor_plan:?}");

    let v2_context_plan = {
        let mut statement = connection
            .prepare(
                "EXPLAIN QUERY PLAN
                 SELECT EXISTS(
                    SELECT 1 FROM budget_steps
                    WHERE reservation_id = ?1 AND request_id = ?2
                 ) OR EXISTS(
                    SELECT 1 FROM budget_operations
                    WHERE parent_request_id = ?2
                 ) OR EXISTS(
                    SELECT 1 FROM request_relations relation
                    JOIN budget_operations operation
                      ON operation.parent_request_id = relation.parent_request_id
                    WHERE relation.child_request_id = ?2
                      AND relation.relationship_kind = 'seedance_assist'
                 )",
            )
            .unwrap();
        statement
            .query_map(params!["reservation-plan-probe", "request-plan-probe"], |row| {
                row.get::<_, String>(3)
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    assert!(
        v2_context_plan.iter().any(|detail| detail.contains("SEARCH budget_steps")),
        "reservation v2-context probe must seek by reservation_id: {v2_context_plan:?}",
    );
    assert!(
        v2_context_plan.iter().any(|detail| detail.contains("SEARCH budget_operations")),
        "operation v2-context probe must seek by parent_request_id: {v2_context_plan:?}",
    );
    assert!(
        v2_context_plan.iter().any(|detail| detail.contains("SEARCH relation")),
        "child-relation v2-context probe must seek by child_request_id: {v2_context_plan:?}",
    );
    assert!(
        v2_context_plan.iter().all(|detail| {
            !["SCAN budget_steps", "SCAN budget_operations", "SCAN relation", "SCAN operation"]
                .iter()
                .any(|table_scan| detail.starts_with(table_scan))
        }),
        "v2-context probes must not scan their history tables: {v2_context_plan:?}",
    );
    eprintln!("V2 context-probe plan: {v2_context_plan:?}");

    drop(store);
    let reopened = CoreStore::open(&directory.0).unwrap();
    reopened.migrate().unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT value FROM schema_meta WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        aiwork_core::CURRENT_SCHEMA_VERSION.to_string(),
        "reopening must keep the migrated schema version",
    );
}

#[test]
fn reconcile_event_group_batches_are_bounded_replay_safe_and_wrapper_compatible() {
    let (directory, store, key_id, _admin) = budget_fixture("reconcile-batch-cursor", 5, 100_000_000);
    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    let (user_id, key_account_id, key_version): (String, String, i64) = connection
        .query_row(
            "SELECT user_id, id, version FROM quota_budget_accounts
             WHERE scope = 'key' AND api_key_id = ?1 AND resource_kind = 'credits'",
            [&key_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    for index in 0..5_i64 {
        let reservation_id = format!("batch-reservation-{index:02}");
        let request_id = format!("batch-request-{index:02}");
        let event_group_id = format!("batch-group-{index:02}");
        let created_at_ms = 1_800_000_000_000 + index;
        connection
            .execute(
                "INSERT INTO quota_reservations
                 (id, user_id, request_id, resource_kind, amount, state, expires_at_ms,
                  created_at_ms, api_key_id, key_budget_account_id, user_cap_account_id, event_group_id)
                 VALUES (?1, ?2, ?3, 'credits', 100, 'held', ?4, ?5, ?6, ?7, NULL, ?8)",
                params![
                    reservation_id,
                    &user_id,
                    request_id,
                    created_at_ms + 60_000,
                    created_at_ms,
                    &key_id,
                    &key_account_id,
                    event_group_id,
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO quota_ledger
                 (entry_id, user_id, resource_kind, event_kind, amount, delta, request_id,
                  created_at_ms, budget_account_id, event_group_id, api_key_id, budget_version)
                 VALUES (?1, ?2, 'credits', 'reserve', 99, -99, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    format!("batch-entry-{index:02}"),
                    &user_id,
                    request_id,
                    created_at_ms,
                    &key_account_id,
                    event_group_id,
                    &key_id,
                    key_version,
                ],
            )
            .unwrap();
    }

    connection
        .execute_batch(
            "CREATE TRIGGER fail_first_reconcile_audit
             BEFORE INSERT ON audit_events
             WHEN NEW.action = 'quota.reconcile_required'
               AND NEW.target_id = 'batch-reservation-00'
             BEGIN
               SELECT RAISE(ABORT, 'injected reconciliation batch failure');
             END;",
        )
        .unwrap();
    assert!(store
        .reconcile_quota_event_groups_batch(1_900_000_000_000, None, 2)
        .is_err());
    let failed_page_state: String = connection
        .query_row(
            "SELECT migration_state FROM quota_budget_accounts WHERE id = ?1",
            [&key_account_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(failed_page_state, "ready", "a failed batch must roll back quarantine changes");
    let failed_page_audits: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM audit_events WHERE action = 'quota.reconcile_required'
             AND target_type = 'quota_reservation'
             AND target_id IN ('batch-reservation-00','batch-reservation-01')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(failed_page_audits, 0, "a failed page must not commit partial audit evidence");
    connection
        .execute_batch("DROP TRIGGER fail_first_reconcile_audit;")
        .unwrap();

    let first = store
        .reconcile_quota_event_groups_batch(1_900_000_000_000, None, 2)
        .unwrap();
    assert_eq!(first.processed, 2);
    assert_eq!(first.scanned, 2);
    assert_eq!(first.raw_rows_read, 3, "one raw look-ahead row establishes continuation");
    assert_eq!(first.invalid_groups, 2);
    assert!(!first.complete);
    let cursor = first.next_cursor.clone().expect("more rows must leave a cursor");

    let replay = store
        .reconcile_quota_event_groups_batch(1_900_000_000_000, None, 2)
        .unwrap();
    assert_eq!(replay.processed, first.processed);
    assert_eq!(replay.scanned, first.scanned);
    assert_eq!(replay.next_cursor, first.next_cursor, "replaying the same cursor is deterministic");
    let first_page_audits: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM audit_events WHERE action = 'quota.reconcile_required'
             AND target_type = 'quota_reservation'
             AND target_id IN ('batch-reservation-00','batch-reservation-01')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(first_page_audits, 2, "cursor replay must not duplicate audit events");

    // This row is newer than the cursor's already-fixed high-water mark. The
    // current sweep must finish the original range; a later full sweep picks
    // this candidate up instead of silently extending its cursor.
    let extra_created_at_ms = 1_800_000_000_010_i64;
    connection
        .execute(
            "INSERT INTO quota_reservations
             (id, user_id, request_id, resource_kind, amount, state, expires_at_ms,
              created_at_ms, api_key_id, key_budget_account_id, event_group_id)
             VALUES ('wrapper-reservation', ?1, 'wrapper-request', 'credits', 100, 'held',
                     ?2, ?3, ?4, ?5, 'wrapper-group')",
            params![&user_id, extra_created_at_ms + 60_000, extra_created_at_ms, &key_id, &key_account_id],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO quota_ledger
             (entry_id, user_id, resource_kind, event_kind, amount, delta, request_id,
              created_at_ms, budget_account_id, event_group_id, api_key_id, budget_version)
             VALUES ('wrapper-entry', ?1, 'credits', 'reserve', 99, -99, 'wrapper-request',
                     ?2, ?3, 'wrapper-group', ?4, ?5)",
            params![&user_id, extra_created_at_ms, &key_account_id, &key_id, key_version],
        )
        .unwrap();

    let second = store
        .reconcile_quota_event_groups_batch(1_900_000_000_000, Some(&cursor), 2)
        .unwrap();
    assert_eq!(second.processed, 2);
    assert_eq!(second.scanned, 2);
    assert_eq!(second.invalid_groups, 2);
    assert!(!second.complete);
    let final_cursor = second.next_cursor.as_ref().expect("one remaining row must continue");
    let final_batch = store
        .reconcile_quota_event_groups_batch(1_900_000_000_000, Some(final_cursor), 2)
        .unwrap();
    assert_eq!(final_batch.processed, 1);
    assert_eq!(final_batch.scanned, 1);
    assert_eq!(final_batch.invalid_groups, 1);
    assert!(final_batch.complete);
    assert!(final_batch.next_cursor.is_none());

    let total_audits: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM audit_events WHERE action = 'quota.reconcile_required'
             AND target_type = 'quota_reservation'
             AND target_id LIKE 'batch-reservation-%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(total_audits, 5, "all rows within the fixed cursor boundary must be visited");

    assert_eq!(store.reconcile_quota_event_groups(1_900_000_000_000).unwrap(), 6);
    assert_eq!(store.reconcile_quota_event_groups(1_900_000_000_000).unwrap(), 6);
    let wrapper_audits: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM audit_events WHERE action = 'quota.reconcile_required'
             AND target_type = 'quota_reservation' AND target_id = 'wrapper-reservation'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(wrapper_audits, 1, "the one-shot wrapper must stay audit-idempotent");
}

#[test]
fn reconcile_batches_bound_raw_work_when_candidates_are_sparse() {
    const NON_CANDIDATE_ROWS: i64 = 20_000;
    const LIMIT: usize = 64;
    let (directory, store, key_id, _admin) = budget_fixture("reconcile-sparse-raw", 5, 100_000_000);
    let database = directory.0.join("data").join(CORE_DB_FILE);
    let mut connection = Connection::open(&database).unwrap();
    let (user_id, key_account_id): (String, String) = connection
        .query_row(
            "SELECT user_id, id FROM quota_budget_accounts
             WHERE scope = 'key' AND api_key_id = ?1 AND resource_kind = 'credits'",
            [&key_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let transaction = connection.transaction().unwrap();
    {
        let mut insert = transaction
            .prepare(
                "INSERT INTO quota_reservations
                 (id, user_id, request_id, resource_kind, amount, state, expires_at_ms,
                  created_at_ms, api_key_id, key_budget_account_id, user_cap_account_id, event_group_id)
                 VALUES (?1, ?2, ?3, 'credits', 1, 'held', ?4, ?5, NULL, NULL, NULL, NULL)",
            )
            .unwrap();
        for index in 0..NON_CANDIDATE_ROWS {
            let created_at_ms = 1_800_000_000_000 + index;
            insert
                .execute(params![
                    format!("ordinary-reservation-{index:05}"),
                    &user_id,
                    format!("ordinary-request-{index:05}"),
                    created_at_ms + 60_000,
                    created_at_ms,
                ])
                .unwrap();
        }
    }
    transaction
        .execute(
            "INSERT INTO quota_reservations
             (id, user_id, request_id, resource_kind, amount, state, expires_at_ms,
              created_at_ms, api_key_id, key_budget_account_id, user_cap_account_id, event_group_id)
             VALUES ('sparse-candidate', ?1, 'sparse-candidate-request', 'credits', 100,
                     'held', ?2, ?3, ?4, ?5, NULL, 'sparse-candidate-group')",
            params![
                &user_id,
                1_800_000_000_000 + NON_CANDIDATE_ROWS + 60_000,
                1_800_000_000_000 + NON_CANDIDATE_ROWS,
                &key_id,
                &key_account_id,
            ],
        )
        .unwrap();
    transaction.commit().unwrap();

    let sweep_started = Instant::now();
    let first = store
        .reconcile_quota_event_groups_batch(1_900_000_000_000, None, LIMIT)
        .unwrap();
    assert_eq!(first.scanned, LIMIT, "the cursor must advance over raw non-candidates");
    assert_eq!(first.raw_rows_read, LIMIT + 1, "the look-ahead is bounded to one raw row");
    assert_eq!(first.processed, 0, "the first raw page contains no candidates");
    assert!(!first.complete, "a sparse candidate later in the raw range must not be skipped");
    let mut cursor = first.next_cursor.clone().expect("raw rows remain after page one");
    let (last_created_at_ms, last_reservation_id) = cursor.last_scanned();
    let expected_last_id: String = connection
        .query_row(
            "SELECT id FROM quota_reservations
             ORDER BY created_at_ms, id LIMIT 1 OFFSET ?1",
            [LIMIT as i64 - 1],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(last_reservation_id, expected_last_id);
    assert_eq!(last_created_at_ms, 1_800_000_000_000 + LIMIT as i64 - 1);

    connection
        .execute_batch("CREATE TABLE between_batch_write_probe (value INTEGER NOT NULL);")
        .unwrap();
    connection
        .execute("INSERT INTO between_batch_write_probe (value) VALUES (1)", [])
        .unwrap();

    let mut batch_count = 1_u64;
    let mut total_scanned = first.scanned as u64;
    let mut total_raw_rows_read = first.raw_rows_read as u64;
    let mut total_processed = first.processed as u64;
    let mut total_invalid = first.invalid_groups;
    let mut max_write_transaction_micros = first.write_transaction_elapsed_micros;
    while !first.complete && total_scanned < (NON_CANDIDATE_ROWS + 1) as u64 {
        let batch = store
            .reconcile_quota_event_groups_batch(
                1_900_000_000_000,
                Some(&cursor),
                LIMIT,
            )
            .unwrap();
        assert!(batch.scanned <= LIMIT);
        assert!(batch.raw_rows_read <= LIMIT + 1);
        assert!(batch.processed <= batch.scanned);
        total_scanned += batch.scanned as u64;
        total_raw_rows_read += batch.raw_rows_read as u64;
        total_processed += batch.processed as u64;
        total_invalid += batch.invalid_groups;
        max_write_transaction_micros =
            max_write_transaction_micros.max(batch.write_transaction_elapsed_micros);
        batch_count += 1;
        if batch.complete {
            assert!(batch.next_cursor.is_none());
            break;
        }
        cursor = batch.next_cursor.expect("incomplete raw page must advance cursor");
    }
    let sweep_elapsed_micros = sweep_started.elapsed().as_micros();
    assert_eq!(total_scanned, (NON_CANDIDATE_ROWS + 1) as u64);
    assert_eq!(total_processed, 1);
    assert_eq!(total_invalid, 1);
    assert_eq!(
        total_raw_rows_read,
        total_scanned + batch_count - 1,
        "each non-final page reads only one additional look-ahead row",
    );
    let audit_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM audit_events WHERE action = 'quota.reconcile_required'
             AND target_type = 'quota_reservation' AND target_id = 'sparse-candidate'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(audit_count, 1, "the sparse final candidate must eventually be visited");
    eprintln!(
        "sparse RAW sweep measured: rows={}, batches={}, max_single_IMMEDIATE_transaction_us={}, complete_sweep_wall_us={sweep_elapsed_micros}",
        total_scanned, batch_count, max_write_transaction_micros
    );
}

#[derive(Clone, Copy)]
enum ForeignV2ReservationAccountPointer {
    Key,
    UserCap,
}

fn held_v2_reservation_with_foreign_account_pointer(
    label: &str,
    wrong_pointer: ForeignV2ReservationAccountPointer,
) -> (TestDirectory, CoreStore, String, String, String, String, String) {
    let (directory, store, request_id, budget_id, key_account_id, user_cap_account_id, admin) =
        held_v2_reservation_with_missing_pointer(label, MissingV2ReservationAccountPointer::Both);
    let foreign_account_id = match wrong_pointer {
        ForeignV2ReservationAccountPointer::Key => {
            let foreign_key = store
                .issue_api_key_as_admin_with_max_concurrency(
                    "budget-user",
                    &format!("{label}-foreign-key"),
                    BTreeSet::from(["video:submit".into()]),
                    5,
                    &admin,
                )
                .unwrap();
            store
                .key_quota_grant_as_admin(
                    &admin,
                    KeyQuotaGrant {
                        api_key_id: foreign_key.id.clone(),
                        resource_kind: "credits".into(),
                        amount: 100_000_000,
                        actor_user_id: "budget-admin".into(),
                        reason: "foreign V2 Key account pointer fixture".into(),
                    },
                )
                .unwrap();
            let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
            connection
                .query_row(
                    "SELECT id FROM quota_budget_accounts
                     WHERE scope = 'key' AND user_id = 'budget-user' AND api_key_id = ?1
                       AND resource_kind = 'credits'",
                    [&foreign_key.id],
                    |row| row.get(0),
                )
                .unwrap()
        }
        ForeignV2ReservationAccountPointer::UserCap => {
            let foreign_user_id = format!("foreign-{label}");
            store
                .create_user(
                    NewUser {
                        id: foreign_user_id.clone(),
                        name: "Foreign User".into(),
                        role: UserRole::User,
                    },
                    "budget-admin",
                )
                .unwrap();
            store
                .quota_pool_grant_as_admin(
                    &admin,
                    aiwork_core::QuotaGrant {
                        user_id: foreign_user_id.clone(),
                        resource_kind: "credits".into(),
                        amount: 100_000_000,
                        actor_user_id: "budget-admin".into(),
                        reason: "foreign V2 user-cap account pointer fixture".into(),
                    },
                )
                .unwrap();
            let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
            connection
                .query_row(
                    "SELECT id FROM quota_budget_accounts
                     WHERE scope = 'user_cap' AND user_id = ?1 AND api_key_id IS NULL
                       AND resource_kind = 'credits'",
                    [&foreign_user_id],
                    |row| row.get(0),
                )
                .unwrap()
        }
    };
    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    let (key_pointer, user_cap_pointer) = match wrong_pointer {
        ForeignV2ReservationAccountPointer::Key => (&foreign_account_id, &user_cap_account_id),
        ForeignV2ReservationAccountPointer::UserCap => (&key_account_id, &foreign_account_id),
    };
    assert_eq!(
        connection
            .execute(
                "UPDATE quota_reservations
                 SET key_budget_account_id = ?2, user_cap_account_id = ?3
                 WHERE request_id = ?1",
                params![&request_id, key_pointer, user_cap_pointer],
            )
            .unwrap(),
        1
    );
    (
        directory,
        store,
        request_id,
        budget_id,
        key_account_id,
        user_cap_account_id,
        foreign_account_id,
    )
}

#[test]
fn dispatch_v2_rejects_ready_key_account_owned_by_another_key() {
    let (directory, store, request_id, budget_id, key_account_id, user_cap_account_id, foreign_account_id) =
        held_v2_reservation_with_foreign_account_pointer(
            "v2-dispatch-foreign-key-account",
            ForeignV2ReservationAccountPointer::Key,
        );
    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    let before = v2_reservation_snapshot(&connection, &request_id);
    let key_before = v2_budget_account_snapshot(&connection, &key_account_id);
    let user_cap_before = v2_budget_account_snapshot(&connection, &user_cap_account_id);
    let foreign_before = v2_budget_account_snapshot(&connection, &foreign_account_id);

    let result = store.mark_budget_step_dispatched(&request_id, &budget_id);

    assert!(result.is_err(), "ready account owned by another Key was accepted: {result:?}");
    assert_eq!(v2_reservation_snapshot(&connection, &request_id), before);
    assert_eq!(v2_budget_account_snapshot(&connection, &key_account_id), key_before);
    assert_eq!(v2_budget_account_snapshot(&connection, &user_cap_account_id), user_cap_before);
    assert_eq!(v2_budget_account_snapshot(&connection, &foreign_account_id), foreign_before);
}

#[test]
fn dispatch_v2_rejects_ready_user_cap_account_owned_by_another_user() {
    let (directory, store, request_id, budget_id, key_account_id, user_cap_account_id, foreign_account_id) =
        held_v2_reservation_with_foreign_account_pointer(
            "v2-dispatch-foreign-user-cap-account",
            ForeignV2ReservationAccountPointer::UserCap,
        );
    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    let before = v2_reservation_snapshot(&connection, &request_id);
    let key_before = v2_budget_account_snapshot(&connection, &key_account_id);
    let user_cap_before = v2_budget_account_snapshot(&connection, &user_cap_account_id);
    let foreign_before = v2_budget_account_snapshot(&connection, &foreign_account_id);

    let result = store.mark_budget_step_dispatched(&request_id, &budget_id);

    assert!(result.is_err(), "ready user-cap account owned by another user was accepted: {result:?}");
    assert_eq!(v2_reservation_snapshot(&connection, &request_id), before);
    assert_eq!(v2_budget_account_snapshot(&connection, &key_account_id), key_before);
    assert_eq!(v2_budget_account_snapshot(&connection, &user_cap_account_id), user_cap_before);
    assert_eq!(v2_budget_account_snapshot(&connection, &foreign_account_id), foreign_before);
}

#[test]
fn reconcile_v2_foreign_account_pointers_quarantines_only_accounts_from_hold_ledger() {
    let cases = [
        (ForeignV2ReservationAccountPointer::Key, "foreign-key"),
        (ForeignV2ReservationAccountPointer::UserCap, "foreign-user-cap"),
    ];
    let mut failures = Vec::new();

    for (wrong_pointer, label) in cases {
        let (directory, store, request_id, _budget_id, key_account_id, user_cap_account_id, foreign_account_id) =
            held_v2_reservation_with_foreign_account_pointer(label, wrong_pointer);
        let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
        let before = v2_reservation_snapshot(&connection, &request_id);
        let key_before = v2_budget_account_snapshot(&connection, &key_account_id);
        let user_cap_before = v2_budget_account_snapshot(&connection, &user_cap_account_id);
        let foreign_before = v2_budget_account_snapshot(&connection, &foreign_account_id);

        let invalid_groups = store
            .reconcile_quota_event_groups(chrono::Utc::now().timestamp_millis())
            .unwrap();
        if invalid_groups != 1 {
            failures.push(format!("{label}: expected one invalid reservation, got {invalid_groups}"));
        }
        if v2_reservation_snapshot(&connection, &request_id) != before {
            failures.push(format!("{label}: reconciliation changed reservation or ledger facts"));
        }
        for (account_id, before) in [(&key_account_id, key_before), (&user_cap_account_id, user_cap_before)] {
            let after = v2_budget_account_snapshot(&connection, account_id);
            if after.0 != before.0 || after.1 != "reconcile_required" {
                failures.push(format!("{label}: original hold account was not quarantined safely: {after:?}"));
            }
        }
        let foreign_after = v2_budget_account_snapshot(&connection, &foreign_account_id);
        if foreign_after != foreign_before || foreign_after.1 != "ready" {
            failures.push(format!("{label}: foreign pointer target was modified: {foreign_after:?}"));
        }
    }

    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn reconcile_v2_unresolved_owner_is_audited_once_and_dispatch_fails_closed() {
    let label = "v2-unresolved-owner-audit";
    let (directory, store, request_id, budget_id, key_account_id, user_cap_account_id, admin) =
        held_v2_reservation_with_missing_pointer(label, MissingV2ReservationAccountPointer::Both);
    let foreign_key = store
        .issue_api_key_as_admin_with_max_concurrency(
            "budget-user",
            "v2-unresolved-owner-audit-foreign-key",
            BTreeSet::from(["video:submit".into()]),
            5,
            &admin,
        )
        .unwrap();
    store
        .key_quota_grant_as_admin(
            &admin,
            KeyQuotaGrant {
                api_key_id: foreign_key.id.clone(),
                resource_kind: "credits".into(),
                amount: 25_000_000,
                actor_user_id: "budget-admin".into(),
                reason: "foreign operation pointer isolation test".into(),
            },
        )
        .unwrap();
    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    let foreign_key_account_id: String = connection
        .query_row(
            "SELECT id FROM quota_budget_accounts
             WHERE scope = 'key' AND api_key_id = ?1 AND resource_kind = 'credits'",
            [&foreign_key.id],
            |row| row.get(0),
        )
        .unwrap();
    let reservation_id: String = connection
        .query_row(
            "SELECT id FROM quota_reservations WHERE request_id = ?1",
            [&request_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        connection
            .execute(
                "UPDATE quota_reservations
                 SET key_budget_account_id = ?2, user_cap_account_id = ?3
                 WHERE request_id = ?1",
                params![&request_id, &key_account_id, &user_cap_account_id],
            )
            .unwrap(),
        1,
    );
    assert_eq!(
        connection
            .execute(
                "UPDATE budget_operations SET api_key_id = ?2 WHERE parent_request_id = ?1",
                params![&request_id, &foreign_key.id],
            )
            .unwrap(),
        1,
    );
    let before = v2_reservation_snapshot(&connection, &request_id);
    let key_before = v2_budget_account_snapshot(&connection, &key_account_id);
    let user_cap_before = v2_budget_account_snapshot(&connection, &user_cap_account_id);
    let foreign_key_before = v2_budget_account_snapshot(&connection, &foreign_key_account_id);
    assert_eq!(key_before.1, "ready");
    assert_eq!(user_cap_before.1, "ready");
    assert_eq!(foreign_key_before.1, "ready");

    assert_eq!(store.reconcile_quota_event_groups(chrono::Utc::now().timestamp_millis()).unwrap(), 1);
    let audit_count_and_metadata: (i64, Option<String>) = connection
        .query_row(
            "SELECT COUNT(*), MAX(metadata_json) FROM audit_events
             WHERE action = 'quota.reconcile_required'
               AND target_type = 'quota_reservation' AND target_id = ?1",
            [&reservation_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(audit_count_and_metadata.0, 1);
    let metadata: serde_json::Value =
        serde_json::from_str(audit_count_and_metadata.1.as_deref().unwrap()).unwrap();
    assert_eq!(metadata["hold_preserved"], true);
    assert!(metadata["reason"].as_str().is_some_and(|reason| !reason.is_empty()));
    assert_eq!(v2_reservation_snapshot(&connection, &request_id), before);
    assert_eq!(
        v2_budget_account_snapshot(&connection, &key_account_id),
        (key_before.0, "reconcile_required".into()),
        "the Key proven by the request record must be durably blocked despite a foreign operation pointer",
    );
    assert_eq!(
        v2_budget_account_snapshot(&connection, &user_cap_account_id),
        (user_cap_before.0, "reconcile_required".into()),
        "only the original hold-time user-cap account may be quarantined",
    );
    assert_eq!(
        v2_budget_account_snapshot(&connection, &foreign_key_account_id),
        foreign_key_before,
        "the foreign operation pointer must not quarantine its Key account",
    );
    let original_key_id: String = connection
        .query_row(
            "SELECT api_key_id FROM requests WHERE id = ?1",
            [&request_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_reconcile_required_key_rejects_budget_admission(
        &store,
        &connection,
        &original_key_id,
        &key_account_id,
        "v2-unresolved-owner-audit-new-admission",
        "v2-unresolved-owner-audit-new-budget",
    );
    assert!(store.mark_budget_step_dispatched(&request_id, &budget_id).is_err());

    assert_eq!(store.reconcile_quota_event_groups(chrono::Utc::now().timestamp_millis()).unwrap(), 1);
    let audit_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM audit_events
             WHERE action = 'quota.reconcile_required'
               AND target_type = 'quota_reservation' AND target_id = ?1",
            [&reservation_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(audit_count, 1, "repeated scans keep the observability event idempotent");
    assert_eq!(v2_reservation_snapshot(&connection, &request_id), before);
}

#[test]
fn reconcile_v2_missing_reservation_account_pointers_quarantines_both_owner_accounts() {
    let cases = [
        (MissingV2ReservationAccountPointer::Key, "key-pointer"),
        (MissingV2ReservationAccountPointer::UserCap, "user-cap-pointer"),
        (MissingV2ReservationAccountPointer::Both, "both-pointers"),
    ];
    let mut failures = Vec::new();

    for (missing, label) in cases {
        let (directory, store, request_id, _budget_id, key_account_id, user_cap_account_id, _admin) =
            held_v2_reservation_with_missing_pointer(label, missing);
        let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
        let before = v2_reservation_snapshot(&connection, &request_id);
        let key_before = v2_budget_account_snapshot(&connection, &key_account_id);
        let user_cap_before = v2_budget_account_snapshot(&connection, &user_cap_account_id);
        assert_eq!(key_before.1, "ready");
        assert_eq!(user_cap_before.1, "ready");

        let invalid_groups = store
            .reconcile_quota_event_groups(chrono::Utc::now().timestamp_millis())
            .unwrap();
        if invalid_groups != 1 {
            failures.push(format!("{label}: expected one invalid V2 group, got {invalid_groups}"));
        }
        if v2_reservation_snapshot(&connection, &request_id) != before {
            failures.push(format!("{label}: reconciliation changed reservation, execution, or ledger facts"));
        }
        let key_after = v2_budget_account_snapshot(&connection, &key_account_id);
        let user_cap_after = v2_budget_account_snapshot(&connection, &user_cap_account_id);
        if key_after.0 != key_before.0 || key_after.1 != "reconcile_required" {
            failures.push(format!("{label}: Key account was not quarantined without changing its version: {key_after:?}"));
        }
        if user_cap_after.0 != user_cap_before.0 || user_cap_after.1 != "reconcile_required" {
            failures.push(format!("{label}: user-cap account was not quarantined without changing its version: {user_cap_after:?}"));
        }
    }

    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn dispatch_v2_with_missing_reservation_account_pointers_fails_closed_without_mutation_after_reopen() {
    let cases = [
        (MissingV2ReservationAccountPointer::Key, "key-pointer"),
        (MissingV2ReservationAccountPointer::UserCap, "user-cap-pointer"),
        (MissingV2ReservationAccountPointer::Both, "both-pointers"),
    ];
    let mut failures = Vec::new();

    for (missing, label) in cases {
        let (directory, store, request_id, budget_id, key_account_id, user_cap_account_id, _admin) =
            held_v2_reservation_with_missing_pointer(label, missing);
        let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
        let before = v2_reservation_snapshot(&connection, &request_id);
        let key_before = v2_budget_account_snapshot(&connection, &key_account_id);
        let user_cap_before = v2_budget_account_snapshot(&connection, &user_cap_account_id);

        let first_dispatch = store.mark_budget_step_dispatched(&request_id, &budget_id);
        if first_dispatch.is_ok() {
            failures.push(format!("{label}: dispatch was accepted before reconciliation: {first_dispatch:?}"));
        }
        if v2_reservation_snapshot(&connection, &request_id) != before {
            failures.push(format!("{label}: rejected dispatch changed request, step, reservation, or ledger facts"));
        }
        if v2_budget_account_snapshot(&connection, &key_account_id) != key_before
            || v2_budget_account_snapshot(&connection, &user_cap_account_id) != user_cap_before
        {
            failures.push(format!("{label}: rejected dispatch changed a budget account"));
        }

        drop(store);
        let reopened = CoreStore::open(&directory.0).unwrap();
        let replay_dispatch = reopened.mark_budget_step_dispatched(&request_id, &budget_id);
        if replay_dispatch.is_ok() {
            failures.push(format!("{label}: dispatch after reopening was accepted: {replay_dispatch:?}"));
        }
        if v2_reservation_snapshot(&connection, &request_id) != before {
            failures.push(format!("{label}: reopened dispatch changed request, step, reservation, or ledger facts"));
        }
        if v2_budget_account_snapshot(&connection, &key_account_id) != key_before
            || v2_budget_account_snapshot(&connection, &user_cap_account_id) != user_cap_before
        {
            failures.push(format!("{label}: reopened dispatch changed a budget account"));
        }
    }

    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn reconcile_v2_wrong_user_cap_ledger_owner_quarantines_accounts_and_blocks_admission() {
    let (directory, store, key_id, admin) = budget_fixture("v2-event-group-owner-mismatch", 5, 100_000_000);
    store
        .quota_pool_grant_as_admin(
            &admin,
            aiwork_core::QuotaGrant {
                user_id: "budget-user".into(),
                resource_kind: "credits".into(),
                amount: 100_000_000,
                actor_user_id: "budget-admin".into(),
                reason: "v2 event owner validation fixture".into(),
            },
        )
        .unwrap();
    let parent = begin_video_parent(&store, &key_id, "v2-event-group-owner-mismatch");
    let mut step = video_budget_step(&store, &key_id, &parent, "v2-event-group-owner-mismatch-budget");
    step.authorization.hold_credits = CreditAmount::parse("2", "credits").unwrap();
    store.begin_budget_operation(&parent, step).unwrap();

    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    let (event_group_id, key_account_id, user_cap_account_id): (String, String, Option<String>) = connection
        .query_row(
            "SELECT event_group_id, key_budget_account_id, user_cap_account_id
             FROM quota_reservations WHERE request_id = ?1",
            [&parent],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    let user_cap_account_id = user_cap_account_id.expect("v2 reservation should have a user-cap account");
    let changed = connection
        .execute(
            "UPDATE quota_ledger SET user_id = 'budget-admin'
             WHERE event_group_id = ?1 AND budget_account_id = ?2 AND event_kind = 'reserve'",
            params![event_group_id, &user_cap_account_id],
        )
        .unwrap();
    assert_eq!(changed, 1);

    assert_eq!(store.reconcile_quota_event_groups(chrono::Utc::now().timestamp_millis()).unwrap(), 1);
    for account_id in [&key_account_id, &user_cap_account_id] {
        let state: String = connection
            .query_row(
                "SELECT migration_state FROM quota_budget_accounts WHERE id = ?1",
                [account_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "reconcile_required");
    }

    let retry_parent = begin_video_parent(&store, &key_id, "v2-event-group-owner-mismatch-retry");
    let mut retry_step = video_budget_step(&store, &key_id, &retry_parent, "v2-event-group-owner-mismatch-retry-budget");
    retry_step.authorization.hold_credits = CreditAmount::parse("1", "credits").unwrap();
    assert!(store.begin_budget_operation(&retry_parent, retry_step).is_err());
    let retry_has_reservation: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM quota_reservations WHERE request_id = ?1)",
            [&retry_parent],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!retry_has_reservation);
}

#[test]
fn reconcile_v2_corrupt_receipt_hash_quarantines_accounts_and_blocks_admission() {
    let (directory, store, key_id, admin) = budget_fixture("v2-event-group-receipt-hash", 5, 100_000_000);
    store
        .quota_pool_grant_as_admin(
            &admin,
            aiwork_core::QuotaGrant {
                user_id: "budget-user".into(),
                resource_kind: "credits".into(),
                amount: 100_000_000,
                actor_user_id: "budget-admin".into(),
                reason: "v2 receipt hash validation fixture".into(),
            },
        )
        .unwrap();
    let parent = begin_video_parent(&store, &key_id, "v2-event-group-receipt-hash");
    let mut step = video_budget_step(&store, &key_id, &parent, "v2-event-group-receipt-hash-budget");
    step.authorization.hold_credits = CreditAmount::parse("2", "credits").unwrap();
    store.begin_budget_operation(&parent, step).unwrap();
    dispatch_current_budget_step(&store, &parent).unwrap();
    store.bind_budget_video_task(&parent, "v2-event-group-receipt-hash-task").unwrap();
    store
        .mark_budget_step_execution(&parent, aiwork_core::BudgetExecutionState::Succeeded)
        .unwrap();
    let mut receipt = final_budget_receipt(&parent, "v2-event-group-receipt-hash-budget", "1");
    receipt.receipt.task_ref = Some("v2-event-group-receipt-hash-task".into());
    assert!(matches!(
        store.apply_budget_receipt(receipt).unwrap(),
        aiwork_core::BudgetReceiptResult::Settled { .. }
    ));
    store
        .finish_budget_execution(&parent, aiwork_core::BudgetExecutionState::Succeeded)
        .unwrap();

    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    let changed = connection
        .execute(
            "UPDATE budget_receipt_evidence SET evidence_hash = 'corrupt-receipt-hash'
             WHERE request_id = ?1 AND record_kind = 'receipt'",
            [&parent],
        )
        .unwrap();
    assert_eq!(changed, 1);
    assert_eq!(store.reconcile_quota_event_groups(chrono::Utc::now().timestamp_millis()).unwrap(), 1);
    let (key_state, user_cap_state): (String, String) = connection
        .query_row(
            "SELECT key_account.migration_state, user_account.migration_state
             FROM quota_budget_accounts key_account
             JOIN quota_reservations reservation ON reservation.key_budget_account_id = key_account.id
             JOIN quota_budget_accounts user_account ON user_account.id = reservation.user_cap_account_id
             WHERE reservation.request_id = ?1",
            [&parent],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((key_state.as_str(), user_cap_state.as_str()), ("reconcile_required", "reconcile_required"));

    let retry_parent = begin_video_parent(&store, &key_id, "v2-event-group-receipt-hash-retry");
    let mut retry_step = video_budget_step(&store, &key_id, &retry_parent, "v2-event-group-receipt-hash-retry-budget");
    retry_step.authorization.hold_credits = CreditAmount::parse("1", "credits").unwrap();
    assert!(store.begin_budget_operation(&retry_parent, retry_step).is_err());
    let retry_has_reservation: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM quota_reservations WHERE request_id = ?1)",
            [&retry_parent],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!retry_has_reservation);
}

#[test]
fn reconcile_v2_corrupt_authorization_hash_quarantines_accounts_and_blocks_admission() {
    let (directory, store, key_id, admin) = budget_fixture("v2-event-group-authorization-hash", 5, 100_000_000);
    store
        .quota_pool_grant_as_admin(
            &admin,
            aiwork_core::QuotaGrant {
                user_id: "budget-user".into(),
                resource_kind: "credits".into(),
                amount: 100_000_000,
                actor_user_id: "budget-admin".into(),
                reason: "v2 authorization hash validation fixture".into(),
            },
        )
        .unwrap();
    let parent = begin_video_parent(&store, &key_id, "v2-event-group-authorization-hash");
    let mut step = video_budget_step(&store, &key_id, &parent, "v2-event-group-authorization-hash-budget");
    step.authorization.hold_credits = CreditAmount::parse("2", "credits").unwrap();
    store.begin_budget_operation(&parent, step).unwrap();

    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    let changed = connection
        .execute(
            "UPDATE budget_steps SET authorization_hash = X'00' WHERE request_id = ?1",
            [&parent],
        )
        .unwrap();
    assert_eq!(changed, 1);
    assert_eq!(store.reconcile_quota_event_groups(chrono::Utc::now().timestamp_millis()).unwrap(), 1);
    let (key_state, user_cap_state): (String, String) = connection
        .query_row(
            "SELECT key_account.migration_state, user_account.migration_state
             FROM quota_budget_accounts key_account
             JOIN quota_reservations reservation ON reservation.key_budget_account_id = key_account.id
             JOIN quota_budget_accounts user_account ON user_account.id = reservation.user_cap_account_id
             WHERE reservation.request_id = ?1",
            [&parent],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((key_state.as_str(), user_cap_state.as_str()), ("reconcile_required", "reconcile_required"));

    let retry_parent = begin_video_parent(&store, &key_id, "v2-event-group-authorization-hash-retry");
    let mut retry_step = video_budget_step(&store, &key_id, &retry_parent, "v2-event-group-authorization-hash-retry-budget");
    retry_step.authorization.hold_credits = CreditAmount::parse("1", "credits").unwrap();
    assert!(store.begin_budget_operation(&retry_parent, retry_step).is_err());
    let retry_has_reservation: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM quota_reservations WHERE request_id = ?1)",
            [&retry_parent],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!retry_has_reservation);
}

#[test]
fn explicit_conflict_evidence_does_not_quarantine_shared_user_cap_but_bad_shape_does() {
    let (directory, store, key_id, admin) = budget_fixture("v2-explicit-conflict-evidence", 5, 100_000_000);
    store
        .quota_pool_grant_as_admin(
            &admin,
            aiwork_core::QuotaGrant {
                user_id: "budget-user".into(),
                resource_kind: "credits".into(),
                amount: 20_000_000,
                actor_user_id: "budget-admin".into(),
                reason: "conflict evidence shared user-cap fixture".into(),
            },
        )
        .unwrap();
    let first_parent = begin_video_parent(&store, &key_id, "v2-explicit-conflict-first");
    let mut first_step = video_budget_step(&store, &key_id, &first_parent, "v2-explicit-conflict-first-budget");
    first_step.authorization.hold_credits = CreditAmount::parse("2", "credits").unwrap();
    store.begin_budget_operation(&first_parent, first_step).unwrap();
    dispatch_current_budget_step(&store, &first_parent).unwrap();
    store
        .mark_budget_receipt_conflict(aiwork_core::BudgetReceiptConflict {
            request_id: first_parent.clone(),
            budget_id: "v2-explicit-conflict-first-budget".into(),
            account_ref: "account-test".into(),
            bridge_instance_id: "bridge-test".into(),
            source_ref: "explicit-conflict-source".into(),
            evidence_hash: "explicit-conflict-hash".into(),
            observed_at_ms: chrono::Utc::now().timestamp_millis(),
        })
        .unwrap();

    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    assert_eq!(store.reconcile_quota_event_groups(chrono::Utc::now().timestamp_millis()).unwrap(), 0);
    let user_cap_state: String = connection
        .query_row(
            "SELECT account.migration_state
             FROM quota_budget_accounts account
             JOIN quota_reservations reservation ON reservation.user_cap_account_id = account.id
             WHERE reservation.request_id = ?1",
            [&first_parent],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(user_cap_state, "ready");

    let blocked_retry = begin_video_parent(&store, &key_id, "v2-explicit-conflict-blocked-retry");
    let blocked_step = video_budget_step(&store, &key_id, &blocked_retry, "v2-explicit-conflict-blocked-retry-budget");
    assert!(store.begin_budget_operation(&blocked_retry, blocked_step).is_err());

    let other_key = store
        .issue_api_key_as_admin_with_max_concurrency(
            "budget-user",
            "conflict-isolated-other-key",
            BTreeSet::from(["video:submit".into()]),
            3,
            &admin,
        )
        .unwrap();
    store
        .key_quota_grant_as_admin(
            &admin,
            KeyQuotaGrant {
                api_key_id: other_key.id.clone(),
                resource_kind: "credits".into(),
                amount: 5_000_000,
                actor_user_id: "budget-admin".into(),
                reason: "other Key remains eligible under shared user-cap".into(),
            },
        )
        .unwrap();
    let other_parent = begin_video_parent(&store, &other_key.id, "v2-explicit-conflict-other-key");
    let mut other_step = video_budget_step(&store, &other_key.id, &other_parent, "v2-explicit-conflict-other-key-budget");
    other_step.authorization.hold_credits = CreditAmount::parse("1", "credits").unwrap();
    store.begin_budget_operation(&other_parent, other_step).unwrap();

    let changed = connection
        .execute(
            "UPDATE budget_receipt_evidence SET receipt_status = 'final'
             WHERE request_id = ?1 AND record_kind = 'conflict'",
            [&first_parent],
        )
        .unwrap();
    assert_eq!(changed, 1);
    assert_eq!(store.reconcile_quota_event_groups(chrono::Utc::now().timestamp_millis()).unwrap(), 1);
    let quarantined_user_cap: String = connection
        .query_row(
            "SELECT account.migration_state
             FROM quota_budget_accounts account
             JOIN quota_reservations reservation ON reservation.user_cap_account_id = account.id
             WHERE reservation.request_id = ?1",
            [&first_parent],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(quarantined_user_cap, "reconcile_required");
}

#[test]
fn reconcile_v2_explicit_conflict_wrong_owner_quarantines_accounts() {
    let (directory, store, key_id, admin) = budget_fixture("v2-explicit-conflict-owner", 3, 20_000_000);
    store
        .quota_pool_grant_as_admin(
            &admin,
            aiwork_core::QuotaGrant {
                user_id: "budget-user".into(),
                resource_kind: "credits".into(),
                amount: 20_000_000,
                actor_user_id: "budget-admin".into(),
                reason: "explicit conflict owner validation fixture".into(),
            },
        )
        .unwrap();
    let parent = begin_video_parent(&store, &key_id, "v2-explicit-conflict-owner-parent");
    store
        .begin_budget_operation(
            &parent,
            video_budget_step(&store, &key_id, &parent, "v2-explicit-conflict-owner-budget"),
        )
        .unwrap();
    dispatch_current_budget_step(&store, &parent).unwrap();
    store
        .mark_budget_receipt_conflict(aiwork_core::BudgetReceiptConflict {
            request_id: parent.clone(),
            budget_id: "v2-explicit-conflict-owner-budget".into(),
            account_ref: "account-test".into(),
            bridge_instance_id: "bridge-test".into(),
            source_ref: "explicit-conflict-owner-source".into(),
            evidence_hash: "explicit-conflict-owner-hash".into(),
            observed_at_ms: chrono::Utc::now().timestamp_millis(),
        })
        .unwrap();

    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    let changed = connection
        .execute(
            "UPDATE budget_receipt_evidence SET account_ref = 'wrong-account'
             WHERE request_id = ?1 AND record_kind = 'conflict'",
            [&parent],
        )
        .unwrap();
    assert_eq!(changed, 1);
    assert_eq!(store.reconcile_quota_event_groups(chrono::Utc::now().timestamp_millis()).unwrap(), 1);
    let (key_state, user_cap_state): (String, String) = connection
        .query_row(
            "SELECT key_account.migration_state, user_account.migration_state
             FROM quota_budget_accounts key_account
             JOIN quota_reservations reservation ON reservation.key_budget_account_id = key_account.id
             JOIN quota_budget_accounts user_account ON user_account.id = reservation.user_cap_account_id
             WHERE reservation.request_id = ?1",
            [&parent],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((key_state.as_str(), user_cap_state.as_str()), ("reconcile_required", "reconcile_required"));
}

#[test]
fn reconcile_v2_explicit_conflict_tampered_hash_quarantines_accounts() {
    let (directory, store, key_id, admin) = budget_fixture("v2-explicit-conflict-hash", 3, 20_000_000);
    store
        .quota_pool_grant_as_admin(
            &admin,
            aiwork_core::QuotaGrant {
                user_id: "budget-user".into(),
                resource_kind: "credits".into(),
                amount: 20_000_000,
                actor_user_id: "budget-admin".into(),
                reason: "explicit conflict hash validation fixture".into(),
            },
        )
        .unwrap();
    let parent = begin_video_parent(&store, &key_id, "v2-explicit-conflict-hash-parent");
    store
        .begin_budget_operation(
            &parent,
            video_budget_step(&store, &key_id, &parent, "v2-explicit-conflict-hash-budget"),
        )
        .unwrap();
    dispatch_current_budget_step(&store, &parent).unwrap();
    store
        .mark_budget_receipt_conflict(aiwork_core::BudgetReceiptConflict {
            request_id: parent.clone(),
            budget_id: "v2-explicit-conflict-hash-budget".into(),
            account_ref: "account-test".into(),
            bridge_instance_id: "bridge-test".into(),
            source_ref: "explicit-conflict-hash-source".into(),
            evidence_hash: "opaque-explicit-conflict-hash".into(),
            observed_at_ms: chrono::Utc::now().timestamp_millis(),
        })
        .unwrap();

    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    let changed = connection
        .execute(
            "UPDATE budget_receipt_evidence SET evidence_hash = 'tampered-explicit-conflict-hash'
             WHERE request_id = ?1 AND record_kind = 'conflict'",
            [&parent],
        )
        .unwrap();
    assert_eq!(changed, 1);
    assert_eq!(store.reconcile_quota_event_groups(chrono::Utc::now().timestamp_millis()).unwrap(), 1);
    let user_cap_state: String = connection
        .query_row(
            "SELECT account.migration_state
             FROM quota_budget_accounts account
             JOIN quota_reservations reservation ON reservation.user_cap_account_id = account.id
             WHERE reservation.request_id = ?1",
            [&parent],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(user_cap_state, "reconcile_required");
}

#[test]
fn reconcile_v1_commit_and_release_event_groups_without_quarantining_key() {
    let (_directory, store, _fixture_key, admin) = budget_fixture("v1-event-group-reconcile", 3, 10_000_000);
    let issued_key = store
        .issue_api_key_as_admin_with_max_concurrency(
            "budget-user",
            "v1-event-group-reconcile-key",
            BTreeSet::from(["chat:invoke".into()]),
            3,
            &admin,
        )
        .unwrap();
    let key_id = issued_key.id.clone();
    store
        .key_quota_grant_as_admin(
            &admin,
            KeyQuotaGrant {
                api_key_id: key_id.clone(),
                resource_kind: "credits".into(),
                amount: 10_000_000,
                actor_user_id: "budget-admin".into(),
                reason: "v1 event-group reconciliation fixture".into(),
            },
        )
        .unwrap();
    let user_principal = store.authenticate_api_key(&issued_key.plaintext).unwrap();
    let committed = create_legacy_credit_request(&store, &key_id, "v1-event-group-commit");
    assert!(matches!(
        store
            .apply_credit_receipt(legacy_receipt(
                &committed,
                BillingReceiptStatus::Final,
                Some(CreditAmount::parse("0.5", "credits").unwrap()),
                "v1-event-group-commit-source",
            ))
            .unwrap(),
        BillingReceiptResult::Settled { .. }
    ));

    let released = create_legacy_credit_request(&store, &key_id, "v1-event-group-release");
    let reservation = store.reservation_for_request(&released).unwrap().unwrap();
    store
        .settle_request(
            &user_principal,
            &reservation.id,
            aiwork_core::Settlement::Release,
            aiwork_core::RequestState::Failed,
            None,
        )
        .unwrap();

    assert_eq!(store.reconcile_quota_event_groups(chrono::Utc::now().timestamp_millis()).unwrap(), 0);
    let balance = store.key_quota_balance_as_admin(&admin, &key_id, "credits").unwrap();
    assert_eq!((balance.available, balance.held, balance.settled), (9_500_000, 0, 500_000));
    assert!(matches!(
        store.reserve_credit_quote({
            let request = match store
                .begin_billed_request(BeginRequestInput {
                    user_id: "budget-user".into(),
                    api_key_id: key_id.clone(),
                    protocol: "openai".into(),
                    endpoint: "/v1/chat/completions".into(),
                    model: "legacy-text-model".into(),
                    idempotency_key: "v1-event-group-after-reconcile".into(),
                    body: json!({"model":"legacy-text-model","messages":[]}),
                })
                .unwrap()
            {
                BeginRequest::Created(request) => request,
                other => panic!("expected fresh legacy request after reconciliation, got {other:?}"),
            };
            BillingQuote {
                request_id: request.id.clone(),
                quote_id: "v1-event-group-after-reconcile-quote".into(),
                request_fingerprint: store.request_fingerprint_for_billing(&request.id).unwrap(),
                endpoint: request.endpoint,
                model: request.model,
                max_credits: CreditAmount::parse("1", "credits").unwrap(),
                unit: "credits".into(),
                expires_at_ms: chrono::Utc::now().timestamp_millis() + 60_000,
                source_ref: "v1-event-group-after-reconcile-source".into(),
            }
        }),
        Ok(BillingReservationResult::Created { .. })
    ));
}

#[test]
fn fresh_database_migrates_to_v27_budget_schema() {
    let directory = TestDirectory::new("fresh-schema");
    let store = CoreStore::open(&directory.0).unwrap();

    store.migrate().unwrap();

    assert_eq!(store.schema_version().unwrap(), aiwork_core::CURRENT_SCHEMA_VERSION);
    assert_eq!(store.table_count("budget_operations").unwrap(), 1);
    assert_eq!(store.table_count("budget_steps").unwrap(), 1);
    assert!(store.foreign_keys_enabled().unwrap());
}

#[test]
fn fresh_database_creates_separate_v2_budget_tables() {
    let directory = TestDirectory::new("budget-tables");
    let store = CoreStore::open(&directory.0).unwrap();

    store.migrate().unwrap();

    for table in [
        "budget_preparations",
        "budget_operations",
        "budget_steps",
        "budget_receipt_evidence",
        "budget_settlements",
        "budget_authorization_revisions",
        "legacy_execution_evidence",
    ] {
        assert_eq!(store.table_count(table).unwrap(), 1, "missing table {table}");
    }
    assert!(store.foreign_keys_enabled().unwrap());

    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    let mut statement = connection.prepare("PRAGMA foreign_key_check").unwrap();
    assert!(statement.query([]).unwrap().next().unwrap().is_none());
}

#[test]
fn newly_created_v2_reserve_is_tagged_with_local_authorization_revision_one() {
    let (directory, store, key_id, _) = budget_fixture("revision-one-reserve", 1, 20_000_000);
    let parent_request_id = begin_video_parent(&store, &key_id, "revision-one-reserve-parent");
    store
        .begin_budget_operation(
            &parent_request_id,
            video_budget_step(&store, &key_id, &parent_request_id, "revision-one-reserve-budget"),
        )
        .unwrap();

    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    let revision_column_exists: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('quota_ledger') WHERE name = 'authorization_revision')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(revision_column_exists, "V26 must add the local authorization revision column");
    let rows: Vec<(Option<i64>, Option<i64>)> = connection
        .prepare(
            "SELECT authorization_revision, budget_version FROM quota_ledger
             WHERE request_id = ?1 AND event_kind = 'reserve' ORDER BY budget_account_id",
        )
        .unwrap()
        .query_map([&parent_request_id], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(!rows.is_empty());
    assert!(rows.iter().all(|(revision, account_version)| *revision == Some(1) && account_version.is_some()));
}

#[test]
fn add_budget_step_records_revision_one_for_reserve_and_authorization_history() {
    let (directory, store, key_id, admin) = budget_fixture("add-step-revision-one", 2, 100_000_000);
    store
        .quota_pool_grant_as_admin(
            &admin,
            aiwork_core::QuotaGrant {
                user_id: "budget-user".into(),
                resource_kind: "credits".into(),
                amount: 100_000_000,
                actor_user_id: "budget-admin".into(),
                reason: "add-budget-step revision-one fixture".into(),
            },
        )
        .unwrap();
    let parent_request_id = begin_video_parent(&store, &key_id, "add-step-revision-one-parent");
    let helper = match store
        .begin_budget_assist_request(
            &parent_request_id,
            budget_assist_input(&key_id, "add-step-revision-one-helper"),
        )
        .unwrap()
    {
        BeginRequest::Created(request) => request,
        other => panic!("expected a new helper request, got {other:?}"),
    };
    store
        .begin_budget_operation(
            &parent_request_id,
            assist_budget_step(
                &store,
                &key_id,
                &parent_request_id,
                &helper.id,
                "add-step-revision-one-assist",
            ),
        )
        .unwrap();
    dispatch_current_budget_step(&store, &helper.id).unwrap();
    store
        .mark_budget_step_execution(&helper.id, aiwork_core::BudgetExecutionState::Succeeded)
        .unwrap();

    let video = video_budget_step(
        &store,
        &key_id,
        &parent_request_id,
        "add-step-revision-one-video",
    );
    let expected_authorization = video.authorization.clone();
    store.add_budget_step(&parent_request_id, video).unwrap();

    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    let event_group_id: String = connection
        .query_row(
            "SELECT event_group_id FROM quota_reservations WHERE request_id = ?1",
            [&parent_request_id],
            |row| row.get(0),
        )
        .unwrap();
    let reserve_rows: Vec<(String, Option<i64>, Option<i64>)> = connection
        .prepare(
            "SELECT budget_account_id, authorization_revision, budget_version
             FROM quota_ledger WHERE event_group_id = ?1 AND event_kind = 'reserve'
             ORDER BY budget_account_id",
        )
        .unwrap()
        .query_map([&event_group_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(reserve_rows.len(), 2, "Key and configured user-cap each have a reserve row");
    assert!(reserve_rows
        .iter()
        .all(|(_, revision, version)| *revision == Some(1) && version.is_some()));

    let history: (i64, String, Vec<u8>, i64) = connection
        .query_row(
            "SELECT local_revision, authorization_json, authorization_hash, hold_microcredits
             FROM budget_authorization_revisions WHERE request_id = ?1",
            [&parent_request_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(history.0, 1);
    assert_eq!(history.2, connection
        .query_row(
            "SELECT authorization_hash FROM budget_steps WHERE request_id = ?1",
            [&parent_request_id],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .unwrap());
    assert_eq!(history.3, expected_authorization.hold_credits.as_microcredits());
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&history.1).unwrap(),
        serde_json::to_value(&expected_authorization).unwrap(),
    );
}

#[test]
fn v24_to_v27_upgrade_preserves_legacy_held_unknown_and_settled_billing() {
    let directory = TestDirectory::new("v24-preservation");
    let store = CoreStore::open(&directory.0).unwrap();
    store.migrate().unwrap();
    let database = directory.0.join("data").join(CORE_DB_FILE);
    let connection = Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "DROP INDEX IF EXISTS quota_ledger_authorization_adjust_unique;
             DROP TRIGGER IF EXISTS budget_authorization_revisions_no_update;
             DROP TRIGGER IF EXISTS budget_authorization_revisions_no_delete;
             DROP TRIGGER IF EXISTS legacy_execution_evidence_no_update;
             DROP TRIGGER IF EXISTS legacy_execution_evidence_no_delete;
             DROP TABLE IF EXISTS legacy_execution_evidence;
             DROP TABLE IF EXISTS budget_authorization_revisions;
             ALTER TABLE quota_ledger DROP COLUMN authorization_revision;
             DROP TABLE IF EXISTS budget_settlements;
             DROP TABLE IF EXISTS budget_receipt_evidence;
             DROP TABLE IF EXISTS budget_steps;
             DROP TABLE IF EXISTS budget_operations;
             DROP TABLE IF EXISTS budget_preparations;
             UPDATE schema_meta SET value = '24' WHERE key = 'schema_version';",
        )
        .unwrap();
    drop(connection);
    store
        .create_user(
            NewUser { id: "budget-admin".into(), name: "Admin".into(), role: UserRole::Admin },
            "bootstrap",
        )
        .unwrap();
    store
        .create_user(
            NewUser { id: "budget-user".into(), name: "User".into(), role: UserRole::User },
            "budget-admin",
        )
        .unwrap();
    let admin_key = store
        .issue_api_key("budget-admin", "admin", BTreeSet::from(["admin:*".into()]), "bootstrap")
        .unwrap();
    let admin = store.authenticate_api_key(&admin_key.plaintext).unwrap();
    let user_key = store
        .issue_api_key_as_admin_with_max_concurrency(
            "budget-user", "legacy", BTreeSet::from(["chat:invoke".into()]), 3, &admin,
        )
        .unwrap();
    store
        .key_quota_grant_as_admin(
            &admin,
            KeyQuotaGrant {
                api_key_id: user_key.id.clone(),
                resource_kind: "credits".into(),
                amount: 10_000_000,
                actor_user_id: "budget-admin".into(),
                reason: "v24 migration fixture".into(),
            },
        )
        .unwrap();

    let held = create_legacy_credit_request(&store, &user_key.id, "legacy-held");
    let unknown = create_legacy_credit_request(&store, &user_key.id, "legacy-unknown");
    assert_eq!(
        store
            .apply_credit_receipt(legacy_receipt(
                &unknown,
                BillingReceiptStatus::Unknown,
                None,
                "legacy-session-unknown",
            ))
            .unwrap(),
        BillingReceiptResult::Pending
    );
    let settled = create_legacy_credit_request(&store, &user_key.id, "legacy-settled");
    assert!(matches!(
        store
            .apply_credit_receipt(legacy_receipt(
                &settled,
                BillingReceiptStatus::Final,
                Some(CreditAmount::parse("0.25", "credits").unwrap()),
                "legacy-session-final",
            ))
            .unwrap(),
        BillingReceiptResult::Settled { .. }
    ));

    let request_ids = [held.as_str(), unknown.as_str(), settled.as_str()];
    let before = legacy_billing_snapshot(&database, request_ids);
    assert_eq!(before.reservations.len(), 3);
    assert_eq!(before.receipt_count, 2);
    assert_eq!(before.settlements, vec![(settled.clone(), 250_000)]);

    store.migrate().unwrap();

    assert_eq!(store.schema_version().unwrap(), aiwork_core::CURRENT_SCHEMA_VERSION);
    assert_eq!(legacy_billing_snapshot(&database, request_ids), before);
    assert_eq!(store.table_count("budget_operations").unwrap(), 1);
    drop(store);
}

#[test]
fn budget_operation_reserves_only_the_authorized_step_amount() {
    let (_directory, store, key_id, admin) = budget_fixture("partial-reserve", 2, 1_000_000_000);
    let parent_request_id = begin_video_parent(&store, &key_id, "partial-reserve-parent");
    let authorization = aiwork_core::BudgetAuthorization {
        budget_id: "budget-video-1".into(),
        parent_request_id: parent_request_id.clone(),
        request_id: parent_request_id.clone(),
        core_key_id: key_id.clone(),
        request_fingerprint: store.request_fingerprint_for_billing(&parent_request_id).unwrap(),
        endpoint: "videos".into(),
        model: "seedance-fast".into(),
        account_ref: "account-test".into(),
        bridge_instance_id: "bridge-test".into(),
        profile_fingerprint: "profile-hash".into(),
        policy_version: "policy-v1".into(),
        hold_credits: CreditAmount::parse("2", "credits").unwrap(),
        expires_at_ms: chrono::Utc::now().timestamp_millis() + 60_000,
    };

    let operation = store
        .begin_budget_operation(
            &parent_request_id,
            aiwork_core::BudgetStepInput {
                kind: aiwork_core::BudgetStepKind::Video,
                authorization,
            },
        )
        .unwrap();

    assert_eq!(operation.parent_request_id, parent_request_id);
    assert_eq!(operation.steps.len(), 1);
    assert_eq!(operation.steps[0].hold_credits.as_microcredits(), 2_000_000);
    assert_eq!(operation.steps[0].dispatch_attempted, false);
    let balance = store.key_quota_balance_as_admin(&admin, &key_id, "credits").unwrap();
    assert_eq!((balance.available, balance.held), (998_000_000, 2_000_000));
    assert_eq!(
        store.budget_operation(&operation.parent_request_id).unwrap(),
        Some(operation)
    );
}

#[test]
fn budget_assist_preparation_links_received_child_without_reserving() {
    let (directory, store, key_id, _) = budget_fixture("assist-preparation", 2, 1_000_000_000);
    let parent_request_id = begin_video_parent(&store, &key_id, "assist-preparation-parent");
    let input = budget_assist_input(&key_id, "assist-preparation-child");

    let child = match store
        .begin_budget_assist_request(&parent_request_id, input.clone())
        .unwrap()
    {
        BeginRequest::Created(child) => child,
        other => panic!("expected a new helper request, got {other:?}"),
    };
    assert_eq!(child.state, aiwork_core::RequestState::Received);
    assert!(matches!(
        store.begin_budget_assist_request(&parent_request_id, input).unwrap(),
        BeginRequest::Existing(existing) if existing.id == child.id
    ));

    let database = directory.0.join("data").join(CORE_DB_FILE);
    let connection = Connection::open(database).unwrap();
    let parent_state: String = connection
        .query_row("SELECT state FROM requests WHERE id = ?1", [&parent_request_id], |row| row.get(0))
        .unwrap();
    let relation: String = connection
        .query_row(
            "SELECT child_request_id FROM request_relations WHERE parent_request_id = ?1 AND relationship_kind = 'seedance_assist'",
            [&parent_request_id],
            |row| row.get(0),
        )
        .unwrap();
    let preparation: (String, String) = connection
        .query_row(
            "SELECT child_request_id, state FROM budget_preparations WHERE parent_request_id = ?1",
            [&parent_request_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let reservations: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM quota_reservations WHERE request_id IN (?1, ?2)",
            params![&parent_request_id, &child.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(parent_state, "received");
    assert_eq!(relation, child.id);
    assert_eq!(preparation, (child.id, "open".into()));
    assert_eq!(reservations, 0);
}

#[test]
fn abort_budget_preparation_is_idempotent_and_keeps_financial_facts_untouched() {
    let (directory, store, key_id, _) = budget_fixture("assist-abort", 2, 1_000_000_000);
    let parent_request_id = begin_video_parent(&store, &key_id, "assist-abort-parent");
    let child = match store
        .begin_budget_assist_request(&parent_request_id, budget_assist_input(&key_id, "assist-abort-child"))
        .unwrap()
    {
        BeginRequest::Created(child) => child,
        other => panic!("expected a new helper request, got {other:?}"),
    };

    assert_eq!(
        store.abort_budget_preparation(&parent_request_id, "bridge preparation failed").unwrap(),
        aiwork_core::BudgetMutation::Applied
    );
    assert_eq!(
        store.abort_budget_preparation(&parent_request_id, "bridge preparation failed").unwrap(),
        aiwork_core::BudgetMutation::Duplicate
    );
    assert!(store.abort_budget_preparation(&parent_request_id, "different reason").is_err());

    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    let states: (String, String) = connection
        .query_row(
            "SELECT parent.state, child.state FROM requests parent JOIN requests child
             ON child.id = ?2 WHERE parent.id = ?1",
            params![&parent_request_id, &child.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let preparation: (String, String) = connection
        .query_row(
            "SELECT state, abort_reason FROM budget_preparations WHERE parent_request_id = ?1",
            [&parent_request_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let financial_rows: i64 = connection
        .query_row(
            "SELECT (SELECT COUNT(*) FROM quota_reservations WHERE request_id IN (?1, ?2))
             + (SELECT COUNT(*) FROM budget_settlements WHERE request_id IN (?1, ?2))
             + (SELECT COUNT(*) FROM billing_receipts WHERE request_id IN (?1, ?2))",
            params![&parent_request_id, &child.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(states, ("failed".into(), "failed".into()));
    assert_eq!(preparation, ("aborted".into(), "bridge preparation failed".into()));
    assert_eq!(financial_rows, 0);
}

#[test]
fn preparation_admit_and_abort_race_has_single_winner() {
    let (directory, store, key_id, _) = budget_fixture("assist-admit-abort-race", 2, 1_000_000_000);
    let parent_request_id = begin_video_parent(&store, &key_id, "assist-admit-abort-race-parent");
    let child = match store
        .begin_budget_assist_request(
            &parent_request_id,
            budget_assist_input(&key_id, "assist-admit-abort-race-child"),
        )
        .unwrap()
    {
        BeginRequest::Created(child) => child,
        other => panic!("expected a new helper request, got {other:?}"),
    };
    let step = assist_budget_step(
        &store,
        &key_id,
        &parent_request_id,
        &child.id,
        "assist-admit-abort-race-budget",
    );

    let db_root = Arc::new(directory.0.clone());
    let barrier = Arc::new(Barrier::new(3));
    let admission_db = Arc::clone(&db_root);
    let admission_barrier = Arc::clone(&barrier);
    let admission_parent = parent_request_id.clone();
    let admission_worker = thread::spawn(move || {
        let worker_store = CoreStore::open(admission_db.as_path()).unwrap();
        admission_barrier.wait();
        worker_store.begin_budget_operation(&admission_parent, step)
    });

    let abort_db = Arc::clone(&db_root);
    let abort_barrier = Arc::clone(&barrier);
    let abort_parent = parent_request_id.clone();
    let abort_worker = thread::spawn(move || {
        let worker_store = CoreStore::open(abort_db.as_path()).unwrap();
        abort_barrier.wait();
        worker_store.abort_budget_preparation(&abort_parent, "raced bridge abort")
    });
    barrier.wait();

    let admission = admission_worker.join().unwrap();
    let abort = abort_worker.join().unwrap();
    let admission_won = admission.is_ok();
    let abort_won = matches!(&abort, Ok(aiwork_core::BudgetMutation::Applied));
    assert_ne!(admission_won, abort_won, "exactly one transaction must win the race");

    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    let (preparation_state, operations, reservations): (String, i64, i64) = connection
        .query_row(
            "SELECT preparation.state,
                    (SELECT COUNT(*) FROM budget_operations WHERE parent_request_id = preparation.parent_request_id),
                    (SELECT COUNT(*) FROM quota_reservations WHERE request_id = preparation.child_request_id)
             FROM budget_preparations preparation WHERE preparation.parent_request_id = ?1",
            [&parent_request_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();

    if admission_won {
        assert!(abort.is_err(), "an admitted preparation must not be aborted");
        assert_eq!(preparation_state, "admitted");
        assert_eq!((operations, reservations), (1, 1));
        assert!(store.budget_operation(&parent_request_id).unwrap().is_some());
    } else {
        assert!(matches!(&abort, Ok(aiwork_core::BudgetMutation::Applied)));
        assert!(admission.is_err(), "an aborted preparation must not be admitted later");
        assert_eq!(preparation_state, "aborted");
        assert_eq!((operations, reservations), (0, 0));
        assert!(store.budget_operation(&parent_request_id).unwrap().is_none());
    }
}

#[test]
fn preparation_recovery_uses_strict_cutoff_and_survives_database_reopen() {
    let (directory, store, key_id, _) = budget_fixture("assist-recovery", 3, 1_000_000_000);
    let old_parent = begin_video_parent(&store, &key_id, "assist-recovery-old-parent");
    store
        .begin_budget_assist_request(&old_parent, budget_assist_input(&key_id, "assist-recovery-old-child"))
        .unwrap();
    let cutoff_parent = begin_video_parent(&store, &key_id, "assist-recovery-cutoff-parent");
    store
        .begin_budget_assist_request(&cutoff_parent, budget_assist_input(&key_id, "assist-recovery-cutoff-child"))
        .unwrap();
    let database = directory.0.join("data").join(CORE_DB_FILE);
    let connection = Connection::open(&database).unwrap();
    connection
        .execute(
            "UPDATE budget_preparations SET created_at_ms = 99 WHERE parent_request_id = ?1",
            [&old_parent],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE budget_preparations SET created_at_ms = 100 WHERE parent_request_id = ?1",
            [&cutoff_parent],
        )
        .unwrap();
    drop(connection);
    drop(store);

    let reopened = CoreStore::open(&directory.0).unwrap();
    reopened.migrate().unwrap();
    assert_eq!(
        reopened.recover_abandoned_budget_preparations_before(100).unwrap(),
        vec![old_parent.clone()]
    );
    let connection = Connection::open(database).unwrap();
    let old_state: String = connection
        .query_row(
            "SELECT state FROM budget_preparations WHERE parent_request_id = ?1",
            [&old_parent],
            |row| row.get(0),
        )
        .unwrap();
    let cutoff_state: String = connection
        .query_row(
            "SELECT state FROM budget_preparations WHERE parent_request_id = ?1",
            [&cutoff_parent],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(old_state, "aborted");
    assert_eq!(cutoff_state, "open");
}

#[test]
fn budget_operation_admission_uses_received_order_for_existing_parents() {
    let (directory, store, key_id, _) = budget_fixture("received-order", 1, 1_000_000_000);
    let first_parent = begin_video_parent(&store, &key_id, "received-order-first");
    let second_parent = begin_video_parent(&store, &key_id, "received-order-second");
    let database = directory.0.join("data").join(CORE_DB_FILE);
    let connection = Connection::open(database).unwrap();
    connection
        .execute("UPDATE requests SET created_at_ms = 100 WHERE id = ?1", [&first_parent])
        .unwrap();
    connection
        .execute("UPDATE requests SET created_at_ms = 200 WHERE id = ?1", [&second_parent])
        .unwrap();
    drop(connection);

    store
        .begin_budget_operation(&first_parent, video_budget_step(&store, &key_id, &first_parent, "received-order-budget-1"))
        .unwrap();
    assert!(store
        .begin_budget_operation(&second_parent, video_budget_step(&store, &key_id, &second_parent, "received-order-budget-2"))
        .is_err());
    assert!(store.budget_operation(&second_parent).unwrap().is_none());
}

#[test]
fn concurrent_marking_budget_step_dispatched_has_one_persisted_attempt() {
    let (directory, store, key_id, _) = budget_fixture("mark-dispatched", 1, 1_000_000_000);
    let parent_request_id = begin_video_parent(&store, &key_id, "mark-dispatched-parent");
    store
        .begin_budget_operation(
            &parent_request_id,
            video_budget_step(&store, &key_id, &parent_request_id, "mark-dispatched-budget"),
        )
        .unwrap();

    let db_root = Arc::new(directory.0.clone());
    let barrier = Arc::new(Barrier::new(3));
    let workers = (0..2)
        .map(|_| {
            let worker_db = Arc::clone(&db_root);
            let worker_barrier = Arc::clone(&barrier);
            let request_id = parent_request_id.clone();
            thread::spawn(move || {
                let worker_store = CoreStore::open(worker_db.as_path()).unwrap();
                worker_barrier.wait();
                dispatch_current_budget_step(&worker_store, &request_id)
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();

    let mut applied = 0;
    let mut duplicate = 0;
    for worker in workers {
        match worker.join().unwrap().unwrap() {
            aiwork_core::BudgetMutation::Applied => applied += 1,
            aiwork_core::BudgetMutation::Duplicate => duplicate += 1,
        }
    }
    assert_eq!((applied, duplicate), (1, 1));
    assert_eq!(
        dispatch_current_budget_step(&store, &parent_request_id).unwrap(),
        aiwork_core::BudgetMutation::Duplicate
    );
    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    let step: (i64, String, String) = connection
        .query_row(
            "SELECT dispatch_attempted, execution_state, financial_state FROM budget_steps WHERE request_id = ?1",
            [&parent_request_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    let parent_state: String = connection
        .query_row("SELECT state FROM requests WHERE id = ?1", [&parent_request_id], |row| row.get(0))
        .unwrap();
    let operation_state: String = connection
        .query_row("SELECT execution_state FROM budget_operations WHERE parent_request_id = ?1", [&parent_request_id], |row| row.get(0))
        .unwrap();
    assert_eq!(step, (1, "running".into(), "held".into()));
    assert_eq!(parent_state, "dispatched");
    assert_eq!(operation_state, "running");
}

#[test]
fn video_task_binding_and_execution_do_not_settle_financial_state_or_parent() {
    let (directory, store, key_id, _) = budget_fixture("video-execution", 1, 1_000_000_000);
    let parent_request_id = begin_video_parent(&store, &key_id, "video-execution-parent");
    store
        .begin_budget_operation(
            &parent_request_id,
            video_budget_step(&store, &key_id, &parent_request_id, "video-execution-budget"),
        )
        .unwrap();
    dispatch_current_budget_step(&store, &parent_request_id).unwrap();

    assert!(store
        .mark_budget_step_execution(&parent_request_id, aiwork_core::BudgetExecutionState::Succeeded)
        .is_err());
    assert_eq!(
        store.bind_budget_video_task(&parent_request_id, "task-video-1").unwrap(),
        aiwork_core::BudgetMutation::Applied
    );
    assert_eq!(
        store.bind_budget_video_task(&parent_request_id, "task-video-1").unwrap(),
        aiwork_core::BudgetMutation::Duplicate
    );
    assert!(store.bind_budget_video_task(&parent_request_id, "task-video-other").is_err());
    assert_eq!(
        store.mark_budget_step_execution(&parent_request_id, aiwork_core::BudgetExecutionState::Succeeded).unwrap(),
        aiwork_core::BudgetMutation::Applied
    );
    assert_eq!(
        store.mark_budget_step_execution(&parent_request_id, aiwork_core::BudgetExecutionState::Succeeded).unwrap(),
        aiwork_core::BudgetMutation::Duplicate
    );

    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    let step: (String, String, Option<i64>, Option<String>) = connection
        .query_row(
            "SELECT execution_state, financial_state, actual_microcredits, task_ref
             FROM budget_steps WHERE request_id = ?1",
            [&parent_request_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    let parent_state: String = connection
        .query_row("SELECT state FROM requests WHERE id = ?1", [&parent_request_id], |row| row.get(0))
        .unwrap();
    assert_eq!(step, ("succeeded".into(), "held".into(), None, Some("task-video-1".into())));
    assert_eq!(parent_state, "dispatched");
}

#[test]
fn video_step_can_be_added_after_assist_execution_without_waiting_for_assist_receipt() {
    let (directory, store, key_id, admin) = budget_fixture("assist-then-video", 1, 1_000_000_000);
    let parent_request_id = begin_video_parent(&store, &key_id, "assist-then-video-parent");
    let child = match store
        .begin_budget_assist_request(&parent_request_id, budget_assist_input(&key_id, "assist-then-video-child"))
        .unwrap()
    {
        BeginRequest::Created(child) => child,
        other => panic!("expected a new helper request, got {other:?}"),
    };
    let operation = store
        .begin_budget_operation(
            &parent_request_id,
            assist_budget_step(&store, &key_id, &parent_request_id, &child.id, "assist-then-video-budget-assist"),
        )
        .unwrap();
    dispatch_current_budget_step(&store, &child.id).unwrap();
    store
        .mark_budget_step_execution(&child.id, aiwork_core::BudgetExecutionState::Succeeded)
        .unwrap();

    let updated = store
        .add_budget_step(
            &parent_request_id,
            video_budget_step(&store, &key_id, &parent_request_id, "assist-then-video-budget-video"),
        )
        .unwrap();
    assert_eq!(updated.operation_id, operation.operation_id);
    assert_eq!(updated.steps.len(), 2);
    let assist = updated.steps.iter().find(|step| step.kind == aiwork_core::BudgetStepKind::Assist).unwrap();
    let video = updated.steps.iter().find(|step| step.kind == aiwork_core::BudgetStepKind::Video).unwrap();
    assert_eq!(assist.execution_state, aiwork_core::BudgetExecutionState::Succeeded);
    assert_eq!(assist.financial_state, aiwork_core::BudgetFinancialState::Held);
    assert_eq!(video.hold_credits.as_microcredits(), 2_000_000);
    let balance = store.key_quota_balance_as_admin(&admin, &key_id, "credits").unwrap();
    assert_eq!((balance.available, balance.held), (997_000_000, 3_000_000));
    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    let operation_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM budget_operations WHERE api_key_id = ?1", [&key_id], |row| row.get(0))
        .unwrap();
    assert_eq!(operation_count, 1);
}

#[test]
fn assist_only_execution_can_finish_parent_without_a_video_charge_step() {
    let (directory, store, key_id, _) = budget_fixture("assist-only-finish", 1, 1_000_000_000);
    let parent_request_id = begin_video_parent(&store, &key_id, "assist-only-finish-parent");
    let child = match store
        .begin_budget_assist_request(&parent_request_id, budget_assist_input(&key_id, "assist-only-finish-child"))
        .unwrap()
    {
        BeginRequest::Created(child) => child,
        other => panic!("expected a new helper request, got {other:?}"),
    };
    store
        .begin_budget_operation(
            &parent_request_id,
            assist_budget_step(&store, &key_id, &parent_request_id, &child.id, "assist-only-finish-budget"),
        )
        .unwrap();
    dispatch_current_budget_step(&store, &child.id).unwrap();
    store
        .mark_budget_step_execution(&child.id, aiwork_core::BudgetExecutionState::Succeeded)
        .unwrap();

    assert_eq!(
        store
            .finish_budget_execution(&parent_request_id, aiwork_core::BudgetExecutionState::Succeeded)
            .unwrap(),
        aiwork_core::BudgetMutation::Applied
    );
    assert_eq!(
        store
            .finish_budget_execution(&parent_request_id, aiwork_core::BudgetExecutionState::Succeeded)
            .unwrap(),
        aiwork_core::BudgetMutation::Duplicate
    );
    assert_eq!(store.active_execution_count_for_key(&key_id).unwrap(), 0);
    let operation = store.budget_operation(&parent_request_id).unwrap().unwrap();
    assert_eq!(operation.execution_state, aiwork_core::BudgetExecutionState::Succeeded);
    assert_eq!(operation.steps.len(), 1);
    assert_eq!(operation.steps[0].financial_state, aiwork_core::BudgetFinancialState::Held);
    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    let parent_state: String = connection
        .query_row("SELECT state FROM requests WHERE id = ?1", [&parent_request_id], |row| row.get(0))
        .unwrap();
    assert_eq!(parent_state, "succeeded");
}

#[test]
fn expired_budget_authorization_only_allows_exact_existing_operation_replay() {
    let (directory, store, key_id, admin) = budget_fixture("expired-auth-replay", 2, 1_000_000_000);
    let parent_request_id = begin_video_parent(&store, &key_id, "expired-auth-existing-parent");
    let original = video_budget_step(&store, &key_id, &parent_request_id, "expired-auth-existing-budget");
    let initial = store.begin_budget_operation(&parent_request_id, original.clone()).unwrap();
    let before = store.key_quota_balance_as_admin(&admin, &key_id, "credits").unwrap();
    let expired_at = chrono::Utc::now().timestamp_millis() - 1;
    let mut expired_replay = original;
    expired_replay.authorization.expires_at_ms = expired_at;
    let expired_hash = aiwork_core::canonical_json_hash(
        &serde_json::to_value(&expired_replay.authorization).unwrap(),
    )
    .to_vec();
    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    connection
        .execute(
            "UPDATE budget_steps SET expires_at_ms = ?1, authorization_hash = ?2 WHERE request_id = ?3",
            params![expired_at, expired_hash, &parent_request_id],
        )
        .unwrap();
    connection
        .execute("UPDATE quota_reservations SET expires_at_ms = ?1 WHERE request_id = ?2", params![expired_at, &parent_request_id])
        .unwrap();
    drop(connection);

    let expected = store.budget_operation(&parent_request_id).unwrap().unwrap();
    let replay = store
        .begin_budget_operation(&parent_request_id, expired_replay)
        .unwrap();
    assert_eq!(replay, expected);
    assert_eq!(replay.operation_id, initial.operation_id);
    assert!(dispatch_current_budget_step(&store, &parent_request_id).is_err());

    let new_parent = begin_video_parent(&store, &key_id, "expired-auth-new-parent");
    let mut expired_new = video_budget_step(&store, &key_id, &new_parent, "expired-auth-new-budget");
    expired_new.authorization.expires_at_ms = expired_at;
    assert!(store.begin_budget_operation(&new_parent, expired_new).is_err());
    assert!(store.budget_operation(&new_parent).unwrap().is_none());
    let after = store.key_quota_balance_as_admin(&admin, &key_id, "credits").unwrap();
    assert_eq!((after.available, after.held), (before.available, before.held));
}

#[test]
fn attempted_assist_can_resume_running_from_unknown_without_dispatching_again() {
    let (directory, store, key_id, _) = budget_fixture("assist-unknown-running", 1, 1_000_000_000);
    let parent_request_id = begin_video_parent(&store, &key_id, "assist-unknown-running-parent");
    let child = match store
        .begin_budget_assist_request(&parent_request_id, budget_assist_input(&key_id, "assist-unknown-running-child"))
        .unwrap()
    {
        BeginRequest::Created(child) => child,
        other => panic!("expected a new helper request, got {other:?}"),
    };
    store
        .begin_budget_operation(
            &parent_request_id,
            assist_budget_step(&store, &key_id, &parent_request_id, &child.id, "assist-unknown-running-budget"),
        )
        .unwrap();
    dispatch_current_budget_step(&store, &child.id).unwrap();
    assert_eq!(
        store.mark_budget_step_execution(&child.id, aiwork_core::BudgetExecutionState::Unknown).unwrap(),
        aiwork_core::BudgetMutation::Applied
    );
    assert_eq!(
        store.mark_budget_step_execution(&child.id, aiwork_core::BudgetExecutionState::Running).unwrap(),
        aiwork_core::BudgetMutation::Applied
    );
    assert_eq!(
        dispatch_current_budget_step(&store, &child.id).unwrap(),
        aiwork_core::BudgetMutation::Duplicate
    );
    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    let (attempted, execution_state): (i64, String) = connection
        .query_row(
            "SELECT dispatch_attempted, execution_state FROM budget_steps WHERE request_id = ?1",
            [&child.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let child_request_state: String = connection
        .query_row("SELECT state FROM requests WHERE id = ?1", [&child.id], |row| row.get(0))
        .unwrap();
    assert_eq!((attempted, execution_state.as_str()), (1, "running"));
    assert_eq!(child_request_state, "dispatched");
}

#[test]
fn video_failed_no_charge_without_task_ref_releases_only_its_hold() {
    let (directory, store, key_id, admin) = budget_fixture("video-no-charge-no-task", 2, 1_000_000_000);
    let parent_request_id = begin_video_parent(&store, &key_id, "video-no-charge-no-task-parent");
    let child = match store
        .begin_budget_assist_request(&parent_request_id, budget_assist_input(&key_id, "video-no-charge-no-task-child"))
        .unwrap()
    {
        BeginRequest::Created(child) => child,
        other => panic!("expected a new helper request, got {other:?}"),
    };
    store
        .begin_budget_operation(
            &parent_request_id,
            assist_budget_step(&store, &key_id, &parent_request_id, &child.id, "video-no-charge-assist-budget"),
        )
        .unwrap();
    dispatch_current_budget_step(&store, &child.id).unwrap();
    store
        .mark_budget_step_execution(&child.id, aiwork_core::BudgetExecutionState::Succeeded)
        .unwrap();
    store
        .add_budget_step(
            &parent_request_id,
            video_budget_step(&store, &key_id, &parent_request_id, "video-no-charge-video-budget"),
        )
        .unwrap();
    dispatch_current_budget_step(&store, &parent_request_id).unwrap();

    let mut receipt = final_budget_receipt(&parent_request_id, "video-no-charge-video-budget", "0");
    receipt.receipt.status = BillingReceiptStatus::FailedNoCharge;
    receipt.receipt.source_ref = "send-fence-not-sent-1".into();
    receipt.receipt.task_ref = None;
    assert_eq!(
        store.apply_budget_receipt(receipt).unwrap(),
        aiwork_core::BudgetReceiptResult::Settled {
            actual_credits: CreditAmount::parse("0", "credits").unwrap(),
            released_microcredits: 2_000_000,
            debt: false,
        }
    );

    let operation = store.budget_operation(&parent_request_id).unwrap().unwrap();
    assert_eq!(operation.execution_state, aiwork_core::BudgetExecutionState::Running);
    assert_eq!(operation.steps.len(), 2);
    let assist = operation.steps.iter().find(|step| step.request_id == child.id).unwrap();
    let video = operation.steps.iter().find(|step| step.request_id == parent_request_id).unwrap();
    assert_eq!(assist.financial_state, aiwork_core::BudgetFinancialState::Held);
    assert_eq!(video.financial_state, aiwork_core::BudgetFinancialState::Settled);
    assert_eq!(video.execution_state, aiwork_core::BudgetExecutionState::Running);
    assert!(store
        .finish_budget_execution(&parent_request_id, aiwork_core::BudgetExecutionState::Failed)
        .is_err());
    let balance = store.key_quota_balance_as_admin(&admin, &key_id, "credits").unwrap();
    assert_eq!((balance.available, balance.held), (999_000_000, 1_000_000));

    store
        .mark_budget_step_execution(&parent_request_id, aiwork_core::BudgetExecutionState::Failed)
        .unwrap();
    assert_eq!(
        store.finish_budget_execution(&parent_request_id, aiwork_core::BudgetExecutionState::Failed).unwrap(),
        aiwork_core::BudgetMutation::Applied
    );
    drop(store);
    assert!(directory.0.join("data").join(CORE_DB_FILE).exists());
}

#[test]
fn zero_final_and_failed_no_charge_with_same_source_are_not_duplicate_statuses() {
    let (directory, store, key_id, _) = budget_fixture("receipt-status-semantic", 1, 1_000_000_000);
    let parent_request_id = begin_video_parent(&store, &key_id, "receipt-status-semantic-parent");
    let budget_id = "receipt-status-semantic-budget";
    store
        .begin_budget_operation(
            &parent_request_id,
            video_budget_step(&store, &key_id, &parent_request_id, budget_id),
        )
        .unwrap();
    dispatch_current_budget_step(&store, &parent_request_id).unwrap();
    store.bind_budget_video_task(&parent_request_id, "task-receipt-video-1").unwrap();
    let mut final_receipt = final_budget_receipt(&parent_request_id, budget_id, "0");
    assert_eq!(
        store.apply_budget_receipt(final_receipt.clone()).unwrap(),
        aiwork_core::BudgetReceiptResult::Settled {
            actual_credits: CreditAmount::parse("0", "credits").unwrap(),
            released_microcredits: 2_000_000,
            debt: false,
        }
    );
    final_receipt.receipt.status = BillingReceiptStatus::FailedNoCharge;
    assert_eq!(
        store.apply_budget_receipt(final_receipt).unwrap(),
        aiwork_core::BudgetReceiptResult::Conflict
    );

    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    let (financial_state, actual): (String, i64) = connection
        .query_row(
            "SELECT financial_state, actual_microcredits FROM budget_steps WHERE request_id = ?1",
            [&parent_request_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let saved_status: String = connection
        .query_row(
            "SELECT receipt_status FROM budget_receipt_evidence
             WHERE request_id = ?1 AND record_kind = 'receipt' ORDER BY recorded_at_ms LIMIT 1",
            [&parent_request_id],
            |row| row.get(0),
        )
        .unwrap();
    let conflict_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM budget_receipt_evidence WHERE request_id = ?1 AND record_kind = 'conflict'",
            [&parent_request_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!((financial_state.as_str(), actual, saved_status.as_str(), conflict_count), ("conflict", 0, "final", 1));
}

#[test]
fn video_final_before_local_task_binding_is_retriable_without_side_effects() {
    let (directory, store, key_id, admin) = budget_fixture("receipt-task-late-binding", 1, 1_000_000_000);
    let parent_request_id = begin_video_parent(&store, &key_id, "receipt-task-late-binding-parent");
    let budget_id = "receipt-task-late-binding-budget";
    store
        .begin_budget_operation(
            &parent_request_id,
            video_budget_step(&store, &key_id, &parent_request_id, budget_id),
        )
        .unwrap();
    dispatch_current_budget_step(&store, &parent_request_id).unwrap();
    let receipt = final_budget_receipt(&parent_request_id, budget_id, "1");
    assert!(store.apply_budget_receipt(receipt.clone()).is_err());

    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    let (financial_state, attempted): (String, i64) = connection
        .query_row(
            "SELECT financial_state, dispatch_attempted FROM budget_steps WHERE request_id = ?1",
            [&parent_request_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let settlements: i64 = connection
        .query_row("SELECT COUNT(*) FROM budget_settlements WHERE request_id = ?1", [&parent_request_id], |row| row.get(0))
        .unwrap();
    let evidence: i64 = connection
        .query_row("SELECT COUNT(*) FROM budget_receipt_evidence WHERE request_id = ?1", [&parent_request_id], |row| row.get(0))
        .unwrap();
    let blocks: i64 = connection.query_row("SELECT COUNT(*) FROM api_key_billing_blocks", [], |row| row.get(0)).unwrap();
    assert_eq!((financial_state.as_str(), attempted, settlements, evidence, blocks), ("held", 1, 0, 0, 0));
    let balance = store.key_quota_balance_as_admin(&admin, &key_id, "credits").unwrap();
    assert_eq!((balance.available, balance.held), (998_000_000, 2_000_000));

    store.bind_budget_video_task(&parent_request_id, "task-receipt-video-1").unwrap();
    assert_eq!(
        store.apply_budget_receipt(receipt).unwrap(),
        aiwork_core::BudgetReceiptResult::Settled {
            actual_credits: CreditAmount::parse("1", "credits").unwrap(),
            released_microcredits: 1_000_000,
            debt: false,
        }
    );
    let blocks: i64 = connection.query_row("SELECT COUNT(*) FROM api_key_billing_blocks", [], |row| row.get(0)).unwrap();
    assert_eq!(blocks, 0);
}

#[test]
fn video_final_wrong_task_after_binding_is_conflict_with_evidence() {
    let (directory, store, key_id, admin) = budget_fixture("receipt-task-wrong-binding", 1, 1_000_000_000);
    let parent_request_id = begin_video_parent(&store, &key_id, "receipt-task-wrong-binding-parent");
    let budget_id = "receipt-task-wrong-binding-budget";
    store
        .begin_budget_operation(
            &parent_request_id,
            video_budget_step(&store, &key_id, &parent_request_id, budget_id),
        )
        .unwrap();
    dispatch_current_budget_step(&store, &parent_request_id).unwrap();
    store.bind_budget_video_task(&parent_request_id, "stored-video-task").unwrap();
    let mut wrong_task = final_budget_receipt(&parent_request_id, budget_id, "1");
    wrong_task.receipt.task_ref = Some("unrelated-video-task".into());
    assert_eq!(store.apply_budget_receipt(wrong_task.clone()).unwrap(), aiwork_core::BudgetReceiptResult::Conflict);
    wrong_task.receipt.observed_at_ms += 1;
    assert_eq!(store.apply_budget_receipt(wrong_task).unwrap(), aiwork_core::BudgetReceiptResult::Duplicate);

    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    let (financial_state, settlements, evidence, blocks): (String, i64, i64, i64) = connection
        .query_row(
            "SELECT step.financial_state,
                    (SELECT COUNT(*) FROM budget_settlements WHERE request_id = step.request_id),
                    (SELECT COUNT(*) FROM budget_receipt_evidence WHERE request_id = step.request_id AND record_kind = 'conflict'),
                    (SELECT COUNT(*) FROM api_key_billing_blocks WHERE key_id = step.core_key_id)
             FROM budget_steps step WHERE request_id = ?1",
            [&parent_request_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!((financial_state.as_str(), settlements, evidence, blocks), ("conflict", 0, 1, 1));
    let blocked_retry = begin_video_parent(&store, &key_id, "receipt-task-wrong-binding-retry");
    let blocked_step = video_budget_step(&store, &key_id, &blocked_retry, "receipt-task-wrong-binding-retry-budget");
    assert!(store.begin_budget_operation(&blocked_retry, blocked_step).is_err());
    let balance = store.key_quota_balance_as_admin(&admin, &key_id, "credits").unwrap();
    assert_eq!((balance.available, balance.held), (998_000_000, 2_000_000));
    assert_eq!(store.reconcile_quota_event_groups(chrono::Utc::now().timestamp_millis()).unwrap(), 0);
    let changed = connection
        .execute(
            "UPDATE budget_receipt_evidence SET evidence_hash = 'corrupt-conflict-receipt-hash'
             WHERE request_id = ?1 AND record_kind = 'conflict'",
            [&parent_request_id],
        )
        .unwrap();
    assert_eq!(changed, 1);
    assert_eq!(store.reconcile_quota_event_groups(chrono::Utc::now().timestamp_millis()).unwrap(), 1);
}

#[test]
fn opaque_conflict_hash_collision_is_not_a_receipt_duplicate() {
    let (directory, store, key_id, admin) = budget_fixture("receipt-opaque-conflict-hash-collision", 2, 20_000_000);
    let parent = begin_video_parent(&store, &key_id, "receipt-opaque-conflict-hash-collision-parent");
    let budget_id = "receipt-opaque-conflict-hash-collision-budget";
    store
        .begin_budget_operation(
            &parent,
            video_budget_step(&store, &key_id, &parent, budget_id),
        )
        .unwrap();
    dispatch_current_budget_step(&store, &parent).unwrap();
    store.bind_budget_video_task(&parent, "opaque-conflict-collision-task").unwrap();

    let mut receipt = final_budget_receipt(&parent, budget_id, "1");
    receipt.receipt.task_ref = Some("opaque-conflict-collision-task".into());
    receipt.receipt.source_ref = "opaque-conflict-collision-source".into();
    let evidence_hash = URL_SAFE_NO_PAD.encode(aiwork_core::canonical_json_hash(&json!({
        "request_id": parent.as_str(),
        "budget_id": budget_id,
        "account_ref": receipt.account_ref.as_str(),
        "bridge_instance_id": receipt.bridge_instance_id.as_str(),
        "status": "final",
        "actual_microcredits": 1_000_000,
        "unit": receipt.receipt.unit.as_str(),
        "source_ref": receipt.receipt.source_ref.as_str(),
        "task_ref": receipt.receipt.task_ref.as_deref(),
    })));
    store
        .mark_budget_receipt_conflict(aiwork_core::BudgetReceiptConflict {
            request_id: parent.clone(),
            budget_id: budget_id.into(),
            account_ref: "account-test".into(),
            bridge_instance_id: "bridge-test".into(),
            source_ref: "opaque-conflict-collision-source".into(),
            evidence_hash,
            observed_at_ms: chrono::Utc::now().timestamp_millis(),
        })
        .unwrap();

    assert_eq!(store.apply_budget_receipt(receipt).unwrap(), aiwork_core::BudgetReceiptResult::Conflict);
    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    let (financial_state, conflicts, receipts, settlements, blocks): (String, i64, i64, i64, i64) = connection
        .query_row(
            "SELECT step.financial_state,
                    (SELECT COUNT(*) FROM budget_receipt_evidence WHERE request_id = step.request_id AND record_kind = 'conflict'),
                    (SELECT COUNT(*) FROM budget_receipt_evidence WHERE request_id = step.request_id AND record_kind = 'receipt'),
                    (SELECT COUNT(*) FROM budget_settlements WHERE request_id = step.request_id),
                    (SELECT COUNT(*) FROM api_key_billing_blocks WHERE key_id = step.core_key_id)
             FROM budget_steps step WHERE request_id = ?1",
            [&parent],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .unwrap();
    assert_eq!((financial_state.as_str(), conflicts, receipts, settlements, blocks), ("conflict", 1, 0, 0, 1));
    let balance = store.key_quota_balance_as_admin(&admin, &key_id, "credits").unwrap();
    assert_eq!((balance.available, balance.held), (18_000_000, 2_000_000));
}

#[test]
fn begin_chat_step_kind_must_match_chat_endpoint_without_leaving_a_hold() {
    let (directory, store, key_id, admin) = budget_fixture("begin-chat-endpoint-kind", 3, 20_000_000);
    let video_parent = begin_video_parent(&store, &key_id, "begin-chat-kind-video-parent");
    let mut wrong_kind = video_budget_step(&store, &key_id, &video_parent, "begin-chat-kind-wrong-budget");
    wrong_kind.kind = aiwork_core::BudgetStepKind::Chat;
    let balance_before = store.key_quota_balance_as_admin(&admin, &key_id, "credits").unwrap();
    assert!(store.begin_budget_operation(&video_parent, wrong_kind).is_err());
    assert!(store.budget_operation(&video_parent).unwrap().is_none());
    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    let (operations, steps, reservations, request_state): (i64, i64, i64, String) = connection
        .query_row(
            "SELECT
               (SELECT COUNT(*) FROM budget_operations WHERE parent_request_id = ?1),
               (SELECT COUNT(*) FROM budget_steps WHERE request_id = ?1),
               (SELECT COUNT(*) FROM quota_reservations WHERE request_id = ?1),
               (SELECT state FROM requests WHERE id = ?1)",
            [&video_parent],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!((operations, steps, reservations, request_state.as_str()), (0, 0, 0, "received"));
    let balance_after = store.key_quota_balance_as_admin(&admin, &key_id, "credits").unwrap();
    assert_eq!(balance_after, balance_before);

    let chat_parent = begin_chat_parent(&store, &key_id, "begin-chat-kind-chat-parent");
    let chat_step = chat_budget_step(&store, &key_id, &chat_parent, "begin-chat-kind-chat-budget");
    let operation = store.begin_budget_operation(&chat_parent, chat_step).unwrap();
    assert_eq!(operation.parent_request_id, chat_parent);
    assert!(store.budget_operation(&operation.parent_request_id).unwrap().is_some());
}

#[test]
fn v2_settlements_remain_in_admin_views_after_later_receipt_conflict() {
    let (directory, store, key_id, admin) = budget_fixture("v2-summary-after-conflict", 2, 10_000_000);
    let parent = begin_video_parent(&store, &key_id, "v2-summary-parent");
    let mut step = video_budget_step(&store, &key_id, &parent, "v2-summary-budget");
    step.authorization.hold_credits = CreditAmount::parse("2", "credits").unwrap();
    store.begin_budget_operation(&parent, step).unwrap();
    dispatch_current_budget_step(&store, &parent).unwrap();
    store.bind_budget_video_task(&parent, "v2-summary-task").unwrap();
    store
        .mark_budget_step_execution(&parent, aiwork_core::BudgetExecutionState::Succeeded)
        .unwrap();
    let mut final_receipt = final_budget_receipt(&parent, "v2-summary-budget", "0.75");
    final_receipt.receipt.task_ref = Some("v2-summary-task".into());
    assert!(matches!(
        store.apply_budget_receipt(final_receipt).unwrap(),
        aiwork_core::BudgetReceiptResult::Settled { .. }
    ));
    store
        .finish_budget_execution(&parent, aiwork_core::BudgetExecutionState::Succeeded)
        .unwrap();
    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    let settled_at_ms: i64 = connection
        .query_row(
            "SELECT settled_at_ms FROM budget_settlements WHERE request_id = ?1",
            [&parent],
            |row| row.get(0),
        )
        .unwrap();

    store
        .mark_budget_receipt_conflict(aiwork_core::BudgetReceiptConflict {
            request_id: parent.clone(),
            budget_id: "v2-summary-budget".into(),
            account_ref: "account-test".into(),
            bridge_instance_id: "bridge-test".into(),
            source_ref: "v2-summary-later-conflict".into(),
            evidence_hash: "v2-summary-conflict-evidence".into(),
            observed_at_ms: settled_at_ms + 3_600_000,
        })
        .unwrap();
    connection
        .execute(
            "UPDATE budget_steps SET updated_at_ms = ?1 WHERE request_id = ?2",
            params![settled_at_ms + 3_600_000, parent],
        )
        .unwrap();

    let summary = store.admin_summary(settled_at_ms + 1).unwrap();
    assert_eq!(summary.verified_spent_credits.to_string(), "0.750000");
    assert_eq!(summary.verified_spent_today_credits.to_string(), "0.750000");
    let key = store
        .list_api_keys_as_admin(&admin, Some("budget-user"))
        .unwrap()
        .into_iter()
        .find(|view| view.id == key_id)
        .unwrap();
    assert_eq!(key.verified_credit_spent, 750_000);

    let start = settled_at_ms - 3_600_000;
    let trend = store
        .usage_trend(start, start + 4 * 3_600_000, 3_600_000, Some(&key_id))
        .unwrap();
    assert_eq!(trend.len(), 4);
    assert_eq!(trend[0].credits.to_string(), "0.000000");
    assert_eq!(trend[1].credits.to_string(), "0.750000");
    assert_eq!(trend[2].credits.to_string(), "0.000000");
    assert_eq!(trend[3].credits.to_string(), "0.000000");
}

#[test]
fn explicit_receipt_conflict_is_owned_idempotent_and_preserves_settlement() {
    let (directory, store, key_id, admin) = budget_fixture("explicit-receipt-conflict", 2, 1_000_000_000);
    let parent_request_id = begin_video_parent(&store, &key_id, "explicit-receipt-conflict-parent");
    store
        .begin_budget_operation(
            &parent_request_id,
            video_budget_step(&store, &key_id, &parent_request_id, "explicit-receipt-conflict-budget"),
        )
        .unwrap();
    dispatch_current_budget_step(&store, &parent_request_id).unwrap();
    store.bind_budget_video_task(&parent_request_id, "explicit-conflict-task").unwrap();
    store
        .mark_budget_step_execution(&parent_request_id, aiwork_core::BudgetExecutionState::Succeeded)
        .unwrap();
    let mut receipt = final_budget_receipt(&parent_request_id, "explicit-receipt-conflict-budget", "1");
    receipt.receipt.task_ref = Some("explicit-conflict-task".into());
    receipt.receipt.source_ref = "explicit-conflict-settlement".into();
    assert!(matches!(
        store.apply_budget_receipt(receipt).unwrap(),
        aiwork_core::BudgetReceiptResult::Settled { .. }
    ));
    let balance_before = store.key_quota_balance_as_admin(&admin, &key_id, "credits").unwrap();
    let conflict = aiwork_core::BudgetReceiptConflict {
        request_id: parent_request_id.clone(),
        budget_id: "explicit-receipt-conflict-budget".into(),
        account_ref: "account-test".into(),
        bridge_instance_id: "bridge-test".into(),
        source_ref: "explicit-conflicting-source".into(),
        evidence_hash: "explicit-conflict-evidence-hash".into(),
        observed_at_ms: chrono::Utc::now().timestamp_millis(),
    };
    assert_eq!(store.mark_budget_receipt_conflict(conflict.clone()).unwrap(), aiwork_core::BudgetMutation::Applied);
    let mut replay = conflict.clone();
    replay.observed_at_ms += 1;
    assert_eq!(store.mark_budget_receipt_conflict(replay).unwrap(), aiwork_core::BudgetMutation::Duplicate);

    let mut wrong_owner = conflict.clone();
    wrong_owner.account_ref = "other-account".into();
    wrong_owner.evidence_hash = "wrong-owner-evidence".into();
    assert!(store.mark_budget_receipt_conflict(wrong_owner).is_err());

    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    let (financial_state, actual, reservation_state, settlement_actual, conflict_evidence, block_count):
        (String, i64, String, i64, i64, i64) = connection
        .query_row(
            "SELECT step.financial_state, step.actual_microcredits, reservation.state,
                    settlement.actual_microcredits,
                    (SELECT COUNT(*) FROM budget_receipt_evidence WHERE request_id = step.request_id AND record_kind = 'conflict'),
                    (SELECT COUNT(*) FROM api_key_billing_blocks WHERE key_id = step.core_key_id)
             FROM budget_steps step
             JOIN quota_reservations reservation ON reservation.id = step.reservation_id
             JOIN budget_settlements settlement ON settlement.request_id = step.request_id
             WHERE step.request_id = ?1",
            [&parent_request_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
        )
        .unwrap();
    assert_eq!(
        (financial_state.as_str(), actual, reservation_state.as_str(), settlement_actual, conflict_evidence, block_count),
        ("conflict", 1_000_000, "committed", 1_000_000, 1, 1)
    );
    let balance_after = store.key_quota_balance_as_admin(&admin, &key_id, "credits").unwrap();
    assert_eq!(balance_after, balance_before);

    let next_parent = begin_video_parent(&store, &key_id, "explicit-conflict-blocks-new-operation");
    assert!(store
        .begin_budget_operation(
            &next_parent,
            video_budget_step(&store, &key_id, &next_parent, "explicit-conflict-blocked-budget"),
        )
        .is_err());
    assert!(store.reservation_for_request(&next_parent).unwrap().is_none());
}

#[test]
fn legacy_settle_request_rejects_v2_holds_before_state_or_replay_changes() {
    let (directory, store, key_id, _) = budget_fixture("legacy-settle-v2-guard", 2, 1_000_000_000);
    let held_parent = begin_video_parent(&store, &key_id, "legacy-settle-held-parent");
    let unknown_parent = begin_video_parent(&store, &key_id, "legacy-settle-unknown-parent");
    for (parent_request_id, budget_id) in [
        (&held_parent, "legacy-settle-held-budget"),
        (&unknown_parent, "legacy-settle-unknown-budget"),
    ] {
        store
            .begin_budget_operation(
                parent_request_id,
                video_budget_step(&store, &key_id, parent_request_id, budget_id),
            )
            .unwrap();
        dispatch_current_budget_step(&store, parent_request_id).unwrap();
    }
    let principal = aiwork_core::Principal {
        user_id: "budget-user".into(),
        key_id: key_id.clone(),
        scopes: BTreeSet::from(["video:submit".into()]),
    };
    let held_reservation = store.reservation_for_request(&held_parent).unwrap().unwrap();
    assert!(store
        .settle_request(
            &principal,
            &held_reservation.id,
            aiwork_core::Settlement::Release,
            aiwork_core::RequestState::Succeeded,
            None,
        )
        .is_err());

    let mut pending = final_budget_receipt(&unknown_parent, "legacy-settle-unknown-budget", "0");
    pending.receipt.status = BillingReceiptStatus::Unknown;
    pending.receipt.actual_credits = None;
    pending.receipt.task_ref = None;
    pending.receipt.source_ref = "legacy-settle-pending".into();
    assert_eq!(store.apply_budget_receipt(pending).unwrap(), aiwork_core::BudgetReceiptResult::Pending);
    let unknown_reservation = store.reservation_for_request(&unknown_parent).unwrap().unwrap();
    assert!(store
        .settle_request(
            &principal,
            &unknown_reservation.id,
            aiwork_core::Settlement::Unknown,
            aiwork_core::RequestState::Succeeded,
            None,
        )
        .is_err());

    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    for (request_id, expected_reservation_state, expected_financial_state) in [
        (&held_parent, "held", "held"),
        (&unknown_parent, "unknown", "unknown"),
    ] {
        let (request_state, reservation_state, financial_state, commit_or_release): (String, String, String, i64) = connection
            .query_row(
                "SELECT request.state, reservation.state, step.financial_state,
                        (SELECT COUNT(*) FROM quota_ledger ledger
                         WHERE ledger.request_id = request.id AND ledger.event_kind IN ('commit','release'))
                 FROM requests request
                 JOIN quota_reservations reservation ON reservation.request_id = request.id
                 JOIN budget_steps step ON step.request_id = request.id
                 WHERE request.id = ?1",
                [request_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!((request_state.as_str(), reservation_state.as_str(), financial_state.as_str(), commit_or_release),
            ("dispatched", expected_reservation_state, expected_financial_state, 0));
    }
}

#[test]
fn legacy_lease_recovery_excludes_an_accidental_v2_lease_binding() {
    let (directory, store, key_id, _) = budget_fixture("legacy-lease-v2-exclusion", 1, 1_000_000_000);
    let parent_request_id = begin_video_parent(&store, &key_id, "legacy-lease-v2-parent");
    store
        .begin_budget_operation(
            &parent_request_id,
            video_budget_step(&store, &key_id, &parent_request_id, "legacy-lease-v2-budget"),
        )
        .unwrap();
    dispatch_current_budget_step(&store, &parent_request_id).unwrap();
    let now = chrono::Utc::now().timestamp_millis() + 60_000;
    let database = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    database.execute(
        "INSERT INTO upstream_accounts
         (id, provider, credentials_ref, region, capabilities_json, enabled, max_concurrency,
          state, cooldown_until_ms, cooldown_reason, consecutive_errors, created_at_ms, updated_at_ms)
         VALUES ('fixture-upstream', 'fixture', 'vault://fixture', NULL, '[]', 1, 1,
                 'available', NULL, NULL, 0, ?1, ?1)",
        [now],
    ).unwrap();
    database.execute(
        "INSERT INTO upstream_leases
         (id, request_id, account_ref, resource_kind, predicted_units, observation_id, state,
          lease_expires_at_ms, reconcile_until_ms, upstream_request_ref, error_kind,
          created_at_ms, updated_at_ms, settled_at_ms)
         VALUES ('accidental-v2-lease', ?1, 'fixture-upstream', 'credits', 1, NULL, 'held',
                 ?2, NULL, NULL, NULL, ?3, ?3, NULL)",
        params![&parent_request_id, now - 1, now - 10],
    ).unwrap();
    drop(database);

    assert!(store.recover_expired_upstream_leases(now).unwrap().is_empty());
    assert!(store.list_recoverable_leases().unwrap().is_empty());
    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    let (lease_state, reservation_state, financial_state, execution_state, request_state): (String, String, String, String, String) = connection
        .query_row(
            "SELECT lease.state, reservation.state, step.financial_state, step.execution_state, request.state
             FROM upstream_leases lease
             JOIN quota_reservations reservation ON reservation.request_id = lease.request_id
             JOIN budget_steps step ON step.request_id = lease.request_id
             JOIN requests request ON request.id = lease.request_id
             WHERE lease.id = 'accidental-v2-lease'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .unwrap();
    assert_eq!((lease_state.as_str(), reservation_state.as_str(), financial_state.as_str(), execution_state.as_str(), request_state.as_str()),
        ("held", "held", "held", "running", "dispatched"));
}

#[test]
fn dispatch_requires_ready_key_and_user_cap_but_inflight_can_settle() {
    let (directory, store, key_id, admin) = budget_fixture("dispatch-budget-readiness", 1, 1_000_000_000);
    store
        .quota_pool_grant_as_admin(
            &admin,
            aiwork_core::QuotaGrant {
                user_id: "budget-user".into(),
                resource_kind: "credits".into(),
                amount: 1_000_000_000,
                actor_user_id: "budget-admin".into(),
                reason: "dispatch readiness fixture".into(),
            },
        )
        .unwrap();
    let parent_request_id = begin_video_parent(&store, &key_id, "dispatch-budget-readiness-parent");
    let budget_id = "dispatch-budget-readiness-budget";
    store
        .begin_budget_operation(
            &parent_request_id,
            video_budget_step(&store, &key_id, &parent_request_id, budget_id),
        )
        .unwrap();
    let reservation = store.reservation_for_request(&parent_request_id).unwrap().unwrap();
    let key_account_id = reservation.key_budget_account_id.as_deref().unwrap();
    let user_cap_account_id = reservation.user_cap_account_id.as_deref().unwrap();
    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();

    connection.execute("UPDATE quota_budget_accounts SET enabled = 0 WHERE id = ?1", [key_account_id]).unwrap();
    assert!(dispatch_current_budget_step(&store, &parent_request_id).is_err());
    connection.execute("UPDATE quota_budget_accounts SET enabled = 1 WHERE id = ?1", [key_account_id]).unwrap();
    connection.execute("UPDATE quota_budget_accounts SET migration_state = 'reconcile_required' WHERE id = ?1", [key_account_id]).unwrap();
    assert!(dispatch_current_budget_step(&store, &parent_request_id).is_err());
    connection.execute("UPDATE quota_budget_accounts SET migration_state = 'ready' WHERE id = ?1", [key_account_id]).unwrap();

    connection.execute("UPDATE quota_budget_accounts SET enabled = 0 WHERE id = ?1", [user_cap_account_id]).unwrap();
    assert!(dispatch_current_budget_step(&store, &parent_request_id).is_err());
    connection.execute("UPDATE quota_budget_accounts SET enabled = 1 WHERE id = ?1", [user_cap_account_id]).unwrap();
    connection.execute("UPDATE quota_budget_accounts SET migration_state = 'reconcile_required' WHERE id = ?1", [user_cap_account_id]).unwrap();
    assert!(dispatch_current_budget_step(&store, &parent_request_id).is_err());
    connection.execute("UPDATE quota_budget_accounts SET migration_state = 'ready' WHERE id = ?1", [user_cap_account_id]).unwrap();

    let (attempted, execution_state, request_state): (i64, String, String) = connection
        .query_row(
            "SELECT step.dispatch_attempted, step.execution_state, request.state
             FROM budget_steps step JOIN requests request ON request.id = step.request_id
             WHERE step.request_id = ?1",
            [&parent_request_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!((attempted, execution_state.as_str(), request_state.as_str()), (0, "ready", "reserved"));

    dispatch_current_budget_step(&store, &parent_request_id).unwrap();
    store.bind_budget_video_task(&parent_request_id, "dispatch-readiness-task").unwrap();
    connection.execute("UPDATE quota_budget_accounts SET enabled = 0 WHERE id = ?1", [key_account_id]).unwrap();
    connection.execute("UPDATE quota_budget_accounts SET migration_state = 'reconcile_required' WHERE id = ?1", [user_cap_account_id]).unwrap();
    assert_eq!(
        store.mark_budget_step_execution(&parent_request_id, aiwork_core::BudgetExecutionState::Succeeded).unwrap(),
        aiwork_core::BudgetMutation::Applied
    );
    let mut receipt = final_budget_receipt(&parent_request_id, budget_id, "1");
    receipt.receipt.task_ref = Some("dispatch-readiness-task".into());
    assert!(matches!(store.apply_budget_receipt(receipt).unwrap(), aiwork_core::BudgetReceiptResult::Settled { .. }));
}

#[test]
fn unattempted_assist_release_is_idempotent_and_keeps_parent_slot() {
    let (directory, store, key_id, admin) = budget_fixture("assist-unattempted-release", 1, 1_000_000_000);
    store
        .quota_pool_grant_as_admin(
            &admin,
            aiwork_core::QuotaGrant {
                user_id: "budget-user".into(),
                resource_kind: "credits".into(),
                amount: 1_000_000_000,
                actor_user_id: "budget-admin".into(),
                reason: "assist release fixture".into(),
            },
        )
        .unwrap();
    let parent_request_id = begin_video_parent(&store, &key_id, "assist-unattempted-release-parent");
    let child = match store
        .begin_budget_assist_request(&parent_request_id, budget_assist_input(&key_id, "assist-unattempted-release-child"))
        .unwrap()
    {
        BeginRequest::Created(child) => child,
        other => panic!("expected a new helper request, got {other:?}"),
    };
    store
        .begin_budget_operation(
            &parent_request_id,
            assist_budget_step(&store, &key_id, &parent_request_id, &child.id, "assist-unattempted-release-budget"),
        )
        .unwrap();
    assert_eq!(
        store.release_unattempted_budget_step(&child.id, "preparation canceled").unwrap(),
        aiwork_core::BudgetMutation::Applied
    );
    assert_eq!(
        store.release_unattempted_budget_step(&child.id, "preparation canceled").unwrap(),
        aiwork_core::BudgetMutation::Duplicate
    );
    assert!(store.release_unattempted_budget_step(&child.id, "different decision").is_err());
    assert_eq!(store.active_execution_count_for_key(&key_id).unwrap(), 1);
    assert_eq!(store.request_state(&child.id).unwrap(), aiwork_core::RequestState::Canceled);
    let balance = store.key_quota_balance_as_admin(&admin, &key_id, "credits").unwrap();
    assert_eq!((balance.available, balance.held), (1_000_000_000, 0));

    let operation = store
        .add_budget_step(
            &parent_request_id,
            video_budget_step(&store, &key_id, &parent_request_id, "assist-release-next-video-budget"),
        )
        .unwrap();
    assert_eq!(operation.steps.len(), 2);
    assert_eq!(operation.steps.iter().find(|step| step.request_id == child.id).unwrap().financial_state,
        aiwork_core::BudgetFinancialState::Released);
    assert_eq!(operation.steps.iter().find(|step| step.request_id == child.id).unwrap().execution_state,
        aiwork_core::BudgetExecutionState::Canceled);
    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    let events: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM quota_ledger WHERE request_id = ?1 AND event_kind = 'release'",
            [&child.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(events, 2);
}

#[test]
fn attempted_budget_step_cannot_be_released() {
    let (directory, store, key_id, _) = budget_fixture("attempted-release-rejected", 1, 1_000_000_000);
    let parent_request_id = begin_video_parent(&store, &key_id, "attempted-release-rejected-parent");
    store
        .begin_budget_operation(
            &parent_request_id,
            video_budget_step(&store, &key_id, &parent_request_id, "attempted-release-rejected-budget"),
        )
        .unwrap();
    dispatch_current_budget_step(&store, &parent_request_id).unwrap();
    store
        .mark_budget_step_execution(&parent_request_id, aiwork_core::BudgetExecutionState::Failed)
        .unwrap();
    assert!(store.release_unattempted_budget_step(&parent_request_id, "late cancellation").is_err());

    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    let (attempted, financial_state, execution_state, reservation_state, releases): (i64, String, String, String, i64) = connection
        .query_row(
            "SELECT step.dispatch_attempted, step.financial_state, step.execution_state,
                    reservation.state,
                    (SELECT COUNT(*) FROM quota_ledger WHERE request_id = step.request_id AND event_kind = 'release')
             FROM budget_steps step
             JOIN quota_reservations reservation ON reservation.id = step.reservation_id
             WHERE step.request_id = ?1",
            [&parent_request_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .unwrap();
    assert_eq!((attempted, financial_state.as_str(), execution_state.as_str(), reservation_state.as_str(), releases),
        (1, "held", "failed", "held", 0));
}

#[test]
fn unattempted_terminal_assist_steps_can_be_released_after_parent_finish() {
    for (suffix, terminal) in [
        ("failed", aiwork_core::BudgetExecutionState::Failed),
        ("canceled", aiwork_core::BudgetExecutionState::Canceled),
    ] {
        let (directory, store, key_id, admin) =
            budget_fixture(&format!("terminal-assist-release-{suffix}"), 1, 1_000_000_000);
        store
            .quota_pool_grant_as_admin(
                &admin,
                aiwork_core::QuotaGrant {
                    user_id: "budget-user".into(),
                    resource_kind: "credits".into(),
                    amount: 1_000_000_000,
                    actor_user_id: "budget-admin".into(),
                    reason: "terminal assist release fixture".into(),
                },
            )
            .unwrap();
        let parent_request_id =
            begin_video_parent(&store, &key_id, &format!("terminal-assist-{suffix}-parent"));
        let child = match store
            .begin_budget_assist_request(
                &parent_request_id,
                budget_assist_input(&key_id, &format!("terminal-assist-{suffix}-child")),
            )
            .unwrap()
        {
            BeginRequest::Created(child) => child,
            other => panic!("expected a new helper request, got {other:?}"),
        };
        store
            .begin_budget_operation(
                &parent_request_id,
                assist_budget_step(
                    &store,
                    &key_id,
                    &parent_request_id,
                    &child.id,
                    &format!("terminal-assist-{suffix}-budget"),
                ),
            )
            .unwrap();

        assert_eq!(
            store.mark_budget_step_execution(&child.id, terminal).unwrap(),
            aiwork_core::BudgetMutation::Applied
        );
        assert_eq!(
            store.finish_budget_execution(&parent_request_id, terminal).unwrap(),
            aiwork_core::BudgetMutation::Applied
        );
        assert_eq!(
            store
                .release_unattempted_budget_step(&child.id, "unattempted terminal")
                .unwrap(),
            aiwork_core::BudgetMutation::Applied
        );
        assert_eq!(
            store
                .release_unattempted_budget_step(&child.id, "unattempted terminal")
                .unwrap(),
            aiwork_core::BudgetMutation::Duplicate
        );
        assert!(store
            .release_unattempted_budget_step(&child.id, "different reason")
            .is_err());

        let operation = store.budget_operation(&parent_request_id).unwrap().unwrap();
        assert_eq!(operation.execution_state, terminal);
        let step = operation.steps.iter().find(|step| step.request_id == child.id).unwrap();
        assert_eq!(step.execution_state, terminal);
        assert_eq!(step.financial_state, aiwork_core::BudgetFinancialState::Released);
        let expected_request_state = match terminal {
            aiwork_core::BudgetExecutionState::Failed => aiwork_core::RequestState::Failed,
            aiwork_core::BudgetExecutionState::Canceled => aiwork_core::RequestState::Canceled,
            _ => unreachable!(),
        };
        assert_eq!(store.request_state(&child.id).unwrap(), expected_request_state);
        assert_eq!(store.request_state(&parent_request_id).unwrap(), expected_request_state);
        let balance = store.key_quota_balance_as_admin(&admin, &key_id, "credits").unwrap();
        assert_eq!((balance.available, balance.held), (1_000_000_000, 0));

        let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
        let (attempted, reservation_state, release_events): (i64, String, i64) = connection
            .query_row(
                "SELECT step.dispatch_attempted, reservation.state,
                        (SELECT COUNT(*) FROM quota_ledger WHERE request_id = step.request_id AND event_kind = 'release')
                 FROM budget_steps step JOIN quota_reservations reservation ON reservation.id = step.reservation_id
                 WHERE step.request_id = ?1",
                [&child.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!((attempted, reservation_state.as_str(), release_events), (0, "released", 2));
    }
}

#[test]
fn pending_budget_steps_are_stable_read_only_and_include_revoked_keys() {
    let (directory, store, key_id, _) = budget_fixture("pending-budget-steps", 2, 1_000_000_000);
    let running_parent = begin_video_parent(&store, &key_id, "pending-budget-running-parent");
    let ready_parent = begin_video_parent(&store, &key_id, "pending-budget-ready-parent");
    store
        .begin_budget_operation(
            &running_parent,
            video_budget_step(&store, &key_id, &running_parent, "pending-budget-running"),
        )
        .unwrap();
    store
        .begin_budget_operation(
            &ready_parent,
            video_budget_step(&store, &key_id, &ready_parent, "pending-budget-ready"),
        )
        .unwrap();
    dispatch_current_budget_step(&store, &running_parent).unwrap();
    store
        .mark_budget_step_execution(&running_parent, aiwork_core::BudgetExecutionState::Unknown)
        .unwrap();
    let mut pending = final_budget_receipt(&running_parent, "pending-budget-running", "0");
    pending.receipt.status = BillingReceiptStatus::Unknown;
    pending.receipt.actual_credits = None;
    pending.receipt.source_ref = "pending-budget-source".into();
    assert_eq!(store.apply_budget_receipt(pending).unwrap(), aiwork_core::BudgetReceiptResult::Pending);

    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    connection.execute(
        "UPDATE api_keys SET status = 'revoked', revoked_at_ms = ?1 WHERE id = ?2",
        params![chrono::Utc::now().timestamp_millis(), &key_id],
    ).unwrap();
    let expected: Vec<String> = connection
        .prepare(
            "SELECT step.request_id FROM budget_steps step
             WHERE step.financial_state IN ('held','unknown','conflict')
                OR step.execution_state IN ('ready','running','unknown')
             ORDER BY step.updated_at_ms, step.request_id LIMIT 2",
        )
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let before_attempts: Vec<(String, i64)> = connection
        .prepare("SELECT request_id, dispatch_attempted FROM budget_steps ORDER BY request_id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    let recovery = store.pending_budget_steps(2).unwrap();
    assert_eq!(recovery.len(), 2);
    assert_eq!(recovery.iter().map(|view| view.step.request_id.clone()).collect::<Vec<_>>(), expected);
    assert!(recovery.iter().any(|view| view.step.request_id == ready_parent && !view.step.dispatch_attempted));
    assert!(recovery.iter().any(|view| view.step.request_id == running_parent
        && view.step.execution_state == aiwork_core::BudgetExecutionState::Unknown
        && view.step.financial_state == aiwork_core::BudgetFinancialState::Unknown));
    assert_eq!(store.pending_budget_steps(1).unwrap().len(), 1);
    assert!(store.pending_budget_steps(0).is_err());
    assert!(store.pending_budget_steps(101).is_err());
    let after_attempts: Vec<(String, i64)> = connection
        .prepare("SELECT request_id, dispatch_attempted FROM budget_steps ORDER BY request_id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(before_attempts, after_attempts);
}

#[test]
fn v2_final_receipt_can_exceed_hold_and_observed_time_retry_is_duplicate() {
    let (directory, store, key_id, admin) = budget_fixture("receipt-over-hold", 1, 1_000_000_000);
    let parent_request_id = begin_video_parent(&store, &key_id, "receipt-over-hold-parent");
    let budget_id = "receipt-over-hold-budget";
    store
        .begin_budget_operation(
            &parent_request_id,
            video_budget_step(&store, &key_id, &parent_request_id, budget_id),
        )
        .unwrap();
    dispatch_current_budget_step(&store, &parent_request_id).unwrap();
    store.bind_budget_video_task(&parent_request_id, "task-receipt-video-1").unwrap();
    let input = final_budget_receipt(&parent_request_id, budget_id, "3");

    let result = store.apply_budget_receipt(input.clone()).unwrap();
    assert_eq!(
        result,
        aiwork_core::BudgetReceiptResult::Settled {
            actual_credits: CreditAmount::parse("3", "credits").unwrap(),
            released_microcredits: -1_000_000,
            debt: false,
        }
    );
    let mut duplicate = input;
    duplicate.receipt.observed_at_ms += 1_000;
    assert_eq!(
        store.apply_budget_receipt(duplicate).unwrap(),
        aiwork_core::BudgetReceiptResult::Duplicate
    );
    let balance = store.key_quota_balance_as_admin(&admin, &key_id, "credits").unwrap();
    assert_eq!((balance.available, balance.held), (997_000_000, 0));
    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    let settlement: (i64, i64, i64) = connection
        .query_row(
            "SELECT actual_microcredits, released_microcredits, debt
             FROM budget_settlements WHERE request_id = ?1",
            [&parent_request_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(settlement, (3_000_000, -1_000_000, 0));
}

#[test]
fn v25_to_v27_upgrade_backfills_revision_one_without_changing_financial_facts_or_inventing_finish_time() {
    let (directory, store, key_id, _) = budget_fixture("v25-to-v27", 2, 20_000_000);
    let database = directory.0.join("data").join(CORE_DB_FILE);
    let connection = Connection::open(&database).unwrap();
    connection
        .execute(
            "INSERT INTO quota_budget_accounts
             (id, scope, user_id, api_key_id, resource_kind, enabled, version,
              migration_state, created_at_ms, updated_at_ms)
             VALUES ('v26-user-cap', 'user_cap', 'budget-user', NULL, 'credits', 1, 1, 'ready', 1, 1)",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO quota_ledger
             (entry_id, user_id, resource_kind, event_kind, amount, delta, actor_user_id,
              reason, created_at_ms, budget_account_id, event_group_id, budget_version)
             VALUES ('v26-user-cap-grant', 'budget-user', 'credits', 'adjust', 20000000, 20000000,
                     'budget-admin', 'migration fixture', 1, 'v26-user-cap', 'v26-user-cap-grant', 1)",
            [],
        )
        .unwrap();
    drop(connection);

    let held_parent = begin_video_parent(&store, &key_id, "v25-held-parent");
    store
        .begin_budget_operation(
            &held_parent,
            video_budget_step(&store, &key_id, &held_parent, "v25-held-budget"),
        )
        .unwrap();

    let settled_parent = begin_video_parent(&store, &key_id, "v25-settled-parent");
    store
        .begin_budget_operation(
            &settled_parent,
            video_budget_step(&store, &key_id, &settled_parent, "v25-settled-budget"),
        )
        .unwrap();
    dispatch_current_budget_step(&store, &settled_parent).unwrap();
    store.bind_budget_video_task(&settled_parent, "v25-settled-task").unwrap();
    store
        .mark_budget_step_execution(&settled_parent, aiwork_core::BudgetExecutionState::Succeeded)
        .unwrap();
    let mut receipt = final_budget_receipt(&settled_parent, "v25-settled-budget", "1");
    receipt.receipt.task_ref = Some("v25-settled-task".into());
    store.apply_budget_receipt(receipt).unwrap();
    store
        .finish_budget_execution(&settled_parent, aiwork_core::BudgetExecutionState::Succeeded)
        .unwrap();

    let connection = Connection::open(&database).unwrap();
    connection
        .execute("UPDATE budget_steps SET created_at_ms = 0 WHERE request_id = ?1", [&held_parent])
        .unwrap();
    let reservations_before: Vec<(String, String, i64, String, i64)> = connection
        .prepare("SELECT id, request_id, amount, state, expires_at_ms FROM quota_reservations ORDER BY id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let ledger_before: Vec<(Option<String>, Option<String>, Option<String>, String, i64, i64, Option<i64>)> = connection
        .prepare(
            "SELECT request_id, event_group_id, budget_account_id, event_kind, amount, delta, budget_version
             FROM quota_ledger ORDER BY entry_id",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let account_balances_before: Vec<(String, i64, i64, i64)> = connection
        .prepare(
            "SELECT account.id, account.version,
                    COALESCE((SELECT SUM(delta) FROM quota_ledger WHERE budget_account_id = account.id), 0),
                    COALESCE((SELECT SUM(amount) FROM quota_reservations
                              WHERE (key_budget_account_id = account.id OR user_cap_account_id = account.id)
                                AND state IN ('held','unknown')), 0)
             FROM quota_budget_accounts account ORDER BY account.id",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    drop(connection);

    let connection = Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "DROP INDEX IF EXISTS quota_ledger_authorization_adjust_unique;
             DROP TABLE IF EXISTS legacy_execution_evidence;
             DROP TABLE IF EXISTS budget_authorization_revisions;",
        )
        .unwrap();
    for (table, column) in [
        ("budget_operations", "execution_finished_at_ms"),
        ("quota_ledger", "authorization_revision"),
    ] {
        let exists: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2)",
                rusqlite::params![table, column],
                |row| row.get(0),
            )
            .unwrap();
        if exists {
            connection
                .execute_batch(&format!("ALTER TABLE {table} DROP COLUMN {column};"))
                .unwrap();
        }
    }
    connection
        .execute("UPDATE schema_meta SET value = '25' WHERE key = 'schema_version'", [])
        .unwrap();
    drop(connection);
    drop(store);

    let upgraded = CoreStore::open(&directory.0).unwrap();
    upgraded.migrate().unwrap();
    assert_eq!(upgraded.schema_version().unwrap(), aiwork_core::CURRENT_SCHEMA_VERSION);
    let connection = Connection::open(&database).unwrap();
    let step_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM budget_steps", [], |row| row.get(0))
        .unwrap();
    let history_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM budget_authorization_revisions", [], |row| row.get(0))
        .unwrap();
    assert_eq!(history_count, step_count);
    let invalid_history_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM budget_authorization_revisions history
             JOIN budget_steps step USING(request_id)
             WHERE history.local_revision <> 1 OR history.budget_id <> step.budget_id
                OR history.authorization_hash <> step.authorization_hash
                OR history.hold_microcredits <> step.hold_microcredits
                OR history.previous_budget_id IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(invalid_history_count, 0);
    let migrated_adopted_at: i64 = connection
        .query_row(
            "SELECT adopted_at_ms FROM budget_authorization_revisions WHERE request_id = ?1 AND local_revision = 1",
            [&held_parent],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(migrated_adopted_at, 0, "migration must preserve valid V25 zero timestamps");
    let terminal_timestamp: Option<i64> = connection
        .query_row(
            "SELECT execution_finished_at_ms FROM budget_operations WHERE parent_request_id = ?1",
            [&settled_parent],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(terminal_timestamp, None);
    let tagged_event_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM quota_ledger WHERE request_id IN (?1, ?2)
               AND event_kind IN ('reserve','release','commit') AND authorization_revision = 1",
            rusqlite::params![held_parent, settled_parent],
            |row| row.get(0),
        )
        .unwrap();
    assert!(tagged_event_count >= 4, "both key and user-cap V2 rows must be tagged");
    let reservations_after: Vec<(String, String, i64, String, i64)> = connection
        .prepare("SELECT id, request_id, amount, state, expires_at_ms FROM quota_reservations ORDER BY id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let ledger_after: Vec<(Option<String>, Option<String>, Option<String>, String, i64, i64, Option<i64>)> = connection
        .prepare(
            "SELECT request_id, event_group_id, budget_account_id, event_kind, amount, delta, budget_version
             FROM quota_ledger ORDER BY entry_id",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let account_balances_after: Vec<(String, i64, i64, i64)> = connection
        .prepare(
            "SELECT account.id, account.version,
                    COALESCE((SELECT SUM(delta) FROM quota_ledger WHERE budget_account_id = account.id), 0),
                    COALESCE((SELECT SUM(amount) FROM quota_reservations
                              WHERE (key_budget_account_id = account.id OR user_cap_account_id = account.id)
                                AND state IN ('held','unknown')), 0)
             FROM quota_budget_accounts account ORDER BY account.id",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(reservations_after, reservations_before);
    assert_eq!(ledger_after, ledger_before);
    assert_eq!(account_balances_after, account_balances_before);
    drop(connection);

    drop(upgraded);
    let reopened = CoreStore::open(&directory.0).unwrap();
    reopened.migrate().unwrap();
    let connection = Connection::open(&database).unwrap();
    let reopened_history_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM budget_authorization_revisions", [], |row| row.get(0))
        .unwrap();
    assert_eq!(reopened_history_count, step_count);
}

#[test]
fn v2_terminal_time_is_written_once_and_not_moved_by_replay_or_late_receipt() {
    let (directory, store, key_id, _) = budget_fixture("execution-finished-at", 1, 20_000_000);
    let parent_request_id = begin_video_parent(&store, &key_id, "execution-finished-at-parent");
    store
        .begin_budget_operation(
            &parent_request_id,
            video_budget_step(&store, &key_id, &parent_request_id, "execution-finished-at-budget"),
        )
        .unwrap();
    dispatch_current_budget_step(&store, &parent_request_id).unwrap();
    store.bind_budget_video_task(&parent_request_id, "execution-finished-at-task").unwrap();
    store
        .mark_budget_step_execution(&parent_request_id, aiwork_core::BudgetExecutionState::Succeeded)
        .unwrap();
    assert_eq!(
        store.finish_budget_execution(&parent_request_id, aiwork_core::BudgetExecutionState::Succeeded).unwrap(),
        aiwork_core::BudgetMutation::Applied,
    );
    let connection = Connection::open(directory.0.join("data").join(CORE_DB_FILE)).unwrap();
    let first_finished_at: Option<i64> = connection
        .query_row(
            "SELECT execution_finished_at_ms FROM budget_operations WHERE parent_request_id = ?1",
            [&parent_request_id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(first_finished_at.is_some_and(|value| value > 0));

    assert_eq!(
        store.finish_budget_execution(&parent_request_id, aiwork_core::BudgetExecutionState::Succeeded).unwrap(),
        aiwork_core::BudgetMutation::Duplicate,
    );
    let mut receipt = final_budget_receipt(&parent_request_id, "execution-finished-at-budget", "1");
    receipt.receipt.task_ref = Some("execution-finished-at-task".into());
    store.apply_budget_receipt(receipt).unwrap();
    let final_finished_at: Option<i64> = connection
        .query_row(
            "SELECT execution_finished_at_ms FROM budget_operations WHERE parent_request_id = ?1",
            [&parent_request_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(final_finished_at, first_finished_at);
}
