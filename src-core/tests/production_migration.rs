//! Explicit opt-in rehearsal against an immutable, private production backup.
use std::{collections::BTreeMap,path::Path};
use rusqlite::{Connection,OpenFlags};

fn quote(name:&str)->String {format!("\"{}\"",name.replace('"',"\"\""))}
fn rows(db:&Connection,table:&str,cols:&[String])->Vec<Vec<rusqlite::types::Value>> {
    let sql=format!("SELECT {} FROM {} ORDER BY {}",cols.iter().map(|c|quote(c)).collect::<Vec<_>>().join(","),quote(table),cols.iter().map(|c|quote(c)).collect::<Vec<_>>().join(","));
    db.prepare(&sql).unwrap().query_map([],|row|(0..cols.len()).map(|n|row.get(n)).collect()).unwrap().map(Result::unwrap).collect()
}
#[test]
#[ignore="requires explicit private immutable Core backup; never opens production writable"]
fn production_backup_migrates_without_changing_historical_rows() {
    assert_eq!(std::env::var("CORE_MIGRATION_SNAPSHOT_ACK").as_deref(),Ok("1"));
    let source=std::env::var("CORE_MIGRATION_SNAPSHOT").unwrap();
    assert!(Path::new(&source).is_absolute());
    let source_bytes=std::fs::read(&source).unwrap();
    let db=Connection::open_with_flags(&source,OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let tables:Vec<String>=db.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' AND name!='schema_meta' ORDER BY name").unwrap().query_map([],|r|r.get(0)).unwrap().map(Result::unwrap).collect();
    let mut history=BTreeMap::new();
    for table in tables {
        let columns:Vec<String>=db.prepare(&format!("PRAGMA table_info({})",quote(&table))).unwrap().query_map([],|r|r.get(1)).unwrap().map(Result::unwrap).collect();
        history.insert(table.clone(),(columns.clone(),rows(&db,&table,&columns)));
    }
    drop(db);
    let root=std::env::temp_dir().join(format!("core-migration-rehearsal-{:032x}",rand::random::<u128>()));
    std::fs::create_dir_all(root.join("data")).unwrap();
    std::fs::write(root.join("data/core.sqlite3"),&source_bytes).unwrap();
    let store=aiwork_core::CoreStore::open(&root).unwrap();store.migrate().unwrap();drop(store);
    let migrated=Connection::open_with_flags(root.join("data/core.sqlite3"),OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert_eq!(migrated.query_row("PRAGMA integrity_check",[],|r|r.get::<_,String>(0)).unwrap(),"ok");
    assert_eq!(migrated.query_row("SELECT value FROM schema_meta WHERE key='schema_version'",[],|r|r.get::<_,String>(0)).unwrap(),"28");
    for (table,(columns,before)) in &history {assert!(&rows(&migrated,table,columns)==before,"historical rows changed in {table}; values intentionally redacted");}
    println!("Real Core backup migrated to schema28; {} historical tables preserved",history.len());
    drop(migrated);
    assert!(std::fs::read(source).unwrap()==source_bytes,"immutable backup must not change");
    std::fs::remove_dir_all(root).unwrap();
}
