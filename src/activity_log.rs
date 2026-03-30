use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{Datelike, Local, Timelike};
use data_encoding::BASE32_NOPAD;
use serde::{Deserialize, Serialize};

const ACTIVITY_EVENT_VERSION: u32 = 1;
const HUMAN_LINE_MAX_CHARS: usize = 220;
const EVENT_ID_LEN: usize = 7;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ActivityStatus {
    Done,
    Changed,
}

impl ActivityStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Changed => "changed",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ActivityEvent {
    pub version: u32,
    pub recorded_at_unix: u64,
    pub status: ActivityStatus,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    pub summary: String,
    pub event_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedupe_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_ref: Option<String>,
}

impl ActivityEvent {
    pub fn done(kind: impl Into<String>, summary: impl Into<String>) -> Self {
        Self::new(ActivityStatus::Done, kind.into(), summary.into())
    }

    pub fn changed(kind: impl Into<String>, summary: impl Into<String>) -> Self {
        Self::new(ActivityStatus::Changed, kind.into(), summary.into())
    }

    fn new(status: ActivityStatus, kind: String, summary: String) -> Self {
        Self {
            version: ACTIVITY_EVENT_VERSION,
            recorded_at_unix: unix_now_secs(),
            status,
            kind,
            route: None,
            scope: None,
            summary,
            event_id: String::new(),
            dedupe_key: None,
            source: None,
            session_id: None,
            runtime_token: None,
            target_path: None,
            launch_path: None,
            artifact_path: None,
            payload_ref: None,
        }
    }
}

#[cfg(unix)]
struct FileLockGuard {
    fd: std::os::fd::RawFd,
}

#[cfg(unix)]
impl Drop for FileLockGuard {
    fn drop(&mut self) {
        let _ = unsafe { libc::flock(self.fd, libc::LOCK_UN) };
    }
}

#[cfg(unix)]
fn acquire_file_lock(file: &std::fs::File) -> Result<FileLockGuard> {
    let fd = file.as_raw_fd();
    let status = unsafe { libc::flock(fd, libc::LOCK_EX) };
    if status == 0 {
        Ok(FileLockGuard { fd })
    } else {
        Err(std::io::Error::last_os_error()).context("failed to lock activity log file")
    }
}

#[cfg(not(unix))]
fn acquire_file_lock(_file: &std::fs::File) -> Result<()> {
    Ok(())
}

fn month_slug(month: u32) -> &'static str {
    match month {
        1 => "january",
        2 => "february",
        3 => "march",
        4 => "april",
        5 => "may",
        6 => "june",
        7 => "july",
        8 => "august",
        9 => "september",
        10 => "october",
        11 => "november",
        12 => "december",
        _ => "unknown",
    }
}

fn daily_log_root() -> PathBuf {
    if let Some(root) = std::env::var_os("FLOW_ACTIVITY_LOG_ROOT").map(PathBuf::from) {
        return root;
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("log")
}

fn daily_log_path_at(root: &Path, now: chrono::DateTime<Local>) -> PathBuf {
    let year = now.format("%y").to_string();
    let month = month_slug(now.month());
    let day = now.format("%d").to_string();
    root.join(year).join(month).join(format!("{day}.md"))
}

fn daily_events_path_at(root: &Path, now: chrono::DateTime<Local>) -> PathBuf {
    let year = now.format("%y").to_string();
    let month = month_slug(now.month());
    let day = now.format("%d").to_string();
    root.join(year)
        .join(month)
        .join(format!("{day}.events.jsonl"))
}

fn daily_dedupe_index_dir_at(root: &Path, now: chrono::DateTime<Local>) -> PathBuf {
    let year = now.format("%y").to_string();
    let month = month_slug(now.month());
    let day = now.format("%d").to_string();
    root.join(year)
        .join(month)
        .join(format!("{day}.events.keys"))
}

fn append_daily_event_at(
    root: &Path,
    now: chrono::DateTime<Local>,
    event: ActivityEvent,
) -> Result<PathBuf> {
    let log_path = daily_log_path_at(root, now);
    let events_path = daily_events_path_at(root, now);
    let dedupe_index_dir = daily_dedupe_index_dir_at(root, now);
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let Some(event) = normalize_event(event) else {
        return Ok(log_path);
    };

    let mut sidecar = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(&events_path)
        .with_context(|| format!("failed to open {}", events_path.display()))?;
    let _lock = acquire_file_lock(&sidecar)?;

    if let Some(dedupe_key) = event.dedupe_key.as_deref()
        && dedupe_key_exists(&events_path, &dedupe_index_dir, dedupe_key)?
    {
        return Ok(log_path);
    }

    sidecar
        .seek(SeekFrom::End(0))
        .with_context(|| format!("failed to seek {}", events_path.display()))?;
    serde_json::to_writer(&mut sidecar, &event)
        .with_context(|| format!("failed to encode {}", events_path.display()))?;
    sidecar
        .write_all(b"\n")
        .with_context(|| format!("failed to terminate {}", events_path.display()))?;
    sidecar
        .flush()
        .with_context(|| format!("failed to flush {}", events_path.display()))?;

    let mut log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("failed to open {}", log_path.display()))?;
    writeln!(log_file, "{}", render_human_line(&event, now))
        .with_context(|| format!("failed to append {}", log_path.display()))?;
    log_file
        .flush()
        .with_context(|| format!("failed to flush {}", log_path.display()))?;
    if let Some(dedupe_key) = event.dedupe_key.as_deref() {
        persist_dedupe_key(&dedupe_index_dir, dedupe_key)?;
    }
    Ok(log_path)
}

fn normalize_event(mut event: ActivityEvent) -> Option<ActivityEvent> {
    if event.version == 0 {
        event.version = ACTIVITY_EVENT_VERSION;
    }
    if event.recorded_at_unix == 0 {
        event.recorded_at_unix = unix_now_secs();
    }

    event.kind = compact_token(&event.kind, 48);
    if event.kind.is_empty() {
        return None;
    }

    event.summary = normalize_text(&event.summary);
    if event.summary.is_empty() {
        return None;
    }

    event.route = event
        .route
        .take()
        .map(|value| compact_token(&value, 32))
        .filter(|value| !value.is_empty());
    event.scope = event
        .scope
        .take()
        .map(|value| compact_token(&value, 24))
        .filter(|value| !value.is_empty())
        .or_else(|| derive_scope_from_event(&event));
    event.source = event
        .source
        .take()
        .map(|value| compact_token(&value, 32))
        .filter(|value| !value.is_empty());
    event.session_id = event
        .session_id
        .take()
        .map(|value| normalize_text(&value))
        .filter(|value| !value.is_empty());
    event.runtime_token = event
        .runtime_token
        .take()
        .map(|value| compact_token(&value, 16))
        .filter(|value| !value.is_empty());
    event.target_path = event
        .target_path
        .take()
        .map(|value| normalize_text(&value))
        .filter(|value| !value.is_empty());
    event.launch_path = event
        .launch_path
        .take()
        .map(|value| normalize_text(&value))
        .filter(|value| !value.is_empty());
    event.artifact_path = event
        .artifact_path
        .take()
        .map(|value| normalize_text(&value))
        .filter(|value| !value.is_empty());
    event.payload_ref = event
        .payload_ref
        .take()
        .map(|value| compact_token(&value, 24))
        .filter(|value| !value.is_empty());
    event.dedupe_key = event
        .dedupe_key
        .take()
        .map(|value| normalize_text(&value))
        .filter(|value| !value.is_empty());

    if event.event_id.trim().is_empty() {
        event.event_id = short_hash(&canonical_event_identity(&event), EVENT_ID_LEN);
    }

    Some(event)
}

fn render_human_line(event: &ActivityEvent, now: chrono::DateTime<Local>) -> String {
    let stamp = format!("{:02}:{:02}", now.hour(), now.minute());
    let kind = render_kind(event);
    let prefix = if let Some(scope) = event.scope.as_deref() {
        format!("{stamp}: [{}] {kind} {scope}: ", event.status.as_str())
    } else {
        format!("{stamp}: [{}] {kind}: ", event.status.as_str())
    };
    let suffix = render_tags(event);
    let summary_budget = HUMAN_LINE_MAX_CHARS
        .saturating_sub(prefix.chars().count())
        .saturating_sub(suffix.chars().count())
        .max(24);
    let summary = truncate_chars(&event.summary, summary_budget);
    format!("{prefix}{summary}{suffix}")
}

fn render_kind(event: &ActivityEvent) -> String {
    match event.route.as_deref() {
        Some(route) => format!("{}[{route}]", event.kind),
        None => event.kind.clone(),
    }
}

fn render_tags(event: &ActivityEvent) -> String {
    let mut tags = Vec::new();
    if let Some(session_id) = event.session_id.as_deref() {
        tags.push(format!("s:{}", truncate_session_id(session_id)));
    }
    if let Some(runtime_token) = event.runtime_token.as_deref() {
        tags.push(format!("r:{runtime_token}"));
    }
    tags.push(format!("e:{}", event.event_id));
    format!(" [{}]", tags.join(" "))
}

fn dedupe_key_exists(events_path: &Path, index_dir: &Path, dedupe_key: &str) -> Result<bool> {
    if dedupe_index_contains(index_dir, dedupe_key)? {
        return Ok(true);
    }
    if !sidecar_contains_dedupe_key(events_path, dedupe_key)? {
        return Ok(false);
    }
    persist_dedupe_key(index_dir, dedupe_key)?;
    Ok(true)
}

fn dedupe_index_contains(index_dir: &Path, dedupe_key: &str) -> Result<bool> {
    let marker_path = dedupe_marker_path(index_dir, dedupe_key);
    if !marker_path.exists() {
        return Ok(false);
    }
    let stored = fs::read_to_string(&marker_path)
        .with_context(|| format!("failed to read {}", marker_path.display()))?;
    Ok(stored.trim_end() == dedupe_key)
}

fn persist_dedupe_key(index_dir: &Path, dedupe_key: &str) -> Result<()> {
    fs::create_dir_all(index_dir)
        .with_context(|| format!("failed to create {}", index_dir.display()))?;
    let marker_path = dedupe_marker_path(index_dir, dedupe_key);
    match OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&marker_path)
    {
        Ok(mut file) => {
            file.write_all(dedupe_key.as_bytes())
                .with_context(|| format!("failed to write {}", marker_path.display()))?;
            file.write_all(b"\n")
                .with_context(|| format!("failed to terminate {}", marker_path.display()))?;
            file.flush()
                .with_context(|| format!("failed to flush {}", marker_path.display()))?;
            Ok(())
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(err).with_context(|| format!("failed to create {}", marker_path.display())),
    }
}

fn dedupe_marker_path(index_dir: &Path, dedupe_key: &str) -> PathBuf {
    let hash = blake3::hash(dedupe_key.as_bytes()).to_hex().to_string();
    index_dir.join(format!("{hash}.key"))
}

fn sidecar_contains_dedupe_key(path: &Path, dedupe_key: &str) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }

    let file =
        std::fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    for line in BufReader::new(file).lines() {
        let line = match line {
            Ok(value) => value,
            Err(_) => continue,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(event) = serde_json::from_str::<ActivityEvent>(trimmed) else {
            continue;
        };
        if event.dedupe_key.as_deref() == Some(dedupe_key) {
            return Ok(true);
        }
    }

    Ok(false)
}

fn derive_scope_from_event(event: &ActivityEvent) -> Option<String> {
    event
        .target_path
        .as_deref()
        .and_then(path_scope_label)
        .or_else(|| event.launch_path.as_deref().and_then(path_scope_label))
        .or_else(|| event.artifact_path.as_deref().and_then(path_scope_label))
}

fn path_scope_label(path: &str) -> Option<String> {
    let home = dirs::home_dir()?;
    let path = Path::new(path);

    if let Ok(stripped) = path.strip_prefix(home.join("code")) {
        let name = stripped.components().next()?.as_os_str().to_str()?;
        return Some(name.to_string());
    }

    if let Ok(stripped) = path.strip_prefix(home.join("repos")) {
        let mut parts = stripped.components();
        let org = parts.next()?.as_os_str().to_str()?;
        let repo = parts.next()?.as_os_str().to_str()?;
        if org == "openai" {
            return Some(format!("{org}/{repo}"));
        }
        return Some(repo.to_string());
    }

    if path.starts_with(home.join("config")) {
        return Some("config".to_string());
    }
    if path.starts_with(home.join("docs").join("plan")) {
        return Some("plan".to_string());
    }
    if path.starts_with(home.join("docs")) {
        return Some("docs".to_string());
    }
    if path.starts_with(home.join("plan")) {
        return Some("plan".to_string());
    }

    path.file_stem()
        .and_then(|value| value.to_str())
        .map(|value| value.to_string())
}

fn canonical_event_identity(event: &ActivityEvent) -> String {
    [
        event.version.to_string(),
        event.recorded_at_unix.to_string(),
        event.status.as_str().to_string(),
        event.kind.clone(),
        event.route.clone().unwrap_or_default(),
        event.scope.clone().unwrap_or_default(),
        event.summary.clone(),
        event.session_id.clone().unwrap_or_default(),
        event.runtime_token.clone().unwrap_or_default(),
        event.target_path.clone().unwrap_or_default(),
        event.launch_path.clone().unwrap_or_default(),
        event.artifact_path.clone().unwrap_or_default(),
        event.payload_ref.clone().unwrap_or_default(),
    ]
    .join("|")
}

fn normalize_text(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn compact_token(value: &str, max_chars: usize) -> String {
    truncate_chars(&normalize_text(value), max_chars)
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut out = value.trim().to_string();
    if out.chars().count() > max_chars {
        out = out
            .chars()
            .take(max_chars.saturating_sub(1))
            .collect::<String>();
        out.push('…');
    }
    out
}

fn truncate_session_id(value: &str) -> String {
    value.chars().take(8).collect()
}

fn short_hash(value: &str, len: usize) -> String {
    let hash = blake3::hash(value.as_bytes());
    let encoded = BASE32_NOPAD.encode(hash.as_bytes()).to_ascii_lowercase();
    encoded[..len.min(encoded.len())].to_string()
}

fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0)
}

pub fn current_daily_log_path() -> PathBuf {
    daily_log_path_at(&daily_log_root(), Local::now())
}

pub fn current_daily_events_path() -> PathBuf {
    daily_events_path_at(&daily_log_root(), Local::now())
}

pub fn append_daily_event(event: ActivityEvent) -> Result<()> {
    if matches!(
        std::env::var("FLOW_DISABLE_ACTIVITY_LOG").ok().as_deref(),
        Some("1" | "true" | "yes" | "on")
    ) {
        return Ok(());
    }

    let _ = append_daily_event_at(&daily_log_root(), Local::now(), event)?;
    Ok(())
}

pub fn append_daily_bullet(message: &str) -> Result<()> {
    append_daily_event(ActivityEvent::done("note", message))
}

pub fn recent_events(limit: usize) -> Result<Vec<ActivityEvent>> {
    recent_events_at(&daily_log_root(), limit)
}

fn recent_events_at(root: &Path, limit: usize) -> Result<Vec<ActivityEvent>> {
    if limit == 0 || !root.exists() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    collect_event_files(root, &mut files)?;
    files.sort_by(|left, right| right.cmp(left));

    let mut events = Vec::new();
    for path in files {
        if events.len() >= limit {
            break;
        }
        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };
        for line in contents.lines().rev() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(event) = serde_json::from_str::<ActivityEvent>(trimmed) else {
                continue;
            };
            events.push(event);
            if events.len() >= limit {
                break;
            }
        }
    }

    events.sort_by(|left, right| {
        right
            .recorded_at_unix
            .cmp(&left.recorded_at_unix)
            .then_with(|| right.event_id.cmp(&left.event_id))
    });
    events.truncate(limit);
    Ok(events)
}

fn collect_event_files(root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(root).with_context(|| format!("failed to read {}", root.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_event_files(&path, out)?;
        } else if path
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|name| name.ends_with(".events.jsonl"))
        {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use tempfile::tempdir;

    #[test]
    fn path_uses_expected_year_month_day_layout() {
        let root = PathBuf::from("/tmp/activity-root");
        let dt = chrono::Local
            .with_ymd_and_hms(2026, 3, 17, 14, 30, 0)
            .single()
            .expect("local datetime");
        let path = daily_log_path_at(&root, dt);
        assert_eq!(path, PathBuf::from("/tmp/activity-root/26/march/17.md"));
        let events_path = daily_events_path_at(&root, dt);
        assert_eq!(
            events_path,
            PathBuf::from("/tmp/activity-root/26/march/17.events.jsonl")
        );
    }

    #[test]
    fn append_daily_event_writes_human_line_and_sidecar() {
        let temp = tempdir().expect("tempdir");
        let now = chrono::Local
            .with_ymd_and_hms(2026, 3, 17, 14, 30, 0)
            .single()
            .expect("local datetime");
        let mut event = ActivityEvent::done("codex.resolve", "summarize codex memory work");
        event.route = Some("new-with-context".to_string());
        event.target_path = Some("/tmp/docs".to_string());
        event.session_id = Some("019cd046-8b33-73c2-abfd-88f49d26eba0".to_string());
        event.dedupe_key = Some("dedupe-1".to_string());

        let path = append_daily_event_at(temp.path(), now, event).expect("append");
        let body = fs::read_to_string(&path).expect("read log");
        assert!(body.starts_with("14:30: [done] codex.resolve[new-with-context] docs:"));
        assert!(body.contains("summarize codex memory work"));
        assert!(body.contains("[s:019cd046 "));
        assert!(body.contains("e:"));

        let events_body =
            fs::read_to_string(temp.path().join("26/march/17.events.jsonl")).expect("sidecar");
        let stored: ActivityEvent =
            serde_json::from_str(events_body.lines().next().expect("stored event line"))
                .expect("decode sidecar event");
        assert_eq!(stored.kind, "codex.resolve");
        assert_eq!(stored.scope.as_deref(), Some("docs"));
        assert_eq!(stored.dedupe_key.as_deref(), Some("dedupe-1"));
        assert!(!stored.event_id.is_empty());
    }

    #[test]
    fn append_daily_event_dedupes_when_explicit_key_matches() {
        let temp = tempdir().expect("tempdir");
        let now = chrono::Local
            .with_ymd_and_hms(2026, 3, 17, 14, 30, 0)
            .single()
            .expect("local datetime");

        let mut first = ActivityEvent::done("codex.done", "implement activity logging");
        first.dedupe_key = Some("codex:done:1".to_string());
        first.session_id = Some("019cd046-8b33-73c2-abfd-88f49d26eba0".to_string());
        first.target_path = Some("/tmp/code/flow".to_string());

        let mut second = ActivityEvent::done("codex.done", "implement activity logging");
        second.dedupe_key = Some("codex:done:1".to_string());
        second.session_id = Some("019cd046-8b33-73c2-abfd-88f49d26eba0".to_string());
        second.target_path = Some("/tmp/code/flow".to_string());
        second.recorded_at_unix += 10;

        append_daily_event_at(temp.path(), now, first).expect("first append");
        append_daily_event_at(temp.path(), now, second).expect("second append");

        let body = fs::read_to_string(temp.path().join("26/march/17.md")).expect("read log");
        assert_eq!(body.lines().count(), 1);
        let sidecar =
            fs::read_to_string(temp.path().join("26/march/17.events.jsonl")).expect("sidecar");
        assert_eq!(sidecar.lines().count(), 1);
    }

    #[test]
    fn append_daily_event_recovers_dedupe_index_from_sidecar() {
        let temp = tempdir().expect("tempdir");
        let now = chrono::Local
            .with_ymd_and_hms(2026, 3, 17, 14, 30, 0)
            .single()
            .expect("local datetime");

        let mut first = ActivityEvent::done("codex.done", "implement activity logging");
        first.dedupe_key = Some("codex:done:recover".to_string());
        append_daily_event_at(temp.path(), now, first).expect("first append");

        let index_dir = daily_dedupe_index_dir_at(temp.path(), now);
        fs::remove_dir_all(&index_dir).expect("remove dedupe index");

        let mut second = ActivityEvent::done("codex.done", "implement activity logging");
        second.dedupe_key = Some("codex:done:recover".to_string());
        append_daily_event_at(temp.path(), now, second).expect("second append");

        let body = fs::read_to_string(temp.path().join("26/march/17.md")).expect("read log");
        assert_eq!(body.lines().count(), 1);
        assert!(index_dir.exists());
        assert_eq!(fs::read_dir(index_dir).expect("index entries").count(), 1);
    }
}
