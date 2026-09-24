use std::{sync::{Arc, Weak}, time::Duration};

use crate::state::StarlinkRouterState;

pub const KEY_REGISTRY_SYNC_INTERVAL: Duration = Duration::from_secs(60);

/// Periodically mirrors only opaque key ids, display names and enabled state
/// to AI Work. The sync is full-snapshot and retried on the next interval;
/// Core remains the only authority for key validity and quota.
pub fn spawn(state: &Arc<StarlinkRouterState>) {
    let weak_state: Weak<StarlinkRouterState> = Arc::downgrade(state);
    tokio::spawn(async move {
        loop {
            let Some(state) = weak_state.upgrade() else { break; };
            let state_for_sync = state.clone();
            let _ = tokio::task::spawn_blocking(move || {
                if !state_for_sync.bridge.lock().unwrap_or_else(|error| error.into_inner())
                    .background_registry_sync_enabled()
                {
                    return Ok::<_, String>(());
                }
                let keys = state_for_sync.store.bridge_api_key_metadata()
                    .map_err(|error| error.to_string())?;
                let version = chrono::Utc::now().timestamp_millis();
                state_for_sync.bridge.lock().unwrap_or_else(|error| error.into_inner())
                    .sync_core_key_registry(version, keys)
                    .map(|_| ())
            }).await;
            tokio::time::sleep(KEY_REGISTRY_SYNC_INTERVAL).await;
        }
    });
}
