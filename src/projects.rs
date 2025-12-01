use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::{db, running};

/// Single project record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectEntry {
    pub name: String,
    pub project_root: PathBuf,
    pub config_path: PathBuf,
    pub updated_ms: u128,
}

/// Persist the project name -> path mapping. Idempotent.
pub fn register_project(name: &str, config_path: &Path) -> Result<()> {
    let canonical_config = config_path
        .canonicalize()
        .unwrap_or_else(|_| config_path.to_path_buf());
    let project_root = config_path
        .parent()
        .unwrap_or(Path::new("."))
        .canonicalize()
        .unwrap_or_else(|_| config_path.parent().unwrap_or(Path::new(".")).to_path_buf());

    let conn = open_db()?;
    create_schema(&conn)?;
    conn.execute(
        r#"
        INSERT INTO projects (name, project_root, config_path, updated_ms)
        VALUES (?1, ?2, ?3, ?4)
        ON CONFLICT(name) DO UPDATE SET
            project_root=excluded.project_root,
            config_path=excluded.config_path,
            updated_ms=excluded.updated_ms
        "#,
        params![
            name,
            project_root.to_string_lossy(),
            canonical_config.to_string_lossy(),
            running::now_ms() as i64
        ],
    )
    .context("failed to upsert project")?;

    Ok(())
}

/// Return the most recent entry for a given project name, if present.
pub fn resolve_project(name: &str) -> Result<Option<ProjectEntry>> {
    let conn = open_db()?;
    create_schema(&conn)?;

    let mut stmt = conn.prepare(
        "SELECT name, project_root, config_path, updated_ms FROM projects WHERE name = ?1",
    )?;
    let mut rows = stmt.query([name])?;
    if let Some(row) = rows.next()? {
        let entry = ProjectEntry {
            name: row.get(0)?,
            project_root: PathBuf::from(row.get::<_, String>(1)?),
            config_path: PathBuf::from(row.get::<_, String>(2)?),
            updated_ms: row.get::<_, i64>(3)? as u128,
        };
        Ok(Some(entry))
    } else {
        Ok(None)
    }
}

/// List all registered projects, ordered by most recently updated.
pub fn list_projects() -> Result<Vec<ProjectEntry>> {
    let conn = open_db()?;
    create_schema(&conn)?;

    let mut stmt = conn.prepare(
        "SELECT name, project_root, config_path, updated_ms FROM projects ORDER BY updated_ms DESC",
    )?;
    let mut rows = stmt.query([])?;
    let mut entries = Vec::new();
    while let Some(row) = rows.next()? {
        entries.push(ProjectEntry {
            name: row.get(0)?,
            project_root: PathBuf::from(row.get::<_, String>(1)?),
            config_path: PathBuf::from(row.get::<_, String>(2)?),
            updated_ms: row.get::<_, i64>(3)? as u128,
        });
    }
    Ok(entries)
}

/// Print all registered projects.
pub fn show_projects() -> Result<()> {
    let projects = list_projects()?;
    if projects.is_empty() {
        println!("No registered projects.");
        println!("Projects are registered when you run a task in a flow.toml with a 'name' field.");
        return Ok(());
    }

    println!("Registered projects:\n");
    for entry in &projects {
        let age = format_age(entry.updated_ms);
        println!("  {} ({})", entry.name, age);
        println!("    {}", entry.project_root.display());
    }
    Ok(())
}

fn format_age(timestamp_ms: u128) -> String {
    let now = running::now_ms();
    let elapsed_secs = ((now.saturating_sub(timestamp_ms)) / 1000) as u64;

    if elapsed_secs < 60 {
        format!("{}s ago", elapsed_secs)
    } else if elapsed_secs < 3600 {
        format!("{}m ago", elapsed_secs / 60)
    } else if elapsed_secs < 86400 {
        format!("{}h ago", elapsed_secs / 3600)
    } else {
        format!("{}d ago", elapsed_secs / 86400)
    }
}

fn open_db() -> Result<Connection> {
    db::open_db()
}

fn create_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS projects (
            name TEXT PRIMARY KEY,
            project_root TEXT NOT NULL,
            config_path TEXT NOT NULL,
            updated_ms INTEGER NOT NULL
        );
        "#,
    )
    .context("failed to create schema")?;
    Ok(())
}
