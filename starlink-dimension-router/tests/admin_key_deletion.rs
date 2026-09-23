use std::{collections::BTreeSet, fs, path::PathBuf, sync::Arc};

use aiwork_core::{CoreStore, KeyQuotaGrant, NewUser, Principal, QuotaGrant, UserRole};
use axum::{
    body::{to_bytes, Body},
    http::{Method, Request, Response, StatusCode},
    Router,
};
use serde_json::Value;
use starlink_dimension_router::{
    bridge_client::BridgeClient, config::RouterConfig, server::build_router,
    state::StarlinkRouterState,
};
use tower::util::ServiceExt;

const ALLOCATED_MICROCREDITS: i64 = 10_123_456;

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "starlink-admin-key-delete-{}",
            rand::random::<u64>()
        )))
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Fixture {
    app: Router,
    cookie: String,
    store: Arc<CoreStore>,
    admin: Principal,
    regular_key_id: String,
    admin_key_id: String,
    user_id: String,
    _dir: TestDir,
}

fn fixture() -> Fixture {
    let dir = TestDir::new();
    let store = Arc::new(CoreStore::open(&dir.0).unwrap());
    store.migrate().unwrap();
    store
        .create_bootstrap_admin(
            NewUser {
                id: "admin".into(),
                name: "系统管理员".into(),
                role: UserRole::Admin,
            },
            "bootstrap",
        )
        .unwrap();
    let admin = Principal {
        user_id: "admin".into(),
        key_id: "admin_session:local-test".into(),
        scopes: BTreeSet::from(["admin:*".into()]),
    };
    let regular_key = store
        .issue_api_key_for_new_user_as_admin(
            &admin,
            "周",
            BTreeSet::from(["videos:submit".into()]),
            1,
        )
        .unwrap();
    store
        .quota_pool_grant_as_admin(
            &admin,
            QuotaGrant {
                user_id: regular_key.user_id.clone(),
                resource_kind: "credits".into(),
                amount: ALLOCATED_MICROCREDITS,
                actor_user_id: "admin".into(),
                reason: "本地删除返还验收 fixture".into(),
            },
        )
        .unwrap();
    store
        .key_quota_allocate_from_pool_as_admin(
            &admin,
            KeyQuotaGrant {
                api_key_id: regular_key.id.clone(),
                resource_kind: "credits".into(),
                amount: ALLOCATED_MICROCREDITS,
                actor_user_id: "admin".into(),
                reason: "本地删除返还验收 fixture".into(),
            },
        )
        .unwrap();
    let admin_key = store
        .issue_api_key_as_admin(
            "admin",
            "管理员 Key",
            BTreeSet::from(["admin:*".into()]),
            &admin,
        )
        .unwrap();

    let config = RouterConfig::defaults(dir.0.clone());
    let state = StarlinkRouterState::for_test(store.clone(), BridgeClient::new("", ""), config);
    let (token, _) = state
        .admin_sessions
        .issue("admin".into(), chrono::Utc::now().timestamp_millis());
    Fixture {
        app: build_router(state),
        cookie: format!("starlink_admin_session={token}"),
        store,
        admin,
        regular_key_id: regular_key.id,
        admin_key_id: admin_key.id,
        user_id: regular_key.user_id,
        _dir: dir,
    }
}

async fn request(
    fixture: &Fixture,
    method: Method,
    path: &str,
    authenticated: bool,
) -> Response<Body> {
    let mut builder = Request::builder().method(method).uri(path);
    if authenticated {
        builder = builder.header("cookie", &fixture.cookie);
    }
    fixture
        .app
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn body_json(response: Response<Body>) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap()
}

#[tokio::test]
async fn deleting_key_returns_exact_available_credits_and_hides_it_from_list() {
    let fixture = fixture();
    assert_eq!(
        fixture
            .store
            .quota_pool_allocatable_as_admin(&fixture.admin, &fixture.user_id, "credits")
            .unwrap(),
        0
    );
    let response = request(
        &fixture,
        Method::DELETE,
        &format!("/admin/v1/api-keys/{}", fixture.regular_key_id),
        true,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let body = body_json(response).await;
    assert_eq!(body["deleted"], true);
    assert_eq!(body["returned_credits"], "10.123456");

    let balance = fixture
        .store
        .quota_pool_balance_as_admin(&fixture.admin, &fixture.user_id, "credits")
        .unwrap();
    assert_eq!(balance.available, ALLOCATED_MICROCREDITS);
    assert_eq!(
        fixture
            .store
            .quota_pool_allocatable_as_admin(&fixture.admin, &fixture.user_id, "credits")
            .unwrap(),
        ALLOCATED_MICROCREDITS
    );

    let listed = request(&fixture, Method::GET, "/admin/v1/api-keys", true).await;
    assert_eq!(listed.status(), StatusCode::OK);
    let listed = body_json(listed).await;
    assert!(listed
        .as_array()
        .unwrap()
        .iter()
        .all(|key| key["id"] != fixture.regular_key_id));
}

#[tokio::test]
async fn repeated_delete_does_not_return_the_same_credits_twice() {
    let fixture = fixture();
    let path = format!("/admin/v1/api-keys/{}", fixture.regular_key_id);
    let first = request(&fixture, Method::DELETE, &path, true).await;
    assert_eq!(first.status(), StatusCode::OK);
    let _ = body_json(first).await;

    let second = request(&fixture, Method::DELETE, &path, true).await;
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(body_json(second).await["returned_credits"], "0.000000");
}

#[tokio::test]
async fn deletion_requires_an_admin_session() {
    let fixture = fixture();
    let response = request(
        &fixture,
        Method::DELETE,
        &format!("/admin/v1/api-keys/{}", fixture.regular_key_id),
        false,
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn administrator_key_cannot_be_deleted_as_a_regular_key() {
    let fixture = fixture();
    let response = request(
        &fixture,
        Method::DELETE,
        &format!("/admin/v1/api-keys/{}", fixture.admin_key_id),
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn deletion_is_blocked_when_key_has_an_active_request() {
    let fixture = fixture();
    let connection =
        rusqlite::Connection::open(fixture._dir.0.join("data").join(aiwork_core::CORE_DB_FILE))
            .unwrap();
    connection
        .execute(
            "INSERT INTO requests
             (id,user_id,api_key_id,protocol,endpoint,model,request_hash,state,created_at_ms,updated_at_ms)
             VALUES ('active-request',?1,?2,'openai','/v1/chat/completions','test',x'01','dispatched',1,1)",
            rusqlite::params![fixture.user_id, fixture.regular_key_id],
        )
        .unwrap();
    drop(connection);

    let response = request(
        &fixture,
        Method::DELETE,
        &format!("/admin/v1/api-keys/{}", fixture.regular_key_id),
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let balance = fixture
        .store
        .key_quota_balance_as_admin(&fixture.admin, &fixture.regular_key_id, "credits")
        .unwrap();
    assert_eq!(balance.available, ALLOCATED_MICROCREDITS);
}

#[tokio::test]
async fn deletion_refunds_only_the_unspent_available_credit_balance() {
    let fixture = fixture();
    let account = fixture
        .store
        .key_quota_balance_as_admin(&fixture.admin, &fixture.regular_key_id, "credits")
        .unwrap();
    let connection =
        rusqlite::Connection::open(fixture._dir.0.join("data").join(aiwork_core::CORE_DB_FILE))
            .unwrap();
    seed_settlement(
        &connection,
        &fixture.user_id,
        &fixture.regular_key_id,
        &account.account_id,
        "settled-request",
        2_000_000,
        false,
    );
    drop(connection);

    let response = request(
        &fixture,
        Method::DELETE,
        &format!("/admin/v1/api-keys/{}", fixture.regular_key_id),
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["returned_credits"], "8.123456");

    let connection =
        rusqlite::Connection::open(fixture._dir.0.join("data").join(aiwork_core::CORE_DB_FILE))
            .unwrap();
    let refunded: i64 = connection
        .query_row(
            "SELECT COALESCE(SUM(-delta), 0) FROM quota_ledger
             WHERE api_key_id = ?1 AND event_kind = 'adjust' AND delta < 0",
            [&fixture.regular_key_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(refunded, 8_123_456);
    let pool = fixture
        .store
        .quota_pool_balance_as_admin(&fixture.admin, &fixture.user_id, "credits")
        .unwrap();
    assert_eq!(pool.available, 8_123_456);
    assert_eq!(
        fixture
            .store
            .quota_pool_allocatable_as_admin(&fixture.admin, &fixture.user_id, "credits")
            .unwrap(),
        8_123_456
    );
}

#[tokio::test]
async fn deletion_is_blocked_by_held_reservation_without_changing_credits() {
    let fixture = fixture();
    let account = fixture
        .store
        .key_quota_balance_as_admin(&fixture.admin, &fixture.regular_key_id, "credits")
        .unwrap();
    let connection =
        rusqlite::Connection::open(fixture._dir.0.join("data").join(aiwork_core::CORE_DB_FILE))
            .unwrap();
    connection
        .execute(
            "INSERT INTO quota_reservations
             (id,user_id,request_id,resource_kind,amount,state,expires_at_ms,created_at_ms,api_key_id,key_budget_account_id)
             VALUES ('held-reservation',?1,'held-request','credits',1,'held',9999999999999,1,?2,?3)",
            rusqlite::params![fixture.user_id, fixture.regular_key_id, account.account_id],
        )
        .unwrap();
    drop(connection);

    let response = request(
        &fixture,
        Method::DELETE,
        &format!("/admin/v1/api-keys/{}", fixture.regular_key_id),
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let balance = fixture
        .store
        .key_quota_balance_as_admin(&fixture.admin, &fixture.regular_key_id, "credits")
        .unwrap();
    assert_eq!(balance.available, ALLOCATED_MICROCREDITS);
}

#[tokio::test]
async fn deletion_is_blocked_by_reconciliation_required_settlement() {
    let fixture = fixture();
    let account = fixture
        .store
        .key_quota_balance_as_admin(&fixture.admin, &fixture.regular_key_id, "credits")
        .unwrap();
    let connection =
        rusqlite::Connection::open(fixture._dir.0.join("data").join(aiwork_core::CORE_DB_FILE))
            .unwrap();
    seed_settlement(
        &connection,
        &fixture.user_id,
        &fixture.regular_key_id,
        &account.account_id,
        "unreconciled-request",
        0,
        true,
    );
    drop(connection);

    let response = request(
        &fixture,
        Method::DELETE,
        &format!("/admin/v1/api-keys/{}", fixture.regular_key_id),
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let balance = fixture
        .store
        .key_quota_balance_as_admin(&fixture.admin, &fixture.regular_key_id, "credits")
        .unwrap();
    assert_eq!(balance.available, ALLOCATED_MICROCREDITS);
}

#[tokio::test]
async fn negative_key_balance_returns_zero() {
    let fixture = fixture();
    let connection =
        rusqlite::Connection::open(fixture._dir.0.join("data").join(aiwork_core::CORE_DB_FILE))
            .unwrap();
    let account_id: String = connection
        .query_row(
            "SELECT id FROM quota_budget_accounts WHERE api_key_id = ?1 AND resource_kind = 'credits'",
            [&fixture.regular_key_id],
            |row| row.get(0),
        )
        .unwrap();
    connection
        .execute(
            "DELETE FROM quota_ledger WHERE budget_account_id = ?1",
            [&account_id],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO quota_ledger
             (entry_id,user_id,resource_kind,event_kind,amount,delta,created_at_ms,budget_account_id,api_key_id)
             VALUES ('negative-adjust',?1,'credits','adjust',1,-1,1,?2,?3)",
            rusqlite::params![fixture.user_id, account_id, fixture.regular_key_id],
        )
        .unwrap();
    drop(connection);

    let response = request(
        &fixture,
        Method::DELETE,
        &format!("/admin/v1/api-keys/{}", fixture.regular_key_id),
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["returned_credits"], "0.000000");
}

#[tokio::test]
async fn quota_query_returns_zero_for_a_key_without_a_credit_account() {
    let fixture = fixture();
    let connection =
        rusqlite::Connection::open(fixture._dir.0.join("data").join(aiwork_core::CORE_DB_FILE))
            .unwrap();
    let account_id: String = connection
        .query_row(
            "SELECT id FROM quota_budget_accounts WHERE api_key_id = ?1 AND resource_kind = 'credits'",
            [&fixture.regular_key_id],
            |row| row.get(0),
        )
        .unwrap();
    connection
        .execute(
            "DELETE FROM quota_ledger WHERE budget_account_id = ?1",
            [&account_id],
        )
        .unwrap();
    connection
        .execute(
            "DELETE FROM quota_budget_accounts WHERE id = ?1",
            [&account_id],
        )
        .unwrap();
    drop(connection);

    let response = request(
        &fixture,
        Method::GET,
        &format!(
            "/admin/v1/api-keys/{}/quota?resource_kind=credits",
            fixture.regular_key_id
        ),
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["available"], "0.000000");
    assert_eq!(body["key_quota_configured"], false);

    let deleted = request(
        &fixture,
        Method::DELETE,
        &format!("/admin/v1/api-keys/{}", fixture.regular_key_id),
        true,
    )
    .await;
    assert_eq!(deleted.status(), StatusCode::OK);
    assert_eq!(body_json(deleted).await["returned_credits"], "0.000000");
}

fn seed_settlement(
    connection: &rusqlite::Connection,
    user_id: &str,
    key_id: &str,
    budget_id: &str,
    request_id: &str,
    actual: i64,
    reconcile_required: bool,
) {
    let now = chrono::Utc::now().timestamp_millis();
    let quote_id = format!("quote-{request_id}");
    let receipt_id = format!("receipt-{request_id}");
    connection
        .execute(
            "INSERT INTO requests
             (id,user_id,api_key_id,protocol,endpoint,model,request_hash,state,created_at_ms,updated_at_ms)
             VALUES (?1,?2,?3,'openai','/v1/chat/completions','test',x'01','settled',?4,?4)",
            rusqlite::params![request_id, user_id, key_id, now],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO billing_quotes
             (quote_id,request_id,request_fingerprint,endpoint,model,max_credits,unit,source_ref,expires_at_ms,created_at_ms)
             VALUES (?1,?2,'test-fingerprint','/v1/chat/completions','test',5000000,'credits','test-quote',?3,?4)",
            rusqlite::params![quote_id, request_id, now + 60_000, now],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO billing_receipts
             (receipt_id,request_id,receipt_hash,status,actual_credits,unit,source_ref,observed_at_ms,received_at_ms)
             VALUES (?1,?2,x'02','final',?3,'credits','test-receipt',?4,?4)",
            rusqlite::params![receipt_id, request_id, actual, now],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO billing_settlements
             (request_id,quote_id,receipt_id,actual_credits,over_quote,reconcile_required,settled_at_ms)
             VALUES (?1,?2,?3,?4,0,?5,?6)",
            rusqlite::params![request_id, quote_id, receipt_id, actual, i64::from(reconcile_required), now],
        )
        .unwrap();
    if actual > 0 {
        connection
            .execute(
                "INSERT INTO quota_ledger
                 (entry_id,user_id,resource_kind,event_kind,amount,delta,request_id,created_at_ms,budget_account_id,api_key_id)
                 VALUES (?1,?2,'credits','commit',?3,-?3,?4,?5,?6,?7)",
                rusqlite::params![format!("ledger-{request_id}"), user_id, actual, request_id, now, budget_id, key_id],
            )
            .unwrap();
        let user_cap_account: String = connection
            .query_row(
                "SELECT id FROM quota_budget_accounts WHERE scope = 'user_cap' AND user_id = ?1 AND resource_kind = 'credits'",
                [user_id],
                |row| row.get(0),
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO quota_ledger
                 (entry_id,user_id,resource_kind,event_kind,amount,delta,request_id,created_at_ms,budget_account_id)
                 VALUES (?1,?2,'credits','commit',?3,-?3,?4,?5,?6)",
                rusqlite::params![format!("cap-ledger-{request_id}"), user_id, actual, request_id, now, user_cap_account],
            )
            .unwrap();
    }
}
