use aiwork_core::{
    BeginRequest, BeginRequestInput, CoreStore, EncryptedWorkSnapshot, NewUser, Principal,
    UserRole, WorkAction, WorkMediaRef, WorkVersionState,
};
use std::{collections::BTreeSet, fs, path::PathBuf};

struct Fixture {
    dir: PathBuf,
    store: CoreStore,
    owner: Principal,
    other: Principal,
}
impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("video-work-{}", rand::random::<u64>()));
        let store = CoreStore::open(&dir).unwrap();
        store.migrate().unwrap();
        store
            .create_user(
                NewUser {
                    id: "admin".into(),
                    name: "Admin".into(),
                    role: UserRole::Admin,
                },
                "bootstrap",
            )
            .unwrap();
        let a = store
            .issue_api_key(
                "admin",
                "A",
                BTreeSet::from(["videos:submit".into()]),
                "bootstrap",
            )
            .unwrap();
        let b = store
            .issue_api_key(
                "admin",
                "B",
                BTreeSet::from(["videos:submit".into()]),
                "bootstrap",
            )
            .unwrap();
        Self {
            dir,
            store,
            owner: Principal {
                user_id: "admin".into(),
                key_id: a.id,
                scopes: BTreeSet::new(),
            },
            other: Principal {
                user_id: "admin".into(),
                key_id: b.id,
                scopes: BTreeSet::new(),
            },
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn work_owner_isolation() {
    let f = Fixture::new();
    let work = f
        .store
        .create_video_work(&f.owner, "random-chat-A")
        .unwrap();
    assert_eq!(
        f.store
            .owned_video_work(&f.owner, &work.work_id)
            .unwrap()
            .unwrap()
            .work_id,
        work.work_id
    );
    assert!(f
        .store
        .owned_video_work(&f.other, &work.work_id)
        .unwrap()
        .is_none());
    let other = f
        .store
        .create_video_work(&f.other, "random-chat-A")
        .unwrap();
    assert_ne!(work.work_id, other.work_id);
    f.store.revoke_api_key(&f.owner.key_id, "admin").unwrap();
    assert!(f.store.owned_video_work(&f.owner, &work.work_id).is_err());
}

#[test]
fn same_conversation_retries_reuse_work_and_restart() {
    let f = Fixture::new();
    let work = f
        .store
        .create_video_work(&f.owner, "opaque-conversation")
        .unwrap();
    assert_eq!(
        f.store
            .create_video_work(&f.owner, "opaque-conversation")
            .unwrap()
            .work_id,
        work.work_id
    );
    let reopened = CoreStore::open(&f.dir).unwrap();
    reopened.migrate().unwrap();
    assert_eq!(
        reopened
            .owned_video_work(&f.owner, &work.work_id)
            .unwrap()
            .unwrap(),
        work
    );
    assert!(f.store.create_video_work(&f.owner, "").is_err());
}

fn request(f: &Fixture, p: &Principal, label: &str) -> String {
    match f
        .store
        .begin_billed_request(BeginRequestInput {
            user_id: p.user_id.clone(),
            api_key_id: p.key_id.clone(),
            protocol: "openai".into(),
            endpoint: "/v1/videos/generations".into(),
            model: "seedance".into(),
            idempotency_key: label.into(),
            body: serde_json::json!({"prompt":label}),
        })
        .unwrap()
    {
        BeginRequest::Created(h) => h.id,
        other => panic!("{other:?}"),
    }
}
fn sealed() -> EncryptedWorkSnapshot {
    EncryptedWorkSnapshot {
        key_version: 1,
        ciphertext: vec![0x7c; 96],
        snapshot_sha256: "a".repeat(64),
    }
}

#[test]
fn snapshot_encrypted_and_restart_readable() {
    let f = Fixture::new();
    let work = f.store.create_video_work(&f.owner, "version-chat").unwrap();
    let req = request(&f, &f.owner, "v1");
    let first = f
        .store
        .bind_work_version(
            &f.owner,
            &work.work_id,
            None,
            &req,
            WorkAction::Create,
            &sealed(),
        )
        .unwrap();
    assert!(first.created);
    let reopened = CoreStore::open(&f.dir).unwrap();
    reopened.migrate().unwrap();
    let result = reopened
        .owned_work_version(&f.owner, &first.version.version_id)
        .unwrap()
        .unwrap();
    assert_eq!(result.sealed_snapshot.ciphertext, vec![0x7c; 96]);
    assert_eq!(result.sealed_snapshot.key_version, 1);
    assert!(reopened
        .owned_work_version(&f.other, &result.version_id)
        .unwrap()
        .is_none());
    let db = rusqlite::Connection::open(f.dir.join("data/core.sqlite3")).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT typeof(encrypted_snapshot) FROM video_work_versions",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "blob"
    );
}

#[test]
fn request_binding_is_immutable_and_parallel_branches_keep_parent() {
    let f = Fixture::new();
    let w = f.store.create_video_work(&f.owner, "branches").unwrap();
    let req = request(&f, &f.owner, "v1");
    let v = f
        .store
        .bind_work_version(
            &f.owner,
            &w.work_id,
            None,
            &req,
            WorkAction::Create,
            &sealed(),
        )
        .unwrap();
    assert!(
        !f.store
            .bind_work_version(
                &f.owner,
                &w.work_id,
                None,
                &req,
                WorkAction::Create,
                &sealed()
            )
            .unwrap()
            .created
    );
    let mut changed = sealed();
    changed.snapshot_sha256 = "b".repeat(64);
    assert!(f
        .store
        .bind_work_version(
            &f.owner,
            &w.work_id,
            None,
            &req,
            WorkAction::Create,
            &changed
        )
        .is_err());
    let ra = request(&f, &f.owner, "branch-A");
    let rb = request(&f, &f.owner, "branch-B");
    let a = f
        .store
        .bind_work_version(
            &f.owner,
            &w.work_id,
            Some(&v.version.version_id),
            &ra,
            WorkAction::Revise,
            &sealed(),
        )
        .unwrap();
    let b = f
        .store
        .bind_work_version(
            &f.owner,
            &w.work_id,
            Some(&v.version.version_id),
            &rb,
            WorkAction::Continue,
            &sealed(),
        )
        .unwrap();
    assert_eq!((a.version.ordinal, b.version.ordinal), (2, 3));
    assert_eq!(a.version.parent_version_id, b.version.parent_version_id);
    f.store
        .set_work_version_state(&f.owner, &ra, WorkVersionState::Running)
        .unwrap();
    f.store
        .set_work_version_state(&f.owner, &ra, WorkVersionState::Completed)
        .unwrap();
    assert!(f
        .store
        .set_work_version_state(&f.owner, &ra, WorkVersionState::Running)
        .is_err());
    assert_eq!(
        f.store
            .owned_work_version(&f.owner, &b.version.version_id)
            .unwrap()
            .unwrap()
            .state,
        WorkVersionState::Preparing
    );
    let foreign_req = request(&f, &f.other, "foreign");
    assert!(f
        .store
        .bind_work_version(
            &f.owner,
            &w.work_id,
            None,
            &foreign_req,
            WorkAction::Create,
            &sealed()
        )
        .is_err());
}

fn media(work: &str, key: &str, id: &str, digest: &str, size: i64) -> WorkMediaRef {
    WorkMediaRef {
        media_id: id.into(),
        work_id: work.into(),
        owner_key_id: key.into(),
        kind: "image".into(),
        content_sha256: digest.repeat(64),
        encrypted_storage_ref: vec![0xa7; 64],
        key_version: 1,
        size_bytes: size,
        expires_at_ms: 1000,
        state: "preparing".into(),
    }
}

#[test]
fn work_media_capacity_rejects_before_eviction_and_leases_survive_restart() {
    let f = Fixture::new();
    let w = f.store.create_video_work(&f.owner, "media").unwrap();
    let m = media(&w.work_id, &f.owner.key_id, "media-A", "a", 80);
    let saved = f.store.reserve_work_media(&f.owner, &m, 100, 120).unwrap();
    f.store
        .activate_work_media(&f.owner, &saved.media_id)
        .unwrap();
    let extra = media(&w.work_id, &f.owner.key_id, "media-B", "b", 30);
    assert!(f
        .store
        .reserve_work_media(&f.owner, &extra, 100, 120)
        .is_err());
    assert_eq!(
        f.store
            .owned_work_media(&f.owner, &m.media_id)
            .unwrap()
            .unwrap()
            .state,
        "active"
    );
    assert!(f
        .store
        .owned_work_media(&f.other, &m.media_id)
        .unwrap()
        .is_none());
    let req = request(&f, &f.owner, "media-lease");
    f.store
        .acquire_work_media_lease(&f.owner, &m.media_id, &req)
        .unwrap();
    let reopened = CoreStore::open(&f.dir).unwrap();
    reopened.migrate().unwrap();
    assert!(reopened.claim_expired_work_media(1001).unwrap().is_empty());
    reopened.release_work_media_leases(&f.owner, &req).unwrap();
    assert_eq!(
        reopened.claim_expired_work_media(1001).unwrap()[0].media_id,
        m.media_id
    );
    assert!(reopened
        .owned_work_media(&f.owner, &m.media_id)
        .unwrap()
        .is_none());
}

#[test]
fn context_handle_remains_stable_after_retry_and_restart() {
    let f = Fixture::new();
    let w = f.store.create_video_work(&f.owner, "handle").unwrap();
    let a = f
        .store
        .save_work_handle(
            &f.owner,
            &w.work_id,
            None,
            &"a".repeat(64),
            1,
            &vec![0x70; 64],
        )
        .unwrap();
    let b = f
        .store
        .save_work_handle(
            &f.owner,
            &w.work_id,
            None,
            &"b".repeat(64),
            1,
            &vec![0x80; 64],
        )
        .unwrap();
    assert_eq!(a.context_handle_sha256, b.context_handle_sha256);
    assert_eq!(b.encrypted_handle, vec![0x70; 64]);
    let reopened = CoreStore::open(&f.dir).unwrap();
    reopened.migrate().unwrap();
    assert!(reopened
        .owned_work_handle(&f.other, &a.context_handle_sha256)
        .unwrap()
        .is_none());
    assert_eq!(
        reopened
            .owned_work_handle(&f.owner, &a.context_handle_sha256)
            .unwrap()
            .unwrap()
            .encrypted_handle,
        vec![0x70; 64]
    );
}

#[test]
fn removing_one_branch_keeps_other_branch_media_and_unknown_execution() {
    let f = Fixture::new();
    let w = f
        .store
        .create_video_work(&f.owner, "delete-branch")
        .unwrap();
    let r1 = request(&f, &f.owner, "base");
    let base = f
        .store
        .bind_work_version(
            &f.owner,
            &w.work_id,
            None,
            &r1,
            WorkAction::Create,
            &sealed(),
        )
        .unwrap()
        .version;
    let r2 = request(&f, &f.owner, "child");
    let child = f
        .store
        .bind_work_version(
            &f.owner,
            &w.work_id,
            Some(&base.version_id),
            &r2,
            WorkAction::Revise,
            &sealed(),
        )
        .unwrap()
        .version;
    let m = media(&w.work_id, &f.owner.key_id, "shared-media", "c", 10);
    f.store.reserve_work_media(&f.owner, &m, 100, 100).unwrap();
    f.store.activate_work_media(&f.owner, &m.media_id).unwrap();
    f.store
        .set_work_version_state(&f.owner, &r1, WorkVersionState::Running)
        .unwrap();
    f.store
        .set_work_version_state(&f.owner, &r1, WorkVersionState::Completed)
        .unwrap();
    f.store
        .set_work_version_state(&f.owner, &r2, WorkVersionState::Unknown)
        .unwrap();
    assert!(f
        .store
        .revoke_work_version(&f.owner, &child.version_id)
        .is_err());
    f.store
        .revoke_work_version(&f.owner, &base.version_id)
        .unwrap();
    assert!(f
        .store
        .owned_work_version(&f.owner, &base.version_id)
        .unwrap()
        .is_none());
    assert!(f
        .store
        .owned_work_version(&f.owner, &child.version_id)
        .unwrap()
        .is_some());
    assert!(f
        .store
        .owned_work_media(&f.owner, &m.media_id)
        .unwrap()
        .is_some());
}

#[test]
fn media_global_capacity_and_concurrent_branches_are_serialized() {
    let f = Fixture::new();
    let w = f.store.create_video_work(&f.owner, "concurrent").unwrap();
    let other = f.store.create_video_work(&f.other, "global").unwrap();
    let m = media(&w.work_id, &f.owner.key_id, "global-a", "a", 80);
    f.store.reserve_work_media(&f.owner, &m, 1000, 100).unwrap();
    let n = media(&other.work_id, &f.other.key_id, "global-b", "b", 30);
    assert!(f.store.reserve_work_media(&f.other, &n, 1000, 100).is_err());
    let req = request(&f, &f.owner, "concurrent-base");
    let v = f
        .store
        .bind_work_version(
            &f.owner,
            &w.work_id,
            None,
            &req,
            WorkAction::Create,
            &sealed(),
        )
        .unwrap()
        .version;
    let ra = request(&f, &f.owner, "parallel-a");
    let rb = request(&f, &f.owner, "parallel-b");
    let (a, b) = std::thread::scope(|scope| {
        let a = scope.spawn(|| {
            f.store
                .bind_work_version(
                    &f.owner,
                    &w.work_id,
                    Some(&v.version_id),
                    &ra,
                    WorkAction::Revise,
                    &sealed(),
                )
                .unwrap()
        });
        let b = scope.spawn(|| {
            f.store
                .bind_work_version(
                    &f.owner,
                    &w.work_id,
                    Some(&v.version_id),
                    &rb,
                    WorkAction::Revise,
                    &sealed(),
                )
                .unwrap()
        });
        (a.join().unwrap(), b.join().unwrap())
    });
    assert_ne!(a.version.ordinal, b.version.ordinal);
    assert_eq!(a.version.parent_version_id, b.version.parent_version_id);
}
