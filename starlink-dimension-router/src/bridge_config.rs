use std::{fs, sync::Arc};

use sha2::{Digest, Sha256};
use serde::Deserialize;

use crate::{bridge_client::BridgeClient, config::{BridgeConfig, RouterConfig}, dto::BridgeStatusSnapshot, state::StarlinkRouterState};

#[derive(Clone, Debug, Deserialize)]
pub struct BridgeConfigCandidate {
    pub base_url: String,
    pub api_key: String,
}

#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error("桥接地址不能为空")]
    EmptyUrl,
    #[error("桥接 API Key 不能为空")]
    EmptyKey,
    #[error("桥接测试失败: {0}")]
    TestFailed(String),
    #[error("保存桥接配置失败: {0}")]
    SaveFailed(String),
}

#[derive(Clone)]
pub struct BridgeConfigStore {
    state: Arc<StarlinkRouterState>,
    verified_base_url: Arc<std::sync::Mutex<String>>,
    verified_digest: Arc<std::sync::Mutex<String>>,
}

#[cfg(test)]
mod tests {
    use super::{BridgeConfigCandidate, BridgeConfigStore};

    #[test]
    fn failed_bridge_test_does_not_replace_last_good_config() {
        let store = BridgeConfigStore::with_verified("https://good.example", "digest");
        assert!(store.test_then_save(BridgeConfigCandidate { base_url: "http://127.0.0.1:9".into(), api_key: "fake-secret".into() }).is_err());
        assert_eq!(store.base_url(), "https://good.example");
    }
}

impl BridgeConfigStore {
    pub fn new(state: Arc<StarlinkRouterState>) -> Self {
        let (base_url, digest) = state
            .config
            .bridge
            .as_ref()
            .map(|c| (c.base_url.clone(), c.key_fingerprint.clone()))
            .unwrap_or_default();
        Self {
            state,
            verified_base_url: Arc::new(std::sync::Mutex::new(base_url)),
            verified_digest: Arc::new(std::sync::Mutex::new(digest)),
        }
    }

    #[cfg(test)]
    pub fn with_verified(base_url: &str, digest: &str) -> Self {
        use crate::config::RouterConfig;
        let config = RouterConfig::defaults(std::env::temp_dir().join(format!("bridge-config-{}", rand::random::<u64>())));
        let state = StarlinkRouterState::for_test(
            Arc::new(aiwork_core::CoreStore::open(&config.data_dir).unwrap()),
            BridgeClient::new(base_url, "test"),
            config,
        );
        Self { state, verified_base_url: Arc::new(std::sync::Mutex::new(base_url.into())), verified_digest: Arc::new(std::sync::Mutex::new(digest.into())) }
    }

    pub fn base_url(&self) -> String { self.verified_base_url.lock().unwrap().clone() }

    pub fn test_then_save(&self, candidate: BridgeConfigCandidate) -> Result<BridgeStatusSnapshot, BridgeError> {
        let base_url = candidate.base_url.trim().trim_end_matches('/');
        let api_key = candidate.api_key.trim();
        if base_url.is_empty() { return Err(BridgeError::EmptyUrl); }
        if api_key.is_empty() { return Err(BridgeError::EmptyKey); }
        let client = BridgeClient::new(base_url, api_key);
        let status = client.test().map_err(BridgeError::TestFailed)?;
        let default_model = status.get("default_model").and_then(|value| value.as_str()).unwrap_or("deepseek-v4-flash");
        let model_count = status.get("model_count").and_then(|value| value.as_u64()).unwrap_or(0) as usize;
        let digest = hex::encode(Sha256::digest(api_key.as_bytes()));
        let mut config = self.state.config.clone();
        config.bridge = Some(BridgeConfig { base_url: base_url.to_string(), key_id: "bridge-admin".into(), key_fingerprint: digest.clone() });
        config.persist().map_err(BridgeError::SaveFailed)?;
        fs::create_dir_all(&config.data_dir).map_err(|e| BridgeError::SaveFailed(e.to_string()))?;
        fs::write(config.data_dir.join("bridge_secret.txt"), api_key).map_err(|e| BridgeError::SaveFailed(e.to_string()))?;
        self.state.replace_bridge(client);
        *self.verified_base_url.lock().unwrap() = base_url.to_string();
        *self.verified_digest.lock().unwrap() = digest;
        Ok(BridgeStatusSnapshot::connected(base_url, default_model, model_count))
    }

    pub fn public_status(&self) -> serde_json::Value {
        let persisted = fs::read(self.state.config.data_dir.join("router.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<RouterConfig>(&bytes).ok())
            .and_then(|config| config.bridge);
        let base_url = persisted.as_ref().map(|value| value.base_url.clone()).unwrap_or_else(|| self.base_url());
        let key_fingerprint = persisted.as_ref().map(|value| value.key_fingerprint.clone()).unwrap_or_else(|| self.verified_digest.lock().unwrap().clone());
        serde_json::json!({"configured": !base_url.is_empty(), "base_url": base_url, "key_fingerprint": key_fingerprint})
    }
}
