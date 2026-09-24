use std::{collections::{HashMap, HashSet}, fs, path::PathBuf, sync::{Arc, Mutex}};

use aiwork_core::CoreStore;
use serde::{Deserialize, Serialize};

use crate::{admin_session::{ensure_initial_admin_credential, AdminSessionStore, LoginThrottle, SESSION_TTL_MS}, assets::AssetLimiter, bridge_client::BridgeClient, config::RouterConfig, key_vault::KeyVault};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserVideoJob {
    pub id: String,
    pub user_id: String,
    #[serde(default)]
    pub api_key_id: String,
    pub request_id: String,
    pub upstream_id: Option<String>,
    pub status: String,
    pub output_ref: Option<String>,
    pub error_code: Option<String>,
    pub reconcile_required: bool,
    #[serde(default)]
    pub reservation_id: Option<String>,
    #[serde(default = "default_billing_state")]
    pub billing_state: String,
    #[serde(default)]
    pub actual_credits: Option<String>,
    #[serde(default)]
    pub last_reconciled_at_ms: Option<i64>,
    #[serde(default)]
    pub one_shot_test: bool,
}

fn default_billing_state() -> String { "held".into() }

pub struct StarlinkRouterState {
    pub store: Arc<CoreStore>,
    pub bridge: Arc<Mutex<BridgeClient>>,
    pub config: RouterConfig,
    pub key_vault: Arc<KeyVault>,
    pub jobs: Arc<Mutex<HashMap<String, UserVideoJob>>>,
    pub video_stream_observers: Arc<Mutex<HashSet<String>>>,
    pub admin_sessions: Arc<AdminSessionStore>,
    pub login_throttle: Arc<LoginThrottle>,
    pub asset_limiter: Arc<AssetLimiter>,
}

impl StarlinkRouterState {
    pub fn open(config: RouterConfig, bridge: BridgeClient) -> Result<Arc<Self>, String> {
        let key_vault = KeyVault::from_env().map_err(|_| {
            "STARLINK_ROUTER_KEY_ENCRYPTION_KEY 缺失或无效；请先配置持久化的 Core Key 加密密钥".to_string()
        })?;
        Self::open_with_key_vault(config, bridge, key_vault)
    }

    pub fn open_with_key_vault(
        config: RouterConfig,
        bridge: BridgeClient,
        key_vault: KeyVault,
    ) -> Result<Arc<Self>, String> {
        fs::create_dir_all(&config.data_dir).map_err(|e| format!("创建 Core 数据目录失败: {e}"))?;
        let store = Arc::new(CoreStore::open(&config.data_dir).map_err(|e| e.to_string())?);
        store.migrate().map_err(|e| e.to_string())?;
        store
            .recover_abandoned_seedance_assist_requests()
            .map_err(|error| format!("恢复已结束 Seedance 请求失败: {error}"))?;
        let initial_password = std::env::var("STARLINK_ADMIN_INITIAL_PASSWORD").ok();
        ensure_initial_admin_credential(&store, initial_password.as_deref()).map_err(|e| e.to_string())?;
        let jobs = load_jobs(&config.data_dir);
        Ok(Arc::new(Self {
            store,
            bridge: Arc::new(Mutex::new(bridge)),
            config,
            key_vault: Arc::new(key_vault),
            jobs: Arc::new(Mutex::new(jobs)),
            video_stream_observers: Arc::new(Mutex::new(HashSet::new())),
            admin_sessions: Arc::new(AdminSessionStore::new(SESSION_TTL_MS)),
            login_throttle: Arc::new(LoginThrottle::new()),
            asset_limiter: Arc::new(AssetLimiter::from_env()),
        }))
    }

    pub fn for_test(store: Arc<CoreStore>, bridge: BridgeClient, config: RouterConfig) -> Arc<Self> {
        Self::for_test_with_key_vault(store, bridge, config, KeyVault::for_test())
    }

    pub fn for_test_with_key_vault(
        store: Arc<CoreStore>,
        bridge: BridgeClient,
        config: RouterConfig,
        key_vault: KeyVault,
    ) -> Arc<Self> {
        Arc::new(Self { store, bridge: Arc::new(Mutex::new(bridge)), config, key_vault: Arc::new(key_vault), jobs: Arc::new(Mutex::new(HashMap::new())), video_stream_observers: Arc::new(Mutex::new(HashSet::new())), admin_sessions: Arc::new(AdminSessionStore::new(SESSION_TTL_MS)), login_throttle: Arc::new(LoginThrottle::new()), asset_limiter: Arc::new(AssetLimiter::from_env()) })
    }

    pub fn replace_bridge(&self, bridge: BridgeClient) {
        *self.bridge.lock().unwrap() = bridge;
    }

    pub fn persist_jobs(&self) {
        let path = self.config.data_dir.join("video_jobs.json");
        let snapshot = self.jobs.lock().unwrap().clone();
        if let Ok(bytes) = serde_json::to_vec_pretty(&snapshot) {
            let _ = fs::write(path, bytes);
        }
    }
}

fn load_jobs(data_dir: &PathBuf) -> HashMap<String, UserVideoJob> {
    fs::read(data_dir.join("video_jobs.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}
