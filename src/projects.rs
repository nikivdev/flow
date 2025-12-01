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

    let mut conn = open_db()?;
    create_schema(&conn)?;
    migrate_legacy_json(&mut conn)?;

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
    let mut conn = open_db()?;
    create_schema(&conn)?;
    migrate_legacy_json(&mut conn)?;

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

/// One-time migration from legacy projects.json (if present) into the DB.
fn migrate_legacy_json(conn: &mut Connection) -> Result<()> {
    let legacy = legacy_registry_path();
    if !legacy.exists() {
        return Ok(());
    }

    let contents = std::fs::read_to_string(&legacy)
        .with_context(|| format!("failed to read legacy registry {}", legacy.display()))?;
    let legacy_registry: serde_json::Value = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse legacy registry {}", legacy.display()))?;
    let Some(map) = legacy_registry.get("projects").and_then(|p| p.as_object()) else {
        return Ok(());
    };

    let tx = conn.transaction()?;
    for (name, entry) in map {
        if let (Some(project_root), Some(config_path), Some(updated_ms)) = (
            entry
                .get("project_root")
                .and_then(|v| v.as_str())
                .map(String::from),
            entry
                .get("config_path")
                .and_then(|v| v.as_str())
                .map(String::from),
            entry.get("updated_ms").and_then(|v| v.as_u64()),
        ) {
            tx.execute(
                r#"
                INSERT INTO projects (name, project_root, config_path, updated_ms)
                VALUES (?1, ?2, ?3, ?4)
                ON CONFLICT(name) DO UPDATE SET
                    project_root=excluded.project_root,
                    config_path=excluded.config_path,
                    updated_ms=excluded.updated_ms
                "#,
                params![name, project_root, config_path, updated_ms as i64],
            )
            .ok();
        }
    }
    tx.commit().ok();

    // Clean up legacy file to avoid repeated migrations.
    let _ = std::fs::remove_file(&legacy);

    Ok(())
}

fn legacy_registry_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config/flow/projects.json")
}
