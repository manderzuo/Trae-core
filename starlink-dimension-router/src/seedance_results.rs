//! In-flight result fan-out. Billing identity remains in SQLite, never in this map.
use std::{collections::HashMap,sync::{Arc,Mutex}};
use serde_json::Value;
use tokio::sync::{watch,OwnedSemaphorePermit,Semaphore};
use crate::seedance_feedback::{Progress,Stage};

type Outcome=Result<Value,String>;
type SharedOutcome=Option<Arc<Outcome>>;
struct Entry {receiver:watch::Receiver<SharedOutcome>,progress:watch::Sender<Progress>}
pub(crate) struct SeedanceResults {
    // Serialize durable identity/checkpoint creation with registration. An
    // immediate duplicate cannot become leader before the fresh caller does.
    pub(crate) admission:Mutex<()>,
    entries:Mutex<HashMap<String,Entry>>,
    slots:Arc<Semaphore>,
}
impl Default for SeedanceResults {
    fn default()->Self {Self {admission:Mutex::new(()),entries:Mutex::new(HashMap::new()),slots:Arc::new(Semaphore::new(256))}}
}
pub(crate) struct Subscription {
    receiver:watch::Receiver<SharedOutcome>,
    progress:watch::Receiver<Progress>,
    _global:OwnedSemaphorePermit,
}
pub(crate) struct Publisher {registry:Arc<SeedanceResults>,request:String,sender:watch::Sender<SharedOutcome>}
impl SeedanceResults {
    pub(crate) fn subscribe(self:&Arc<Self>,request:&str)->Result<(Subscription,Option<Publisher>),&'static str> {
        let global=self.slots.clone().try_acquire_owned().map_err(|_|"stream_observer_limit")?;
        let mut entries=self.entries.lock().unwrap_or_else(|e|e.into_inner());
        if let Some(entry)=entries.get(request) {
            return Ok((Subscription {receiver:entry.receiver.clone(),progress:entry.progress.subscribe(),_global:global},None));
        }
        if entries.len()>=128 {return Err("budget_preparation_busy");}
        let (sender,receiver)=watch::channel(None);
        let (progress,progress_receiver)=watch::channel(Progress::default());
        entries.insert(request.into(),Entry {receiver:receiver.clone(),progress});
        Ok((Subscription {receiver,progress:progress_receiver,_global:global},Some(Publisher {registry:self.clone(),request:request.into(),sender})))
    }
    pub(crate) fn progress(&self,request:&str,stage:Stage) {
        if let Some(entry)=self.entries.lock().unwrap_or_else(|e|e.into_inner()).get(request) {
            entry.progress.send_if_modified(|current| {
                let changed=current.stage!=stage;current.stage=stage;
                if stage!=Stage::QueryDelayed {current.last_confirmed_at_ms=Some(chrono::Utc::now().timestamp_millis());}
                changed
            });
        }
    }
}
impl Subscription {
    pub(crate) fn progress_receiver(&self)->watch::Receiver<Progress> {self.progress.clone()}
    pub(crate) async fn result(mut self)->Outcome {
        loop {
            if let Some(result)=self.receiver.borrow().clone() {return (*result).clone();}
            self.receiver.changed().await.map_err(|_|"seedance_budget_execution_failed".to_string())?;
        }
    }
}
impl Publisher {
    pub(crate) fn complete(self,result:Outcome) {self.sender.send_replace(Some(Arc::new(result)));}
}
impl Drop for Publisher {
    fn drop(&mut self) {
        // Unwind/cancellation must complete every waiter, never leave a ghost owner.
        if self.sender.borrow().is_none() {
            self.sender.send_replace(Some(Arc::new(Err("seedance_budget_execution_failed".into()))));
        }
        self.registry.entries.lock().unwrap_or_else(|e|e.into_inner()).remove(&self.request);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use serde_json::json;

    #[tokio::test]
    async fn canceled_publisher_completes_waiters_and_releases_registry() {
        let registry=Arc::new(SeedanceResults::default());
        let (first,owner)=registry.subscribe("one").unwrap();
        let (second,no_owner)=registry.subscribe("one").unwrap();assert!(no_owner.is_none());
        let owner=owner.unwrap();let (ready,entered)=tokio::sync::oneshot::channel();
        let task=tokio::spawn(async move {let _owner=owner;ready.send(()).unwrap();std::future::pending::<()>().await;});
        entered.await.unwrap();task.abort();assert!(task.await.unwrap_err().is_cancelled());
        for waiter in [first,second] {assert_eq!(tokio::time::timeout(Duration::from_secs(1),waiter.result()).await.unwrap().unwrap_err(),"seedance_budget_execution_failed");}
        assert!(registry.entries.lock().unwrap().is_empty());assert_eq!(registry.slots.available_permits(),256);
        let (_,owner)=registry.subscribe("one").unwrap();assert!(owner.is_some());
    }

    #[tokio::test]
    async fn subscriber_limits_release_on_disconnect_without_cancelling_worker() {
        let registry=Arc::new(SeedanceResults::default());
        let (first,owner)=registry.subscribe("one").unwrap();let mut waiters=vec![first];
        for _ in 1..256 {waiters.push(registry.subscribe("one").unwrap().0);}
        assert!(matches!(registry.subscribe("one"),Err("stream_observer_limit")));
        drop(waiters.pop());let (replacement,duplicate_owner)=registry.subscribe("one").unwrap();assert!(duplicate_owner.is_none());
        drop(waiters);owner.unwrap().complete(Ok(json!({"request_id":"one","status":"completed"})));
        assert_eq!(replacement.result().await.unwrap()["status"],"completed");
        assert!(registry.entries.lock().unwrap().is_empty());assert_eq!(registry.slots.available_permits(),256);
    }

    #[tokio::test]
    async fn different_requests_are_independent_and_global_capacity_is_bounded() {
        let registry=Arc::new(SeedanceResults::default());let mut owners=Vec::new();let mut waiters=Vec::new();
        for i in 0..16 {
            let id=format!("request-{i}");let (first,owner)=registry.subscribe(&id).unwrap();waiters.push(first);owners.push(owner.unwrap());
            for _ in 1..16 {waiters.push(registry.subscribe(&id).unwrap().0);}
        }
        assert!(matches!(registry.subscribe("extra"),Err("stream_observer_limit")));
        drop(waiters.pop());let (extra,owner)=registry.subscribe("extra").unwrap();owner.unwrap().complete(Ok(json!({"id":"extra"})));
        assert_eq!(extra.result().await.unwrap()["id"],"extra","unrelated requests do not wait for earlier owners");
        drop(owners);drop(waiters);assert!(registry.entries.lock().unwrap().is_empty());assert_eq!(registry.slots.available_permits(),256);
        let mut owners=Vec::new();
        for i in 0..128 {let (waiter,owner)=registry.subscribe(&format!("run-{i}")).unwrap();drop(waiter);owners.push(owner.unwrap());}
        assert!(matches!(registry.subscribe("run-extra"),Err("budget_preparation_busy")));
        drop(owners);assert!(registry.entries.lock().unwrap().is_empty());
    }
}
