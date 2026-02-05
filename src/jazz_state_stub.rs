use anyhow::Result;

use crate::history::InvocationRecord;

pub fn record_task_run(_record: &InvocationRecord) -> Result<()> {
    Ok(())
}

pub fn state_dir() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".config/flow/jazz2")
}
