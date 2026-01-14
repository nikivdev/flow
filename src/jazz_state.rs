use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::executor::block_on;
use groove::ObjectId;
use groove::sql::{Database, DatabaseError};
use groove_rocksdb::RocksEnvironment;

use crate::history::InvocationRecord;

const CATALOG_ID_FILE: &str = "catalog.id";
const DEFAULT_DB_DIR: &str = ".config/flow/jazz2";
const DEFAULT_REPO_ROOT: &str = "/Users/nikiv/repos/garden-co/jazz2";
const OUTPUT_LIMIT: usize = 80_000;

static DB: OnceLock<Mutex<Option<Database>>> = OnceLock::new();

pub fn record_task_run(record: &InvocationRecord) -> Result<()> {
    if env_flag("FLOW_JAZZ2_DISABLE") {
        return Ok(());
    }

    with_db(|db| {
        ensure_schema(db).context("ensure jazz2 schema")?;
        insert_task_run(db, record).context("insert task run")?;
        Ok(())
    })?;
    Ok(())
}

fn with_db<F>(op: F) -> Result<()>
where
    F: FnOnce(&Database) -> Result<()>,
{
    let mutex = DB.get_or_init(|| Mutex::new(None));
    let mut guard = mutex.lock().expect("jazz2 db mutex poisoned");
    if guard.is_none() {
        *guard = Some(open_db_with_retry().context("open jazz2 state db")?);
    }
    let db = guard.as_ref().expect("jazz2 db missing after init");
    op(db)
}

fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn open_db() -> Result<Database> {
    use groove::Environment;

    let path = state_dir();
    fs::create_dir_all(&path).with_context(|| format!("create jazz2 dir {}", path.display()))?;

    let env: Arc<dyn Environment> =
        Arc::new(RocksEnvironment::open(&path).context("open rocksdb env")?);
    if let Some(catalog_id) = load_catalog_id(&path)? {
        if let Ok(db) = block_on(Database::from_env(Arc::clone(&env), catalog_id)) {
            return Ok(db);
        }
    }

    let db = Database::new(env);
    save_catalog_id(&path, db.catalog_object_id())?;
    Ok(db)
}

fn open_db_with_retry() -> Result<Database> {
    let mut last_err: Option<anyhow::Error> = None;
    for _ in 0..3 {
        match open_db() {
            Ok(db) => return Ok(db),
            Err(err) => {
                last_err = Some(err);
                thread::sleep(Duration::from_millis(60));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("open jazz2 failed")))
}

pub fn state_dir() -> PathBuf {
    if let Ok(path) = std::env::var("AI_JAZZ2_PATH") {
        return PathBuf::from(path);
    }
    if let Ok(path) = std::env::var("FLOW_JAZZ2_PATH") {
        return PathBuf::from(path);
    }
    let repo_root = PathBuf::from(DEFAULT_REPO_ROOT);
    if repo_root.exists() {
        return repo_root.join(".jazz2");
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(DEFAULT_DB_DIR)
}

fn load_catalog_id(base: &Path) -> Result<Option<ObjectId>> {
    let path = base.join(CATALOG_ID_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let trimmed = contents.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let id = trimmed
        .parse::<ObjectId>()
        .with_context(|| format!("parse catalog id {}", trimmed))?;
    Ok(Some(id))
}

fn save_catalog_id(base: &Path, id: ObjectId) -> Result<()> {
    let path = base.join(CATALOG_ID_FILE);
    fs::write(&path, id.to_string()).with_context(|| format!("write {}", path.display()))
}

fn ensure_schema(db: &Database) -> Result<()> {
    let sql = r#"
        CREATE TABLE flow_task_runs (
            project_root STRING NOT NULL,
            project_name STRING,
            config_path STRING NOT NULL,
            task STRING NOT NULL,
            command STRING NOT NULL,
            user_input STRING NOT NULL,
            success BOOL NOT NULL,
            status I64,
            duration_ms I64 NOT NULL,
            timestamp_ms I64 NOT NULL,
            flow_version STRING NOT NULL,
            used_flox BOOL NOT NULL,
            output STRING NOT NULL
        )
    "#;

    match db.execute(sql) {
        Ok(_) => Ok(()),
        Err(DatabaseError::TableExists(_)) => Ok(()),
        Err(err) => Err(anyhow::anyhow!("create table failed: {:?}", err)),
    }
}

fn insert_task_run(db: &Database, record: &InvocationRecord) -> Result<()> {
    let status = record
        .status
        .map(|value| value.to_string())
        .unwrap_or_else(|| "NULL".to_string());
    let duration_ms = record.duration_ms.min(i64::MAX as u128) as i64;
    let timestamp_ms = record.timestamp_ms.min(i64::MAX as u128) as i64;
    let project_name = record
        .project_name
        .as_ref()
        .map(|value| format!("'{}'", sql_escape(value)))
        .unwrap_or_else(|| "NULL".to_string());
    let output = truncate_output(&record.output, OUTPUT_LIMIT);

    let sql = format!(
        "INSERT INTO flow_task_runs \
        (project_root, project_name, config_path, task, command, user_input, success, status, \
        duration_ms, timestamp_ms, flow_version, used_flox, output) \
        VALUES ('{}', {}, '{}', '{}', '{}', '{}', {}, {}, {}, {}, '{}', {}, '{}')",
        sql_escape(&record.project_root),
        project_name,
        sql_escape(&record.config_path),
        sql_escape(&record.task_name),
        sql_escape(&record.command),
        sql_escape(&record.user_input),
        if record.success { "true" } else { "false" },
        status,
        duration_ms,
        timestamp_ms,
        sql_escape(&record.flow_version),
        if record.used_flox { "true" } else { "false" },
        sql_escape(&output),
    );

    db.execute(&sql)
        .map(|_| ())
        .map_err(|err| anyhow::anyhow!("insert failed: {:?}", err))
}

fn sql_escape(value: &str) -> String {
    // Replace single quotes with backticks since groove SQL doesn't handle '' escaping well
    // Also remove null bytes
    value.replace('\'', "`").replace('\0', "")
}

fn truncate_output(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_string();
    }

    let mut start = value.len() - limit;
    while start < value.len() && !value.is_char_boundary(start) {
        start += 1;
    }

    let mut truncated = String::from("... ");
    truncated.push_str(&value[start..]);
    truncated
}
