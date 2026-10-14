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
    let mut binding=if state.config.work_context_for_key(&principal.key_id) {
        Some(crate::work_execution::freeze_context(state,principal,headers,body)?)
    }else{None};
    let mut normalized=body.clone();
    if normalized.get("messages").is_none() {normalized["messages"]=serde_json::json!([{"role":"user","content":normalized["prompt"]}]);}
    let needs_inputs=!crate::work_planner::clear_reference(&crate::work_planner::current_text(&normalized))
        && !binding.as_ref().is_some_and(|b|b.clarification.is_some())
        && crate::work_planner::read_only_decision(body,binding.as_ref().is_some_and(|b|b.base_version_id.is_some())).is_none();
    if let Err(code)=if needs_inputs {crate::reference_context::recover(state,principal,&mut normalized)} else {Ok(())} {
        // Inherited references are resolved from the immutable parent snapshot.
        if code!="reference_image_missing" || binding.as_ref().and_then(|b|b.base_version_id.as_ref()).is_none() {return Err(code.into());}
    }
    let has_assets=["image_asset_ids","video_asset_ids"].iter().any(|f|normalized[*f].as_array().is_some_and(|v|!v.is_empty()));
    let mut retained=serde_json::Map::new();
    if needs_inputs && has_assets {
        aiwork_core::require_scope(principal,"assets:write").map_err(|_|"insufficient_scope")?;
        let association=binding.as_ref().map(|b|b.association.clone()).unwrap_or_else(||format!("checkpoint:{request}"));
        let work=match binding.as_ref().and_then(|b|b.work_id.clone()) {
            Some(id)=>id,
            None=>state.store.create_video_work(principal,&association).map_err(|_|"work_context_unavailable")?.work_id,
        };
        if let Some(b)=binding.as_mut() {b.work_id=Some(work.clone());}
        for field in ["image_asset_ids","video_asset_ids"] {
            let mut ids=Vec::new();
            for id in normalized[field].as_array().into_iter().flatten() {
                let owned=crate::assets::read_reference(state,principal,id.as_str().ok_or("invalid_reference_context")?)?;
                let upload=crate::assets::ParsedAssetUpload {filename:owned.record.filename,declared_mime:Some(owned.record.mime_type),bytes:owned.bytes};
                let media=crate::work_media::pin(state,principal,&work,&upload,chrono::Utc::now().timestamp_millis())?;
                state.store.acquire_work_media_lease(principal,&media.media_id,request).map_err(|_|"work_media_unavailable")?;
                ids.push(media.media_id);
            }
            retained.insert(field.into(),serde_json::json!(ids));
        }
    }
    if needs_inputs && !has_assets && crate::budget_flow::inline_images(&normalized)?.is_empty() && body["action"]!="create" {
        if let Some(version_id)=binding.as_ref().and_then(|b|b.base_version_id.as_ref()) {
            let version=state.store.owned_work_version(principal,version_id).map_err(|_|"work_context_unavailable")?.ok_or("work_context_unavailable")?;
            let snapshot=crate::work_context::read_snapshot(state,principal,&version)?;
            let mut images=Vec::new();let mut videos=Vec::new();
            for id in snapshot.user_media_ids {
                let media=state.store.owned_work_media(principal,&id).map_err(|_|"reference_asset_unavailable")?.ok_or("reference_asset_missing")?;
                if media.expires_at_ms<=chrono::Utc::now().timestamp_millis() && !state.store.work_media_has_lease(principal,&id,None).map_err(|_|"reference_asset_unavailable")? {return Err("reference_asset_expired".into());}
                state.store.acquire_work_media_lease(principal,&id,request).map_err(|_|"work_media_unavailable")?;
                if media.kind=="video" {videos.push(id);} else {images.push(id);}
            }
            if !images.is_empty(){retained.insert("image_asset_ids".into(),serde_json::json!(images));}
            if !videos.is_empty(){retained.insert("video_asset_ids".into(),serde_json::json!(videos));}
        }
    }
    if let Some(b)=binding.as_mut() {
        if body.get("messages").is_none() && body["prompt"].as_str().is_some_and(|s|!s.trim().is_empty()) && b.work_id.is_none() && b.clarification.is_none() {
            b.work_id=Some(state.store.create_video_work(principal,&b.association).map_err(|_|"work_context_unavailable")?.work_id);
        }
    }
    // The original body remains the immutable request fingerprint. Only this
    // authenticated envelope holds the owned material used during recovery.
    let stored=serde_json::json!({"format":"aiwork-checkpoint-v2","body":body,"binding":binding,"retained_inputs":retained,"retained_input_origin":if has_assets {"current"} else {"inherited"}});
    let plaintext=zeroize::Zeroizing::new(serde_json::to_string(&stored).map_err(|_|"checkpoint encoding failed")?);
    let encrypted=state.key_vault.encrypt(&context,&plaintext).map_err(|_|"checkpoint encryption failed")?;
    state.store.save_budget_continuation(principal,request,body,encrypted.key_version,&encrypted.ciphertext).map_err(|_|"checkpoint persistence failed")?;
    Ok(())
}

pub(crate) fn decode_checkpoint(raw:&str)->Result<(serde_json::Value,Option<crate::work_execution::ContextBinding>),String> {
    let v:serde_json::Value=serde_json::from_str(raw).map_err(|_|"budget_checkpoint_invalid")?;
    if v["format"]=="aiwork-work-checkpoint-v1" || v["format"]=="aiwork-checkpoint-v2" {
        let binding=if v["binding"].is_null() {None} else {Some(serde_json::from_value(v["binding"].clone()).map_err(|_|"budget_checkpoint_invalid")?)};
        if !v["body"].is_object(){return Err("budget_checkpoint_invalid".into());}
        Ok((v["body"].clone(),binding))
    }else{Ok((v,None))}
}
pub(crate) fn restore_inputs(state:&StarlinkRouterState,p:&aiwork_core::Principal,request:&str,body:&serde_json::Value)->Result<serde_json::Value,String> {
    restore_inputs_scoped(state,p,request,body,false)
}
pub(crate) fn restore_current_inputs(state:&StarlinkRouterState,p:&aiwork_core::Principal,request:&str,body:&serde_json::Value)->Result<serde_json::Value,String> {
    restore_inputs_scoped(state,p,request,body,true)
}
fn restore_inputs_scoped(state:&StarlinkRouterState,p:&aiwork_core::Principal,request:&str,body:&serde_json::Value,current_only:bool)->Result<serde_json::Value,String> {
    let Some(saved)=state.store.budget_continuation(request).map_err(|_|"budget_checkpoint_unavailable")? else {return Ok(body.clone());};
    let raw=zeroize::Zeroizing::new(state.key_vault.decrypt(&saved.encryption_context(),saved.key_version,&saved.ciphertext).map_err(|_|"budget_checkpoint_invalid")?);
    let envelope:serde_json::Value=serde_json::from_str(&raw).map_err(|_|"budget_checkpoint_invalid")?;
    let (original,_)=decode_checkpoint(&raw)?;
    if aiwork_core::canonical_json_hash(&original)!=aiwork_core::canonical_json_hash(body) {return Err("budget_checkpoint_invalid".into());}
    if envelope["format"]!="aiwork-checkpoint-v2" {
        let mut restored=body.clone();
        if crate::reference_context::has_current_marker(&restored) {
            crate::reference_context::recover_checkpoint(state,p,&mut restored)?;
        }
        for field in ["image_asset_ids","video_asset_ids"] {
            for id in restored[field].as_array().into_iter().flatten() {
                crate::assets::read_reference(state,p,id.as_str().ok_or("invalid_reference_context")?)?;
            }
        }
        return Ok(restored);
    }
    let mut restored=body.clone();
    // Older v2 envelopes predate explicit provenance. Their immutable original
    // body identifies current uploads/markers; absent those, retained IDs came
    // from the parent snapshot and must not enter an independent creation.
    let current=match envelope["retained_input_origin"].as_str() {
        Some("current")=>true,
        Some("inherited")=>false,
        Some(_)=>return Err("budget_checkpoint_invalid".into()),
        None=>["image_asset_ids","video_asset_ids"].iter().any(|f|body[*f].as_array().is_some_and(|ids|!ids.is_empty())) || crate::reference_context::has_current_marker(body),
    };
    if current_only && !current {return Ok(restored);}
    if envelope["retained_inputs"].as_object().is_some_and(|m|!m.is_empty()) {
        crate::reference_context::strip_body_markers(&mut restored);
    }
    for field in ["image_asset_ids","video_asset_ids"] {
        if let Some(ids)=envelope["retained_inputs"][field].as_array() {
            let mut assets=Vec::new();
            for id in ids {
                let id=id.as_str().ok_or("budget_checkpoint_invalid")?;
                let media=state.store.owned_work_media(p,id).map_err(|_|"reference_asset_unavailable")?.ok_or("reference_asset_missing")?;
                if !state.store.work_media_has_lease(p,id,Some(request)).map_err(|_|"reference_asset_unavailable")? {return Err("budget_checkpoint_invalid".into());}
                let asset=crate::work_media::materialize(state,p,&media,chrono::Utc::now().timestamp_millis())?;
                assets.push(asset.id);
            }
            restored[field]=serde_json::json!(assets);
        }
    }
    Ok(restored)
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
            if !matches!(tokio::task::spawn_blocking(move || {
                store.release_attention_execution_slots(8)?;
                store.prune_completed_budget_continuations(chrono::Utc::now().timestamp_millis(),8)
            }).await,Ok(Ok(_))) {
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
                        _=>{record_error(&state,&request,"budget_checkpoint_invalid");return;},
                    };
                    if let Err(code)=crate::budget_flow::resume_checkpoint(state.clone(),checkpoint).await {
                        crate::budget_flow::finish_definite_failure(&state,&request,&code);
                        record_error(&state,&request,&code);
                    }
                });
            }
            if consumed_page && !full_page {after.clear();}
        }
    });
}

pub(crate) fn ensure_retry_due(state:&StarlinkRouterState,request:&str)->Result<(),String> {
    match state.store.budget_continuation_retry_block(request,chrono::Utc::now().timestamp_millis()).map_err(|_|"budget_checkpoint_invalid")?.as_deref() {
        Some("attention")=>Err("budget_continuation_requires_attention".into()),
        Some(_)=>Err("budget_continuation_deferred".into()),
        None=>Ok(()),
    }
}

pub(crate) fn record_error(state:&StarlinkRouterState,request:&str,code:&str) {
    // Being denied another attempt is not itself an attempted continuation.
    if matches!(code,"budget_continuation_deferred"|"budget_continuation_requires_attention") {return;}
    let safe=crate::budget_errors::public_code(code).unwrap_or("budget_continuation_requires_attention");
    match state.store.record_budget_continuation_error(request,safe,chrono::Utc::now().timestamp_millis()) {
        Ok(true)=>eprintln!("v2 continuation {request}: attention required after bounded retries: {safe}"),
        Ok(false)=>eprintln!("v2 continuation {request}: {safe}"),
        Err(_)=>eprintln!("v2 continuation {request}: retry persistence unavailable"),
    }
}
