use std::{collections::BTreeMap, fs, path::{Path, PathBuf}, sync::Arc};

use axum::{body::{to_bytes, Body}, http::{Request, Response, StatusCode}, Router};
use serde_json::{json, Value};
use starlink_dimension_router::{
    admin_session::hash_password,
    bridge_client::{BridgeClient, BridgeResponse, BridgeTransport},
    config::RouterConfig,
    server::build_router,
    state::StarlinkRouterState,
};
use tower::util::ServiceExt;

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("starlink-admin-trend-{}", rand::random::<u64>()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path { &self.0 }
}

impl Drop for TestDir {
    fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
}

struct SummaryTransport(Vec<u8>);

impl BridgeTransport for SummaryTransport {
    fn send(&self, method: &str, url: &str, _headers: &BTreeMap<String, String>, _body: &[u8]) -> Result<BridgeResponse, String> {
        if method == "GET" && url.ends_with("/internal/bridge/summary") {
            Ok(BridgeResponse { status: 200, headers: BTreeMap::new(), body: self.0.clone() })
        } else {
            Ok(BridgeResponse { status: 404, headers: BTreeMap::new(), body: b"{}".to_vec() })
        }
    }
}

fn test_app(dir: &TestDir, upstream_total: &str) -> Router {
    let store = Arc::new(aiwork_core::CoreStore::open(dir.path()).unwrap());
    store.migrate().unwrap();
    store.create_bootstrap_admin(aiwork_core::NewUser { id: "admin".into(), name: "系统管理员".into(), role: aiwork_core::UserRole::Admin }, "bootstrap").unwrap();
    let password = hash_password("test-password").unwrap();
    store.upsert_admin_credential(aiwork_core::NewAdminCredential {
        user_id: "admin".into(), username: "admin".into(), password_hash: password.hash,
        salt: password.salt, iterations: password.iterations, must_change_password: false,
    }).unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    let summary = json!({
        "active_accounts": 17,
        "upstream_credits": {
            "general": "12.000000", "work": "8.500000",
            "video_available": upstream_total, "value": upstream_total,
            "source": "aiwork-upstream-aggregate", "fresh": true, "updated_at": now
        }
    });
    let bridge = BridgeClient::from_transport("http://bridge", "bridge-secret", Arc::new(SummaryTransport(serde_json::to_vec(&summary).unwrap())));
    let config = RouterConfig::defaults(dir.path().to_path_buf());
    build_router(StarlinkRouterState::for_test(store, bridge, config))
}

async fn post_json(app: &Router, path: &str, value: Value) -> Response<Body> {
    app.clone().oneshot(Request::post(path).header("content-type", "application/json").body(Body::from(value.to_string())).unwrap()).await.unwrap()
}

async fn post_with_cookie(app: &Router, path: &str, cookie: &str, value: Value) -> Response<Body> {
    app.clone().oneshot(Request::post(path).header("content-type", "application/json").header("cookie", cookie).body(Body::from(value.to_string())).unwrap()).await.unwrap()
}

async fn get_with_cookie(app: &Router, path: &str, cookie: &str) -> Response<Body> {
    app.clone().oneshot(Request::get(path).header("cookie", cookie).body(Body::empty()).unwrap()).await.unwrap()
}

async fn login(app: &Router) -> String {
    let response = post_json(app, "/admin/v1/login", json!({"username":"admin","password":"test-password"})).await;
    assert_eq!(response.status(), StatusCode::OK);
    response.headers()["set-cookie"].to_str().unwrap().to_owned()
}

async fn create_key(app: &Router, cookie: &str, display_name: &str) -> String {
    let response = post_with_cookie(app, "/admin/v1/api-keys", cookie, json!({
        "display_name": display_name, "scopes": ["chat:invoke"], "max_concurrency": 2
    })).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    body["id"].as_str().unwrap().to_owned()
}

fn seed_settlement(dir: &TestDir, request: &str, key: &str, actual: i64, settled_at_ms: i64) {
    let database = dir.path().join("data").join(aiwork_core::CORE_DB_FILE);
    let connection = rusqlite::Connection::open(database).unwrap();
    let user_id: String = connection.query_row("SELECT user_id FROM api_keys WHERE id = ?1", [key], |row| row.get(0)).unwrap();
    let budget_id = format!("budget-{key}");
    connection.execute("INSERT INTO quota_budget_accounts (id,scope,user_id,api_key_id,resource_kind,enabled,version,migration_state,created_at_ms,updated_at_ms) VALUES (?1,'key',?2,?3,'credits',1,1,'ready',1,1)", rusqlite::params![budget_id, user_id, key]).unwrap();
    connection.execute("INSERT INTO quota_ledger (entry_id,user_id,resource_kind,event_kind,amount,delta,created_at_ms,budget_account_id,api_key_id) VALUES (?1,?2,'credits','adjust',10000000,10000000,1,?3,?4)", rusqlite::params![format!("grant-{key}"), user_id, budget_id, key]).unwrap();
    connection.execute("INSERT INTO requests (id,user_id,api_key_id,protocol,endpoint,model,request_hash,state,created_at_ms,updated_at_ms) VALUES (?1,?2,?3,'openai','/v1/chat/completions','model',x'01','settled',1,?4)", rusqlite::params![request, user_id, key, settled_at_ms]).unwrap();
    connection.execute("INSERT INTO billing_quotes (quote_id,request_id,request_fingerprint,endpoint,model,max_credits,unit,source_ref,expires_at_ms,created_at_ms) VALUES (?1,?2,'fingerprint','/v1/chat/completions','model',5000000,'credits','quote-source',?3,1)", rusqlite::params![format!("quote-{request}"), request, settled_at_ms + 60_000]).unwrap();
    connection.execute("INSERT INTO billing_receipts (receipt_id,request_id,receipt_hash,status,actual_credits,unit,source_ref,observed_at_ms,received_at_ms) VALUES (?1,?2,x'02','final',?3,'credits','receipt-source',?4,?4)", rusqlite::params![format!("receipt-{request}"), request, actual, settled_at_ms - 3_600_000]).unwrap();
    connection.execute("INSERT INTO billing_settlements (request_id,quote_id,receipt_id,actual_credits,over_quote,reconcile_required,settled_at_ms) VALUES (?1,?2,?3,?4,0,0,?5)", rusqlite::params![request, format!("quote-{request}"), format!("receipt-{request}"), actual, settled_at_ms]).unwrap();
    connection.execute("INSERT INTO quota_ledger (entry_id,user_id,resource_kind,event_kind,amount,delta,request_id,created_at_ms,budget_account_id,api_key_id) VALUES (?1,?2,'credits','commit',?3,-?3,?4,?5,?6,?7)", rusqlite::params![format!("ledger-{request}"), user_id, actual, request, settled_at_ms, budget_id, key]).unwrap();
}

#[tokio::test]
async fn summary_reports_unified_fresh_balance_and_never_exposes_account_counts_or_pool_breakdown() {
    let dir = TestDir::new();
    let app = test_app(&dir, "20.500000");
    let cookie = login(&app).await;
    let response = get_with_cookie(&app, "/admin/v1/summary", &cookie).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let raw = String::from_utf8_lossy(&body);
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert!(!raw.contains("active_accounts"));
    assert!(!raw.contains("active_api_keys"));
    assert!(value["upstream"]["upstream_credits"].get("general").is_none());
    assert!(value["upstream"]["upstream_credits"].get("work").is_none());
    assert_eq!(value["credits"]["upstream_total"], "20.500000");
    assert_eq!(value["credits"]["available_to_allocate"], "20.500000");
    assert_eq!(value["credits"]["allocated"], "0.000000");
    assert_eq!(value["credits"]["upstream_fresh"], true);
}

#[tokio::test]
async fn key_list_uses_decimal_credit_strings_and_one_key_filter_excludes_other_usage() {
    let dir = TestDir::new();
    let app = test_app(&dir, "20.500000");
    let cookie = login(&app).await;
    let key_one = create_key(&app, &cookie, "周").await;
    let key_two = create_key(&app, &cookie, "另一个 Key").await;
    let settled_at_ms = chrono::Utc::now().timestamp_millis() - 5 * 60 * 1000;
    seed_settlement(&dir, "request-one", &key_one, 1_250_000, settled_at_ms);
    seed_settlement(&dir, "request-two", &key_two, 3_000_000, settled_at_ms + 1_000);

    let list = get_with_cookie(&app, "/admin/v1/api-keys", &cookie).await;
    assert_eq!(list.status(), StatusCode::OK);
    let keys: Value = serde_json::from_slice(&to_bytes(list.into_body(), usize::MAX).await.unwrap()).unwrap();
    let one = keys.as_array().unwrap().iter().find(|item| item["id"] == key_one).unwrap();
    assert_eq!(one["display_name"], "周");
    assert_eq!(one["credits"]["used"], "1.250000");
    assert_eq!(one["credits"]["allocated"], "10.000000");
    assert_eq!(one["credits"]["remaining"], "8.750000");

    let trend = get_with_cookie(&app, &format!("/admin/v1/usage-trend?window=24h&key_id={key_one}"), &cookie).await;
    assert_eq!(trend.status(), StatusCode::OK);
    let trend: Value = serde_json::from_slice(&to_bytes(trend.into_body(), usize::MAX).await.unwrap()).unwrap();
    let total: i64 = trend["points"].as_array().unwrap().iter()
        .map(|point| aiwork_core::CreditAmount::parse(point["credits"].as_str().unwrap(), "credits").unwrap().as_microcredits())
        .sum();
    assert_eq!(total, 1_250_000);
}

#[tokio::test]
async fn summary_compares_upstream_balance_with_unspent_key_commitments_after_spend() {
    let dir = TestDir::new();
    let app = test_app(&dir, "9.750000");
    let cookie = login(&app).await;
    let key = create_key(&app, &cookie, "已消费的 Key").await;
    seed_settlement(
        &dir,
        "spent-request",
        &key,
        1_250_000,
        chrono::Utc::now().timestamp_millis() - 60_000,
    );

    let response = get_with_cookie(&app, "/admin/v1/summary", &cookie).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value: Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(value["credits"]["allocated"], "10.000000");
    assert_eq!(value["credits"]["available_to_allocate"], "1.000000");
    assert_eq!(value["credits"]["upstream_deficit"], "0.000000");
    assert_eq!(value["credits"]["key_commitments"], "8.750000");
}

#[tokio::test]
async fn over_quote_debt_keeps_key_management_and_quota_views_available() {
    let dir = TestDir::new();
    let app = test_app(&dir, "0.000000");
    let cookie = login(&app).await;
    let key = create_key(&app, &cookie, "超额扣费 Key").await;
    let settled_at_ms = chrono::Utc::now().timestamp_millis() - 60_000;
    seed_settlement(&dir, "over-quote-request", &key, 6_000_000, settled_at_ms);

    let database = dir.path().join("data").join(aiwork_core::CORE_DB_FILE);
    let connection = rusqlite::Connection::open(database).unwrap();
    connection.execute(
        "UPDATE quota_ledger SET amount = 5000000, delta = 5000000 WHERE entry_id = ?1",
        [format!("grant-{key}")],
    ).unwrap();
    connection.execute(
        "UPDATE billing_settlements SET over_quote = 1, reconcile_required = 1 WHERE request_id = 'over-quote-request'",
        [],
    ).unwrap();
    connection.execute(
        "INSERT INTO api_key_billing_blocks (key_id,request_id,reason,quote_max_credits,actual_credits,excess_credits,source_ref,blocked_at_ms) VALUES (?1,'over-quote-request','over_quote',5000000,6000000,1000000,'receipt-source',?2)",
        rusqlite::params![key, settled_at_ms],
    ).unwrap();
    drop(connection);

    let list = get_with_cookie(&app, "/admin/v1/api-keys", &cookie).await;
    assert_eq!(list.status(), StatusCode::OK);
    let keys: Value = serde_json::from_slice(&to_bytes(list.into_body(), usize::MAX).await.unwrap()).unwrap();
    let overdrawn = keys.as_array().unwrap().iter().find(|item| item["id"] == key).unwrap();
    assert_eq!(overdrawn["billing_blocked"], true);
    assert_eq!(overdrawn["credits"]["allocated"], "5.000000");
    assert_eq!(overdrawn["credits"]["used"], "6.000000");
    assert_eq!(overdrawn["credits"]["remaining"], "0.000000");
    assert_eq!(overdrawn["credits"]["deficit"], "1.000000");

    let quota = get_with_cookie(&app, &format!("/admin/v1/api-keys/{key}/quota?resource_kind=credits"), &cookie).await;
    assert_eq!(quota.status(), StatusCode::OK);
    let quota: Value = serde_json::from_slice(&to_bytes(quota.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(quota["available"], "0.000000");
    assert_eq!(quota["deficit"], "1.000000");
}
