//! Resume only persisted successful helper work; never replay a paid helper.
use std::sync::Arc;
use crate::state::StarlinkRouterState;

pub(crate) fn save(state:&StarlinkRouterState,principal:&aiwork_core::Principal,request:&str,body:&serde_json::Value)->Result<(),String> {
    let fingerprint=state.store.request_fingerprint_for_billing(request).map_err(|_|"checkpoint request missing")?;
    let context=format!("budget-continuation-v1:{request}:{}:{fingerprint}",principal.key_id);
    let plaintext=zeroize::Zeroizing::new(serde_json::to_string(body).map_err(|_|"checkpoint encoding failed")?);
    let encrypted=state.key_vault.encrypt(&context,&plaintext).map_err(|_|"checkpoint encryption failed")?;
    state.store.save_budget_continuation(principal,request,body,encrypted.key_version,&encrypted.ciphertext).map_err(|_|"checkpoint persistence failed")?;
    Ok(())
}
struct Observer {state:Arc<StarlinkRouterState>,request:String}
impl Drop for Observer {
    fn drop(&mut self) {self.state.video_stream_observers.lock().unwrap_or_else(|e|e.into_inner()).remove(&self.request);}
}
pub(crate) fn spawn(state:&Arc<StarlinkRouterState>) {
    if !state.config.budget_billing_v2 {return;}
    let maintenance=Arc::downgrade(state);
    tokio::spawn(async move {
        let mut tick=tokio::time::interval(std::time::Duration::from_secs(60));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let Some(state)=maintenance.upgrade() else {break};
            let store=state.store.clone();
            // One small SQL batch, independent from paid work and its recovery.
            if !matches!(tokio::task::spawn_blocking(move ||store.prune_completed_budget_continuations(chrono::Utc::now().timestamp_millis(),8)).await,Ok(Ok(_))) {
                eprintln!("v2 completed-input retention failed; no billing records removed");
            }
        }
    });
    let weak=Arc::downgrade(state);
    tokio::spawn(async move {
        let slots=Arc::new(tokio::sync::Semaphore::new(4));let mut after=String::new();
        let mut tick=tokio::time::interval(std::time::Duration::from_secs(2));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let Some(state)=weak.upgrade() else {break};
            let store=state.store.clone();let cursor=after.clone();
            let page=match tokio::task::spawn_blocking(move ||store.pending_budget_continuation_ids_after(&cursor,100)).await {
                Ok(Ok(page))=>page,_=>{eprintln!("v2 continuation index unavailable; preserved for retry");continue;},
            };
            let full_page=page.len()==100;let mut consumed_page=true;
            for request in page {
                let Ok(permit)=slots.clone().try_acquire_owned() else {consumed_page=false;break};
                // Advance only across candidates actually visited. Advancing to
                // row 100 after using four slots would starve the other 96.
                after=request.clone();
                {
                    let mut busy=state.video_stream_observers.lock().unwrap_or_else(|e|e.into_inner());
                    if busy.len()>=128 || !busy.insert(request.clone()) {continue;}
                }
                let observer=Observer {state:state.clone(),request:request.clone()};let state=state.clone();
                tokio::spawn(async move {
                    let _permit=permit;let _observer=observer;
                    let store=state.store.clone();let id=request.clone();
                    let checkpoint=match tokio::task::spawn_blocking(move ||store.budget_continuation(&id)).await {
                        Ok(Ok(Some(checkpoint)))=>checkpoint,
                        Ok(Ok(None))=>return,
                        _=>{eprintln!("v2 continuation {request}: checkpoint unavailable; other requests continue");return;},
                    };
                    if let Err(code)=crate::budget_flow::resume_checkpoint(state.clone(),checkpoint).await {
                        crate::budget_flow::finish_definite_failure(&state,&request,&code);
                        let safe=crate::budget_errors::public_code(&code).unwrap_or("budget_continuation_requires_attention");
                        eprintln!("v2 continuation {request}: {safe}");
                    }
                });
            }
            if consumed_page && !full_page {after.clear();}
        }
    });
}
