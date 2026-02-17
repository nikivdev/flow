use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use uuid::Uuid;

use crate::ai;
use crate::cli::{TodoAction, TodoCommand, TodoStatusArg};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct TodoItem {
    pub id: String,
    pub title: String,
    pub status: String,
    pub created_at: String,
    pub updated_at: Option<String>,
    pub note: Option<String>,
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
}

pub fn run(cmd: TodoCommand) -> Result<()> {
    match cmd.action {
        None | Some(TodoAction::Bike) => open_bike(),
        Some(TodoAction::Add {
            title,
            note,
            session,
            no_session,
            status,
        }) => add(
            &title,
            note.as_deref(),
            session.as_deref(),
            no_session,
            status,
        ),
        Some(TodoAction::List { all }) => list(all),
        Some(TodoAction::Done { id }) => set_status(&id, TodoStatusArg::Completed),
        Some(TodoAction::Edit {
            id,
            title,
            status,
            note,
        }) => edit(&id, title.as_deref(), status, note),
        Some(TodoAction::Remove { id }) => remove(&id),
    }
}

fn open_bike() -> Result<()> {
    let root = project_root();
    let project_name = root
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.to_string())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "project".to_string());

    let dir = root.join(".ai").join("todos");
    let path = dir.join(format!("{}.bike", project_name));
    fs::create_dir_all(&dir)?;
    let needs_init = match fs::read_to_string(&path) {
        Ok(content) => !looks_like_bike(&content),
        Err(_) => true,
    };
    if needs_init {
        let content = render_bike_template(&project_name);
        fs::write(&path, content)?;
    }

    let bike_app = Path::new("/System/Volumes/Data/Applications/Bike.app");
    if !bike_app.exists() {
        bail!("Bike.app not found at {}", bike_app.display());
    }

    let status = Command::new("open")
        .arg("-a")
        .arg(bike_app)
        .arg(&path)
        .status()
        .context("failed to launch Bike.app")?;
    if !status.success() {
        bail!("Bike.app failed to open {}", path.display());
    }

    Ok(())
}

fn looks_like_bike(content: &str) -> bool {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("<?xml") {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    lower.contains("<html") && lower.contains("<body") && lower.contains("<ul")
}

fn render_bike_template(project_name: &str) -> String {
    let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let ul_id = format!("_{}", Uuid::new_v4().simple());
    let li_id = Uuid::new_v4().simple().to_string();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<html>\n  <head>\n    <meta charset=\"utf-8\"/>\n  </head>\n  <body>\n    <ul id=\"{}\" data-created=\"{}\" data-modified=\"{}\">\n      <li id=\"{}\" data-created=\"{}\" data-modified=\"{}\">\n        <p>{}</p>\n      </li>\n    </ul>\n  </body>\n</html>\n",
        ul_id, now, now, li_id, now, now, project_name
    )
}

fn add(
    title: &str,
    note: Option<&str>,
    session: Option<&str>,
    no_session: bool,
    status: TodoStatusArg,
) -> Result<()> {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        bail!("todo title cannot be empty");
    }
    let (path, mut items) = load_items()?;
    let session_ref = resolve_session_ref(session, no_session)?;
    let now = Utc::now().to_rfc3339();
    let item = TodoItem {
        id: Uuid::new_v4().simple().to_string(),
        title: trimmed.to_string(),
        status: status_to_string(status).to_string(),
        created_at: now,
        updated_at: None,
        note: note.map(|n| n.trim().to_string()).filter(|n| !n.is_empty()),
        session: session_ref,
        external_ref: None,
        priority: None,
    };
    items.push(item.clone());
    save_items(&path, &items)?;
    println!("✓ Added {} [{}]", item.id, item.title);
    Ok(())
}

fn list(show_all: bool) -> Result<()> {
    let (_path, items) = load_items()?;
    if items.is_empty() {
        println!("No todos yet.");
        return Ok(());
    }

    let mut count = 0;
    for item in &items {
        if !show_all && item.status == status_to_string(TodoStatusArg::Completed) {
            continue;
        }
        count += 1;
        println!("[{}] {} {}", item.status, item.id, item.title);
        if let Some(note) = &item.note {
            println!("  - {}", note);
        }
        if let Some(session) = &item.session {
            println!("  @ {}", session);
        }
    }
    if count == 0 {
        println!("No active todos.");
    }
    Ok(())
}

fn edit(
    id: &str,
    title: Option<&str>,
    status: Option<TodoStatusArg>,
    note: Option<String>,
) -> Result<()> {
    let (path, mut items) = load_items()?;
    let idx = find_item_index(&items, id)?;
    let item_id = {
        let item = &mut items[idx];

        if let Some(title) = title {
            let title = title.trim();
            if !title.is_empty() {
                item.title = title.to_string();
            }
        }

        if let Some(status) = status {
            item.status = status_to_string(status).to_string();
        }

        if let Some(note) = note {
            let note = note.trim().to_string();
            item.note = if note.is_empty() { None } else { Some(note) };
        }

        item.updated_at = Some(Utc::now().to_rfc3339());
        item.id.clone()
    };
    save_items(&path, &items)?;
    println!("✓ Updated {}", item_id);
    Ok(())
}

fn set_status(id: &str, status: TodoStatusArg) -> Result<()> {
    let (path, mut items) = load_items()?;
    let idx = find_item_index(&items, id)?;
    let (item_id, item_status) = {
        let item = &mut items[idx];
        item.status = status_to_string(status).to_string();
        item.updated_at = Some(Utc::now().to_rfc3339());
        (item.id.clone(), item.status.clone())
    };
    save_items(&path, &items)?;
    println!("✓ {} -> {}", item_id, item_status);
    Ok(())
}

fn remove(id: &str) -> Result<()> {
    let (path, mut items) = load_items()?;
    let idx = find_item_index(&items, id)?;
    let item = items.remove(idx);
    save_items(&path, &items)?;
    println!("✓ Removed {}", item.id);
    Ok(())
}

fn status_to_string(status: TodoStatusArg) -> &'static str {
    match status {
        TodoStatusArg::Pending => "pending",
        TodoStatusArg::InProgress => "in_progress",
        TodoStatusArg::Completed => "completed",
        TodoStatusArg::Blocked => "blocked",
    }
}

fn load_items() -> Result<(PathBuf, Vec<TodoItem>)> {
    let root = project_root();
    let dir = root.join(".ai").join("todos");
    let path = dir.join("todos.json");

    if !path.exists() {
        return Ok((path, Vec::new()));
    }

    let content =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    if content.trim().is_empty() {
        return Ok((path, Vec::new()));
    }
    let items = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok((path, items))
}

pub(crate) fn load_items_at_root(root: &Path) -> Result<(PathBuf, Vec<TodoItem>)> {
    let dir = root.join(".ai").join("todos");
    let path = dir.join("todos.json");

    if !path.exists() {
        return Ok((path, Vec::new()));
    }

    let content =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    if content.trim().is_empty() {
        return Ok((path, Vec::new()));
    }
    let items = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok((path, items))
}

pub(crate) fn save_items(path: &Path, items: &[TodoItem]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(items)?;
    fs::write(path, content)?;
    Ok(())
}

fn todo_title_compact(title: &str) -> String {
    let trimmed = title.trim().trim_start_matches('-').trim();
    let max_len = 120;
    let mut out = String::new();
    let mut count = 0;
    for ch in trimmed.chars() {
        if count >= max_len {
            out.push_str("...");
            break;
        }
        out.push(ch);
        count += 1;
    }
    if out.is_empty() {
        "todo".to_string()
    } else {
        out
    }
}

fn external_ref_for_review_issue(commit_sha: &str, issue: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(commit_sha.trim().as_bytes());
    hasher.update(b":");
    hasher.update(issue.trim().as_bytes());
    let hex = hex::encode(hasher.finalize());
    let short = hex.get(..12).unwrap_or(&hex);
    format!("flow-review-issue-{}", short)
}

/// Infer priority from issue text using keyword heuristics.
pub(crate) fn parse_priority_from_issue(issue: &str) -> String {
    let lower = issue.to_lowercase();
    if lower.contains("secret")
        || lower.contains("credential")
        || lower.contains("api key")
        || lower.contains("injection")
        || lower.contains("vulnerability")
        || lower.contains("security")
    {
        return "P1".to_string();
    }
    if lower.contains("crash")
        || lower.contains("data loss")
        || lower.contains("race condition")
        || lower.contains("memory leak")
        || lower.contains("buffer overflow")
    {
        return "P2".to_string();
    }
    if lower.contains("bug")
        || lower.contains("error handling")
        || lower.contains("panic")
        || lower.contains("unwrap")
        || lower.contains("missing validation")
    {
        return "P3".to_string();
    }
    "P4".to_string()
}

/// Load only review todos (those with external_ref starting with "flow-review-issue-").
pub(crate) fn load_review_todos(repo_root: &Path) -> Result<Vec<TodoItem>> {
    let (_path, items) = load_items_at_root(repo_root)?;
    Ok(items
        .into_iter()
        .filter(|item| {
            item.external_ref
                .as_deref()
                .map(|r| r.starts_with("flow-review-issue-"))
                .unwrap_or(false)
        })
        .collect())
}

/// Count open (non-completed) review todos by priority.
/// Returns (p1, p2, p3, p4, total).
pub(crate) fn count_open_review_todos_by_priority(
    repo_root: &Path,
) -> Result<(usize, usize, usize, usize, usize)> {
    let items = load_review_todos(repo_root)?;
    let (mut p1, mut p2, mut p3, mut p4) = (0, 0, 0, 0);
    for item in &items {
        if item.status == "completed" {
            continue;
        }
        match item.priority.as_deref().unwrap_or("P4") {
            "P1" => p1 += 1,
            "P2" => p2 += 1,
            "P3" => p3 += 1,
            _ => p4 += 1,
        }
    }
    let total = p1 + p2 + p3 + p4;
    Ok((p1, p2, p3, p4, total))
}

/// Record review issues as project-scoped todos under `.ai/todos/todos.json`.
/// Returns ids for created items (deduplicated by `external_ref`).
pub fn record_review_issues_as_todos(
    repo_root: &Path,
    commit_sha: &str,
    issues: &[String],
    summary: Option<&str>,
    model_label: &str,
) -> Result<Vec<String>> {
    if issues.is_empty() {
        return Ok(Vec::new());
    }

    let (path, mut items) = load_items_at_root(repo_root)?;
    let mut existing_refs = std::collections::HashSet::new();
    for item in &items {
        if let Some(r) = item
            .external_ref
            .as_deref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            existing_refs.insert(r.to_string());
        }
    }

    let mut created_ids = Vec::new();
    let now = Utc::now().to_rfc3339();
    let summary = summary.map(|s| s.trim()).filter(|s| !s.is_empty());

    for issue in issues {
        let ext = external_ref_for_review_issue(commit_sha, issue);
        if existing_refs.contains(&ext) {
            continue;
        }

        let title = todo_title_compact(issue);
        let mut note = String::new();
        note.push_str("Source: flow review\n");
        note.push_str("Commit: ");
        note.push_str(commit_sha.trim());
        note.push('\n');
        note.push_str("Model: ");
        note.push_str(model_label.trim());
        note.push('\n');
        if let Some(summary) = summary {
            note.push_str("Review summary: ");
            note.push_str(summary);
            note.push('\n');
        }
        note.push('\n');
        note.push_str(issue.trim());

        let id = Uuid::new_v4().simple().to_string();
        let priority = parse_priority_from_issue(issue);
        items.push(TodoItem {
            id: id.clone(),
            title,
            status: status_to_string(TodoStatusArg::Pending).to_string(),
            created_at: now.clone(),
            updated_at: None,
            note: Some(note),
            session: None,
            external_ref: Some(ext.clone()),
            priority: Some(priority),
        });
        existing_refs.insert(ext);
        created_ids.push(id);
    }

    if !created_ids.is_empty() {
        save_items(&path, &items)?;
    }

    Ok(created_ids)
}

/// Mark review-timeout follow-up todos as completed for the given todo ids.
/// Returns number of todos updated.
pub fn complete_review_timeout_todos(repo_root: &Path, ids: &[String]) -> Result<usize> {
    if ids.is_empty() {
        return Ok(0);
    }

    let targets: std::collections::HashSet<String> = ids
        .iter()
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
        .collect();
    if targets.is_empty() {
        return Ok(0);
    }

    let (path, mut items) = load_items_at_root(repo_root)?;
    let mut updated = 0usize;
    let now = Utc::now().to_rfc3339();

    for item in &mut items {
        if !targets.contains(&item.id) {
            continue;
        }
        if !is_review_timeout_followup(item) {
            continue;
        }
        if item.status == status_to_string(TodoStatusArg::Completed) {
            continue;
        }
        item.status = status_to_string(TodoStatusArg::Completed).to_string();
        item.updated_at = Some(now.clone());
        updated += 1;
    }

    if updated > 0 {
        save_items(&path, &items)?;
    }

    Ok(updated)
}

/// Count review todos by ids that are still not completed.
pub fn count_open_todos(repo_root: &Path, ids: &[String]) -> Result<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    let targets: std::collections::HashSet<String> = ids
        .iter()
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
        .collect();
    if targets.is_empty() {
        return Ok(0);
    }

    let (_path, items) = load_items_at_root(repo_root)?;
    let mut open = 0usize;
    for item in items {
        if !targets.contains(&item.id) {
            continue;
        }
        if item.status != status_to_string(TodoStatusArg::Completed) {
            open += 1;
        }
    }
    Ok(open)
}

fn is_review_timeout_followup(item: &TodoItem) -> bool {
    let title = item.title.trim().to_lowercase();
    if title.starts_with("re-run review:") || title.contains("review timed out") {
        return true;
    }
    item.note
        .as_deref()
        .map(|n| n.to_lowercase().contains("review timed out"))
        .unwrap_or(false)
}

pub(crate) fn find_item_index(items: &[TodoItem], id: &str) -> Result<usize> {
    let mut matches = Vec::new();
    for (idx, item) in items.iter().enumerate() {
        if item.id == id || item.id.starts_with(id) {
            matches.push(idx);
        }
    }

    match matches.len() {
        0 => bail!("Todo '{}' not found", id),
        1 => Ok(matches[0]),
        _ => bail!("Todo id '{}' is ambiguous", id),
    }
}

fn resolve_session_ref(session: Option<&str>, no_session: bool) -> Result<Option<String>> {
    if no_session {
        return Ok(None);
    }

    if let Some(session) = session {
        let trimmed = session.trim();
        return Ok(if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        });
    }

    let root = project_root();
    match ai::get_latest_session_ref_for_path(&root)? {
        Some(latest) => Ok(Some(latest)),
        None => Ok(None),
    }
}

pub(crate) fn project_root() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if let Some(flow_path) = find_flow_toml(&cwd) {
        return flow_path.parent().unwrap_or(&cwd).to_path_buf();
    }
    cwd
}

fn find_flow_toml(start: &PathBuf) -> Option<PathBuf> {
    let mut current = start.clone();
    loop {
        let candidate = current.join("flow.toml");
        if candidate.exists() {
            return Some(candidate);
        }
        if !current.pop() {
            return None;
        }
    }
}
