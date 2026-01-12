//! Browse and analyze git commits with AI session metadata.
//!
//! Shows commits with attached AI sessions, reviews, and other metadata.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::cli::{CommitsAction, CommitsCommand, CommitsOpts};

const TOP_COMMITS_PATH: &str = ".ai/internal/commits/top.txt";

/// Commit with associated metadata
#[derive(Debug, Clone)]
struct CommitEntry {
    /// Git commit hash (short)
    hash: String,
    /// Full commit hash
    full_hash: String,
    /// Commit subject line (used in display)
    #[allow(dead_code)]
    subject: String,
    /// Relative time (e.g., "2 hours ago")
    relative_time: String,
    /// Author name
    author: String,
    /// Whether this commit has AI session metadata
    has_ai_metadata: bool,
    /// Whether this commit is marked notable
    is_top: bool,
    /// Display string for fzf
    display: String,
}

/// Run the commits subcommand.
pub fn run(cmd: CommitsCommand) -> Result<()> {
    match cmd.action {
        Some(CommitsAction::Top) => run_top(),
        Some(CommitsAction::Mark { hash }) => mark_top_commit(&hash),
        Some(CommitsAction::Unmark { hash }) => unmark_top_commit(&hash),
        None => run_list(&cmd.opts),
    }
}

fn run_list(opts: &CommitsOpts) -> Result<()> {
    let top_entries = load_top_entries()?;
    let top_set = top_hashes(&top_entries);
    let commits = list_commits(opts.limit, opts.all, &top_set)?;

    if commits.is_empty() {
        println!("No commits found.");
        return Ok(());
    }

    // Check for fzf
    if which::which("fzf").is_err() {
        println!("fzf not found – install it for fuzzy selection.");
        println!("\nCommits:");
        for commit in &commits {
            println!("{}", commit.display);
        }
        return Ok(());
    }

    // Run fzf with preview
    println!("Tip: press ctrl-t to toggle notable for the selected commit.");
    if let Some(selection) = run_commits_fzf(&commits)? {
        match selection.action {
            CommitAction::Show => show_commit_details(selection.entry)?,
            CommitAction::ToggleTop => toggle_top_commit(selection.entry)?,
        }
    }

    Ok(())
}

fn run_top() -> Result<()> {
    let top_entries = load_top_entries()?;
    if top_entries.is_empty() {
        println!("No notable commits yet.");
        return Ok(());
    }

    let commits = list_commits_by_hashes(&top_entries)?;
    if commits.is_empty() {
        println!("No notable commits found.");
        return Ok(());
    }

    if which::which("fzf").is_err() {
        println!("fzf not found – install it for fuzzy selection.");
        println!("\nNotable commits:");
        for commit in &commits {
            println!("{}", commit.display);
        }
        return Ok(());
    }

    println!("Tip: press ctrl-t to toggle notable for the selected commit.");
    if let Some(selection) = run_commits_fzf(&commits)? {
        match selection.action {
            CommitAction::Show => show_commit_details(selection.entry)?,
            CommitAction::ToggleTop => toggle_top_commit(selection.entry)?,
        }
    }

    Ok(())
}

fn mark_top_commit(hash: &str) -> Result<()> {
    let commit = load_commit_by_ref(hash)?.context("Commit not found")?;
    let mut entries = load_top_entries()?;
    if entries.iter().any(|entry| entry.hash == commit.full_hash) {
        println!("Commit already marked notable: {}", commit.hash);
        return Ok(());
    }
    let label = commit.subject.replace('\t', " ");
    entries.push(TopEntry {
        hash: commit.full_hash.clone(),
        label: Some(label),
    });
    write_top_entries(&entries)?;
    println!("Marked notable: {} {}", commit.hash, commit.subject);
    Ok(())
}

fn unmark_top_commit(hash: &str) -> Result<()> {
    let full_hash = resolve_full_hash(hash)?;
    let mut entries = load_top_entries()?;
    let before = entries.len();
    entries.retain(|entry| entry.hash != full_hash);
    if entries.len() == before {
        println!("Commit not in notable list: {}", hash);
        return Ok(());
    }
    write_top_entries(&entries)?;
    println!("Removed notable commit: {}", hash);
    Ok(())
}

/// List recent commits with metadata.
fn list_commits(
    limit: usize,
    all_branches: bool,
    top_hashes: &HashSet<String>,
) -> Result<Vec<CommitEntry>> {
    let mut args = vec!["log", "--pretty=format:%h|%H|%s|%ar|%an", "-n"];
    let limit_str = limit.to_string();
    args.push(&limit_str);

    if all_branches {
        args.push("--all");
    }

    let output = Command::new("git")
        .args(&args)
        .output()
        .context("failed to run git log")?;

    if !output.status.success() {
        bail!("git log failed");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut commits = Vec::new();

    for line in stdout.lines() {
        let parts: Vec<&str> = line.splitn(5, '|').collect();
        if parts.len() < 5 {
            continue;
        }

        let hash = parts[0].to_string();
        let full_hash = parts[1].to_string();
        let subject = parts[2].to_string();
        let relative_time = parts[3].to_string();
        let author = parts[4].to_string();

        // Check if commit has AI metadata (check git notes or commit trailers)
        let has_ai_metadata = check_ai_metadata(&full_hash);
        let is_top = top_hashes.contains(&full_hash);

        // Build display string
        let ai_indicator = if has_ai_metadata { "◆ " } else { "  " };
        let top_indicator = if is_top { "TOP " } else { "    " };
        let pretty = format!(
            "{}{}{} | {} | {} | {}",
            top_indicator,
            ai_indicator,
            hash,
            truncate_str(&subject, 50),
            relative_time,
            author
        );
        let display = format!("{}\t{}", hash, pretty);

        commits.push(CommitEntry {
            hash,
            full_hash,
            subject,
            relative_time,
            author,
            has_ai_metadata,
            is_top,
            display,
        });
    }

    Ok(commits)
}

fn list_commits_by_hashes(entries: &[TopEntry]) -> Result<Vec<CommitEntry>> {
    let mut commits = Vec::new();
    for entry in entries {
        if let Some(commit) = load_commit_by_ref(&entry.hash)? {
            commits.push(commit);
        }
    }
    Ok(commits)
}

/// Check if a commit has AI session metadata attached.
fn check_ai_metadata(commit_hash: &str) -> bool {
    // Check git notes for AI metadata
    let output = Command::new("git")
        .args(["notes", "show", commit_hash])
        .output();

    if let Ok(output) = output {
        if output.status.success() {
            let notes = String::from_utf8_lossy(&output.stdout);
            if notes.contains("ai-session") || notes.contains("claude") || notes.contains("codex") {
                return true;
            }
        }
    }

    // Check commit message for AI-related trailers
    let output = Command::new("git")
        .args(["log", "-1", "--format=%B", commit_hash])
        .output();

    if let Ok(output) = output {
        if output.status.success() {
            let body = String::from_utf8_lossy(&output.stdout).to_lowercase();
            if body.contains("reviewed-by: codex")
                || body.contains("reviewed-by: claude")
                || body.contains("ai-session:")
            {
                return true;
            }
        }
    }

    false
}

enum CommitAction {
    Show,
    ToggleTop,
}

struct CommitSelection<'a> {
    entry: &'a CommitEntry,
    action: CommitAction,
}

/// Run fzf with preview for commits.
fn run_commits_fzf(commits: &[CommitEntry]) -> Result<Option<CommitSelection<'_>>> {
    let mut child = Command::new("fzf")
        .arg("--prompt")
        .arg("commits> ")
        .arg("--ansi")
        .arg("--header")
        .arg("ctrl-t: toggle notable")
        .arg("--expect")
        .arg("ctrl-t")
        .arg("--delimiter")
        .arg("\t")
        .arg("--with-nth")
        .arg("2..")
        .arg("--preview")
        .arg("git show --stat --color=always {1}")
        .arg("--preview-window")
        .arg("down:50%:wrap")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("failed to spawn fzf")?;

    {
        let stdin = child.stdin.as_mut().context("failed to open fzf stdin")?;
        for commit in commits {
            // Write with hash first for preview extraction
            writeln!(stdin, "{}", commit.display)?;
        }
    }

    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Ok(None);
    }

    let selection = String::from_utf8(output.stdout).context("fzf output was not valid UTF-8")?;
    let mut lines = selection.lines();
    let key = lines.next().unwrap_or("");
    let selection = lines.next().unwrap_or("").trim();

    if selection.is_empty() {
        return Ok(None);
    }

    let action = if key == "ctrl-t" {
        CommitAction::ToggleTop
    } else {
        CommitAction::Show
    };
    let Some(entry) = commits.iter().find(|c| c.display == selection) else {
        return Ok(None);
    };
    Ok(Some(CommitSelection { entry, action }))
}

/// Show detailed commit information including AI metadata.
fn show_commit_details(commit: &CommitEntry) -> Result<()> {
    println!("\n────────────────────────────────────────");
    println!("Commit: {} ({})", commit.hash, commit.relative_time);
    println!("Author: {}", commit.author);
    if commit.is_top {
        println!("Notable: yes");
    }
    println!("────────────────────────────────────────\n");

    // Show commit message
    let output = Command::new("git")
        .args(["log", "-1", "--format=%B", &commit.full_hash])
        .output()
        .context("failed to get commit message")?;

    if output.status.success() {
        let message = String::from_utf8_lossy(&output.stdout);
        println!("Message:\n{}", message);
    }

    // Show AI metadata if present
    if commit.has_ai_metadata {
        println!("────────────────────────────────────────");
        println!("AI Session Metadata:");
        println!("────────────────────────────────────────\n");

        // Try to get notes
        let notes_output = Command::new("git")
            .args(["notes", "show", &commit.full_hash])
            .output();

        if let Ok(notes) = notes_output {
            if notes.status.success() {
                let notes_content = String::from_utf8_lossy(&notes.stdout);
                println!("{}", notes_content);
            }
        }
    }

    // Show files changed
    println!("────────────────────────────────────────");
    println!("Files Changed:");
    println!("────────────────────────────────────────\n");

    let files_output = Command::new("git")
        .args(["show", "--stat", "--format=", &commit.full_hash])
        .output()
        .context("failed to get files changed")?;

    if files_output.status.success() {
        let files = String::from_utf8_lossy(&files_output.stdout);
        println!("{}", files);
    }

    Ok(())
}

/// Truncate a string to a maximum length, adding "..." if truncated.
fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        // Find valid UTF-8 char boundary
        let target = max_len.saturating_sub(3);
        let mut end = target.min(s.len());
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &s[..end])
    }
}

#[derive(Debug, Clone)]
struct TopEntry {
    hash: String,
    label: Option<String>,
}

fn toggle_top_commit(commit: &CommitEntry) -> Result<()> {
    let mut entries = load_top_entries()?;
    if entries.iter().any(|entry| entry.hash == commit.full_hash) {
        entries.retain(|entry| entry.hash != commit.full_hash);
        write_top_entries(&entries)?;
        println!("Removed notable commit: {} {}", commit.hash, commit.subject);
    } else {
        let label = commit.subject.replace('\t', " ");
        entries.push(TopEntry {
            hash: commit.full_hash.clone(),
            label: Some(label),
        });
        write_top_entries(&entries)?;
        println!("Marked notable: {} {}", commit.hash, commit.subject);
    }
    Ok(())
}

fn load_commit_by_ref(commit_ref: &str) -> Result<Option<CommitEntry>> {
    let output = Command::new("git")
        .args(["log", "-1", "--pretty=format:%h|%H|%s|%ar|%an", commit_ref])
        .output()
        .context("failed to run git log")?;

    if !output.status.success() {
        return Ok(None);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().next().unwrap_or("");
    let parts: Vec<&str> = line.splitn(5, '|').collect();
    if parts.len() < 5 {
        return Ok(None);
    }

    let hash = parts[0].to_string();
    let full_hash = parts[1].to_string();
    let subject = parts[2].to_string();
    let relative_time = parts[3].to_string();
    let author = parts[4].to_string();
    let has_ai_metadata = check_ai_metadata(&full_hash);

    let ai_indicator = if has_ai_metadata { "◆ " } else { "  " };
    let pretty = format!(
        "TOP {}{} | {} | {} | {}",
        ai_indicator,
        hash,
        truncate_str(&subject, 50),
        relative_time,
        author
    );
    let display = format!("{}\t{}", hash, pretty);

    Ok(Some(CommitEntry {
        hash,
        full_hash,
        subject,
        relative_time,
        author,
        has_ai_metadata,
        is_top: true,
        display,
    }))
}

fn resolve_full_hash(commit_ref: &str) -> Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", commit_ref])
        .output()
        .context("failed to run git rev-parse")?;
    if !output.status.success() {
        bail!("commit not found: {}", commit_ref);
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn load_top_entries() -> Result<Vec<TopEntry>> {
    let path = top_file_path()?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(&path).context("failed to read top commits")?;
    let mut entries = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (hash, label) = match trimmed.split_once('\t') {
            Some((hash, label)) => (hash.to_string(), Some(label.to_string())),
            None => (trimmed.to_string(), None),
        };
        entries.push(TopEntry { hash, label });
    }
    Ok(entries)
}

fn write_top_entries(entries: &[TopEntry]) -> Result<()> {
    let path = top_file_path()?;
    if entries.is_empty() {
        if path.exists() {
            fs::remove_file(&path).context("failed to remove top commits file")?;
        }
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("failed to create top commits dir")?;
    }
    let mut out = String::new();
    for entry in entries {
        if let Some(label) = &entry.label {
            out.push_str(&format!("{}\t{}\n", entry.hash, label));
        } else {
            out.push_str(&format!("{}\n", entry.hash));
        }
    }
    fs::write(&path, out).context("failed to write top commits")?;
    Ok(())
}

fn top_hashes(entries: &[TopEntry]) -> HashSet<String> {
    entries.iter().map(|entry| entry.hash.clone()).collect()
}

fn top_file_path() -> Result<PathBuf> {
    let root = repo_root()?;
    Ok(root.join(TOP_COMMITS_PATH))
}

fn repo_root() -> Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("failed to run git rev-parse")?;
    if !output.status.success() {
        bail!("not inside a git repository");
    }
    Ok(Path::new(String::from_utf8_lossy(&output.stdout).trim()).to_path_buf())
}
