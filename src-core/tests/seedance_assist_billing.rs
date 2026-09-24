use std::{collections::BTreeSet, fs, path::PathBuf};

use aiwork_core::{
    BeginRequest, BeginRequestInput, CoreError, CoreStore, NewUser, RequestResult, RequestState,
    UserRole,
};
use rusqlite::Connection;
use serde_json::json;

fn test_store() -> (CoreStore, PathBuf, String, String) {
    let dir = std::env::temp_dir().join(format!("seedance-assist-billing-{}", rand::random::<u64>()));
    fs::create_dir_all(&dir).unwrap();
    let store = CoreStore::open(&dir).unwrap();
    store.migrate().unwrap();
    store.create_user(NewUser { id: "admin".into(), name: "Admin".into(), role: UserRole::Admin }, "bootstrap").unwrap();
    store.create_user(NewUser { id: "user".into(), name: "User".into(), role: UserRole::User }, "admin").unwrap();
    let first = store.issue_api_key("user", "first", BTreeSet::new(), "admin").unwrap();
    let second = store.issue_api_key("user", "second", BTreeSet::new(), "admin").unwrap();
    (store, dir, first.id, second.id)
}

fn request(store: &CoreStore, key_id: &str, endpoint: &str, model: &str, idempotency: &str) -> String {
    let result = store.begin_billed_request(BeginRequestInput {
        user_id: "user".into(),
        api_key_id: key_id.into(),
        protocol: "openai".into(),
        endpoint: endpoint.into(),
        model: model.into(),
        idempotency_key: idempotency.into(),
        body: json!({"model": model, "messages": []}),
    }).unwrap();
    match result {
        BeginRequest::Created(handle) => handle.id,
        _ => panic!("expected a new request"),
    }
}

#[test]
fn assistant_child_is_linked_to_the_same_key_and_linking_is_idempotent() {
    let (store, dir, key_a, key_b) = test_store();
    let parent = request(&store, &key_a, "videos", "seedance", "video-parent-a");
    let child = request(&store, &key_a, "chat", "deepseek-v4-flash", "assist-child-a");
    let other_key_child = request(&store, &key_b, "chat", "deepseek-v4-flash", "assist-child-b");

    store.link_seedance_assist_request(&parent, &child).unwrap();
    store.link_seedance_assist_request(&parent, &child).unwrap();
    assert_eq!(store.seedance_assist_request_for_parent(&parent).unwrap(), Some(child.clone()));
    assert!(matches!(
        store.link_seedance_assist_request(&parent, &other_key_child),
        Err(CoreError::InvalidRequestIdentity { .. })
    ));

    let duplicate_child = request(&store, &key_a, "chat", "deepseek-v4-flash", "assist-child-duplicate");
    assert!(matches!(
        store.link_seedance_assist_request(&parent, &duplicate_child),
        Err(CoreError::IdempotencyConflict)
    ));
    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn startup_recovery_fails_only_unreserved_pre_dispatch_assist_children_of_terminal_parents() {
    let (store, dir, key_a, _) = test_store();
    let stale_parent = request(&store, &key_a, "videos", "seedance", "stale-parent");
    let stale_child = request(&store, &key_a, "chat", "deepseek-v4-flash", "stale-child");
    let active_parent = request(&store, &key_a, "videos", "seedance", "active-parent");
    let active_child = request(&store, &key_a, "chat", "deepseek-v4-flash", "active-child");
    let dispatched_parent = request(&store, &key_a, "videos", "seedance", "dispatched-parent");
    let dispatched_child = request(&store, &key_a, "chat", "deepseek-v4-flash", "dispatched-child");
    let reserved_parent = request(&store, &key_a, "videos", "seedance", "reserved-parent");
    let reserved_child = request(&store, &key_a, "chat", "deepseek-v4-flash", "reserved-child");

    for (parent, child) in [
        (&stale_parent, &stale_child),
        (&active_parent, &active_child),
        (&dispatched_parent, &dispatched_child),
        (&reserved_parent, &reserved_child),
    ] {
        store.link_seedance_assist_request(parent, child).unwrap();
    }

    let mark_parent_terminal = |parent: &str| {
        store
            .transition_request(
                parent,
                RequestState::Received,
                RequestState::Validating,
                None,
            )
            .unwrap();
        store
            .transition_request(
                parent,
                RequestState::Validating,
                RequestState::Failed,
                Some(RequestResult {
                    status: Some(503),
                    error_code: Some("quote_unavailable".into()),
                }),
            )
            .unwrap();
        store
            .transition_request(parent, RequestState::Failed, RequestState::Settled, None)
            .unwrap();
    };
    mark_parent_terminal(&stale_parent);
    mark_parent_terminal(&dispatched_parent);
    mark_parent_terminal(&reserved_parent);

    store
        .transition_request(
            &dispatched_child,
            RequestState::Received,
            RequestState::Validating,
            None,
        )
        .unwrap();
    store
        .transition_request(
            &dispatched_child,
            RequestState::Validating,
            RequestState::Reserved,
            None,
        )
        .unwrap();
    store
        .transition_request(
            &dispatched_child,
            RequestState::Reserved,
            RequestState::Queued,
            None,
        )
        .unwrap();
    store
        .transition_request(
            &dispatched_child,
            RequestState::Queued,
            RequestState::Dispatched,
            None,
        )
        .unwrap();

    let database = dir.join("data").join(aiwork_core::CORE_DB_FILE);
    let connection = Connection::open(database).unwrap();
    connection
        .execute(
            "INSERT INTO quota_budget_accounts
             (id, scope, user_id, api_key_id, resource_kind, enabled, version, migration_state, created_at_ms, updated_at_ms)
             VALUES ('test-key-budget', 'key', 'user', ?1, 'credits', 1, 1, 'ready', 1, 1)",
            [&key_a],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO quota_reservations
             (id, user_id, request_id, resource_kind, amount, state, expires_at_ms, created_at_ms,
              api_key_id, key_budget_account_id, user_cap_account_id, event_group_id)
             VALUES ('test-held-reservation', 'user', ?1, 'credits', 1, 'held', 9999999999999, 1,
                     ?2, 'test-key-budget', NULL, NULL)",
            rusqlite::params![reserved_child, key_a],
        )
        .unwrap();
    drop(connection);

    assert_eq!(store.recover_abandoned_seedance_assist_requests().unwrap(), 1);
    assert_eq!(store.request_state(&stale_child).unwrap(), RequestState::Failed);
    assert_eq!(store.request_state(&active_child).unwrap(), RequestState::Received);
    assert_eq!(store.request_state(&dispatched_child).unwrap(), RequestState::Dispatched);
    assert_eq!(store.request_state(&reserved_child).unwrap(), RequestState::Received);
    assert_eq!(store.recover_abandoned_seedance_assist_requests().unwrap(), 0);

    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn schema_v21_upgrades_request_relations_without_losing_existing_data() {
    let (store, dir, key_a, _) = test_store();
    let parent = request(&store, &key_a, "videos", "seedance", "migration-parent");
    let child = request(&store, &key_a, "chat", "deepseek-v4-flash", "migration-child");
    store.link_seedance_assist_request(&parent, &child).unwrap();
    drop(store);

    let database = dir.join("data").join(aiwork_core::CORE_DB_FILE);
    let connection = Connection::open(&database).unwrap();
    connection.execute_batch(
        "DROP TABLE request_relations;
         UPDATE schema_meta SET value = '21' WHERE key = 'schema_version';",
    ).unwrap();
    drop(connection);

    let upgraded = CoreStore::open(&dir).unwrap();
    upgraded.migrate().unwrap();
    assert_eq!(upgraded.schema_version().unwrap(), aiwork_core::CURRENT_SCHEMA_VERSION);
    assert_eq!(upgraded.seedance_assist_request_for_parent(&parent).unwrap(), None);
    drop(upgraded);
    let connection = Connection::open(&database).unwrap();
    let preserved_requests: i64 = connection.query_row(
        "SELECT COUNT(*) FROM requests WHERE id IN (?1, ?2)",
        rusqlite::params![parent, child],
        |row| row.get(0),
    ).unwrap();
    assert_eq!(preserved_requests, 2);
    drop(connection);
    fs::remove_dir_all(dir).unwrap();
}
