use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Location and metadata for the lin binary that flow should launch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinRuntime {
    pub binary: PathBuf,
    pub version: Option<String>,
}

/// Path where the runtime metadata is stored.
pub fn runtime_path() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".config/flow/hub-runtime.json")
    } else {
        PathBuf::from(".config/flow/hub-runtime.json")
    }
}

/// Persist the runtime selection so flow can reuse it.
pub fn persist_runtime(runtime: &LinRuntime) -> Result<()> {
    let path = runtime_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let payload =
        serde_json::to_string_pretty(runtime).context("failed to serialize lin runtime info")?;
    fs::write(&path, payload).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

/// Load previously registered runtime details, if present.
pub fn load_runtime() -> Result<Option<LinRuntime>> {
    let path = runtime_path();
    if !path.exists() {
        return Ok(None);
    }
    let contents =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let runtime: LinRuntime = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse lin runtime at {}", path.display()))?;
    Ok(Some(runtime))
}

/// Best-effort version detection from the supplied binary.
pub fn detect_binary_version(path: &Path) -> Option<String> {
    Command::new(path)
        .arg("--version")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.trim().to_string())
}
