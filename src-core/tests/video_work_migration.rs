use aiwork_core::{BeginRequest, BeginRequestInput, CoreStore, NewUser, UserRole, CORE_DB_FILE};
use rusqlite::{params, Connection};
use serde_json::json;
use std::{collections::BTreeSet, fs};

#[test]
fn v29_to_v30_preserves_ledger_and_unknown_holds() {
    let dir = std::env::temp_dir().join(format!("video-work-migration-{}", rand::random::<u64>()));
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
    let key = store
        .issue_api_key(
            "admin",
            "migration",
            BTreeSet::from(["admin:*".into()]),
            "bootstrap",
        )
        .unwrap();
    let request = match store
        .begin_billed_request(BeginRequestInput {
            user_id: "admin".into(),
            api_key_id: key.id.clone(),
            protocol: "openai".into(),
            endpoint: "/v1/videos/generations".into(),
            model: "seedance".into(),
            idempotency_key: "migration-held".into(),
            body: json!({"prompt":"fixture"}),
        })
        .unwrap()
    {
        BeginRequest::Created(h) => h,
        other => panic!("{other:?}"),
    };
    drop(store);
    let db = dir.join("data").join(CORE_DB_FILE);
    let con = Connection::open(&db).unwrap();
    con.execute("INSERT INTO quota_reservations (id,user_id,request_id,resource_kind,amount,state,expires_at_ms,created_at_ms,api_key_id) VALUES ('unknown-hold','admin',?1,'credits',123000000,'held',9999999999999,1,?2)", params![request.id,key.id]).unwrap();
    con.execute_batch("DROP TABLE IF EXISTS video_work_contexts; DROP TABLE IF EXISTS video_work_versions; DROP TABLE IF EXISTS video_work_media; DROP TABLE IF EXISTS video_works; UPDATE schema_meta SET value='29' WHERE key='schema_version';").unwrap();
    let before = snapshot(&con);
    drop(con);
    let reopened = CoreStore::open(&dir).unwrap();
    reopened.migrate().unwrap();
    reopened.migrate().unwrap();
    assert_eq!(reopened.schema_version().unwrap(), 30);
    for name in [
        "video_works",
        "video_work_versions",
        "video_work_media",
        "video_work_contexts",
    ] {
        assert_eq!(reopened.table_count(name).unwrap(), 1);
    }
    let con = Connection::open(&db).unwrap();
    assert_eq!(snapshot(&con), before);
    assert_eq!(
        reopened
            .reservation_for_request(&request.id)
            .unwrap()
            .unwrap()
            .amount,
        123000000
    );
    drop(con);
    drop(reopened);
    fs::remove_dir_all(dir).unwrap();
}

fn snapshot(con: &Connection) -> Vec<String> {
    [
        "requests",
        "quota_reservations",
        "quota_ledger",
        "budget_operations",
        "budget_steps",
        "billing_receipts",
    ]
    .iter()
    .map(|table| {
        let mut s = con
            .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
            .unwrap();
        let cols = s.column_count();
        let mut rows = s.query([]).unwrap();
        let mut out = String::from(*table);
        while let Some(row) = rows.next().unwrap() {
            for n in 0..cols {
                out.push_str(&format!("|{:?}", row.get_ref(n).unwrap()));
            }
        }
        out
    })
    .collect()
}
