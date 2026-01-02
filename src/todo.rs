use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::ai;
use crate::cli::{TodoAction, TodoCommand, TodoStatusArg};

#[derive(Debug, Serialize, Deserialize, Clone)]
struct TodoItem {
    id: String,
    title: String,
    status: String,
    created_at: String,
    updated_at: Option<String>,
    note: Option<String>,
    session: Option<String>,
}

pub fn run(cmd: TodoCommand) -> Result<()> {
    let action = cmd.action.unwrap_or(TodoAction::List { all: false });
    match action {
        TodoAction::Add {
            title,
            note,
            session,
            no_session,
            status,
        } => add(&title, note.as_deref(), session.as_deref(), no_session, status),
        TodoAction::List { all } => list(all),
        TodoAction::Done { id } => set_status(&id, TodoStatusArg::Completed),
        TodoAction::Edit { id, title, status, note } => edit(&id, title.as_deref(), status, note),
        TodoAction::Remove { id } => remove(&id),
    }
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

fn edit(id: &str, title: Option<&str>, status: Option<TodoStatusArg>, note: Option<String>) -> Result<()> {
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

    let content = fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    if content.trim().is_empty() {
        return Ok((path, Vec::new()));
    }
    let items = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok((path, items))
}

fn save_items(path: &Path, items: &[TodoItem]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(items)?;
    fs::write(path, content)?;
    Ok(())
}

fn find_item_index(items: &[TodoItem], id: &str) -> Result<usize> {
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

fn project_root() -> PathBuf {
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
