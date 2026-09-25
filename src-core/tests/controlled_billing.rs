use std::{
    collections::BTreeSet,
    fs,
    path::PathBuf,
    sync::{Arc, Barrier},
    thread,
};

use aiwork_core::{
    BeginRequest, BeginRequestInput, BillingQuote, BillingReceipt, BillingReceiptStatus,
    BillingReservationResult, ControlledStepKind, ControlledStepResult, CoreError, CoreStore,
    CreditAmount, KeyQuotaGrant, NewUser, Principal, UpstreamCreditSnapshot, UserRole,
};
use serde_json::json;

fn fixture(label: &str) -> (CoreStore, PathBuf, String, Principal) {
    let dir =
        std::env::temp_dir().join(format!("core-controlled-{label}-{}", rand::random::<u64>()));
    fs::create_dir_all(&dir).unwrap();
    let store = CoreStore::open(&dir).unwrap();
    store.migrate().unwrap();
    store
        .create_user(
            NewUser {
                id: "admin".into(),
                name: "Admin".into(),
                role: UserRole::Admin,
            },
            "bootstrap",
        )
        .unwrap();
    store
        .create_user(
            NewUser {
                id: "user".into(),
                name: "User".into(),
                role: UserRole::User,
            },
            "admin",
        )
        .unwrap();
    let admin_key = store
        .issue_api_key(
            "admin",
            "admin",
            BTreeSet::from(["admin:*".into()]),
            "bootstrap",
        )
        .unwrap();
    let admin = store.authenticate_api_key(&admin_key.plaintext).unwrap();
    let key = store
        .issue_api_key_as_admin_with_max_concurrency(
            "user",
            "video",
            BTreeSet::from(["chat:invoke".into(), "video:submit".into()]),
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
                actor_user_id: "ignored".into(),
                reason: "controlled billing test".into(),
            },
        )
        .unwrap();
    (store, dir, key.id, admin)
}

fn request(store: &CoreStore, key_id: &str, model: &str, idempotency: &str) -> String {
    let result = store
        .begin_billed_request(BeginRequestInput {
            user_id: "user".into(),
            api_key_id: key_id.into(),
            protocol: "openai".into(),
            endpoint: "/v1/chat/completions".into(),
            model: model.into(),
            idempotency_key: idempotency.into(),
            body: json!({"model": model, "messages": [{"role":"user","content":"cat"}]}),
        })
        .unwrap();
    match result {
        BeginRequest::Created(handle) => handle.id,
        other => panic!("unexpected request: {other:?}"),
    }
}

fn billing_quote(store: &CoreStore, request_id: &str, model: &str, quote_id: &str) -> BillingQuote {
    BillingQuote {
        request_id: request_id.into(),
        quote_id: quote_id.into(),
        request_fingerprint: store.request_fingerprint_for_billing(request_id).unwrap(),
        endpoint: "/v1/chat/completions".into(),
        model: model.into(),
        max_credits: CreditAmount::parse("25", "credits").unwrap(),
        unit: "credits".into(),
        expires_at_ms: chrono::Utc::now().timestamp_millis() + 60_000,
        source_ref: format!("aiwork-test-quote:{quote_id}"),
    }
}

fn snapshot() -> UpstreamCreditSnapshot {
    UpstreamCreditSnapshot {
        total: CreditAmount::parse("1000", "credits").unwrap(),
        updated_at_ms: chrono::Utc::now().timestamp_millis(),
    }
}

fn receipt(
    request_id: &str,
    status: BillingReceiptStatus,
    credits: Option<&str>,
    task_ref: Option<&str>,
) -> BillingReceipt {
    BillingReceipt {
        request_id: request_id.into(),
        status,
        actual_credits: credits.map(|value| CreditAmount::parse(value, "credits").unwrap()),
        unit: "credits".into(),
        source_ref: format!("trae-usage-session:{request_id}"),
        task_ref: task_ref.map(str::to_owned),
        observed_at_ms: chrono::Utc::now().timestamp_millis(),
    }
}

#[test]
fn controlled_assist_and_video_commit_once() {
    let (store, dir, key, admin) = fixture("both-steps");
    let parent = request(&store, &key, "seedance", "both-parent");
    let child = request(&store, &key, "deepseek-v4-flash", "both-child");
    store.link_seedance_assist_request(&parent, &child).unwrap();

    let operation = store
        .begin_controlled_operation(&parent, snapshot())
        .unwrap();
    assert_eq!(operation.held.as_microcredits(), 100_000_000);
    let held = store
        .key_quota_balance_as_admin(&admin, &key, "credits")
        .unwrap();
    assert_eq!((held.available, held.held), (0, 100_000_000));

    let assist = receipt(&child, BillingReceiptStatus::Final, Some("0.5"), None);
    let video = receipt(
        &parent,
        BillingReceiptStatus::Final,
        Some("45.25"),
        Some("video-1"),
    );
    assert_eq!(
        store
            .record_controlled_step(&parent, &child, ControlledStepKind::Assist, assist.clone())
            .unwrap(),
        ControlledStepResult::Verified
    );
    assert_eq!(
        store
            .record_controlled_step(&parent, &parent, ControlledStepKind::Video, video.clone())
            .unwrap(),
        ControlledStepResult::Verified
    );
    let settled = store.finish_controlled_operation(&parent, Some(true)).unwrap();
    assert_eq!(settled.actual_credits.as_microcredits(), 45_750_000);
    let balance = store
        .key_quota_balance_as_admin(&admin, &key, "credits")
        .unwrap();
    assert_eq!(
        (balance.available, balance.held, balance.settled),
        (54_250_000, 0, 45_750_000)
    );
    let key_view = store
        .list_api_keys_as_admin(&admin, None)
        .unwrap()
        .into_iter()
        .find(|item| item.id == key)
        .unwrap();
    assert_eq!(key_view.verified_credit_spent, 45_750_000);
    let now = chrono::Utc::now().timestamp_millis();
    let summary = store.admin_summary(now).unwrap();
    assert_eq!(summary.verified_spent_credits.as_microcredits(), 45_750_000);
    assert_eq!(summary.verified_spent_today_credits.as_microcredits(), 45_750_000);
    let trend = store.usage_trend(now - 60_000, now + 60_000, 120_000, Some(&key)).unwrap();
    assert_eq!(trend.len(), 1);
    assert_eq!(trend[0].credits.as_microcredits(), 45_750_000);

    store
        .record_controlled_step(&parent, &child, ControlledStepKind::Assist, assist)
        .unwrap();
    store
        .record_controlled_step(&parent, &parent, ControlledStepKind::Video, video)
        .unwrap();
    assert_eq!(
        store
            .finish_controlled_operation(&parent, Some(true))
            .unwrap()
            .actual_credits
            .as_microcredits(),
        45_750_000
    );
    assert_eq!(
        store
            .key_quota_balance_as_admin(&admin, &key, "credits")
            .unwrap()
            .settled,
        45_750_000
    );
    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn controlled_assist_only_failure_commits_assist() {
    let (store, dir, key, admin) = fixture("assist-only");
    let parent = request(&store, &key, "seedance", "assist-only-parent");
    let child = request(&store, &key, "deepseek-v4-flash", "assist-only-child");
    store.link_seedance_assist_request(&parent, &child).unwrap();
    store
        .begin_controlled_operation(&parent, snapshot())
        .unwrap();
    store
        .record_controlled_step(
            &parent,
            &child,
            ControlledStepKind::Assist,
            receipt(&child, BillingReceiptStatus::Final, Some("0.5"), None),
        )
        .unwrap();
    assert_eq!(
        store
            .finish_controlled_operation(&parent, None)
            .unwrap()
            .actual_credits
            .as_microcredits(),
        500_000
    );
    let balance = store
        .key_quota_balance_as_admin(&admin, &key, "credits")
        .unwrap();
    assert_eq!(
        (balance.available, balance.held, balance.settled),
        (99_500_000, 0, 500_000)
    );
    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn controlled_unknown_receipt_survives_restart() {
    let (store, dir, key, admin) = fixture("unknown-restart");
    let parent = request(&store, &key, "seedance", "unknown-parent");
    let child = request(&store, &key, "deepseek-v4-flash", "unknown-child");
    store.link_seedance_assist_request(&parent, &child).unwrap();
    store
        .begin_controlled_operation(&parent, snapshot())
        .unwrap();
    assert_eq!(
        store
            .record_controlled_step(
                &parent,
                &child,
                ControlledStepKind::Assist,
                receipt(&child, BillingReceiptStatus::Unknown, None, None)
            )
            .unwrap(),
        ControlledStepResult::Held
    );
    drop(store);

    let reopened = CoreStore::open(&dir).unwrap();
    reopened.migrate().unwrap();
    assert!(reopened
        .finish_controlled_operation(&parent, None)
        .is_err());
    let balance = reopened
        .key_quota_balance_as_admin(&admin, &key, "credits")
        .unwrap();
    assert_eq!(
        (balance.available, balance.held, balance.settled),
        (0, 100_000_000, 0)
    );
    drop(reopened);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn controlled_same_key_cannot_start_second_operation() {
    let (store, dir, key, _) = fixture("same-key");
    let first = request(&store, &key, "seedance", "same-key-first");
    let second = request(&store, &key, "seedance", "same-key-second");
    store
        .begin_controlled_operation(&first, snapshot())
        .unwrap();
    assert!(store
        .begin_controlled_operation(&second, snapshot())
        .is_err());
    assert_eq!(
        store
            .begin_controlled_operation(&first, snapshot())
            .unwrap()
            .held
            .as_microcredits(),
        100_000_000
    );
    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn concurrent_controlled_starts_for_same_key_allow_only_one_hold() {
    let (store, dir, key, admin) = fixture("same-key-concurrent");
    let first = request(&store, &key, "seedance", "same-key-concurrent-first");
    let second = request(&store, &key, "seedance", "same-key-concurrent-second");
    let first_store = Arc::new(store);
    let second_store = Arc::new(CoreStore::open(&dir).unwrap());
    let barrier = Arc::new(Barrier::new(3));

    let first_thread = {
        let store = Arc::clone(&first_store);
        let barrier = Arc::clone(&barrier);
        thread::spawn(move || {
            barrier.wait();
            store.begin_controlled_operation(&first, snapshot())
        })
    };
    let second_thread = {
        let store = Arc::clone(&second_store);
        let barrier = Arc::clone(&barrier);
        thread::spawn(move || {
            barrier.wait();
            store.begin_controlled_operation(&second, snapshot())
        })
    };
    barrier.wait();
    let results = [first_thread.join().unwrap(), second_thread.join().unwrap()];

    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(CoreError::ApiKeyBillingBlocked { .. })))
            .count(),
        1,
        "the losing parent must be rejected because this Key already has an active operation"
    );
    let balance = first_store
        .key_quota_balance_as_admin(&admin, &key, "credits")
        .unwrap();
    assert_eq!(
        (balance.available, balance.held, balance.settled),
        (0, 100_000_000, 0),
        "only one full-Key hold may exist after the race"
    );

    drop(second_store);
    drop(first_store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn accepted_video_task_can_be_bound_durably_before_job_snapshot() {
    let (store, dir, key, _) = fixture("task-ref-recovery");
    let parent = request(&store, &key, "seedance", "task-ref-parent");
    store.begin_controlled_operation(&parent, snapshot()).unwrap();
    store.mark_controlled_step_dispatched(&parent, &parent, ControlledStepKind::Video).unwrap();
    store.bind_controlled_video_task(&parent, "video-bound").unwrap();
    assert!(store.bind_controlled_video_task(&parent, "video-other").is_err());
    drop(store);
    let reopened = CoreStore::open(&dir).unwrap();
    reopened.migrate().unwrap();
    let step = reopened.recoverable_controlled_steps(10).unwrap().remove(0);
    assert_eq!(step.task_ref.as_deref(), Some("video-bound"));
    assert_eq!(step.api_key_id, key);
    assert_eq!(step.user_id, "user");
    assert!(!step.hold_id.is_empty());
    drop(reopened);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn restart_inventory_releases_undispatched_hold_and_settles_assist_only_receipt() {
    let (store, dir, key, admin) = fixture("orphan-recovery");
    let parent = request(&store, &key, "seedance", "orphan-parent");
    let child = request(&store, &key, "deepseek-v4-flash", "orphan-child");
    store.begin_controlled_operation(&parent, snapshot()).unwrap();
    store.link_seedance_assist_request(&parent, &child).unwrap();
    let cutoff = chrono::Utc::now().timestamp_millis() + 1;
    assert_eq!(store.recoverable_undispatched_operations_before(cutoff).unwrap(), vec![parent.clone()]);
    store.finish_controlled_operation(&parent, None).unwrap();
    let balance = store.key_quota_balance_as_admin(&admin, &key, "credits").unwrap();
    assert_eq!((balance.available, balance.held, balance.settled), (100_000_000, 0, 0));

    let second = request(&store, &key, "seedance", "orphan-second-parent");
    let second_child = request(&store, &key, "deepseek-v4-flash", "orphan-second-child");
    store.begin_controlled_operation(&second, snapshot()).unwrap();
    store.link_seedance_assist_request(&second, &second_child).unwrap();
    store.mark_controlled_step_dispatched(&second, &second_child, ControlledStepKind::Assist).unwrap();
    assert!(store.recoverable_undispatched_operations_before(cutoff + 1000).unwrap().is_empty());
    store.record_controlled_step(&second, &second_child, ControlledStepKind::Assist,
        receipt(&second_child, BillingReceiptStatus::Final, Some("0.25"), None)).unwrap();
    assert_eq!(store.recoverable_undispatched_operations_before(cutoff + 1000).unwrap(), vec![second.clone()]);
    store.finish_controlled_operation(&second, None).unwrap();
    let balance = store.key_quota_balance_as_admin(&admin, &key, "credits").unwrap();
    assert_eq!((balance.available, balance.held, balance.settled), (99_750_000, 0, 250_000));
    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn controlled_overrun_records_true_cost_and_blocks_key() {
    let (store, dir, key, admin) = fixture("overrun");
    let parent = request(&store, &key, "seedance", "overrun-parent");
    store
        .begin_controlled_operation(&parent, snapshot())
        .unwrap();
    store
        .record_controlled_step(
            &parent,
            &parent,
            ControlledStepKind::Video,
            receipt(
                &parent,
                BillingReceiptStatus::Final,
                Some("120"),
                Some("video-overrun"),
            ),
        )
        .unwrap();
    let settled = store.finish_controlled_operation(&parent, Some(true)).unwrap();
    assert!(settled.over_authorized_hold);
    assert_eq!(settled.actual_credits.as_microcredits(), 120_000_000);
    let balance = store
        .key_quota_balance_as_admin(&admin, &key, "credits")
        .unwrap();
    assert_eq!(
        (balance.available, balance.held, balance.settled),
        (-20_000_000, 0, 120_000_000)
    );
    let next = request(&store, &key, "seedance", "overrun-next");
    assert!(store.begin_controlled_operation(&next, snapshot()).is_err());
    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn charged_failed_video_debits_real_receipt_without_marking_request_successful() {
    let (store, dir, key, admin) = fixture("charged-failure");
    let parent = request(&store, &key, "seedance", "charged-failure-parent");
    store.begin_controlled_operation(&parent, snapshot()).unwrap();
    store.mark_controlled_step_dispatched(&parent, &parent, ControlledStepKind::Video).unwrap();
    store.record_controlled_step(&parent, &parent, ControlledStepKind::Video,
        receipt(&parent, BillingReceiptStatus::Final, Some("3.5"), Some("video-failed"))).unwrap();
    store.finish_controlled_operation(&parent, Some(false)).unwrap();
    assert_eq!(store.request_state(&parent).unwrap(), aiwork_core::RequestState::Settled);
    let db = rusqlite::Connection::open(dir.join("data").join(aiwork_core::CORE_DB_FILE)).unwrap();
    let outcome: (Option<i64>, Option<String>) = db.query_row(
        "SELECT result_status, error_code FROM requests WHERE id = ?1", [&parent],
        |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
    assert_eq!(outcome, (Some(502), Some("video_generation_failed".into())));
    drop(db);
    let balance = store.key_quota_balance_as_admin(&admin, &key, "credits").unwrap();
    assert_eq!((balance.available, balance.held, balance.settled), (96_500_000, 0, 3_500_000));
    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn controlled_conflicting_receipt_preserves_hold_and_blocks_settlement() {
    let (store, dir, key, admin) = fixture("conflict");
    let parent = request(&store, &key, "seedance", "conflict-parent");
    store
        .begin_controlled_operation(&parent, snapshot())
        .unwrap();
    store
        .record_controlled_step(
            &parent,
            &parent,
            ControlledStepKind::Video,
            receipt(
                &parent,
                BillingReceiptStatus::Final,
                Some("25"),
                Some("video-conflict"),
            ),
        )
        .unwrap();
    assert_eq!(
        store
            .record_controlled_step(
                &parent,
                &parent,
                ControlledStepKind::Video,
                receipt(
                    &parent,
                    BillingReceiptStatus::Final,
                    Some("26"),
                    Some("video-conflict")
                )
            )
            .unwrap(),
        ControlledStepResult::Conflict
    );
    assert!(store.finish_controlled_operation(&parent, Some(true)).is_err());
    let balance = store
        .key_quota_balance_as_admin(&admin, &key, "credits")
        .unwrap();
    assert_eq!(
        (balance.available, balance.held, balance.settled),
        (0, 100_000_000, 0)
    );
    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn controlled_unknown_can_be_resolved_without_second_charge() {
    let (store, dir, key, admin) = fixture("unknown-resolve");
    let parent = request(&store, &key, "seedance", "unknown-resolve-parent");
    store
        .begin_controlled_operation(&parent, snapshot())
        .unwrap();
    assert_eq!(
        store
            .record_controlled_step(
                &parent,
                &parent,
                ControlledStepKind::Video,
                receipt(
                    &parent,
                    BillingReceiptStatus::Unknown,
                    None,
                    Some("video-resolve")
                )
            )
            .unwrap(),
        ControlledStepResult::Held
    );
    assert!(store.finish_controlled_operation(&parent, Some(true)).is_err());
    assert_eq!(
        store
            .record_controlled_step(
                &parent,
                &parent,
                ControlledStepKind::Video,
                receipt(
                    &parent,
                    BillingReceiptStatus::Final,
                    Some("30"),
                    Some("video-resolve")
                )
            )
            .unwrap(),
        ControlledStepResult::Verified
    );
    assert_eq!(
        store
            .finish_controlled_operation(&parent, Some(true))
            .unwrap()
            .actual_credits
            .as_microcredits(),
        30_000_000
    );
    assert_eq!(
        store
            .key_quota_balance_as_admin(&admin, &key, "credits")
            .unwrap()
            .settled,
        30_000_000
    );
    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn controlled_different_keys_keep_independent_holds_and_receipts() {
    let (store, dir, first_key, admin) = fixture("two-keys");
    let second_key = store
        .issue_api_key_as_admin_with_max_concurrency(
            "user",
            "second-video",
            BTreeSet::from(["chat:invoke".into(), "video:submit".into()]),
            3,
            &admin,
        )
        .unwrap();
    store
        .key_quota_grant_as_admin(
            &admin,
            KeyQuotaGrant {
                api_key_id: second_key.id.clone(),
                resource_kind: "credits".into(),
                amount: 80_000_000,
                actor_user_id: "ignored".into(),
                reason: "second controlled key".into(),
            },
        )
        .unwrap();
    let first = request(&store, &first_key, "seedance", "two-keys-first");
    let second = request(&store, &second_key.id, "seedance", "two-keys-second");
    store
        .begin_controlled_operation(&first, snapshot())
        .unwrap();
    assert_eq!(
        store
            .begin_controlled_operation(&second, snapshot())
            .unwrap()
            .held
            .as_microcredits(),
        80_000_000
    );
    let wrong = store.record_controlled_step(
        &first,
        &second,
        ControlledStepKind::Assist,
        receipt(&second, BillingReceiptStatus::Final, Some("2"), None),
    );
    assert!(wrong.is_err());
    store
        .record_controlled_step(
            &first,
            &first,
            ControlledStepKind::Video,
            receipt(
                &first,
                BillingReceiptStatus::Final,
                Some("25"),
                Some("video-first"),
            ),
        )
        .unwrap();
    store
        .record_controlled_step(
            &second,
            &second,
            ControlledStepKind::Video,
            receipt(
                &second,
                BillingReceiptStatus::Final,
                Some("35"),
                Some("video-second"),
            ),
        )
        .unwrap();
    store.finish_controlled_operation(&first, Some(true)).unwrap();
    store.finish_controlled_operation(&second, Some(true)).unwrap();
    assert_eq!(
        store
            .key_quota_balance_as_admin(&admin, &first_key, "credits")
            .unwrap()
            .settled,
        25_000_000
    );
    assert_eq!(
        store
            .key_quota_balance_as_admin(&admin, &second_key.id, "credits")
            .unwrap()
            .settled,
        35_000_000
    );
    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn different_keys_can_settle_in_parallel_without_cross_attributing_receipts() {
    let (store, dir, first_key, admin) = fixture("two-keys-concurrent");
    let second_key = store
        .issue_api_key_as_admin_with_max_concurrency(
            "user",
            "second-video",
            BTreeSet::from(["chat:invoke".into(), "video:submit".into()]),
            3,
            &admin,
        )
        .unwrap();
    store
        .key_quota_grant_as_admin(
            &admin,
            KeyQuotaGrant {
                api_key_id: second_key.id.clone(),
                resource_kind: "credits".into(),
                amount: 80_000_000,
                actor_user_id: "ignored".into(),
                reason: "second concurrent controlled key".into(),
            },
        )
        .unwrap();
    let first = request(&store, &first_key, "seedance", "two-keys-concurrent-first");
    let second = request(&store, &second_key.id, "seedance", "two-keys-concurrent-second");
    let first_store = Arc::new(store);
    let second_store = Arc::new(CoreStore::open(&dir).unwrap());

    let start_barrier = Arc::new(Barrier::new(3));
    let first_parent = first.clone();
    let first_thread = {
        let store = Arc::clone(&first_store);
        let barrier = Arc::clone(&start_barrier);
        thread::spawn(move || {
            barrier.wait();
            store.begin_controlled_operation(&first_parent, snapshot())
        })
    };
    let second_parent = second.clone();
    let second_thread = {
        let store = Arc::clone(&second_store);
        let barrier = Arc::clone(&start_barrier);
        thread::spawn(move || {
            barrier.wait();
            store.begin_controlled_operation(&second_parent, snapshot())
        })
    };
    start_barrier.wait();
    assert_eq!(first_thread.join().unwrap().unwrap().held.as_microcredits(), 100_000_000);
    assert_eq!(second_thread.join().unwrap().unwrap().held.as_microcredits(), 80_000_000);

    first_store
        .mark_controlled_step_dispatched(&first, &first, ControlledStepKind::Video)
        .unwrap();
    second_store
        .mark_controlled_step_dispatched(&second, &second, ControlledStepKind::Video)
        .unwrap();
    let receipt_barrier = Arc::new(Barrier::new(3));
    let first_receipt = receipt(&first, BillingReceiptStatus::Final, Some("25"), Some("video-first"));
    let first_receipt_parent = first.clone();
    let first_receipt_step = first.clone();
    let first_thread = {
        let store = Arc::clone(&first_store);
        let barrier = Arc::clone(&receipt_barrier);
        thread::spawn(move || {
            barrier.wait();
            store.record_controlled_step(
                &first_receipt_parent,
                &first_receipt_step,
                ControlledStepKind::Video,
                first_receipt,
            )
        })
    };
    let second_receipt = receipt(&second, BillingReceiptStatus::Final, Some("35"), Some("video-second"));
    let second_receipt_parent = second.clone();
    let second_receipt_step = second.clone();
    let second_thread = {
        let store = Arc::clone(&second_store);
        let barrier = Arc::clone(&receipt_barrier);
        thread::spawn(move || {
            barrier.wait();
            store.record_controlled_step(
                &second_receipt_parent,
                &second_receipt_step,
                ControlledStepKind::Video,
                second_receipt,
            )
        })
    };
    receipt_barrier.wait();
    assert_eq!(first_thread.join().unwrap().unwrap(), ControlledStepResult::Verified);
    assert_eq!(second_thread.join().unwrap().unwrap(), ControlledStepResult::Verified);

    let settle_barrier = Arc::new(Barrier::new(3));
    let first_parent = first.clone();
    let first_thread = {
        let store = Arc::clone(&first_store);
        let barrier = Arc::clone(&settle_barrier);
        thread::spawn(move || {
            barrier.wait();
            store.finish_controlled_operation(&first_parent, Some(true))
        })
    };
    let second_parent = second.clone();
    let second_thread = {
        let store = Arc::clone(&second_store);
        let barrier = Arc::clone(&settle_barrier);
        thread::spawn(move || {
            barrier.wait();
            store.finish_controlled_operation(&second_parent, Some(true))
        })
    };
    settle_barrier.wait();
    assert_eq!(first_thread.join().unwrap().unwrap().actual_credits.as_microcredits(), 25_000_000);
    assert_eq!(second_thread.join().unwrap().unwrap().actual_credits.as_microcredits(), 35_000_000);

    let first_balance = first_store
        .key_quota_balance_as_admin(&admin, &first_key, "credits")
        .unwrap();
    let second_balance = second_store
        .key_quota_balance_as_admin(&admin, &second_key.id, "credits")
        .unwrap();
    assert_eq!((first_balance.available, first_balance.held, first_balance.settled), (75_000_000, 0, 25_000_000));
    assert_eq!((second_balance.available, second_balance.held, second_balance.settled), (45_000_000, 0, 35_000_000));

    drop(second_store);
    drop(first_store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn concurrent_duplicate_controlled_receipts_settle_only_once() {
    let (store, dir, key, admin) = fixture("receipt-race");
    let parent = request(&store, &key, "seedance", "receipt-race-parent");
    store.begin_controlled_operation(&parent, snapshot()).unwrap();
    store
        .mark_controlled_step_dispatched(&parent, &parent, ControlledStepKind::Video)
        .unwrap();
    let first_store = Arc::new(store);
    let second_store = Arc::new(CoreStore::open(&dir).unwrap());
    let barrier = Arc::new(Barrier::new(3));
    let receipt_value = receipt(
        &parent,
        BillingReceiptStatus::Final,
        Some("25"),
        Some("video-receipt-race"),
    );
    let first_thread = {
        let store = Arc::clone(&first_store);
        let barrier = Arc::clone(&barrier);
        let parent = parent.clone();
        let receipt_value = receipt_value.clone();
        thread::spawn(move || {
            barrier.wait();
            store.record_controlled_step(
                &parent,
                &parent,
                ControlledStepKind::Video,
                receipt_value,
            )
        })
    };
    let second_thread = {
        let store = Arc::clone(&second_store);
        let barrier = Arc::clone(&barrier);
        let parent = parent.clone();
        thread::spawn(move || {
            barrier.wait();
            store.record_controlled_step(
                &parent,
                &parent,
                ControlledStepKind::Video,
                receipt_value,
            )
        })
    };
    barrier.wait();
    let results = [first_thread.join().unwrap().unwrap(), second_thread.join().unwrap().unwrap()];
    assert_eq!(
        results.iter().filter(|result| **result == ControlledStepResult::Verified).count(),
        1
    );
    assert_eq!(
        results.iter().filter(|result| **result == ControlledStepResult::Duplicate).count(),
        1
    );
    assert_eq!(
        first_store
            .finish_controlled_operation(&parent, Some(true))
            .unwrap()
            .actual_credits
            .as_microcredits(),
        25_000_000
    );
    let balance = first_store
        .key_quota_balance_as_admin(&admin, &key, "credits")
        .unwrap();
    assert_eq!((balance.available, balance.held, balance.settled), (75_000_000, 0, 25_000_000));

    drop(second_store);
    drop(first_store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn concurrent_quote_reservations_admit_one_request_at_key_limit_one() {
    let (store, dir, _, admin) = fixture("key-concurrency-race");
    let limited_key = store
        .issue_api_key_as_admin_with_max_concurrency(
            "user",
            "one-at-a-time",
            BTreeSet::from(["chat:invoke".into(), "video:submit".into()]),
            1,
            &admin,
        )
        .unwrap();
    store
        .key_quota_grant_as_admin(
            &admin,
            KeyQuotaGrant {
                api_key_id: limited_key.id.clone(),
                resource_kind: "credits".into(),
                amount: 100_000_000,
                actor_user_id: "ignored".into(),
                reason: "parallel quote reservation test".into(),
            },
        )
        .unwrap();
    let first = request(&store, &limited_key.id, "seedance", "parallel-quote-first");
    let second = request(&store, &limited_key.id, "seedance", "parallel-quote-second");
    let first_quote = billing_quote(&store, &first, "seedance", "parallel-quote-first");
    let second_quote = billing_quote(&store, &second, "seedance", "parallel-quote-second");
    let first_store = Arc::new(store);
    let second_store = Arc::new(CoreStore::open(&dir).unwrap());
    let barrier = Arc::new(Barrier::new(3));
    let first_thread = {
        let store = Arc::clone(&first_store);
        let barrier = Arc::clone(&barrier);
        thread::spawn(move || {
            barrier.wait();
            store.reserve_credit_quote(first_quote)
        })
    };
    let second_thread = {
        let store = Arc::clone(&second_store);
        let barrier = Arc::clone(&barrier);
        thread::spawn(move || {
            barrier.wait();
            store.reserve_credit_quote(second_quote)
        })
    };
    barrier.wait();
    let results = [first_thread.join().unwrap(), second_thread.join().unwrap()];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| result.as_ref().err().is_some_and(|error| {
                matches!(error, CoreError::KeyConcurrencyExceeded { max_concurrency: 1, .. })
            }))
            .count(),
        1,
        "exactly one paid request should acquire the only concurrency slot"
    );
    let balance = first_store
        .key_quota_balance_as_admin(&admin, &limited_key.id, "credits")
        .unwrap();
    assert_eq!((balance.available, balance.held, balance.settled), (75_000_000, 25_000_000, 0));

    drop(second_store);
    drop(first_store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn seedance_assist_child_uses_parent_concurrency_slot() {
    let (store, dir, _, admin) = fixture("assist-single-slot");
    let key = store
        .issue_api_key_as_admin_with_max_concurrency(
            "user",
            "seedance-one-slot",
            BTreeSet::from(["chat:invoke".into(), "video:submit".into()]),
            1,
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
                actor_user_id: "ignored".into(),
                reason: "single Seedance operation slot".into(),
            },
        )
        .unwrap();
    let parent = request(&store, &key.id, "seedance", "assist-single-slot-parent");
    assert!(matches!(
        store.reserve_credit_quote(billing_quote(
            &store,
            &parent,
            "seedance",
            "assist-single-slot-parent-quote",
        )),
        Ok(BillingReservationResult::Created { .. })
    ));
    let child = match store
        .begin_seedance_assist_billed_request(
            &parent,
            BeginRequestInput {
                user_id: "user".into(),
                api_key_id: key.id.clone(),
                protocol: "openai".into(),
                endpoint: "/v1/chat/completions".into(),
                model: "deepseek-v4-flash".into(),
                idempotency_key: "assist-single-slot-child".into(),
                body: json!({"model":"deepseek-v4-flash","messages":[{"role":"user","content":"cat"}]}),
            },
        )
        .unwrap()
    {
        BeginRequest::Created(request) => request.id,
        other => panic!("expected a newly linked assist request, got {other:?}"),
    };

    let result = store.reserve_credit_quote(billing_quote(
        &store,
        &child,
        "deepseek-v4-flash",
        "assist-single-slot-quote",
    ));
    assert!(
        matches!(result, Ok(BillingReservationResult::Created { .. })),
        "the internal text-assist step belongs to its active video parent and must not consume a second Key slot; got {result:?}"
    );
    let balance = store
        .key_quota_balance_as_admin(&admin, &key.id, "credits")
        .unwrap();
    assert_eq!(
        (balance.available, balance.held, balance.settled),
        (50_000_000, 50_000_000, 0),
        "parent and helper both reserve their charges while sharing one concurrency slot"
    );

    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn controlled_rejects_stale_upstream_snapshot_before_hold() {
    let (store, dir, key, admin) = fixture("stale-snapshot");
    let parent = request(&store, &key, "seedance", "stale-parent");
    let stale = UpstreamCreditSnapshot {
        total: CreditAmount::parse("1000", "credits").unwrap(),
        updated_at_ms: chrono::Utc::now().timestamp_millis() - 600_000,
    };
    assert!(store.begin_controlled_operation(&parent, stale).is_err());
    let balance = store
        .key_quota_balance_as_admin(&admin, &key, "credits")
        .unwrap();
    assert_eq!(
        (balance.available, balance.held, balance.settled),
        (100_000_000, 0, 0)
    );
    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn controlled_dispatched_video_cannot_be_released_without_receipt() {
    let (store, dir, key, admin) = fixture("video-dispatch");
    let parent = request(&store, &key, "seedance", "video-dispatch-parent");
    store
        .begin_controlled_operation(&parent, snapshot())
        .unwrap();
    store
        .mark_controlled_step_dispatched(&parent, &parent, ControlledStepKind::Video)
        .unwrap();
    assert!(store.finish_controlled_operation(&parent, None).is_err());
    assert!(store.finish_controlled_operation(&parent, Some(true)).is_err());
    let held = store
        .key_quota_balance_as_admin(&admin, &key, "credits")
        .unwrap();
    assert_eq!(
        (held.available, held.held, held.settled),
        (0, 100_000_000, 0)
    );
    store
        .record_controlled_step(
            &parent,
            &parent,
            ControlledStepKind::Video,
            receipt(
                &parent,
                BillingReceiptStatus::Final,
                Some("33"),
                Some("video-dispatch"),
            ),
        )
        .unwrap();
    assert_eq!(
        store
            .finish_controlled_operation(&parent, Some(true))
            .unwrap()
            .actual_credits
            .as_microcredits(),
        33_000_000
    );
    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn controlled_restart_lists_pending_step_without_releasing_hold() {
    let (store, dir, key, admin) = fixture("recover-list");
    let parent = request(&store, &key, "seedance", "recover-parent");
    let child = request(&store, &key, "deepseek-v4-flash", "recover-child");
    store.link_seedance_assist_request(&parent, &child).unwrap();
    store
        .begin_controlled_operation(&parent, snapshot())
        .unwrap();
    store
        .mark_controlled_step_dispatched(&parent, &child, ControlledStepKind::Assist)
        .unwrap();
    drop(store);
    let reopened = CoreStore::open(&dir).unwrap();
    reopened.migrate().unwrap();
    assert!(reopened.controlled_operation_exists(&parent).unwrap());
    let steps = reopened.recoverable_controlled_steps(10).unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(
        (
            steps[0].parent_request_id.as_str(),
            steps[0].request_id.as_str()
        ),
        (parent.as_str(), child.as_str())
    );
    assert_eq!(steps[0].kind, ControlledStepKind::Assist);
    assert_eq!(steps[0].state, "pending");
    assert_eq!(
        reopened
            .key_quota_balance_as_admin(&admin, &key, "credits")
            .unwrap()
            .held,
        100_000_000
    );
    drop(reopened);
    fs::remove_dir_all(dir).unwrap();
}
