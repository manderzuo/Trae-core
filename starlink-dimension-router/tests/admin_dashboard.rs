use std::{fs, path::{Path, PathBuf}, sync::Arc};

use axum::{body::to_bytes, http::{Request, StatusCode}};
use starlink_dimension_router::{bridge_client::BridgeClient, config::RouterConfig, server::build_router, state::StarlinkRouterState};
use tower::util::ServiceExt;

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("starlink-admin-dashboard-{}", rand::random::<u64>()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path { &self.0 }
}

impl Drop for TestDir {
    fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
}

#[tokio::test]
async fn dashboard_has_separate_overview_keys_and_settings_without_legacy_pool_or_active_counts() {
    let dir = TestDir::new();
    let store = Arc::new(aiwork_core::CoreStore::open(dir.path()).unwrap());
    store.migrate().unwrap();
    let app = build_router(StarlinkRouterState::for_test(
        store,
        BridgeClient::new("", ""),
        RouterConfig::defaults(dir.path().to_path_buf()),
    ));

    let response = app.oneshot(Request::get("/admin").body(axum::body::Body::empty()).unwrap()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let html = String::from_utf8(to_bytes(response.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap();

    assert!(html.contains("data-page=\"overview\""));
    assert!(html.contains("data-page=\"keys\""));
    assert!(html.contains("data-page=\"settings\""));
    assert!(html.contains("id=\"page-overview\""));
    assert!(html.contains("id=\"page-keys\""));
    assert!(html.contains("id=\"page-settings\""));
    assert!(html.contains("id=\"trendKey\""));
    assert!(html.contains("data-action=\"copy\""));
    assert!(html.contains("data-action=\"delete\""));
    assert!(html.contains("quota?resource_kind=credits"));
    assert!(html.contains("DELETE"));
    assert!(html.contains("复制 Key"));
    assert!(!html.contains("活跃 API Key"));
    assert!(!html.contains("活跃账号数"));
    assert!(!html.contains("积分池"));
    assert!(!html.contains("充值到积分池"));
    assert!(!html.contains("quota/pool"));

    let overview_start = html.find("id=\"page-overview\"").unwrap();
    let keys_start = html.find("id=\"page-keys\"").unwrap();
    let settings_start = html.find("id=\"page-settings\"").unwrap();
    let overview = &html[overview_start..keys_start];
    let keys = &html[keys_start..settings_start];
    assert!(!overview.contains("bridgeUrl"));
    assert!(!overview.contains("bridgeKey"));
    assert!(keys.contains("剩余可分配"));
    assert!(keys.contains("分配积分"));
    assert!(html[settings_start..].contains("AI Work Base URL"));
    assert!(html[settings_start..].contains("AI Work 桥接管理员 Key"));
}
