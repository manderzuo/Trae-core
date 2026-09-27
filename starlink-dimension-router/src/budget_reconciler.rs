//! V2 recovery never submits upstream work or turns an unknown bill into zero.
use aiwork_core::{BudgetStepView, BudgetStepKind, BudgetExecutionState as Execution,
    BudgetReceiptInput, BudgetReceiptConflict, BillingReceipt, BillingReceiptStatus};
use serde_json::Value;
use crate::{bridge_client::BridgeClient, state::StarlinkRouterState};

fn identity(value: &Value, step: &BudgetStepView) -> Result<(), String> {
    for (name, expected) in [("request_id", &step.request_id), ("core_key_id", &step.core_key_id),
        ("budget_id", &step.budget_id), ("account_ref", &step.account_ref), ("bridge_instance_id", &step.bridge_instance_id)] {
        if value[name].as_str() != Some(expected.as_str()) { return Err(format!("v2 {name} binding mismatch")); }
    }
    Ok(())
}
pub(crate) fn request_path(step:&BudgetStepView,kind:&str)->String {
    // IDs are persisted locally, but must still be escaped before URL interpolation.
    let segment = |s: &str| s.bytes().map(|b| if b.is_ascii_alphanumeric() || matches!(b,b'-'|b'_'|b'.'|b'~') {
        (b as char).to_string()
    } else {format!("%{b:02X}")}).collect::<String>();
    format!("/internal/bridge/v2/requests/{}/{kind}?budget_id={}",segment(&step.request_id),segment(&step.budget_id))
}
pub(crate) fn read(client: &BridgeClient, step: &BudgetStepView, kind: &str) -> Result<Value,String> {
    let path=request_path(step,kind);
    let value=client.json_request("GET",&path,&[],Some(&step.request_id))?;
    if value["wire_version"] != 2 {return Err("unsupported budget wire version".into());}
    identity(&value,step)?;
    Ok(value)
}

pub(crate) fn sync_execution(state: &StarlinkRouterState, client: &BridgeClient, step: &BudgetStepView) -> Result<(),String> {
    let value=read(client,step,"execution")?;
    if value["status"] == "not_started" && value["execution"].is_null() {return Ok(());}
    let execution=&value["execution"];
    identity(execution,step)?;
    let kind=match step.kind {BudgetStepKind::Video=>"video",BudgetStepKind::Chat=>"chat",BudgetStepKind::Assist=>"assist"};
    if execution["step_kind"]!=kind || execution["state"]!=value["status"] {return Err("execution kind or state mismatch".into());}
    let next=match value["status"].as_str() {
        Some("succeeded")=>Execution::Succeeded,Some("failed")=>Execution::Failed,
        Some("unknown")=>Execution::Unknown,Some("running")=>Execution::Running,
        _=>return Err("invalid execution state".into()),
    };
    let terminal=matches!(next,Execution::Succeeded|Execution::Failed);
    if terminal && (execution["finished_at_ms"].as_i64().is_none_or(|v|v<0)
        || execution["result_available"]!=true) {return Err("terminal result has no durable evidence".into());}
    if let Some(task)=execution["task_ref"].as_str() {
        if step.kind!=BudgetStepKind::Video {return Err("non-video task reference".into());}
        state.store.bind_budget_video_task(&step.request_id,task).map_err(|e|e.to_string())?;
    }
    // Ignore stale nonterminal observations after a locally committed terminal state.
    if matches!(step.execution_state,Execution::Succeeded|Execution::Failed|Execution::Canceled) {
        if step.execution_state!=next {return Err("execution terminal conflict".into());}
    } else {
        state.store.mark_budget_step_execution(&step.request_id,next).map_err(|e|e.to_string())?;
    }
    // A successful helper alone must NOT finish a parent still awaiting video preparation.
    if terminal && step.kind!=BudgetStepKind::Assist && step.request_id==step.parent_request_id {
        state.store.finish_budget_execution(&step.parent_request_id,next).map_err(|e|e.to_string())?;
    }
    Ok(())
}

pub(crate) fn sync_receipt(state: &StarlinkRouterState, client: &BridgeClient, step: &BudgetStepView) -> Result<(),String> {
    let value=read(client,step,"billing")?;
    apply_receipt_envelope(state,step,&value)
}
fn apply_receipt_envelope(state:&StarlinkRouterState,step:&BudgetStepView,value:&Value)->Result<(),String> {
    let status=value["status"].as_str().ok_or("missing billing status")?;
    if status=="pending" && value["receipt"].is_null() && value["event"].is_null() {return Ok(());}
    let event=&value["event"]; identity(event,step)?;
    if event["wire_version"]!=2 || event["kind"]!=status {return Err("receipt event binding mismatch".into());}
    if status=="conflict" {
        // A changed candidate amount is evidence for quarantine, never a second debit.
        let evidence=if event["conflict"].is_object() {&event["conflict"]} else {&event["receipt"]};
        let source=evidence["source_ref"].as_str().filter(|s|!s.is_empty()).ok_or("conflict source missing")?;
        let observed=evidence["observed_at_ms"].as_i64().filter(|v|*v>=0).ok_or("conflict time missing")?;
        let hash=event["evidence_hash"].as_str().filter(|s|!s.is_empty()).ok_or("conflict hash missing")?;
        state.store.mark_budget_receipt_conflict(BudgetReceiptConflict {request_id:step.request_id.clone(),budget_id:step.budget_id.clone(),
            account_ref:step.account_ref.clone(),bridge_instance_id:step.bridge_instance_id.clone(),source_ref:source.into(),evidence_hash:hash.into(),observed_at_ms:observed}).map_err(|e|e.to_string())?;
        return Ok(());
    }
    if !matches!(status,"final"|"failed_no_charge") || event["receipt"]!=value["receipt"] || !event["conflict"].is_null() {
        return Err("invalid v2 receipt envelope".into());
    }
    let receipt:BillingReceipt=serde_json::from_value(value["receipt"].clone()).map_err(|_|"invalid exact receipt")?;
    if receipt.request_id!=step.request_id || !matches!(receipt.status,BillingReceiptStatus::Final|BillingReceiptStatus::FailedNoCharge)
        || (status=="failed_no_charge")!=(receipt.status==BillingReceiptStatus::FailedNoCharge) {return Err("receipt identity or status mismatch".into());}
    let no_send=receipt.status==BillingReceiptStatus::FailedNoCharge;
    if no_send && (event["confirmation_policy"]!="durable-local-no-send-v1" || !receipt.source_ref.starts_with("aiwork-v2-local-no-send:")) {
        return Err("no-send receipt lacks durable disposition provenance".into());
    }
    if no_send && !step.dispatch_attempted {
        if receipt.actual_credits.is_none_or(|v|v.as_microcredits()!=0) || receipt.unit!="credits"
            || receipt.observed_at_ms<=0 || receipt.task_ref.is_some() {return Err("invalid no-send release evidence".into());}
        state.store.release_unattempted_budget_step(&step.request_id,"bridge-durable-no-send").map_err(|e|e.to_string())?;
        // No paid dispatch was ever attempted. Release a reservation without
        // creating an invented upstream bill; the release CAS excludes dispatch.
        if step.request_id==step.parent_request_id {
            state.store.finish_budget_execution(&step.parent_request_id,Execution::Failed).map_err(|e|e.to_string())?;
        }
        return Ok(());
    }
    let applied=state.store.apply_budget_receipt(BudgetReceiptInput {budget_id:step.budget_id.clone(),account_ref:step.account_ref.clone(),bridge_instance_id:step.bridge_instance_id.clone(),receipt}).map_err(|e|e.to_string())?;
    if no_send && matches!(applied,aiwork_core::BudgetReceiptResult::Settled {..}|aiwork_core::BudgetReceiptResult::Duplicate) {
        state.store.mark_budget_step_execution(&step.request_id,Execution::Failed).map_err(|e|e.to_string())?;
        if step.kind!=BudgetStepKind::Assist && step.request_id==step.parent_request_id {
            state.store.finish_budget_execution(&step.parent_request_id,Execution::Failed).map_err(|e|e.to_string())?;
        }
    }
    Ok(())
}

pub(crate) fn reconcile_once(state: &StarlinkRouterState) {
    if let Err(error)=sync_event_page(state) {eprintln!("v2 receipt events: {error}");}
    if let Err(error)=reconcile_page(state, "") {eprintln!("v2 recovery index: {error}");}
}

fn sync_event_page(state:&StarlinkRouterState)->Result<(),String> {
    let client=state.bridge_client();
    // Empty generation is a read-only discovery, not permission to reuse a
    // cursor from an unrelated bridge instance or generation.
    let head=client.json_request("GET","/internal/bridge/v2/receipt-events?generation=&after=0&limit=1",&[],None)?;
    if head["wire_version"]!=2 {return Err("unsupported event protocol".into());}
    let instance=head["bridge_instance_id"].as_str().ok_or("event instance missing")?;
    let generation=head["generation"].as_str().ok_or("event generation missing")?;
    let mut after=state.store.budget_receipt_cursor(instance,generation).map_err(|e|e.to_string())?;
    let query_generation=generation.bytes().map(|b|if b.is_ascii_alphanumeric() || matches!(b,b'-'|b'_'|b'.'|b'~') {(b as char).to_string()} else {format!("%{b:02X}")}).collect::<String>();
    let page=client.json_request("GET",&format!("/internal/bridge/v2/receipt-events?generation={query_generation}&after={after}&limit=100"),&[],None)?;
    if page["wire_version"]!=2 || page["generation"]!=generation || page["bridge_instance_id"]!=instance {return Err("event page identity mismatch".into());}
    let events=page["events"].as_array().filter(|e|e.len()<=100).ok_or("invalid event page")?;
    for event in events {
        let next=event["sequence"].as_i64().filter(|n|*n>after).ok_or("event order invalid")?;
        let request=event["request_id"].as_str().filter(|s|!s.is_empty() && s.len()<=256).ok_or("event request missing")?;
        // The page is fenced to the active generation. Individual facts retain
        // their original generation across clean restart; full identity below
        // binds each replay to this persisted Core step, not to another budget.
        if !event["generation"].as_str().is_some_and(|s|!s.is_empty() && s.len()<=256 && !s.chars().any(char::is_control))
            || event["bridge_instance_id"]!=instance || event["wire_version"]!=2 {
            return Err("event generation mismatch".into());
        }
        let step=state.store.budget_step_for_request(request).map_err(|e|e.to_string())?;
        if let Some(step)=&step {
            identity(event,step)?;
            let mut envelope=event.clone();envelope["status"]=event["kind"].clone();envelope["event"]=event.clone();
            apply_receipt_envelope(state,step,&envelope)?;
        }
        state.store.advance_budget_receipt_cursor(instance,generation,after,next,if step.is_none(){Some(request)}else{None}).map_err(|e|e.to_string())?;
        after=next;
    }
    Ok(())
}

fn reconcile_page(state: &StarlinkRouterState, after: &str) -> Result<String,String> {
    let steps=state.store.pending_budget_steps_after(100,after).map_err(|e|e.to_string())?;
    let next=if steps.len()==100 {steps.last().unwrap().step.request_id.clone()} else {String::new()};
    // Never hold the shared configuration mutex across blocking I/O.
    let client=state.bridge_client().clone();
    for_each_bounded(&steps, |recovery| {
        let step=&recovery.step;
        if step.dispatch_attempted {
            if let Err(error)=sync_execution(state,&client,&step) {eprintln!("v2 execution recovery {}: {error}",step.request_id);}
        }
        // One invalid result or bill must not prevent another Key's reconciliation.
        if let Err(error)=sync_receipt(state,&client,&step) {eprintln!("v2 receipt recovery {}: {error}",step.request_id);}
    });
    Ok(next)
}

pub(crate) fn spawn(state: &std::sync::Arc<StarlinkRouterState>) {
    use std::{sync::{Arc,atomic::Ordering},time::Duration};
    if state.budget_reconciler_started.swap(true,Ordering::AcqRel) {return;}
    let weak=Arc::downgrade(state);
    tokio::spawn(async move {
        let mut after=String::new();
        let mut tick=tokio::time::interval(Duration::from_secs(2));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let Some(state)=weak.upgrade() else {break};
            let previous=after.clone();
            match tokio::task::spawn_blocking(move || {
                if let Err(error)=sync_event_page(&state) {eprintln!("v2 receipt events: {error}");}
                reconcile_page(&state,&previous)
            }).await {
                Ok(Ok(next))=>after=next,
                _=>eprintln!("v2 background reconciliation failed; next tick will retry"),
            }
        }
    });
}

fn for_each_bounded<T: Sync>(items: &[T], action: impl Fn(&T) + Sync) {
    use std::sync::atomic::{AtomicUsize,Ordering};
    let next=AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..items.len().min(4) {
            let action=&action; let next=&next;
            scope.spawn(move || loop {
                let index=next.fetch_add(1,Ordering::Relaxed);
                let Some(item)=items.get(index) else {break};
                action(item);
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn slow_bridge_read_does_not_block_another_keys_reconciliation() {
        use std::{sync::{Arc,Mutex,mpsc},time::Duration};
        let (release,blocked)=mpsc::channel();
        let blocked=Arc::new(Mutex::new(blocked));
        let (done,completed)=mpsc::channel();
        let worker=std::thread::spawn(move || for_each_bounded(&[0,1], |id| {
            if *id==0 {blocked.lock().unwrap().recv().unwrap();}
            else {done.send(()).unwrap();}
        }));
        let fast_completed=completed.recv_timeout(Duration::from_millis(300)).is_ok();
        release.send(()).unwrap();worker.join().unwrap();
        assert!(fast_completed,"a blocked read must not serialize unrelated Key reconciliation");
    }
}
