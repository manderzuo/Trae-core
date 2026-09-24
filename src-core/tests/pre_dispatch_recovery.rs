use std::{collections::BTreeSet, fs};

use aiwork_core::{BeginRequest, BeginRequestInput, CoreStore, NewUser, RequestState, UserRole};
use rusqlite::{params, Connection};
use serde_json::json;

#[test]
fn startup_recovers_only_old_unreserved_unrelated_requests_before_dispatch() {
    let dir = std::env::temp_dir().join(format!("pre-dispatch-recovery-{}", rand::random::<u64>()));
    fs::create_dir_all(&dir).unwrap();
    let store = CoreStore::open(&dir).unwrap();
    store.migrate().unwrap();
    store.create_user(NewUser { id: "admin".into(), name: "Admin".into(), role: UserRole::Admin }, "bootstrap").unwrap();
    store.create_user(NewUser { id: "user".into(), name: "User".into(), role: UserRole::User }, "admin").unwrap();
    let key = store.issue_api_key("user", "test", BTreeSet::new(), "admin").unwrap();
    let begin = |idempotency: &str| {
        match store.begin_billed_request(BeginRequestInput {
            user_id: "user".into(), api_key_id: key.id.clone(), protocol: "openai".into(),
            endpoint: "chat".into(), model: "test".into(), idempotency_key: idempotency.into(),
            body: json!({"model":"test","messages":[]}),
        }).unwrap() {
            BeginRequest::Created(request) => request.id,
            _ => panic!("expected new request"),
        }
    };
    let old_received = begin("old-received");
    let old_validating = begin("old-validating");
    let recent = begin("recent");
    let protected = begin("protected");
    store.transition_request(&old_validating, RequestState::Received, RequestState::Validating, None).unwrap();

    let database = dir.join("data").join(aiwork_core::CORE_DB_FILE);
    let connection = Connection::open(&database).unwrap();
    connection.execute(
        "UPDATE requests SET created_at_ms = 1, updated_at_ms = 1 WHERE id IN (?1, ?2, ?3)",
        params![old_received, old_validating, protected],
    ).unwrap();
    connection.execute(
        "INSERT INTO quota_budget_accounts
         (id, scope, user_id, api_key_id, resource_kind, enabled, version, migration_state, created_at_ms, updated_at_ms)
         VALUES ('protected-budget', 'key', 'user', ?1, 'credits', 1, 1, 'ready', 1, 1)",
        [&key.id],
    ).unwrap();
    connection.execute(
        "INSERT INTO quota_reservations
         (id, user_id, request_id, resource_kind, amount, state, expires_at_ms, created_at_ms,
          api_key_id, key_budget_account_id, user_cap_account_id, event_group_id)
         VALUES ('protected-reservation', 'user', ?1, 'credits', 1, 'held', 9999999999999, 1,
                 ?2, 'protected-budget', NULL, NULL)",
        params![protected, key.id],
    ).unwrap();

    assert_eq!(store.recover_abandoned_pre_dispatch_requests().unwrap(), 2);
    assert_eq!(store.request_state(&old_received).unwrap(), RequestState::Failed);
    assert_eq!(store.request_state(&old_validating).unwrap(), RequestState::Failed);
    assert_eq!(store.request_state(&recent).unwrap(), RequestState::Received);
    assert_eq!(store.request_state(&protected).unwrap(), RequestState::Received);
    let error_code: String = connection.query_row(
        "SELECT error_code FROM requests WHERE id = ?1", [&old_received], |row| row.get(0),
    ).unwrap();
    assert_eq!(error_code, "abandoned_pre_dispatch_recovered");
    let audited: i64 = connection.query_row(
        "SELECT COUNT(*) FROM audit_events WHERE action = 'request.abandoned_pre_dispatch_recovered'", [], |row| row.get(0),
    ).unwrap();
    assert_eq!(audited, 2);
    assert_eq!(store.recover_abandoned_pre_dispatch_requests().unwrap(), 0);

    drop(connection);
    drop(store);
    fs::remove_dir_all(dir).unwrap();
}
