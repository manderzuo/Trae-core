//! Local operator tool. No network or paid execution. The review file must be
//! produced from fresh authenticated worker observations by an authorized operator.
fn main() -> Result<(),Box<dyn std::error::Error>> {
    let args:Vec<String>=std::env::args().collect();
    if args.len()!=4 || args[1]!="--apply-reviewed" {
        return Err("usage: review_execution_slot --apply-reviewed CORE_ROOT REVIEW_JSON".into());
    }
    let proof:aiwork_core::ReviewedExecutionSlotRelease=serde_json::from_slice(&std::fs::read(&args[3])?)?;
    let store=aiwork_core::CoreStore::open(std::path::Path::new(&args[2]))?;store.migrate()?;
    let result=store.release_reviewed_unknown_execution_slot(&proof)?;
    let attention=store.release_attention_execution_slots(100)?;
    println!("{}",serde_json::json!({"schema":store.schema_version()?,"reviewed":format!("{result:?}"),"attentionReleased":attention,"keyActive":store.active_execution_count_for_key(&proof.core_key_id)?}));
    Ok(())
}
