use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use axum::{
    body::{to_bytes, Body},
    http::{Request, Response, StatusCode},
    Router,
};
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
        let path = std::env::temp_dir().join(format!(
            "starlink-upstream-credit-allocation-{}",
            rand::random::<u64>()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct SummaryTransport {
    summary: Vec<u8>,
}

impl BridgeTransport for SummaryTransport {
    fn send(
        &self,
        method: &str,
        url: &str,
        _headers: &BTreeMap<String, String>,
        _body: &[u8],
    ) -> Result<BridgeResponse, String> {
        if method == "GET" && url.ends_with("/internal/bridge/summary") {
            Ok(BridgeResponse {
                status: 200,
                headers: BTreeMap::new(),
                body: self.summary.clone(),
            })
        } else {
            Ok(BridgeResponse {
                status: 404,
                headers: BTreeMap::new(),
                body: br#"{"error":"unexpected bridge request"}"#.to_vec(),
            })
        }
    }
}

fn test_app(dir: &Path, upstream_total: &str, fresh: bool, updated_at_ms: i64) -> Router {
    let summary = json!({
        "upstream_credits": {
            "general": "8.000000",
            "work": "2.000000",
            "video_available": upstream_total,
            "value": upstream_total,
            "source": "aiwork-upstream-aggregate",
            "fresh": fresh,
            "updated_at": updated_at_ms
        }
    });
    test_app_with_summary(dir, summary)
}

fn test_app_with_summary(dir: &Path, summary: Value) -> Router {
    let store = Arc::new(aiwork_core::CoreStore::open(dir).unwrap());
    store.migrate().unwrap();
    store
        .create_bootstrap_admin(
            aiwork_core::NewUser {
                id: "admin".into(),
                name: "系统管理员".into(),
                role: aiwork_core::UserRole::Admin,
            },
            "bootstrap",
        )
        .unwrap();
    let password = hash_password("test-password").unwrap();
    store
        .upsert_admin_credential(aiwork_core::NewAdminCredential {
            user_id: "admin".into(),
            username: "admin".into(),
            password_hash: password.hash,
            salt: password.salt,
            iterations: password.iterations,
            must_change_password: false,
        })
        .unwrap();

    let bridge = BridgeClient::from_transport(
        "http://bridge",
        "bridge-secret",
        Arc::new(SummaryTransport {
            summary: serde_json::to_vec(&summary).unwrap(),
        }),
    );
    let config = RouterConfig::defaults(dir.to_path_buf());
    let state = StarlinkRouterState::for_test(store, bridge, config);
    build_router(state)
}

async fn post_json(app: &Router, path: &str, value: Value) -> Response<Body> {
    app.clone()
        .oneshot(
            Request::post(path)
                .header("content-type", "application/json")
                .body(Body::from(value.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn post_with_cookie(
    app: &Router,
    path: &str,
    cookie: &str,
    value: Value,
) -> Response<Body> {
    app.clone()
        .oneshot(
            Request::post(path)
                .header("content-type", "application/json")
                .header("cookie", cookie)
                .body(Body::from(value.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn admin_cookie(app: &Router) -> String {
    let login = post_json(
        app,
        "/admin/v1/login",
        json!({"username":"admin","password":"test-password"}),
    )
    .await;
    assert_eq!(login.status(), StatusCode::OK);
    login.headers()["set-cookie"].to_str().unwrap().to_owned()
}

#[tokio::test]
async fn admin_summary_accepts_live_numeric_upstream_credit_balances() {
    let dir = TestDir::new();
    let now = chrono::Utc::now().timestamp_millis();
    let app = test_app_with_summary(
        dir.path(),
        json!({
            "active_accounts": null,
            "active_models": 2,
            "current_inflight": 0,
            "total_requests": 0,
            "queued_jobs": 0,
            "running_jobs": 0,
            "reconciliation_jobs": 0,
            "updated_at": now,
            "upstream_credits": {
                "general": 16526.190000000002_f64,
                "work": 8237.73_f64,
                "video_available": 24763.920000000002_f64,
                "value": 24763.920000000002_f64,
                "source": "aiwork-upstream-aggregate",
                "fresh": true,
                "updated_at": now
            }
        }),
    );
    let cookie = admin_cookie(&app).await;
    let response = app
        .oneshot(
            Request::get("/admin/v1/summary")
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let summary: Value = serde_json::from_slice(
        &to_bytes(response.into_body(), usize::MAX).await.unwrap(),
    )
    .unwrap();
    assert_eq!(summary["credits"]["upstream_fresh"], true);
    assert_eq!(summary["credits"]["upstream_total"], "24763.920000");
    assert_eq!(summary["credits"]["available_to_allocate"], "24763.920000");
}

async fn create_key(app: &Router, cookie: &str, display_name: &str) -> Value {
    let created = post_with_cookie(
        app,
        "/admin/v1/api-keys",
        cookie,
        json!({
            "display_name": display_name,
            "scopes": ["chat:invoke", "videos:submit"],
            "max_concurrency": 2
        }),
    )
    .await;
    assert_eq!(created.status(), StatusCode::OK);
    serde_json::from_slice(&to_bytes(created.into_body(), usize::MAX).await.unwrap()).unwrap()
}

#[tokio::test]
async fn admin_allocates_key_directly_from_fresh_unified_upstream_credits() {
    let dir = TestDir::new();
    let app = test_app(
        dir.path(),
        "10.000000",
        true,
        chrono::Utc::now().timestamp_millis(),
    );
    let login = post_json(
        &app,
        "/admin/v1/login",
        json!({"username":"admin","password":"test-password"}),
    )
    .await;
    assert_eq!(login.status(), StatusCode::OK);
    let cookie = login.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .to_owned();

    let created = post_with_cookie(
        &app,
        "/admin/v1/api-keys",
        &cookie,
        json!({
            "display_name": "工作室甲",
            "scopes": ["chat:invoke", "videos:submit"],
            "max_concurrency": 2
        }),
    )
    .await;
    assert_eq!(created.status(), StatusCode::OK);
    let created: Value = serde_json::from_slice(
        &to_bytes(created.into_body(), usize::MAX).await.unwrap(),
    )
    .unwrap();
    let key_id = created["id"].as_str().unwrap();

    let allocated = post_with_cookie(
        &app,
        &format!("/admin/v1/api-keys/{key_id}/quota"),
        &cookie,
        json!({
            "resource_kind": "credits",
            "amount": "7.000000",
            "reason": "direct upstream allocation"
        }),
    )
    .await;
    let status = allocated.status();
    let body = to_bytes(allocated.into_body(), usize::MAX).await.unwrap();
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let balance: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(balance["available"], "7.000000");
    assert_eq!(balance["allocatable"], "3.000000");
}

#[tokio::test]
async fn stale_upstream_credits_block_key_allocation() {
    let dir = TestDir::new();
    let app = test_app(
        dir.path(),
        "10.000000",
        false,
        chrono::Utc::now().timestamp_millis() - 300_001,
    );
    let cookie = admin_cookie(&app).await;
    let created = create_key(&app, &cookie, "工作室乙").await;
    let key_id = created["id"].as_str().unwrap();

    let response = post_with_cookie(
        &app,
        &format!("/admin/v1/api-keys/{key_id}/quota"),
        &cookie,
        json!({
            "resource_kind": "credits",
            "amount": "1.000000",
            "reason": "stale snapshot must not allocate"
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = serde_json::from_slice(
        &to_bytes(response.into_body(), usize::MAX).await.unwrap(),
    )
    .unwrap();
    assert_eq!(body["error"]["type"], "upstream_credits_unavailable");
}

#[tokio::test]
async fn legacy_pool_and_unbound_grant_writes_are_disabled() {
    let dir = TestDir::new();
    let app = test_app(
        dir.path(),
        "10.000000",
        true,
        chrono::Utc::now().timestamp_millis(),
    );
    let cookie = admin_cookie(&app).await;
    let created = create_key(&app, &cookie, "工作室丙").await;
    let user_id = created["user_id"].as_str().unwrap();

    let pool = post_with_cookie(
        &app,
        "/admin/v1/quota/pool",
        &cookie,
        json!({
            "user_id": user_id,
            "resource_kind": "credits",
            "amount": 10_000_000,
            "reason": "legacy pool recharge"
        }),
    )
    .await;
    assert_eq!(pool.status(), StatusCode::GONE);

    let generic = post_with_cookie(
        &app,
        "/admin/v1/quota/grant",
        &cookie,
        json!({
            "user_id": user_id,
            "resource_kind": "credits",
            "amount": 10_000_000,
            "reason": "unbound legacy grant"
        }),
    )
    .await;
    assert_eq!(generic.status(), StatusCode::GONE);
}
