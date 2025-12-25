//! Browse and analyze git commits with AI session metadata.
//!
//! Shows commits with attached AI sessions, reviews, and other metadata.

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::cli::CommitsOpts;

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
    /// Display string for fzf
    display: String,
}

/// Run the commits subcommand.
pub fn run(opts: CommitsOpts) -> Result<()> {
    let commits = list_commits(opts.limit, opts.all)?;

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
    if let Some(selected) = run_commits_fzf(&commits)? {
        show_commit_details(&selected)?;
    }

    Ok(())
}

/// List recent commits with metadata.
fn list_commits(limit: usize, all_branches: bool) -> Result<Vec<CommitEntry>> {
    let mut args = vec![
        "log",
        "--pretty=format:%h|%H|%s|%ar|%an",
        "-n",
    ];
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

        // Build display string
        let ai_indicator = if has_ai_metadata { "◆ " } else { "  " };
        let display = format!(
            "{}{} | {} | {} | {}",
            ai_indicator,
            hash,
            truncate_str(&subject, 50),
            relative_time,
            author
        );

        commits.push(CommitEntry {
            hash,
            full_hash,
            subject,
            relative_time,
            author,
            has_ai_metadata,
            display,
        });
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

/// Run fzf with preview for commits.
fn run_commits_fzf(commits: &[CommitEntry]) -> Result<Option<&CommitEntry>> {
    let mut child = Command::new("fzf")
        .arg("--prompt")
        .arg("commits> ")
        .arg("--ansi")
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

    let selection = String::from_utf8(output.stdout)
        .context("fzf output was not valid UTF-8")?;
    let selection = selection.trim();

    if selection.is_empty() {
        return Ok(None);
    }

    Ok(commits.iter().find(|c| c.display == selection))
}

/// Show detailed commit information including AI metadata.
fn show_commit_details(commit: &CommitEntry) -> Result<()> {
    println!("\n────────────────────────────────────────");
    println!("Commit: {} ({})", commit.hash, commit.relative_time);
    println!("Author: {}", commit.author);
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
