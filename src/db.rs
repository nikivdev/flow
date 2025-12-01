use std::path::PathBuf;

use anyhow::{Context, Result};
use rusqlite::Connection;

/// Path to the shared SQLite database.
pub fn db_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config/flow/flow.db")
}

/// Open the SQLite database, creating parent directories if needed.
pub fn open_db() -> Result<Connection> {
    let path = db_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create db dir {}", parent.display()))?;
    }
    Connection::open(path).context("failed to open flow.db")
}
