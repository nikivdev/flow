use std::path::Path;
use std::process::Command;

use anyhow::{Result, bail};
use chrono::Utc;

use crate::cli::{CommitQueueAction, CommitQueueCommand, ReviewsTodoAction, ReviewsTodoCommand};
use crate::commit;
use crate::todo;

pub fn run(cmd: ReviewsTodoCommand) -> Result<()> {
    let action = cmd.action.unwrap_or(ReviewsTodoAction::List);
    match action {
        ReviewsTodoAction::List => list_review_todos(),
        ReviewsTodoAction::Show { id } => show_review_todo(&id),
        ReviewsTodoAction::Done { id } => done_review_todo(&id),
        ReviewsTodoAction::Fix { id, all } => fix_review_todos(id.as_deref(), all),
        ReviewsTodoAction::Codex { hashes, all } => commit::run_commit_queue(CommitQueueCommand {
            action: Some(CommitQueueAction::Review { hashes, all }),
        }),
        ReviewsTodoAction::ApproveAll {
            force,
            allow_issues,
            allow_unreviewed,
        } => commit::run_commit_queue(CommitQueueCommand {
            action: Some(CommitQueueAction::ApproveAll {
                force,
                allow_issues,
                allow_unreviewed,
            }),
        }),
    }
}

fn list_review_todos() -> Result<()> {
    let root = todo::project_root();
    let items = todo::load_review_todos(&root)?;
    if items.is_empty() {
        println!("No review todos.");
        return Ok(());
    }

    let mut open_items: Vec<_> = items
        .iter()
        .filter(|item| item.status != "completed")
        .collect();

    if open_items.is_empty() {
        println!("All review todos resolved.");
        return Ok(());
    }

    // Sort by priority (P1 first)
    open_items.sort_by(|a, b| {
        let pa = a.priority.as_deref().unwrap_or("P4");
        let pb = b.priority.as_deref().unwrap_or("P4");
        pa.cmp(pb)
    });

    let (p1, p2, p3, p4, total) = todo::count_open_review_todos_by_priority(&root)?;
    println!(
        "Review todos: {} open (P1:{} P2:{} P3:{} P4:{})\n",
        total, p1, p2, p3, p4
    );

    for item in &open_items {
        let priority = item.priority.as_deref().unwrap_or("P4");
        let indicator = match priority {
            "P1" => "[P1 !!]",
            "P2" => "[P2 ! ]",
            "P3" => "[P3   ]",
            _ => "[P4   ]",
        };
        let short_id = &item.id[..item.id.len().min(8)];
        println!("{} {} {}", indicator, short_id, item.title);
    }

    Ok(())
}

fn show_review_todo(id: &str) -> Result<()> {
    let root = todo::project_root();
    let (_, items) = todo::load_items_at_root(&root)?;
    let review_items: Vec<_> = items
        .iter()
        .filter(|item| {
            item.external_ref
                .as_deref()
                .map(|r| r.starts_with("flow-review-issue-"))
                .unwrap_or(false)
        })
        .cloned()
        .collect();

    let idx = todo::find_item_index(&review_items, id)?;
    let item = &review_items[idx];

    let priority = item.priority.as_deref().unwrap_or("P4");
    let indicator = match priority {
        "P1" => "P1 (critical)",
        "P2" => "P2 (high)",
        "P3" => "P3 (medium)",
        _ => "P4 (low)",
    };

    println!("ID:       {}", item.id);
    println!("Title:    {}", item.title);
    println!("Priority: {}", indicator);
    println!("Status:   {}", item.status);
    println!("Created:  {}", item.created_at);
    if let Some(updated) = &item.updated_at {
        println!("Updated:  {}", updated);
    }
    if let Some(note) = &item.note {
        println!("\n{}", note);
    }

    Ok(())
}

fn done_review_todo(id: &str) -> Result<()> {
    let root = todo::project_root();
    let (path, mut items) = todo::load_items_at_root(&root)?;

    // Find among review items only
    let review_indices: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            item.external_ref
                .as_deref()
                .map(|r| r.starts_with("flow-review-issue-"))
                .unwrap_or(false)
        })
        .map(|(i, _)| i)
        .collect();

    // Match by id prefix among review items
    let mut matches = Vec::new();
    for &idx in &review_indices {
        if items[idx].id == id || items[idx].id.starts_with(id) {
            matches.push(idx);
        }
    }

    let idx = match matches.len() {
        0 => bail!("Review todo '{}' not found", id),
        1 => matches[0],
        _ => bail!("Review todo id '{}' is ambiguous", id),
    };

    if items[idx].status == "completed" {
        println!("Already completed: {}", items[idx].id);
        return Ok(());
    }

    items[idx].status = "completed".to_string();
    items[idx].updated_at = Some(Utc::now().to_rfc3339());
    todo::save_items(&path, &items)?;
    println!("✓ {} -> completed", items[idx].id);

    Ok(())
}

fn fix_review_todos(id: Option<&str>, all: bool) -> Result<()> {
    let root = todo::project_root();
    let items = todo::load_review_todos(&root)?;

    let open_items: Vec<_> = items
        .iter()
        .filter(|item| item.status != "completed")
        .collect();

    if open_items.is_empty() {
        println!("No open review todos to fix.");
        return Ok(());
    }

    let to_fix: Vec<_> = if let Some(id) = id {
        let mut matched = Vec::new();
        for item in &open_items {
            if item.id == id || item.id.starts_with(id) {
                matched.push(*item);
            }
        }
        if matched.is_empty() {
            bail!("Review todo '{}' not found among open items", id);
        }
        if matched.len() > 1 {
            bail!("Review todo id '{}' is ambiguous", id);
        }
        matched
    } else if all {
        open_items
    } else {
        bail!("Specify a todo id or use --all to fix all open review todos");
    };

    for item in &to_fix {
        fix_single_todo(&root, item)?;
    }

    Ok(())
}

fn fix_single_todo(root: &Path, item: &todo::TodoItem) -> Result<()> {
    let short_id = &item.id[..item.id.len().min(8)];
    let priority = item.priority.as_deref().unwrap_or("P4");
    println!("==> Fixing [{}] {} : {}", priority, short_id, item.title);

    // Extract commit SHA from note (line starting with "Commit: ")
    let commit_sha = item
        .note
        .as_deref()
        .and_then(|note| {
            note.lines()
                .find(|line| line.starts_with("Commit: "))
                .map(|line| line.trim_start_matches("Commit: ").trim().to_string())
        })
        .unwrap_or_default();

    // Get the original diff if we have a commit SHA
    let diff = if !commit_sha.is_empty() {
        let output = Command::new("git")
            .args(["show", "--format=", "--patch", &commit_sha])
            .current_dir(root)
            .output();
        match output {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
            _ => String::new(),
        }
    } else {
        String::new()
    };

    // Build the fix prompt
    let mut prompt = String::new();
    prompt.push_str("Fix the following code review issue.\n\n");
    prompt.push_str("Issue: ");
    prompt.push_str(&item.title);
    prompt.push('\n');
    if let Some(note) = &item.note {
        prompt.push_str("\nDetails:\n");
        prompt.push_str(note);
        prompt.push('\n');
    }
    if !diff.is_empty() {
        prompt.push_str("\nOriginal diff:\n```\n");
        // Truncate very large diffs
        let max_diff = 8000;
        if diff.len() > max_diff {
            prompt.push_str(&diff[..max_diff]);
            prompt.push_str("\n... (truncated)\n");
        } else {
            prompt.push_str(&diff);
        }
        prompt.push_str("```\n");
    }
    prompt
        .push_str("\nApply the minimal fix to resolve this issue. Only change what is necessary.");

    let codex_bin = commit::configured_codex_bin_for_workdir(root);
    // Run codex with the same configured binary resolution as commit reviews.
    let status = Command::new(&codex_bin)
        .args(["--approval-mode", "full-auto", "--quiet", &prompt])
        .current_dir(root)
        .status();

    match status {
        Ok(s) if s.success() => {
            println!("  ✓ Codex fix applied for {}", short_id);
            // Mark todo as completed
            let (path, mut all_items) = todo::load_items_at_root(root)?;
            if let Ok(idx) = todo::find_item_index(&all_items, &item.id) {
                all_items[idx].status = "completed".to_string();
                all_items[idx].updated_at = Some(Utc::now().to_rfc3339());
                todo::save_items(&path, &all_items)?;
            }
            Ok(())
        }
        Ok(s) => {
            eprintln!(
                "  ✗ Codex exited with status {} for {}",
                s.code().unwrap_or(-1),
                short_id
            );
            Ok(())
        }
        Err(e) => {
            eprintln!(
                "  ✗ Failed to run codex (bin: {}) for {}: {}",
                codex_bin, short_id, e
            );
            Ok(())
        }
    }
}
