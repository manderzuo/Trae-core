use std::{collections::BTreeSet, fs, path::PathBuf};

use aiwork_core::{CoreStore, KeyQuotaGrant, NewUser, Principal, QuotaGrant, UserRole};
use rusqlite::Connection;

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "aiwork-core-key-deletion-migration-{}",
            rand::random::<u64>()
        )))
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn v20_to_v21_migration_preserves_keys_and_quota_history_and_is_idempotent() {
    let dir = TestDir::new();
    let store = CoreStore::open(&dir.0).unwrap();
    store.migrate().unwrap();
    store
        .create_bootstrap_admin(
            NewUser {
                id: "admin".into(),
                name: "管理员".into(),
                role: UserRole::Admin,
            },
            "bootstrap",
        )
        .unwrap();
    let admin = Principal {
        user_id: "admin".into(),
        key_id: "admin_session:migration-test".into(),
        scopes: BTreeSet::from(["admin:*".into()]),
    };
    let key = store
        .issue_api_key_for_new_user_as_admin(
            &admin,
            "迁移保留 Key",
            BTreeSet::from(["chat:invoke".into()]),
            1,
        )
        .unwrap();
    store
        .quota_pool_grant_as_admin(
            &admin,
            QuotaGrant {
                user_id: key.user_id.clone(),
                resource_kind: "credits".into(),
                amount: 1_234_567,
                actor_user_id: "admin".into(),
                reason: "migration preservation fixture".into(),
            },
        )
        .unwrap();
    store
        .key_quota_allocate_from_pool_as_admin(
            &admin,
            KeyQuotaGrant {
                api_key_id: key.id.clone(),
                resource_kind: "credits".into(),
                amount: 1_234_567,
                actor_user_id: "admin".into(),
                reason: "migration preservation fixture".into(),
            },
        )
        .unwrap();
    drop(store);

    let database = dir.0.join("data").join(aiwork_core::CORE_DB_FILE);
    let connection = Connection::open(&database).unwrap();
    let has_deleted_at: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('api_keys') WHERE name = 'deleted_at_ms')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    if has_deleted_at {
        connection
            .execute("ALTER TABLE api_keys DROP COLUMN deleted_at_ms", [])
            .unwrap();
    }
    connection
        .execute(
            "UPDATE schema_meta SET value = '20' WHERE key = 'schema_version'",
            [],
        )
        .unwrap();
    drop(connection);

    let upgraded = CoreStore::open(&dir.0).unwrap();
    upgraded.migrate().unwrap();
    assert_eq!(upgraded.schema_version().unwrap(), 22);
    assert_eq!(aiwork_core::CURRENT_SCHEMA_VERSION, 22);

    let listed = upgraded
        .list_api_keys_as_admin(&admin, Some(&key.user_id))
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, key.id);
    assert_eq!(listed[0].status, "active");
    assert_eq!(listed[0].key_quota[0].available, 1_234_567);

    let connection = Connection::open(&database).unwrap();
    let deleted_at: Option<i64> = connection
        .query_row(
            "SELECT deleted_at_ms FROM api_keys WHERE id = ?1",
            [&key.id],
            |row| row.get(0),
        )
        .unwrap();
    let ledger_entries: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM quota_ledger WHERE api_key_id = ?1",
            [&key.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(deleted_at, None);
    assert_eq!(ledger_entries, 1);
    drop(connection);

    upgraded.migrate().unwrap();
    assert_eq!(upgraded.schema_version().unwrap(), 22);
}
