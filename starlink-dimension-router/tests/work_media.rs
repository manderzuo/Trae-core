use aiwork_core::{
    BeginRequest, BeginRequestInput, CoreStore, EncryptedWorkSnapshot, NewUser, Principal,
    UserRole, WorkAction,
};
use chrono::Utc;
use starlink_dimension_router::{
    assets::{self, ParsedAssetUpload},
    bridge_client::BridgeClient,
    config::RouterConfig,
    key_vault::KeyVault,
    state::StarlinkRouterState,
    work_media,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::PathBuf,
    sync::Arc,
};
const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfixture-owned-reference";
struct Fixture {
    state: Arc<StarlinkRouterState>,
    p: Principal,
    work: String,
    dir: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("work-media-{}", rand::random::<u64>()));
        let store = Arc::new(CoreStore::open(&dir).unwrap());
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
        let key = store
            .issue_api_key(
                "admin",
                "media",
                BTreeSet::from(["admin:*".into()]),
                "bootstrap",
            )
            .unwrap();
        let p = Principal {
            user_id: "admin".into(),
            key_id: key.id,
            scopes: BTreeSet::from(["admin:*".into()]),
        };
        let work = store.create_video_work(&p, "opaque-chat").unwrap().work_id;
        let state = StarlinkRouterState::for_test(
            store,
            BridgeClient::new("http://127.0.0.1:1", "unused"),
            RouterConfig::defaults(dir.clone()),
        );
        Self {
            state,
            p,
            work,
            dir,
        }
    }
    fn image(&self) -> ParsedAssetUpload {
        ParsedAssetUpload {
            filename: "reference.png".into(),
            declared_mime: Some("image/png".into()),
            bytes: PNG.into(),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn work_media_survives_upload_ttl_and_is_encrypted_at_rest() {
    let f = Fixture::new();
    let now = Utc::now().timestamp_millis();
    let media = work_media::pin(&f.state, &f.p, &f.work, &f.image(), now).unwrap();
    let later = now + 31 * 60 * 1000;
    let temporary = work_media::materialize(&f.state, &f.p, &media, later).unwrap();
    assert_eq!(
        assets::read_owned(&f.state.store, &f.dir, &f.p, &temporary.id)
            .unwrap()
            .bytes,
        PNG
    );
    let files = fs::read_dir(f.dir.join("data/work-media"))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(files.len(), 1);
    let encrypted = fs::read(files[0].path()).unwrap();
    assert!(!encrypted.windows(PNG.len()).any(|w| w == PNG));
    assert_eq!(media.size_bytes, encrypted.len() as i64);
    assert!(work_media::materialize(&f.state, &f.p, &media, now + 31 * 24 * 3600 * 1000).is_err());
}

#[test]
fn work_media_capacity_rejects_before_eviction() {
    let mut f = Fixture::new();
    Arc::get_mut(&mut f.state)
        .unwrap()
        .config
        .work_media_key_limit_bytes = 1;
    assert!(work_media::pin(
        &f.state,
        &f.p,
        &f.work,
        &f.image(),
        Utc::now().timestamp_millis()
    )
    .is_err());
    assert!(
        !f.dir.join("data/work-media").exists()
            || fs::read_dir(f.dir.join("data/work-media"))
                .unwrap()
                .next()
                .is_none()
    );
}

#[test]
fn media_rotation_and_inflight_cleanup() {
    let f = Fixture::new();
    let now = Utc::now().timestamp_millis();
    let m = work_media::pin(&f.state, &f.p, &f.work, &f.image(), now).unwrap();
    let req = match f
        .state
        .store
        .begin_billed_request(BeginRequestInput {
            user_id: f.p.user_id.clone(),
            api_key_id: f.p.key_id.clone(),
            protocol: "openai".into(),
            endpoint: "/v1/videos/generations".into(),
            model: "seedance".into(),
            idempotency_key: "media-lease".into(),
            body: serde_json::json!({}),
        })
        .unwrap()
    {
        BeginRequest::Created(h) => h.id,
        other => panic!("{other:?}"),
    };
    f.state
        .store
        .acquire_work_media_lease(&f.p, &m.media_id, &req)
        .unwrap();
    let context = aiwork_core::work_snapshot_context(&f.p.key_id, &f.work, &req);
    let snapshot = f
        .state
        .key_vault
        .encrypt(&context, "private-video-prompt")
        .unwrap();
    let version = f
        .state
        .store
        .bind_work_version(
            &f.p,
            &f.work,
            None,
            &req,
            WorkAction::Create,
            &EncryptedWorkSnapshot {
                key_version: 1,
                ciphertext: snapshot.ciphertext,
                snapshot_sha256: "a".repeat(64),
            },
        )
        .unwrap()
        .version;
    let hc = aiwork_core::work_handle_context(&f.p.key_id, &f.work, Some(&version.version_id));
    let h = f
        .state
        .key_vault
        .encrypt(&hc, "private-context-handle")
        .unwrap();
    let handle = f
        .state
        .store
        .save_work_handle(
            &f.p,
            &f.work,
            Some(&version.version_id),
            &"a".repeat(64),
            1,
            &h.ciphertext,
        )
        .unwrap();
    let newer = KeyVault::from_material(2, [0x77; 32], BTreeMap::from([(1, [0x5a; 32])])).unwrap();
    let next = StarlinkRouterState::for_test_with_key_vault(
        f.state.store.clone(),
        BridgeClient::new("http://127.0.0.1:1", "unused"),
        f.state.config.clone(),
        newer,
    );
    assert_eq!(work_media::rotate(&next).unwrap(), 3);
    let v = next
        .store
        .owned_work_version(&f.p, &version.version_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        next.key_vault
            .decrypt(
                &context,
                v.sealed_snapshot.key_version,
                &v.sealed_snapshot.ciphertext
            )
            .unwrap(),
        "private-video-prompt"
    );
    let h = next
        .store
        .owned_work_handle(&f.p, &handle.context_handle_sha256)
        .unwrap()
        .unwrap();
    assert_eq!(
        next.key_vault
            .decrypt(&hc, h.key_version, &h.encrypted_handle)
            .unwrap(),
        "private-context-handle"
    );
    let updated = next
        .store
        .owned_work_media(&f.p, &m.media_id)
        .unwrap()
        .unwrap();
    assert_eq!(updated.key_version, 2);
    assert!(work_media::materialize(&next, &f.p, &updated, now).is_ok());
    assert_eq!(
        work_media::cleanup(&next, now + 31 * 24 * 3600 * 1000).unwrap(),
        0
    );
    next.store.release_work_media_leases(&f.p, &req).unwrap();
    assert_eq!(
        work_media::cleanup(&next, now + 31 * 24 * 3600 * 1000).unwrap(),
        2
    );
}

#[test]
fn corrupt_private_reference_never_materializes_and_interrupted_publish_recovers() {
    let f = Fixture::new();
    let now = Utc::now().timestamp_millis();
    let m = work_media::pin(&f.state, &f.p, &f.work, &f.image(), now).unwrap();
    let file = fs::read_dir(f.dir.join("data/work-media"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let con = rusqlite::Connection::open(f.dir.join("data/core.sqlite3")).unwrap();
    con.execute(
        "UPDATE video_work_media SET state='preparing' WHERE media_id=?1",
        [&m.media_id],
    )
    .unwrap();
    assert_eq!(work_media::recover(&f.state).unwrap(), 1);
    assert!(work_media::materialize(&f.state, &f.p, &m, now).is_ok());
    fs::write(&file, vec![0xff; m.size_bytes as usize]).unwrap();
    assert!(work_media::materialize(&f.state, &f.p, &m, now).is_err());
    fs::remove_file(&file).unwrap();
    con.execute(
        "UPDATE video_work_media SET state='preparing' WHERE media_id=?1",
        [&m.media_id],
    )
    .unwrap();
    assert_eq!(work_media::recover(&f.state).unwrap(), 1);
    assert!(f
        .state
        .store
        .owned_work_media(&f.p, &m.media_id)
        .unwrap()
        .is_none());
    assert_eq!(
        work_media::cleanup(&f.state, now + 2 * 3600 * 1000).unwrap(),
        0
    );
}

#[test]
fn unavailable_old_key_does_not_discard_preparing_media() {
    let f = Fixture::new();
    let m = work_media::pin(
        &f.state,
        &f.p,
        &f.work,
        &f.image(),
        Utc::now().timestamp_millis(),
    )
    .unwrap();
    let con = rusqlite::Connection::open(f.dir.join("data/core.sqlite3")).unwrap();
    con.execute(
        "UPDATE video_work_media SET state='preparing' WHERE media_id=?1",
        [&m.media_id],
    )
    .unwrap();
    let wrong = KeyVault::from_material(2, [0x77; 32], BTreeMap::new()).unwrap();
    let s = StarlinkRouterState::for_test_with_key_vault(
        f.state.store.clone(),
        BridgeClient::new("http://127.0.0.1:1", "unused"),
        f.state.config.clone(),
        wrong,
    );
    assert!(work_media::recover(&s).is_err());
    assert_eq!(
        f.state.store.work_media_records().unwrap()[0].state,
        "preparing"
    );
}

#[test]
fn orphan_cleanup_preserves_recent_files_and_foreign_names() {
    let f = Fixture::new();
    let now = Utc::now().timestamp_millis();
    work_media::pin(&f.state, &f.p, &f.work, &f.image(), now).unwrap();
    let dir = f.dir.join("data/work-media");
    let orphan = dir.join(format!("media_{}-{}.bin", "a".repeat(32), "b".repeat(32)));
    fs::write(&orphan, b"orphan").unwrap();
    fs::write(dir.join("user-content.bin"), b"not ours").unwrap();
    assert_eq!(work_media::cleanup(&f.state, now).unwrap(), 0);
    assert_eq!(
        work_media::cleanup(&f.state, now + 2 * 3600 * 1000).unwrap(),
        1
    );
    assert!(!orphan.exists());
    assert!(dir.join("user-content.bin").exists());
}
