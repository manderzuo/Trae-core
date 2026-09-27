use std::{sync::{Arc, Weak}, time::Duration};

use crate::state::StarlinkRouterState;

pub const KEY_REGISTRY_SYNC_INTERVAL: Duration = Duration::from_secs(60);

pub(crate) fn sync_now(state:&StarlinkRouterState)->Result<(),String> {
    let mut last=String::new();
    // Concurrent sends of identical metadata share one persisted revision. A
    // real metadata change may overtake an older in-flight snapshot: retry one
    // fresh snapshot, never serialize paid I/O behind a shared network lock.
    for _ in 0..2 {
        let (version,keys)=state.store.bridge_api_key_snapshot(chrono::Utc::now().timestamp_millis()).map_err(|e|e.to_string())?;
        match state.bridge_client().sync_core_key_registry(version,keys) {
            Ok(_)=>return Ok(()),Err(error)=>last=error,
        }
    }
    Err(last)
}

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
                if !state_for_sync.bridge_client()
                    .background_registry_sync_enabled()
                {
                    return Ok::<_, String>(());
                }
                sync_now(&state_for_sync)
            }).await;
            tokio::time::sleep(KEY_REGISTRY_SYNC_INTERVAL).await;
        }
    });
}
