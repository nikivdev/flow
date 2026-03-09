use std::{
    collections::HashSet,
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config;
use crate::secret_redact;

const HISTORY_REVERSE_SCAN_CHUNK_BYTES: usize = 16 * 1024;

#[derive(Serialize, Deserialize)]
pub struct InvocationRecord {
    pub timestamp_ms: u128,
    pub duration_ms: u128,
    pub project_root: String,
    #[serde(default)]
    pub project_name: Option<String>,
    pub config_path: String,
    pub task_name: String,
    pub command: String,
    #[serde(default)]
    pub user_input: String,
    pub status: Option<i32>,
    pub success: bool,
    pub used_flox: bool,
    pub output: String,
    pub flow_version: String,
}

impl InvocationRecord {
    pub fn new(
        project_root: impl Into<String>,
        config_path: impl Into<String>,
        project_name: Option<&str>,
        task_name: impl Into<String>,
        command: impl Into<String>,
        user_input: impl Into<String>,
        used_flox: bool,
    ) -> Self {
        Self {
            timestamp_ms: now_ms(),
            duration_ms: 0,
            project_root: project_root.into(),
            project_name: project_name.map(|s| s.to_string()),
            config_path: config_path.into(),
            task_name: task_name.into(),
            command: command.into(),
            user_input: user_input.into(),
            status: None,
            success: false,
            used_flox,
            output: String::new(),
            flow_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

pub fn record(invocation: InvocationRecord) -> Result<()> {
    let mut invocation = invocation;
    invocation.command = secret_redact::redact_text(&invocation.command);
    invocation.user_input = secret_redact::redact_text(&invocation.user_input);
    invocation.output = secret_redact::redact_text(&invocation.output);

    let path = history_path();
    let _ = config::ensure_global_state_dir()
        .with_context(|| format!("failed to create history dir {}", path.display()))?;

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open history file {}", path.display()))?;

    let line = serde_json::to_string(&invocation).context("failed to serialize invocation")?;
    writeln!(file, "{line}").context("failed to write invocation to history")?;
    Ok(())
}

/// Print the most recent invocation with only the user input and the resulting output or error.
pub fn print_last_record() -> Result<()> {
    let path = history_path();
    let record = load_last_record(&path)?;
    let Some(rec) = record else {
        if path.exists() {
            println!("No valid history entries found in {}", path.display());
        } else {
            println!("No history found at {}", path.display());
        }
        return Ok(());
    };

    let user_input = if rec.user_input.trim().is_empty() {
        rec.task_name.clone()
    } else {
        rec.user_input.clone()
    };
    println!("{user_input}");

    if rec.output.trim().is_empty() {
        if !rec.success {
            let status = rec
                .status
                .map(|s| s.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            println!("error (status: {status})");
        }
    } else {
        let output = secret_redact::redact_text(&rec.output);
        print!("{}", output);
        if !output.ends_with('\n') {
            println!();
        }
    }

    Ok(())
}

/// Print the most recent invocation with output and status.
pub fn print_last_record_full() -> Result<()> {
    let path = history_path();
    let record = load_last_record(&path)?;
    let Some(rec) = record else {
        if path.exists() {
            println!("No valid history entries found in {}", path.display());
        } else {
            println!("No history found at {}", path.display());
        }
        return Ok(());
    };

    println!("task: {}", rec.task_name);
    println!("command: {}", secret_redact::redact_text(&rec.command));
    println!("project: {}", rec.project_root);
    if let Some(name) = rec.project_name.as_deref() {
        println!("project_name: {name}");
    }
    println!("config: {}", rec.config_path);
    println!(
        "status: {} (code: {})",
        if rec.success { "success" } else { "failure" },
        rec.status
            .map(|s| s.to_string())
            .unwrap_or_else(|| "unknown".to_string())
    );
    println!("duration_ms: {}", rec.duration_ms);
    println!("flow_version: {}", rec.flow_version);
    println!("--- output ---");
    print!("{}", secret_redact::redact_text(&rec.output));
    Ok(())
}

fn load_last_record(path: &Path) -> Result<Option<InvocationRecord>> {
    find_last_record_matching(path, |_| true)
}

/// Load the last invocation record for a specific project root.
pub fn load_last_record_for_project(project_root: &Path) -> Result<Option<InvocationRecord>> {
    let path = history_path();
    if !path.exists() {
        return Ok(None);
    }

    let canonical_root = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    let canonical_str = canonical_root.to_string_lossy();

    find_last_record_matching(&path, |rec| rec.project_root == canonical_str)
}

pub fn history_path() -> PathBuf {
    config::global_state_dir().join("history.jsonl")
}

/// Load unique task-history entries, most recent first, deduped by project + task name.
pub fn load_unique_task_records() -> Result<Vec<InvocationRecord>> {
    let path = history_path();
    if !path.exists() {
        return Ok(Vec::new());
    }

    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut records = Vec::new();
    let _ = visit_lines_reverse(&path, |line| {
        if line.trim().is_empty() {
            return None::<()>;
        }
        let record = serde_json::from_str::<InvocationRecord>(line).ok()?;
        let key = (record.project_root.clone(), record.task_name.clone());
        if seen.insert(key) {
            records.push(record);
        }
        None::<()>
    })?;
    Ok(records)
}

fn find_last_record_matching<F>(path: &Path, mut predicate: F) -> Result<Option<InvocationRecord>>
where
    F: FnMut(&InvocationRecord) -> bool,
{
    if !path.exists() {
        return Ok(None);
    }

    visit_lines_reverse(path, |line| {
        if line.trim().is_empty() {
            return None;
        }
        let record = serde_json::from_str::<InvocationRecord>(line).ok()?;
        if predicate(&record) {
            Some(record)
        } else {
            None
        }
    })
}

fn visit_lines_reverse<T, F>(path: &Path, mut on_line: F) -> Result<Option<T>>
where
    F: FnMut(&str) -> Option<T>,
{
    let mut file = File::open(path)
        .with_context(|| format!("failed to read history at {}", path.display()))?;
    let mut pos = file.seek(SeekFrom::End(0))?;
    if pos == 0 {
        return Ok(None);
    }

    let mut chunk = vec![0u8; HISTORY_REVERSE_SCAN_CHUNK_BYTES];
    let mut carry = Vec::new();

    while pos > 0 {
        let read_len = usize::try_from(pos.min(chunk.len() as u64)).unwrap_or(chunk.len());
        pos -= read_len as u64;
        file.seek(SeekFrom::Start(pos))?;
        file.read_exact(&mut chunk[..read_len])
            .with_context(|| format!("failed to read history at {}", path.display()))?;

        let buf = &chunk[..read_len];
        let mut end = read_len;
        while let Some(idx) = buf[..end].iter().rposition(|&byte| byte == b'\n') {
            if let Some(value) =
                process_reverse_line_segment(&buf[idx + 1..end], &mut carry, &mut on_line)
            {
                return Ok(Some(value));
            }
            end = idx;
        }

        if end > 0 {
            let mut combined = Vec::with_capacity(end + carry.len());
            combined.extend_from_slice(&buf[..end]);
            combined.extend_from_slice(&carry);
            carry = combined;
        }
    }

    if !carry.is_empty()
        && let Ok(line) = std::str::from_utf8(&carry)
        && let Some(value) = on_line(line.trim_end_matches('\r'))
    {
        return Ok(Some(value));
    }

    Ok(None)
}

fn process_reverse_line_segment<T, F>(
    segment: &[u8],
    carry: &mut Vec<u8>,
    on_line: &mut F,
) -> Option<T>
where
    F: FnMut(&str) -> Option<T>,
{
    if carry.is_empty() {
        let line = std::str::from_utf8(segment).ok()?;
        return on_line(line.trim_end_matches('\r'));
    }

    let suffix = std::mem::take(carry);
    let mut line_bytes = Vec::with_capacity(segment.len() + suffix.len());
    line_bytes.extend_from_slice(segment);
    line_bytes.extend_from_slice(&suffix);
    let line = std::str::from_utf8(&line_bytes).ok()?;
    on_line(line.trim_end_matches('\r'))
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{InvocationRecord, find_last_record_matching, load_last_record, now_ms};

    fn sample_record(project_root: &str, task_name: &str, user_input: &str) -> InvocationRecord {
        InvocationRecord {
            timestamp_ms: now_ms(),
            duration_ms: 1,
            project_root: project_root.to_string(),
            project_name: None,
            config_path: format!("{project_root}/flow.toml"),
            task_name: task_name.to_string(),
            command: "echo hi".to_string(),
            user_input: user_input.to_string(),
            status: Some(0),
            success: true,
            used_flox: false,
            output: "ok".to_string(),
            flow_version: "test".to_string(),
        }
    }

    #[test]
    fn load_last_record_reads_from_end_without_full_file_parse() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("history.jsonl");
        let long_output = "x".repeat(super::HISTORY_REVERSE_SCAN_CHUNK_BYTES + 256);
        let mut first = sample_record("/tmp/a", "first", "first");
        first.output = long_output;
        let second = sample_record("/tmp/b", "second", "second");
        let payload = format!(
            "{}\n{}\n",
            serde_json::to_string(&first).expect("first json"),
            serde_json::to_string(&second).expect("second json")
        );
        fs::write(&path, payload).expect("write history");

        let found = load_last_record(&path)
            .expect("load last record")
            .expect("record should exist");
        assert_eq!(found.task_name, "second");
    }

    #[test]
    fn find_last_record_matching_finds_latest_matching_project() {
        let dir = tempdir().expect("tempdir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("project dir");
        let path = dir.path().join("history.jsonl");

        let first = sample_record(&project.to_string_lossy(), "one", "one");
        let second = sample_record("/tmp/other", "other", "other");
        let third = sample_record(&project.to_string_lossy(), "two", "two");
        let payload = format!(
            "{}\n{}\n{}\n",
            serde_json::to_string(&first).expect("first json"),
            serde_json::to_string(&second).expect("second json"),
            serde_json::to_string(&third).expect("third json")
        );
        fs::write(&path, payload).expect("write history");

        let found =
            find_last_record_matching(&path, |rec| rec.project_root == project.to_string_lossy())
                .expect("load project record")
                .expect("record should exist");

        assert_eq!(found.task_name, "two");
    }
}
