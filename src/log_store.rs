use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::db;

/// A log entry for ingestion and storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub project: String,
    pub content: String,
    pub timestamp: i64, // unix ms
    #[serde(rename = "type")]
    pub log_type: String, // "log" | "error"
    pub service: String, // task name or custom service
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
    #[serde(default = "default_format")]
    pub format: String, // "json" | "text"
}

fn default_format() -> String {
    "text".to_string()
}

/// Stored log entry with ID.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredLogEntry {
    pub id: i64,
    #[serde(flatten)]
    pub entry: LogEntry,
}

/// Query parameters for filtering logs.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LogQuery {
    pub project: Option<String>,
    pub service: Option<String>,
    #[serde(rename = "type")]
    pub log_type: Option<String>,
    pub since: Option<i64>, // timestamp ms
    pub until: Option<i64>, // timestamp ms
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
}

fn default_limit() -> usize {
    100
}

/// Initialize the logs table schema.
pub fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS logs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            project TEXT NOT NULL,
            content TEXT NOT NULL,
            timestamp INTEGER NOT NULL,
            log_type TEXT NOT NULL,
            service TEXT NOT NULL,
            stack TEXT,
            format TEXT NOT NULL DEFAULT 'text'
        );
        CREATE INDEX IF NOT EXISTS idx_logs_project ON logs(project);
        CREATE INDEX IF NOT EXISTS idx_logs_timestamp ON logs(timestamp);
        CREATE INDEX IF NOT EXISTS idx_logs_type ON logs(log_type);
        CREATE INDEX IF NOT EXISTS idx_logs_service ON logs(service);
        "#,
    )
    .context("failed to create logs schema")?;
    Ok(())
}

/// Insert a single log entry.
pub fn insert_log(conn: &Connection, entry: &LogEntry) -> Result<i64> {
    conn.execute(
        r#"
        INSERT INTO logs (project, content, timestamp, log_type, service, stack, format)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
        "#,
        params![
            entry.project,
            entry.content,
            entry.timestamp,
            entry.log_type,
            entry.service,
            entry.stack,
            entry.format,
        ],
    )
    .context("failed to insert log")?;
    Ok(conn.last_insert_rowid())
}

/// Insert multiple log entries in a transaction.
pub fn insert_logs(conn: &mut Connection, entries: &[LogEntry]) -> Result<Vec<i64>> {
    let tx = conn.transaction()?;
    let mut ids = Vec::with_capacity(entries.len());

    for entry in entries {
        tx.execute(
            r#"
            INSERT INTO logs (project, content, timestamp, log_type, service, stack, format)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            "#,
            params![
                entry.project,
                entry.content,
                entry.timestamp,
                entry.log_type,
                entry.service,
                entry.stack,
                entry.format,
            ],
        )
        .context("failed to insert log")?;
        ids.push(tx.last_insert_rowid());
    }

    tx.commit()?;
    Ok(ids)
}

/// Query logs with filters.
pub fn query_logs(conn: &Connection, query: &LogQuery) -> Result<Vec<StoredLogEntry>> {
    let mut sql = String::from(
        "SELECT id, project, content, timestamp, log_type, service, stack, format FROM logs WHERE 1=1",
    );
    let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if let Some(ref project) = query.project {
        sql.push_str(" AND project = ?");
        params_vec.push(Box::new(project.clone()));
    }
    if let Some(ref service) = query.service {
        sql.push_str(" AND service = ?");
        params_vec.push(Box::new(service.clone()));
    }
    if let Some(ref log_type) = query.log_type {
        sql.push_str(" AND log_type = ?");
        params_vec.push(Box::new(log_type.clone()));
    }
    if let Some(since) = query.since {
        sql.push_str(" AND timestamp >= ?");
        params_vec.push(Box::new(since));
    }
    if let Some(until) = query.until {
        sql.push_str(" AND timestamp <= ?");
        params_vec.push(Box::new(until));
    }

    sql.push_str(" ORDER BY timestamp DESC LIMIT ? OFFSET ?");
    params_vec.push(Box::new(query.limit as i64));
    params_vec.push(Box::new(query.offset as i64));

    let params_refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|p| p.as_ref()).collect();

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_refs.as_slice(), |row| {
        Ok(StoredLogEntry {
            id: row.get(0)?,
            entry: LogEntry {
                project: row.get(1)?,
                content: row.get(2)?,
                timestamp: row.get(3)?,
                log_type: row.get(4)?,
                service: row.get(5)?,
                stack: row.get(6)?,
                format: row.get(7)?,
            },
        })
    })?;

    let mut entries = Vec::new();
    for row in rows {
        entries.push(row?);
    }
    Ok(entries)
}

/// Get error logs for a project (convenience function).
pub fn get_errors(conn: &Connection, project: &str, limit: usize) -> Result<Vec<StoredLogEntry>> {
    query_logs(
        conn,
        &LogQuery {
            project: Some(project.to_string()),
            log_type: Some("error".to_string()),
            limit,
            ..Default::default()
        },
    )
}

/// Open database and ensure schema exists.
pub fn open_log_db() -> Result<Connection> {
    let conn = db::open_db()?;
    init_schema(&conn)?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_insert_and_query() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();

        let entry = LogEntry {
            project: "test-project".to_string(),
            content: "Test log message".to_string(),
            timestamp: 1234567890000,
            log_type: "log".to_string(),
            service: "web".to_string(),
            stack: None,
            format: "text".to_string(),
        };

        let id = insert_log(&conn, &entry).unwrap();
        assert!(id > 0);

        let results = query_logs(
            &conn,
            &LogQuery {
                project: Some("test-project".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].entry.content, "Test log message");
    }

    #[test]
    fn test_error_query() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();

        let log_entry = LogEntry {
            project: "test".to_string(),
            content: "Normal log".to_string(),
            timestamp: 1000,
            log_type: "log".to_string(),
            service: "api".to_string(),
            stack: None,
            format: "text".to_string(),
        };

        let error_entry = LogEntry {
            project: "test".to_string(),
            content: "Error occurred".to_string(),
            timestamp: 2000,
            log_type: "error".to_string(),
            service: "api".to_string(),
            stack: Some("at main.rs:10".to_string()),
            format: "text".to_string(),
        };

        insert_log(&conn, &log_entry).unwrap();
        insert_log(&conn, &error_entry).unwrap();

        let errors = get_errors(&conn, "test", 10).unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].entry.log_type, "error");
    }
}
