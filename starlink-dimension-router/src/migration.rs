use std::{collections::BTreeMap, fs, path::{Path, PathBuf}};

use serde::Serialize;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Serialize)]
pub struct MigrationReport {
    pub migration_id: String,
    pub source_root: String,
    pub target_root: String,
    pub source_hashes: BTreeMap<String, String>,
    pub file_count: usize,
    pub unmapped_records: Vec<String>,
    pub applied: bool,
}

pub fn inspect(source_root: impl AsRef<Path>, target_root: impl AsRef<Path>, migration_id: &str) -> Result<MigrationReport, String> {
    let source_root = source_root.as_ref();
    let target_root = target_root.as_ref();
    if !source_root.exists() { return Err("源数据目录不存在".into()); }
    let mut hashes = BTreeMap::new();
    for name in ["core.sqlite3", "data/api_keys.json", "data/remaining_credits.json", "data/video_tasks.json"] {
        let path = source_root.join(name);
        if let Ok(bytes) = fs::read(&path) {
            hashes.insert(name.into(), hex::encode(Sha256::digest(bytes)));
        }
    }
    Ok(MigrationReport { migration_id: migration_id.into(), source_root: source_root.display().to_string(), target_root: target_root.display().to_string(), file_count: hashes.len(), source_hashes: hashes, unmapped_records: vec!["legacy account-pool rows are intentionally not imported automatically".into()], applied: false })
}

pub fn apply(report: &MigrationReport, confirmed: bool) -> Result<MigrationReport, String> {
    if !confirmed { return Err("迁移必须由管理员显式确认".into()); }
    let source = PathBuf::from(&report.source_root);
    let target = PathBuf::from(&report.target_root);
    fs::create_dir_all(&target).map_err(|e| e.to_string())?;
    let target_db = target.join("data").join("core.sqlite3");
    if target_db.exists() { return Err("目标 Core 已有数据库，拒绝覆盖".into()); }
    let source_db = source.join("data").join("core.sqlite3");
    if source_db.exists() {
        fs::create_dir_all(target.join("data")).map_err(|e| e.to_string())?;
        fs::copy(source_db, &target_db).map_err(|e| e.to_string())?;
    }
    let mut next = report.clone();
    next.applied = true;
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::{apply, inspect};
    #[test]
    fn migration_requires_explicit_confirmation_and_never_deletes_source() {
        let root = std::env::temp_dir().join(format!("starlink-migration-{}", rand::random::<u64>()));
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::write(root.join("data/api_keys.json"), b"[]").unwrap();
        let target = root.join("target");
        let report = inspect(&root, &target, "mig-1").unwrap();
        assert!(apply(&report, false).is_err());
        assert!(root.join("data/api_keys.json").exists());
        let _ = std::fs::remove_dir_all(root);
    }
}
