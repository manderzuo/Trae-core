//! Resume only persisted successful helper work; never replay a paid helper.
use std::sync::Arc;
use crate::state::StarlinkRouterState;

pub(crate) fn save_with_headers(state:&StarlinkRouterState,principal:&aiwork_core::Principal,request:&str,body:&serde_json::Value,headers:&axum::http::HeaderMap)->Result<(),String> {
    if let Some(saved)=state.store.budget_continuation(request).map_err(|_|"checkpoint persistence failed")? {
        state.store.save_budget_continuation(principal,request,body,saved.key_version,&saved.ciphertext).map_err(|_|"checkpoint identity conflict")?;
        return Ok(());
    }
    let fingerprint=state.store.request_fingerprint_for_billing(request).map_err(|_|"checkpoint request missing")?;
    let context=format!("budget-continuation-v1:{request}:{}:{fingerprint}",principal.key_id);
    let stored=if state.config.work_context_for_key(&principal.key_id) {
        let mut binding=crate::work_execution::freeze_context(state,principal,headers,body)?;
        if body.get("messages").is_none() && body["prompt"].as_str().is_some_and(|s|!s.trim().is_empty()) && binding.work_id.is_none() && binding.clarification.is_none() {
            binding.work_id=Some(state.store.create_video_work(principal,&binding.association).map_err(|_|"work_context_unavailable")?.work_id);
        }
        serde_json::json!({"format":"aiwork-work-checkpoint-v1","body":body,"binding":binding})
    }else{body.clone()};
    let plaintext=zeroize::Zeroizing::new(serde_json::to_string(&stored).map_err(|_|"checkpoint encoding failed")?);
    let encrypted=state.key_vault.encrypt(&context,&plaintext).map_err(|_|"checkpoint encryption failed")?;
    state.store.save_budget_continuation(principal,request,body,encrypted.key_version,&encrypted.ciphertext).map_err(|_|"checkpoint persistence failed")?;
    Ok(())
}

pub(crate) fn decode_checkpoint(raw:&str)->Result<(serde_json::Value,Option<crate::work_execution::ContextBinding>),String> {
    let v:serde_json::Value=serde_json::from_str(raw).map_err(|_|"budget_checkpoint_invalid")?;
    if v["format"]=="aiwork-work-checkpoint-v1" {
        let binding=serde_json::from_value(v["binding"].clone()).map_err(|_|"budget_checkpoint_invalid")?;
        if !v["body"].is_object(){return Err("budget_checkpoint_invalid".into());}
        Ok((v["body"].clone(),Some(binding)))
    }else{Ok((v,None))}
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
                let Some(observer)=crate::budget_observer::Observer::acquire(state.video_stream_observers.clone(),request.clone()) else {continue};
                let state=state.clone();
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
