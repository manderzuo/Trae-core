use std::{collections::BTreeSet, fs, path::PathBuf};

use aiwork_core::{
    BeginRequest, BeginRequestInput, BillingReceipt, BillingReceiptStatus, ControlledStepKind,
    ControlledStepResult, CoreStore, CreditAmount, KeyQuotaGrant, NewUser, Principal,
    UpstreamCreditSnapshot, UserRole,
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
    let settled = store.finish_controlled_operation(&parent, true).unwrap();
    assert_eq!(settled.actual_credits.as_microcredits(), 45_750_000);
    let balance = store
        .key_quota_balance_as_admin(&admin, &key, "credits")
        .unwrap();
    assert_eq!(
        (balance.available, balance.held, balance.settled),
        (54_250_000, 0, 45_750_000)
    );

    store
        .record_controlled_step(&parent, &child, ControlledStepKind::Assist, assist)
        .unwrap();
    store
        .record_controlled_step(&parent, &parent, ControlledStepKind::Video, video)
        .unwrap();
    assert_eq!(
        store
            .finish_controlled_operation(&parent, true)
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
            .finish_controlled_operation(&parent, false)
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
        .finish_controlled_operation(&parent, false)
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
    let settled = store.finish_controlled_operation(&parent, true).unwrap();
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
    assert!(store.finish_controlled_operation(&parent, true).is_err());
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
    assert!(store.finish_controlled_operation(&parent, true).is_err());
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
            .finish_controlled_operation(&parent, true)
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
    store.finish_controlled_operation(&first, true).unwrap();
    store.finish_controlled_operation(&second, true).unwrap();
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
