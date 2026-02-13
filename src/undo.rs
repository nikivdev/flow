//! Undo system for flow actions.
//!
//! Tracks undoable actions (commit, push, etc.) and provides undo functionality.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{debug, info};

use crate::cli::{UndoAction, UndoCommand};

/// Run the undo command.
pub fn run(cmd: UndoCommand) -> Result<()> {
    let cwd = std::env::current_dir()?;

    // Find repository root
    let repo_root = find_repo_root(&cwd)?;

    match cmd.action {
        Some(UndoAction::Show) => {
            show_last(&repo_root)?;
        }
        Some(UndoAction::List { limit }) => {
            list_actions(&repo_root, limit)?;
        }
        None => {
            // Default action: undo the last action
            let opts = UndoOpts {
                dry_run: cmd.dry_run,
                force: cmd.force,
            };

            match undo_last(&repo_root, &opts) {
                Ok(result) => {
                    if !cmd.dry_run {
                        if result.force_pushed {
                            println!("\nAction undone. Remote has been updated.");
                        } else {
                            println!("\nAction undone.");
                        }
                    }
                }
                Err(e) => {
                    bail!("{}", e);
                }
            }
        }
    }

    Ok(())
}

/// Find the git repository root from a path.
fn find_repo_root(start: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(start)
        .output()
        .context("failed to find git repository")?;

    if !output.status.success() {
        bail!("Not in a git repository");
    }

    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(PathBuf::from(path))
}

/// Action types that can be undone.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ActionType {
    /// Git commit (can be undone with reset)
    Commit,
    /// Git push (can be undone with force push)
    Push,
    /// Commit + push together
    CommitPush,
}

impl std::fmt::Display for ActionType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ActionType::Commit => write!(f, "commit"),
            ActionType::Push => write!(f, "push"),
            ActionType::CommitPush => write!(f, "commit+push"),
        }
    }
}

/// Record of an undoable action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoRecord {
    /// Timestamp when action was performed (ISO 8601)
    pub timestamp: String,
    /// Type of action
    pub action: ActionType,
    /// Git commit SHA before the action (for reverting)
    pub before_sha: String,
    /// Git commit SHA after the action
    pub after_sha: String,
    /// Branch name
    pub branch: String,
    /// Whether the action included a push
    pub pushed: bool,
    /// Remote name (if pushed)
    pub remote: Option<String>,
    /// Commit message (for display)
    pub message: Option<String>,
}

/// Get the undo log path for a repository.
fn undo_log_path(repo_root: &Path) -> PathBuf {
    repo_root
        .join(".ai")
        .join("internal")
        .join("undo-log.jsonl")
}

/// Record an undoable action.
pub fn record_action(
    repo_root: &Path,
    action: ActionType,
    before_sha: &str,
    after_sha: &str,
    branch: &str,
    pushed: bool,
    remote: Option<&str>,
    message: Option<&str>,
) -> Result<()> {
    let log_path = undo_log_path(repo_root);

    // Ensure parent directory exists
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let record = UndoRecord {
        timestamp: chrono::Utc::now().to_rfc3339(),
        action,
        before_sha: before_sha.to_string(),
        after_sha: after_sha.to_string(),
        branch: branch.to_string(),
        pushed,
        remote: remote.map(|s| s.to_string()),
        message: message.map(|s| s.to_string()),
    };

    let line = serde_json::to_string(&record)?;

    // Append to log file
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;

    use std::io::Write;
    writeln!(file, "{}", line)?;

    debug!(
        action = %record.action,
        before = %record.before_sha,
        after = %record.after_sha,
        "recorded undo action"
    );

    Ok(())
}

/// Get the last undoable action for the current repository.
pub fn get_last_action(repo_root: &Path) -> Result<Option<UndoRecord>> {
    let log_path = undo_log_path(repo_root);

    if !log_path.exists() {
        return Ok(None);
    }

    let content = fs::read_to_string(&log_path)?;
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();

    if lines.is_empty() {
        return Ok(None);
    }

    // Get the last line
    let last_line = lines.last().unwrap();
    let record: UndoRecord =
        serde_json::from_str(last_line).context("failed to parse last undo record")?;

    Ok(Some(record))
}

/// Remove the last action from the undo log.
fn remove_last_action(repo_root: &Path) -> Result<()> {
    let log_path = undo_log_path(repo_root);

    if !log_path.exists() {
        return Ok(());
    }

    let content = fs::read_to_string(&log_path)?;
    let mut lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();

    if lines.is_empty() {
        return Ok(());
    }

    // Remove last line
    lines.pop();

    // Rewrite file
    let new_content = if lines.is_empty() {
        String::new()
    } else {
        lines.join("\n") + "\n"
    };

    fs::write(&log_path, new_content)?;

    Ok(())
}

/// Options for undo operation.
#[derive(Debug, Default)]
pub struct UndoOpts {
    /// Dry run - show what would be done without doing it
    pub dry_run: bool,
    /// Force undo even if it requires force push
    pub force: bool,
}

/// Result of an undo operation.
#[derive(Debug)]
pub struct UndoResult {
    pub action_type: ActionType,
    pub before_sha: String,
    pub after_sha: String,
    pub force_pushed: bool,
}

/// Undo the last action.
pub fn undo_last(repo_root: &Path, opts: &UndoOpts) -> Result<UndoResult> {
    let record =
        get_last_action(repo_root)?.ok_or_else(|| anyhow::anyhow!("No actions to undo"))?;

    // Check if we're on the same branch
    let current_branch = git_capture(repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if current_branch.trim() != record.branch {
        bail!(
            "Currently on branch '{}', but last action was on '{}'. Switch branches first.",
            current_branch.trim(),
            record.branch
        );
    }

    // Check if HEAD matches the after_sha
    let current_sha = git_capture(repo_root, &["rev-parse", "HEAD"])?;
    if !current_sha
        .trim()
        .starts_with(&record.after_sha[..7.min(record.after_sha.len())])
    {
        // Try short comparison
        let current_short = &current_sha.trim()[..7.min(current_sha.len())];
        let record_short = &record.after_sha[..7.min(record.after_sha.len())];
        if current_short != record_short {
            bail!(
                "HEAD ({}) doesn't match the recorded action ({}). \
                 The repository state has changed since the action was recorded.",
                current_short,
                record_short
            );
        }
    }

    if opts.dry_run {
        println!(
            "Would undo: {} ({})",
            record.action,
            short_sha(&record.after_sha)
        );
        println!("  Reset to: {}", short_sha(&record.before_sha));
        if record.pushed {
            println!("  Would force push to remote");
        }
        return Ok(UndoResult {
            action_type: record.action.clone(),
            before_sha: record.before_sha.clone(),
            after_sha: record.after_sha.clone(),
            force_pushed: false,
        });
    }

    // Perform the undo based on action type
    match &record.action {
        ActionType::Commit => {
            undo_commit(repo_root, &record)?;
        }
        ActionType::Push => {
            if !opts.force {
                bail!("Undoing a push requires --force flag (this will force push to remote)");
            }
            undo_push(repo_root, &record)?;
        }
        ActionType::CommitPush => {
            if record.pushed && !opts.force {
                bail!("This action was pushed to remote. Use --force to undo (will force push)");
            }
            undo_commit_push(repo_root, &record, opts.force)?;
        }
    }

    // Remove from undo log after successful undo
    remove_last_action(repo_root)?;

    let force_pushed = record.pushed
        && (record.action == ActionType::Push || record.action == ActionType::CommitPush);

    Ok(UndoResult {
        action_type: record.action,
        before_sha: record.before_sha,
        after_sha: record.after_sha,
        force_pushed,
    })
}

/// Undo a commit (reset --soft to keep changes staged).
fn undo_commit(repo_root: &Path, record: &UndoRecord) -> Result<()> {
    info!(sha = %record.after_sha, "undoing commit");

    // Use --soft to keep changes staged
    git_run(repo_root, &["reset", "--soft", &record.before_sha])?;

    println!("✓ Undid commit {}", short_sha(&record.after_sha));
    println!("  Changes are still staged");

    Ok(())
}

/// Undo a push (force push the previous state).
fn undo_push(repo_root: &Path, record: &UndoRecord) -> Result<()> {
    let remote = record.remote.as_deref().unwrap_or("origin");

    info!(
        sha = %record.after_sha,
        remote = %remote,
        branch = %record.branch,
        "undoing push with force push"
    );

    // Force push the before_sha to the branch
    git_run(
        repo_root,
        &[
            "push",
            "--force",
            remote,
            &format!("{}:{}", record.before_sha, record.branch),
        ],
    )?;

    println!(
        "✓ Force pushed {} to {}/{}",
        short_sha(&record.before_sha),
        remote,
        record.branch
    );

    Ok(())
}

/// Undo a commit+push operation.
fn undo_commit_push(repo_root: &Path, record: &UndoRecord, force: bool) -> Result<()> {
    info!(sha = %record.after_sha, pushed = record.pushed, "undoing commit+push");

    // First, reset the local commit
    git_run(repo_root, &["reset", "--soft", &record.before_sha])?;
    println!("✓ Undid commit {}", short_sha(&record.after_sha));

    // If it was pushed, force push to revert remote
    if record.pushed && force {
        let remote = record.remote.as_deref().unwrap_or("origin");
        git_run(repo_root, &["push", "--force", remote, &record.branch])?;
        println!("✓ Force pushed to {}/{}", remote, record.branch);
    }

    println!("  Changes are still staged");

    Ok(())
}

/// Show the last undoable action without undoing it.
pub fn show_last(repo_root: &Path) -> Result<()> {
    match get_last_action(repo_root)? {
        Some(record) => {
            println!("Last undoable action:");
            println!("  Type: {}", record.action);
            println!("  Time: {}", record.timestamp);
            println!("  Branch: {}", record.branch);
            println!("  Before: {}", short_sha(&record.before_sha));
            println!("  After: {}", short_sha(&record.after_sha));
            if record.pushed {
                println!(
                    "  Pushed: yes (to {})",
                    record.remote.as_deref().unwrap_or("origin")
                );
            }
            if let Some(msg) = &record.message {
                let short_msg = if msg.len() > 60 {
                    format!("{}...", &msg[..57])
                } else {
                    msg.clone()
                };
                println!("  Message: {}", short_msg);
            }
        }
        None => {
            println!("No undoable actions recorded for this repository.");
        }
    }

    Ok(())
}

/// List recent undoable actions.
pub fn list_actions(repo_root: &Path, limit: usize) -> Result<()> {
    let log_path = undo_log_path(repo_root);

    if !log_path.exists() {
        println!("No undo history for this repository.");
        return Ok(());
    }

    let content = fs::read_to_string(&log_path)?;
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();

    if lines.is_empty() {
        println!("No undo history for this repository.");
        return Ok(());
    }

    println!("Recent actions (newest first):");
    println!();

    let start = if lines.len() > limit {
        lines.len() - limit
    } else {
        0
    };

    for (i, line) in lines[start..].iter().rev().enumerate() {
        if let Ok(record) = serde_json::from_str::<UndoRecord>(line) {
            let pushed_indicator = if record.pushed { " [pushed]" } else { "" };
            let msg_short = record
                .message
                .as_ref()
                .map(|m| {
                    if m.len() > 40 {
                        format!("{:.40}...", m)
                    } else {
                        m.clone()
                    }
                })
                .unwrap_or_default();

            if i == 0 {
                println!(
                    "  → {} {} {}{} {}",
                    short_sha(&record.after_sha),
                    record.action,
                    record.branch,
                    pushed_indicator,
                    msg_short
                );
            } else {
                println!(
                    "    {} {} {}{} {}",
                    short_sha(&record.after_sha),
                    record.action,
                    record.branch,
                    pushed_indicator,
                    msg_short
                );
            }
        }
    }

    println!();
    println!("Use 'f undo' to undo the most recent action (→)");

    Ok(())
}

// Helper functions

fn short_sha(sha: &str) -> &str {
    &sha[..7.min(sha.len())]
}

fn git_capture(repo_root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .output()
        .context("failed to run git command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {} failed: {}", args.join(" "), stderr);
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn git_run(repo_root: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .status()
        .context("failed to run git command")?;

    if !status.success() {
        bail!("git {} failed", args.join(" "));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_action_type_display() {
        assert_eq!(format!("{}", ActionType::Commit), "commit");
        assert_eq!(format!("{}", ActionType::Push), "push");
        assert_eq!(format!("{}", ActionType::CommitPush), "commit+push");
    }

    #[test]
    fn test_short_sha() {
        assert_eq!(short_sha("abc1234567890"), "abc1234");
        assert_eq!(short_sha("abc"), "abc");
    }
}
