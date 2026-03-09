use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

/// A process started by flow
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunningProcess {
    /// Process ID of the main task process
    pub pid: u32,
    /// Process group ID (for killing child processes)
    pub pgid: u32,
    /// Name of the task from flow.toml
    pub task_name: String,
    /// Full command that was executed
    pub command: String,
    /// Timestamp when the process was started (ms since epoch)
    pub started_at: u128,
    /// Canonical path to the flow.toml that defines this task
    pub config_path: PathBuf,
    /// Canonical path to the project root directory
    pub project_root: PathBuf,
    /// Whether flox environment was used
    pub used_flox: bool,
    /// Optional project name from flow.toml
    #[serde(default)]
    pub project_name: Option<String>,
}

/// All running processes tracked by flow
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunningProcesses {
    /// Map from project config path to list of running processes
    pub projects: HashMap<String, Vec<RunningProcess>>,
}

/// Returns ~/.config/flow/running.sqlite
pub fn running_processes_path() -> PathBuf {
    crate::config::global_state_dir().join("running.sqlite")
}

fn open_running_db(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let conn =
        Connection::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    conn.busy_timeout(Duration::from_secs(5))
        .context("failed to set running DB busy timeout")?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .context("failed to enable running DB WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .context("failed to tune running DB sync mode")?;
    conn.pragma_update(None, "temp_store", "MEMORY")
        .context("failed to tune running DB temp store")?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS running_processes (
            pid INTEGER PRIMARY KEY,
            pgid INTEGER NOT NULL,
            task_name TEXT NOT NULL,
            command TEXT NOT NULL,
            started_at INTEGER NOT NULL,
            config_path TEXT NOT NULL,
            project_root TEXT NOT NULL,
            used_flox INTEGER NOT NULL,
            project_name TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_running_processes_config_path
        ON running_processes(config_path);
        CREATE INDEX IF NOT EXISTS idx_running_processes_started_at
        ON running_processes(started_at);",
    )
    .context("failed to initialize running-process schema")?;
    Ok(conn)
}

fn read_processes(conn: &Connection, config_path: Option<&Path>) -> Result<Vec<RunningProcess>> {
    let mut processes = Vec::new();

    if let Some(config_path) = config_path {
        let config_path = config_path.to_string_lossy().to_string();
        let mut stmt = conn
            .prepare(
                "SELECT pid, pgid, task_name, command, started_at, config_path, project_root,
                        used_flox, project_name
                 FROM running_processes
                 WHERE config_path = ?1
                 ORDER BY started_at ASC",
            )
            .context("failed to prepare filtered running-process query")?;
        let rows = stmt
            .query_map(params![config_path], row_to_running_process)
            .context("failed to query filtered running processes")?;
        for row in rows {
            processes.push(row.context("failed to decode running process row")?);
        }
    } else {
        let mut stmt = conn
            .prepare(
                "SELECT pid, pgid, task_name, command, started_at, config_path, project_root,
                        used_flox, project_name
                 FROM running_processes
                 ORDER BY started_at ASC",
            )
            .context("failed to prepare running-process query")?;
        let rows = stmt
            .query_map([], row_to_running_process)
            .context("failed to query running processes")?;
        for row in rows {
            processes.push(row.context("failed to decode running process row")?);
        }
    }

    Ok(processes)
}

fn row_to_running_process(row: &rusqlite::Row<'_>) -> rusqlite::Result<RunningProcess> {
    let started_at_raw: i64 = row.get(4)?;
    Ok(RunningProcess {
        pid: row.get(0)?,
        pgid: row.get(1)?,
        task_name: row.get(2)?,
        command: row.get(3)?,
        started_at: u128::try_from(started_at_raw.max(0)).unwrap_or(0),
        config_path: PathBuf::from(row.get::<_, String>(5)?),
        project_root: PathBuf::from(row.get::<_, String>(6)?),
        used_flox: row.get::<_, i64>(7)? != 0,
        project_name: row.get(8)?,
    })
}

fn remove_processes(conn: &mut Connection, pids: &[u32]) -> Result<()> {
    if pids.is_empty() {
        return Ok(());
    }

    let tx = conn
        .transaction()
        .context("failed to start running-process cleanup transaction")?;
    {
        let mut stmt = tx
            .prepare("DELETE FROM running_processes WHERE pid = ?1")
            .context("failed to prepare running-process cleanup statement")?;
        for pid in pids {
            stmt.execute(params![pid])
                .with_context(|| format!("failed to delete stale running process {}", pid))?;
        }
    }
    tx.commit()
        .context("failed to commit running-process cleanup transaction")?;
    Ok(())
}

fn collect_alive_processes(
    conn: &mut Connection,
    config_path: Option<&Path>,
) -> Result<Vec<RunningProcess>> {
    let rows = read_processes(conn, config_path)?;
    let mut alive = Vec::with_capacity(rows.len());
    let mut stale = Vec::new();

    for process in rows {
        if process_alive(process.pid) {
            alive.push(process);
        } else {
            stale.push(process.pid);
        }
    }

    remove_processes(conn, &stale)?;
    Ok(alive)
}

fn load_running_processes_at(path: &Path) -> Result<RunningProcesses> {
    let mut conn = open_running_db(path)?;
    let processes = collect_alive_processes(&mut conn, None)?;
    let mut grouped: HashMap<String, Vec<RunningProcess>> = HashMap::new();
    for process in processes {
        grouped
            .entry(process.config_path.display().to_string())
            .or_default()
            .push(process);
    }
    Ok(RunningProcesses { projects: grouped })
}

fn register_process_at(path: &Path, entry: RunningProcess) -> Result<()> {
    let mut conn = open_running_db(path)?;
    let tx = conn
        .transaction()
        .context("failed to start running-process register transaction")?;
    tx.execute(
        "INSERT INTO running_processes (
            pid, pgid, task_name, command, started_at, config_path, project_root,
            used_flox, project_name
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
        ON CONFLICT(pid) DO UPDATE SET
            pgid = excluded.pgid,
            task_name = excluded.task_name,
            command = excluded.command,
            started_at = excluded.started_at,
            config_path = excluded.config_path,
            project_root = excluded.project_root,
            used_flox = excluded.used_flox,
            project_name = excluded.project_name",
        params![
            entry.pid,
            entry.pgid,
            entry.task_name,
            entry.command,
            i64::try_from(entry.started_at).unwrap_or(i64::MAX),
            entry.config_path.display().to_string(),
            entry.project_root.display().to_string(),
            if entry.used_flox { 1i64 } else { 0i64 },
            entry.project_name,
        ],
    )
    .with_context(|| format!("failed to register running process {}", entry.task_name))?;
    tx.commit()
        .context("failed to commit running-process register transaction")?;
    Ok(())
}

fn unregister_process_at(path: &Path, pid: u32) -> Result<()> {
    let conn = open_running_db(path)?;
    conn.execute("DELETE FROM running_processes WHERE pid = ?1", params![pid])
        .with_context(|| format!("failed to unregister running process {}", pid))?;
    Ok(())
}

fn get_project_processes_at(path: &Path, config_path: &Path) -> Result<Vec<RunningProcess>> {
    let mut conn = open_running_db(path)?;
    collect_alive_processes(&mut conn, Some(config_path))
}

/// Load running processes, validating that PIDs are still alive.
pub fn load_running_processes() -> Result<RunningProcesses> {
    load_running_processes_at(&running_processes_path())
}

/// Register a new running process.
pub fn register_process(entry: RunningProcess) -> Result<()> {
    register_process_at(&running_processes_path(), entry)
}

/// Unregister a process by PID.
pub fn unregister_process(pid: u32) -> Result<()> {
    unregister_process_at(&running_processes_path(), pid)
}

/// Get processes for a specific project.
pub fn get_project_processes(config_path: &Path) -> Result<Vec<RunningProcess>> {
    get_project_processes_at(&running_processes_path(), config_path)
}

/// Check if a process is alive.
pub fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if result == 0 {
            return true;
        }
        matches!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EPERM)
        )
    }
    #[cfg(windows)]
    {
        use std::process::Command;
        use std::process::Stdio;
        Command::new("tasklist")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .map(|o| {
                o.status.success() && String::from_utf8_lossy(&o.stdout).contains(&pid.to_string())
            })
            .unwrap_or(false)
    }
}

/// Get process group ID for a PID.
#[cfg(unix)]
pub fn get_pgid(pid: u32) -> Option<u32> {
    let pgid = unsafe { libc::getpgid(pid as libc::pid_t) };
    if pgid < 0 { None } else { Some(pgid as u32) }
}

#[cfg(not(unix))]
pub fn get_pgid(pid: u32) -> Option<u32> {
    Some(pid)
}

/// Get current timestamp in milliseconds.
pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_process(pid: u32, root: &Path) -> RunningProcess {
        RunningProcess {
            pid,
            pgid: get_pgid(std::process::id()).unwrap_or(pid),
            task_name: "dev".to_string(),
            command: "cargo run".to_string(),
            started_at: now_ms(),
            config_path: root.join("flow.toml"),
            project_root: root.to_path_buf(),
            used_flox: false,
            project_name: Some("flow".to_string()),
        }
    }

    #[test]
    fn register_load_and_unregister_round_trip() {
        let dir = TempDir::new().expect("tempdir");
        let db_path = dir.path().join("running.sqlite");
        let process = sample_process(std::process::id(), dir.path());

        register_process_at(&db_path, process.clone()).expect("register process");
        let loaded = load_running_processes_at(&db_path).expect("load processes");
        let key = process.config_path.display().to_string();
        let entries = loaded.projects.get(&key).expect("project entries");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].pid, process.pid);

        let project_entries =
            get_project_processes_at(&db_path, &process.config_path).expect("project entries");
        assert_eq!(project_entries.len(), 1);
        assert_eq!(project_entries[0].task_name, process.task_name);

        unregister_process_at(&db_path, process.pid).expect("unregister process");
        let loaded = load_running_processes_at(&db_path).expect("reload processes");
        assert!(loaded.projects.is_empty());
    }

    #[test]
    fn stale_processes_are_removed_on_read() {
        let dir = TempDir::new().expect("tempdir");
        let db_path = dir.path().join("running.sqlite");
        let process = sample_process(999_999, dir.path());

        register_process_at(&db_path, process.clone()).expect("register process");
        let loaded = load_running_processes_at(&db_path).expect("load processes");
        assert!(
            loaded.projects.is_empty(),
            "stale process should be dropped"
        );

        let project_entries =
            get_project_processes_at(&db_path, &process.config_path).expect("project entries");
        assert!(
            project_entries.is_empty(),
            "stale project process should be removed"
        );
    }
}
