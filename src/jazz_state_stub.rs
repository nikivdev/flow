use anyhow::Result;

use crate::history::InvocationRecord;
use crate::base_tool;

pub fn record_task_run(record: &InvocationRecord) -> Result<()> {
    // Best-effort: never fail the parent task run if base isn't installed or errors out.
    let Some(bin) = base_tool::resolve_bin() else {
        return Ok(());
    };

    let Ok(payload) = serde_json::to_string(record) else {
        return Ok(());
    };

    let args: Vec<String> = vec!["ingest".to_string(), "task-run".to_string()];
    let _ = base_tool::run_with_stdin(&bin, &args, &payload);
    Ok(())
}

pub fn state_dir() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".config/flow/jazz2")
}
