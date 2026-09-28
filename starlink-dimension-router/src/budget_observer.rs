//! HTTP and background work share a bounded, unwind-safe request owner.
use std::{collections::HashSet,sync::{Arc,Mutex}};

pub(crate) struct Observer {owners:Arc<Mutex<HashSet<String>>>,request:String}
impl Observer {
    pub(crate) fn acquire(owners:Arc<Mutex<HashSet<String>>>,request:String)->Option<Self> {
        Self::acquire_with_capacity_rejection(owners,request,|| {})
    }
    pub(crate) fn acquire_with_capacity_rejection(owners:Arc<Mutex<HashSet<String>>>,request:String,reject:impl FnOnce())->Option<Self> {
        {
            let mut busy=owners.lock().unwrap_or_else(|e|e.into_inner());
            if busy.contains(&request) {return None;}
            if busy.len()>=128 {reject();return None;}
            busy.insert(request.clone());
        }
        Some(Self {owners,request})
    }
}
impl Drop for Observer {
    fn drop(&mut self) {self.owners.lock().unwrap_or_else(|e|e.into_inner()).remove(&self.request);}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashSet,sync::{Arc,Mutex}};

    #[tokio::test]
    async fn canceled_or_panicked_worker_releases_only_its_own_request() {
        let owners=Arc::new(Mutex::new(HashSet::new()));
        let other=Observer::acquire(owners.clone(),"other".into()).unwrap();
        let guard=Observer::acquire(owners.clone(),"canceled".into()).unwrap();
        assert!(Observer::acquire(owners.clone(),"canceled".into()).is_none());
        let (send,ready)=tokio::sync::oneshot::channel();
        let task=tokio::spawn(async move {let _guard=guard;let _=send.send(());std::future::pending::<()>().await;});
        ready.await.unwrap();task.abort();assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(*owners.lock().unwrap(),HashSet::from(["other".to_string()]));
        let guard=Observer::acquire(owners.clone(),"panicked".into()).unwrap();
        assert!(tokio::spawn(async move {let _guard=guard;panic!("fixture worker panic")}).await.unwrap_err().is_panic());
        assert_eq!(*owners.lock().unwrap(),HashSet::from(["other".to_string()]));
        drop(other);assert!(owners.lock().unwrap().is_empty());
    }

    #[test]
    fn capacity_rejection_never_removes_an_existing_owner() {
        let owners=Arc::new(Mutex::new(HashSet::new()));
        let guards=(0..128).map(|i|Observer::acquire(owners.clone(),i.to_string()).unwrap()).collect::<Vec<_>>();
        assert!(Observer::acquire(owners.clone(),"extra".into()).is_none());
        assert!(Observer::acquire_with_capacity_rejection(owners.clone(),"0".into(),|| panic!("existing owner must never be cleaned")).is_none());
        let mut rejected=false;
        assert!(Observer::acquire_with_capacity_rejection(owners.clone(),"extra".into(),|| rejected=true).is_none());
        assert!(rejected);
        assert_eq!(owners.lock().unwrap().len(),128);drop(guards);
        assert!(Observer::acquire(owners,"extra".into()).is_some());
    }
}
