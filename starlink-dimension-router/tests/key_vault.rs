use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use axum::{body::{to_bytes, Body}, http::{Request, Response, StatusCode}, Router};
use base64::Engine as _;
use serde_json::{json, Value};
use starlink_dimension_router::{
    admin_session::hash_password,
    bridge_client::BridgeClient,
    config::RouterConfig,
    key_vault::KeyVault,
    server::build_router,
    state::StarlinkRouterState,
};
use tower::util::ServiceExt;

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "starlink-key-vault-{}",
            rand::random::<u64>()
        )))
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

fn test_app(dir: &TestDir, initialize: bool) -> Router {
    test_app_with_vault(dir, initialize, KeyVault::for_test())
}

fn test_app_with_vault(dir: &TestDir, initialize: bool, key_vault: KeyVault) -> Router {
    let store = Arc::new(aiwork_core::CoreStore::open(dir.path()).unwrap());
    store.migrate().unwrap();
    if initialize {
        store.create_bootstrap_admin(
            aiwork_core::NewUser {
                id: "admin".into(),
                name: "系统管理员".into(),
                role: aiwork_core::UserRole::Admin,
            },
            "bootstrap",
        ).unwrap();
    }
    let password = hash_password("key-vault-test-password").unwrap();
    store.upsert_admin_credential(aiwork_core::NewAdminCredential {
        user_id: "admin".into(),
        username: "admin".into(),
        password_hash: password.hash,
        salt: password.salt,
        iterations: password.iterations,
        must_change_password: false,
    }).unwrap();
    let state = StarlinkRouterState::for_test_with_key_vault(
        store,
        BridgeClient::new("", ""),
        RouterConfig::defaults(dir.path().to_path_buf()),
        key_vault,
    );
    build_router(state)
}

async fn post_json(app: &Router, path: &str, value: Value) -> Response<Body> {
    app.clone().oneshot(
        Request::post(path)
            .header("content-type", "application/json")
            .body(Body::from(value.to_string()))
            .unwrap(),
    ).await.unwrap()
}

async fn post_with_cookie(app: &Router, path: &str, cookie: &str, value: Value) -> Response<Body> {
    app.clone().oneshot(
        Request::post(path)
            .header("content-type", "application/json")
            .header("cookie", cookie)
            .body(Body::from(value.to_string()))
            .unwrap(),
    ).await.unwrap()
}

async fn get_with_cookie(app: &Router, path: &str, cookie: &str) -> Response<Body> {
    app.clone().oneshot(
        Request::get(path)
            .header("cookie", cookie)
            .body(Body::empty())
            .unwrap(),
    ).await.unwrap()
}

async fn login(app: &Router) -> String {
    let response = post_json(app, "/admin/v1/login", json!({
        "username":"admin",
        "password":"key-vault-test-password"
    })).await;
    assert_eq!(response.status(), StatusCode::OK);
    response.headers().get("set-cookie").unwrap().to_str().unwrap().to_owned()
}

async fn response_json(response: Response<Body>) -> Value {
    serde_json::from_slice(
        &to_bytes(response.into_body(), usize::MAX).await.unwrap(),
    ).unwrap()
}

async fn create_key(app: &Router, cookie: &str) -> (String, String) {
    let response = post_with_cookie(app, "/admin/v1/api-keys", cookie, json!({
        "display_name":"复制验收 Key",
        "scopes":["chat:invoke"],
        "max_concurrency":1
    })).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    (
        body["id"].as_str().unwrap().to_owned(),
        body["plaintext"].as_str().unwrap().to_owned(),
    )
}

#[tokio::test]
async fn issued_key_can_be_copied_after_router_restart_without_exposing_it_in_list() {
    let dir = TestDir::new();
    let (key_id, plaintext) = {
        let app = test_app(&dir, true);
        let cookie = login(&app).await;
        let (key_id, plaintext) = create_key(&app, &cookie).await;
        assert!(!plaintext.trim().is_empty());

        let denied = post_json(&app, &format!("/admin/v1/api-keys/{key_id}/copy"), json!({})).await;
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);

        let listed = get_with_cookie(&app, "/admin/v1/api-keys", &cookie).await;
        assert_eq!(listed.status(), StatusCode::OK);
        let listed_body = response_json(listed).await;
        assert!(!listed_body.to_string().contains(&plaintext));
        (key_id, plaintext)
    };

    let app = test_app(&dir, false);
    let cookie = login(&app).await;
    let copied = post_with_cookie(
        &app,
        &format!("/admin/v1/api-keys/{key_id}/copy"),
        &cookie,
        json!({}),
    ).await;
    assert_eq!(copied.status(), StatusCode::OK);
    assert_eq!(copied.headers()["cache-control"], "no-store");
    let body = response_json(copied).await;
    assert_eq!(body["plaintext"], plaintext);

    let connection = rusqlite::Connection::open(
        dir.path().join("data").join(aiwork_core::CORE_DB_FILE),
    ).unwrap();
    let (ciphertext, key_version): (Vec<u8>, i64) = connection.query_row(
        "SELECT secret_ciphertext, secret_key_version FROM api_keys WHERE id = ?1",
        [&key_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).unwrap();
    assert_ne!(ciphertext, plaintext.as_bytes());
    assert!(ciphertext.len() > 28);
    assert_eq!(key_version, 1);
    let audit_metadata = {
        let mut statement = connection.prepare("SELECT metadata_json FROM audit_events").unwrap();
        statement.query_map([], |row| row.get::<_, String>(0)).unwrap()
            .collect::<Result<Vec<_>, _>>().unwrap()
    };
    assert!(audit_metadata.iter().all(|entry| !entry.contains(&plaintext)));
}

#[tokio::test]
async fn rotated_key_is_persistently_copyable_and_old_key_stops_authenticating() {
    let dir = TestDir::new();
    let app = test_app(&dir, true);
    let cookie = login(&app).await;
    let (old_id, old_plaintext) = create_key(&app, &cookie).await;

    let rotated = post_with_cookie(
        &app,
        &format!("/admin/v1/api-keys/{old_id}/rotate"),
        &cookie,
        json!({}),
    ).await;
    assert_eq!(rotated.status(), StatusCode::OK);
    let rotated_body = response_json(rotated).await;
    let new_id = rotated_body["id"].as_str().unwrap().to_owned();
    let new_plaintext = rotated_body["plaintext"].as_str().unwrap().to_owned();

    let verifier = aiwork_core::CoreStore::open(dir.path()).unwrap();
    assert!(verifier.authenticate_api_key(&old_plaintext).is_err());
    assert!(verifier.authenticate_api_key(&new_plaintext).is_ok());

    let copied = post_with_cookie(
        &app,
        &format!("/admin/v1/api-keys/{new_id}/copy"),
        &cookie,
        json!({}),
    ).await;
    assert_eq!(copied.status(), StatusCode::OK);
    assert_eq!(response_json(copied).await["plaintext"], new_plaintext);
}

#[tokio::test]
async fn admin_rewraps_all_key_copies_before_the_previous_vault_key_is_removed() {
    let dir = TestDir::new();
    let (key_id, plaintext) = {
        let old_app = test_app_with_vault(
            &dir,
            true,
            KeyVault::from_material(1, [0x31; 32], BTreeMap::new()).unwrap(),
        );
        let old_cookie = login(&old_app).await;
        create_key(&old_app, &old_cookie).await
    };

    let new_keyring_app = test_app_with_vault(
        &dir,
        false,
        KeyVault::from_material(2, [0x42; 32], BTreeMap::from([(1, [0x31; 32])])).unwrap(),
    );
    let new_keyring_cookie = login(&new_keyring_app).await;
    let rewrapped = post_with_cookie(
        &new_keyring_app,
        "/admin/v1/key-vault/rewrap",
        &new_keyring_cookie,
        json!({}),
    ).await;
    assert_eq!(rewrapped.status(), StatusCode::OK);
    assert_eq!(response_json(rewrapped).await["rewrapped"], 1);
    drop(new_keyring_app);

    let active_only_app = test_app_with_vault(
        &dir,
        false,
        KeyVault::from_material(2, [0x42; 32], BTreeMap::new()).unwrap(),
    );
    let active_only_cookie = login(&active_only_app).await;
    let copied = post_with_cookie(
        &active_only_app,
        &format!("/admin/v1/api-keys/{key_id}/copy"),
        &active_only_cookie,
        json!({}),
    ).await;
    assert_eq!(copied.status(), StatusCode::OK);
    assert_eq!(response_json(copied).await["plaintext"], plaintext);
}

#[test]
fn aes_gcm_copy_ciphertext_is_bound_to_key_id_and_can_be_rewrapped_by_version() {
    let old_vault = KeyVault::from_material(1, [0x11; 32], BTreeMap::new()).unwrap();
    let encrypted = old_vault.encrypt("key_test", "aw_live_private-key-material").unwrap();
    assert_ne!(encrypted.ciphertext, b"aw_live_private-key-material");
    assert_eq!(
        old_vault.decrypt("key_test", encrypted.key_version, &encrypted.ciphertext).unwrap(),
        "aw_live_private-key-material"
    );
    assert!(old_vault.decrypt("key_other", encrypted.key_version, &encrypted.ciphertext).is_err());

    let new_vault = KeyVault::from_material(
        2,
        [0x22; 32],
        BTreeMap::from([(1, [0x11; 32])]),
    ).unwrap();
    let rewrapped = new_vault.reencrypt("key_test", 1, &encrypted.ciphertext).unwrap();
    assert_eq!(rewrapped.key_version, 2);
    assert_eq!(
        new_vault.decrypt("key_test", rewrapped.key_version, &rewrapped.ciphertext).unwrap(),
        "aw_live_private-key-material"
    );
    let active_only = KeyVault::from_material(2, [0x22; 32], BTreeMap::new()).unwrap();
    assert!(active_only.decrypt("key_test", 1, &encrypted.ciphertext).is_err());
}

#[test]
fn key_vault_configuration_rejects_missing_malformed_and_wrong_size_keys() {
    assert!(KeyVault::from_configuration(None, None, None).is_err());
    assert!(KeyVault::from_configuration(Some("not-base64"), Some("1"), None).is_err());
    let short_key = base64::engine::general_purpose::STANDARD.encode([0_u8; 16]);
    assert!(KeyVault::from_configuration(Some(&short_key), Some("1"), None).is_err());
    let key = base64::engine::general_purpose::STANDARD.encode([0x44_u8; 32]);
    assert!(KeyVault::from_configuration(Some(&key), Some("1"), None).is_ok());
}
