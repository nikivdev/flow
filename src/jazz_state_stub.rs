use anyhow::Result;

use crate::base_tool;
use crate::config;
use crate::history::InvocationRecord;

const DEFAULT_DB_DIR: &str = ".config/flow/jazz2";
const DEFAULT_REPO_ROOT: &str = "~/repos/garden-co/jazz2";

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
    if let Ok(path) = std::env::var("FLOW_JAZZ2_PATH") {
        return config::expand_path(&path);
    }
    let repo_root = config::expand_path(DEFAULT_REPO_ROOT);
    if repo_root.exists() {
        return repo_root.join(".jazz2");
    }
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(DEFAULT_DB_DIR)
}
