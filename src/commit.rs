//! AI-powered git commit command using OpenAI.

use std::io::{self, Write};
use std::net::IpAddr;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tempfile::NamedTempFile;
use tracing::{debug, info};

use crate::ai;
use crate::config;
use crate::hub;
use crate::notify;

const MODEL: &str = "gpt-4.1-nano";
const MAX_DIFF_CHARS: usize = 12_000;
const MAX_REVIEW_CONTEXT_CHARS: usize = 50_000;
const HUB_HOST: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1));
const HUB_PORT: u16 = 9050;

const SYSTEM_PROMPT: &str = "You are an expert software engineer who writes clear, concise git commit messages. Use imperative mood, keep the subject line under 72 characters, and include an optional body with bullet points if helpful. Never wrap the message in quotes. Never include secrets, credentials, or file contents from .env files, environment variables, keys, or other sensitive data—even if they appear in the diff.";

#[derive(Debug, Deserialize)]
struct ReviewJson {
    issues_found: bool,
    #[serde(default)]
    issues: Vec<String>,
    #[serde(default)]
    summary: Option<String>,
}

#[derive(Debug)]
struct ReviewResult {
    issues_found: bool,
    issues: Vec<String>,
    summary: Option<String>,
    timed_out: bool,
}

#[derive(Debug)]
struct StagedSnapshot {
    patch_path: Option<std::path::PathBuf>,
}

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<Message>,
    temperature: f32,
}

#[derive(Debug, Serialize)]
struct Message {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: Option<ResponseMessage>,
}

#[derive(Debug, Deserialize)]
struct ResponseMessage {
    content: String,
}

/// Dry run: show the context that would be passed to Codex without committing.
pub fn dry_run_context() -> Result<()> {
    println!("Dry run: showing context that would be passed to Codex\n");

    // Ensure we're in a git repo
    ensure_git_repo()?;

    // Show checkpoint info
    let cwd = std::env::current_dir()?;
    let checkpoints = ai::load_checkpoints(&cwd).unwrap_or_default();
    println!("────────────────────────────────────────");
    println!("COMMIT CHECKPOINT");
    println!("────────────────────────────────────────");
    if let Some(ref checkpoint) = checkpoints.last_commit {
        println!("Last commit: {}", checkpoint.timestamp);
        if let Some(ref ts) = checkpoint.last_entry_timestamp {
            println!("Last entry included: {}", ts);
        }
        if let Some(ref sid) = checkpoint.session_id {
            println!("Session: {}...", &sid[..8.min(sid.len())]);
        }
    } else {
        println!("No previous checkpoint (first commit with context)");
    }

    // Get diff
    let diff = git_capture(&["diff", "--cached"])
        .or_else(|_| git_capture(&["diff"]))?;

    if diff.trim().is_empty() {
        println!("\nNo changes to show (no staged or unstaged diff)");
        println!("\nTrying to show what would be staged with 'git add .'...");
        git_run(&["add", "--dry-run", "."])?;
    }

    // Get AI session context since checkpoint
    println!("\n────────────────────────────────────────");
    println!("AI SESSION CONTEXT (since checkpoint)");
    println!("────────────────────────────────────────");

    match ai::get_context_since_checkpoint() {
        Ok(Some(context)) => {
            println!("Context length: {} chars, {} lines\n", context.len(), context.lines().count());
            println!("{}", context);
        }
        Ok(None) => {
            println!("No new AI session context since last checkpoint.");
            println!("\nThis could mean:");
            println!("  - No exchanges since last commit");
            println!("  - No Claude Code or Codex session in this project");
        }
        Err(e) => {
            println!("Error getting context: {}", e);
        }
    }

    println!("────────────────────────────────────────");
    println!("\nDiff that would be reviewed:");
    println!("────────────────────────────────────────");

    let (diff_for_prompt, truncated) = truncate_diff(&diff);
    println!("{}", diff_for_prompt);

    if truncated {
        println!("\n[Diff truncated to {} chars]", MAX_DIFF_CHARS);
    }

    println!("────────────────────────────────────────");

    Ok(())
}

/// Run the commit workflow: stage, generate message, commit, push.
/// If hub is running, delegates to it for async execution.
pub fn run(push: bool) -> Result<()> {
    // Check if hub is running - if so, delegate
    if hub::hub_healthy(HUB_HOST, HUB_PORT) {
        return delegate_to_hub(push);
    }

    run_sync(push)
}

/// Run commit synchronously (called directly or by hub).
pub fn run_sync(push: bool) -> Result<()> {
    info!(push = push, "starting commit workflow");

    // Ensure we're in a git repo
    ensure_git_repo()?;
    debug!("verified git repository");

    // Get API key
    let api_key = get_openai_key()?;
    debug!("got OpenAI API key");

    // Stage all changes
    print!("Staging changes... ");
    io::stdout().flush()?;
    git_run(&["add", "."])?;
    println!("done");
    debug!("staged all changes");

    // Get diff
    let diff = git_capture(&["diff", "--cached"])?;
    if diff.trim().is_empty() {
        bail!("No staged changes to commit");
    }
    debug!(diff_len = diff.len(), "got cached diff");

    // Get status
    let status = git_capture(&["status", "--short"]).unwrap_or_default();
    debug!(status_lines = status.lines().count(), "got git status");

    // Truncate diff if needed
    let (diff_for_prompt, truncated) = truncate_diff(&diff);
    debug!(truncated = truncated, prompt_len = diff_for_prompt.len(), "prepared diff for prompt");

    // Generate commit message
    print!("Generating commit message... ");
    io::stdout().flush()?;
    info!(model = MODEL, "calling OpenAI API");
    let message = generate_commit_message(&api_key, &diff_for_prompt, &status, truncated)?;
    println!("done\n");
    debug!(message_len = message.len(), "got commit message");

    // Show the message
    println!("Commit message:");
    println!("────────────────────────────────────────");
    println!("{}", message);
    println!("────────────────────────────────────────\n");

    // Commit
    let paragraphs = split_paragraphs(&message);
    debug!(paragraphs = paragraphs.len(), "split message into paragraphs");
    let mut args = vec!["commit"];
    for p in &paragraphs {
        args.push("-m");
        args.push(p);
    }
    git_run(&args)?;
    println!("✓ Committed");
    info!("created commit");

    // Push if requested
    if push {
        print!("Pushing... ");
        io::stdout().flush()?;

        match git_try(&["push"]) {
            Ok(_) => {
                println!("done");
                info!("pushed to remote");
            }
            Err(_) => {
                // Push failed, likely remote has new commits
                println!("failed (remote ahead)");
                print!("Pulling with rebase... ");
                io::stdout().flush()?;

                match git_try(&["pull", "--rebase"]) {
                    Ok(_) => {
                        println!("done");
                        print!("Pushing... ");
                        io::stdout().flush()?;
                        git_run(&["push"])?;
                        println!("done");
                        info!("pulled and pushed to remote");
                    }
                    Err(_) => {
                        println!("conflict!");
                        println!();
                        println!("Rebase conflict detected. Resolve manually:");
                        println!("  1. Fix conflicts in the listed files");
                        println!("  2. git add <files>");
                        println!("  3. git rebase --continue");
                        println!("  4. git push");
                        println!();
                        println!("Or abort with: git rebase --abort");
                        bail!("Rebase conflict - manual resolution required");
                    }
                }
            }
        }
    }

    Ok(())
}

/// Run commit with Codex code review: stage, review with Codex, generate message, commit, push.
/// If hub is running, delegates to it for async execution.
pub fn run_with_check(push: bool, include_context: bool) -> Result<()> {
    run_with_check_sync(push, include_context)
}

fn commit_with_check_timeout_secs() -> u64 {
    let cwd = std::env::current_dir().ok();

    if let Some(cwd) = cwd {
        let local_config = cwd.join("flow.toml");
        if local_config.exists() {
            if let Ok(cfg) = config::load(&local_config) {
                return cfg.options.commit_with_check_timeout_secs.unwrap_or(120);
            }
            return 120;
        }
    }

    let global_config = config::default_config_path();
    if global_config.exists() {
        if let Ok(cfg) = config::load(&global_config) {
            return cfg.options.commit_with_check_timeout_secs.unwrap_or(120);
        }
    }

    120
}

/// Run commit with Codex code review synchronously (called directly or by hub).
/// If `include_context` is true, AI session context is passed to Codex for better understanding.
pub fn run_with_check_sync(push: bool, include_context: bool) -> Result<()> {
    info!(push = push, include_context = include_context, "starting commit with check workflow");

    // Ensure we're in a git repo
    ensure_git_repo()?;

    // Capture current staged changes so we can restore if we cancel.
    let staged_snapshot = capture_staged_snapshot()?;

    // Stage all changes
    print!("Staging changes... ");
    io::stdout().flush()?;
    git_run(&["add", "."])?;
    println!("done");

    // Get diff
    let diff = git_capture(&["diff", "--cached"])?;
    if diff.trim().is_empty() {
        println!("\nnotify: No staged changes to commit");
        bail!("No staged changes to commit");
    }

    // Get AI session context since last checkpoint (if enabled)
    let session_context = if include_context {
        ai::get_context_since_checkpoint()
            .ok()
            .flatten()
            .map(|context| truncate_context(&context, MAX_REVIEW_CONTEXT_CHARS))
    } else {
        None
    };
    if let Some(context) = session_context.as_ref() {
        let line_count = context.lines().count();
        println!(
            "Using AI session context ({} chars, {} lines) since last checkpoint",
            context.len(),
            line_count
        );
        if should_show_review_context() {
            println!("--- AI session context ---");
            println!("{}", context);
            println!("--- End AI session context ---");
        }
    }

    // Run Codex review
    println!("\nRunning Codex code review...");
    if session_context.is_some() {
        println!("(with AI session context)");
    }
    println!("────────────────────────────────────────");

    let review = match run_codex_review(&diff, session_context.as_deref()) {
        Ok(review) => review,
        Err(err) => {
            restore_staged_snapshot(&staged_snapshot)?;
            return Err(err);
        }
    };

    println!("────────────────────────────────────────\n");

    if review.timed_out {
        println!(
            "⚠ Codex review timed out after {}s; proceeding without review",
            commit_with_check_timeout_secs()
        );
        println!();
    }

    // Check if review indicates issues
    let has_issues = review.issues_found;

    if has_issues {
        if let Some(summary) = review.summary.as_ref() {
            if !summary.trim().is_empty() {
                println!("Summary: {}", summary.trim());
                println!();
            }
        }
        if !review.issues.is_empty() {
            println!("Issues found:");
            for issue in &review.issues {
                println!("- {}", issue);
            }
            println!();
        }

        // Attempt auto-fix via codex
        println!("Attempting auto-fix...");
        let fix_prompt = format!(
            "Fix these issues in the staged changes:\n{}\n\nApply minimal fixes, don't refactor.",
            review.issues.join("\n")
        );

        match run_codex_fix(&fix_prompt) {
            Ok(true) => {
                println!("✓ Auto-fix applied");
                // Re-stage any fixed files
                let _ = git_run(&["add", "-u"]);
            }
            Ok(false) => {
                println!("⚠ Auto-fix had no changes");
            }
            Err(e) => {
                println!("⚠ Auto-fix failed: {}", e);
                // Send warning alert to Lin
                let alert_msg = format!("⚠️ Auto-fix failed: {} - committing anyway", e);
                if let Err(e) = notify::send_warning(&alert_msg) {
                    debug!("Failed to send alert to Lin: {}", e);
                }
            }
        }

        // Always continue with commit - never block
        println!("Proceeding with commit...");
    } else if !review.timed_out {
        println!("✓ Codex review passed");
    }

    // Continue with normal commit flow
    let api_key = get_openai_key()?;

    // Get status
    let status = git_capture(&["status", "--short"]).unwrap_or_default();

    // Truncate diff if needed
    let (diff_for_prompt, truncated) = truncate_diff(&diff);

    // Generate commit message
    print!("Generating commit message... ");
    io::stdout().flush()?;
    let message = generate_commit_message(&api_key, &diff_for_prompt, &status, truncated)?;
    println!("done\n");

    // Show the message
    println!("Commit message:");
    println!("────────────────────────────────────────");
    println!("{}", message);
    println!("────────────────────────────────────────\n");

    // Commit
    let paragraphs = split_paragraphs(&message);
    let mut args = vec!["commit"];
    for p in &paragraphs {
        args.push("-m");
        args.push(p);
    }
    git_run(&args)?;
    println!("✓ Committed");

    // Push if requested
    if push {
        print!("Pushing... ");
        io::stdout().flush()?;

        match git_try(&["push"]) {
            Ok(_) => {
                println!("done");
            }
            Err(_) => {
                println!("failed (remote ahead)");
                print!("Pulling with rebase... ");
                io::stdout().flush()?;

                match git_try(&["pull", "--rebase"]) {
                    Ok(_) => {
                        println!("done");
                        print!("Pushing... ");
                        io::stdout().flush()?;
                        git_run(&["push"])?;
                        println!("done");
                    }
                    Err(_) => {
                        println!("conflict!");
                        println!();
                        println!("Rebase conflict detected. Resolve manually:");
                        println!("  1. Fix conflicts in the listed files");
                        println!("  2. git add <files>");
                        println!("  3. git rebase --continue");
                        println!("  4. git push");
                        println!();
                        println!("Or abort with: git rebase --abort");
                        println!("\nnotify: Rebase conflict - manual resolution required");
                        bail!("Rebase conflict - manual resolution required");
                    }
                }
            }
        }
    }

    cleanup_staged_snapshot(&staged_snapshot);

    // Save checkpoint for next commit
    if include_context {
        let cwd = std::env::current_dir()?;
        let now = chrono::Utc::now().to_rfc3339();
        let (session_id, last_ts) = match ai::get_last_entry_timestamp() {
            Ok(Some((session_id, last_ts))) => (Some(session_id), Some(last_ts)),
            Ok(None) => (None, Some(now.clone())),
            Err(_) => (None, Some(now.clone())),
        };
        let checkpoint = ai::CommitCheckpoint {
            timestamp: now,
            session_id,
            last_entry_timestamp: last_ts,
        };
        if let Err(e) = ai::save_checkpoint(&cwd, checkpoint) {
            debug!("failed to save commit checkpoint: {}", e);
        } else {
            debug!("saved commit checkpoint");
        }
    }

    Ok(())
}

/// Run Codex to review staged changes for bugs and performance issues.
fn run_codex_review(diff: &str, session_context: Option<&str>) -> Result<ReviewResult> {
    use std::io::{BufRead, BufReader};
    use std::sync::mpsc;
    use std::time::Instant;

    let (diff_for_prompt, _truncated) = truncate_diff(diff);

    // Build the review prompt with optional session context
    let prompt = if let Some(context) = session_context {
        format!(
            "Deep review the following git diff. Be thorough and check for:\n\
1. Bugs and logic errors\n\
2. Security vulnerabilities\n\
3. Performance issues\n\
4. Code quality\n\
5. Documentation: Are docs up to date? Do new public APIs have docstrings? Does README need updates?\n\n\
Return ONLY a JSON object with fields: issues_found (bool), issues (array of short strings), summary (string).\n\
Never include secrets, credentials, personal data, or other sensitive data in the response.\n\n\
AI session context (intent behind changes):\n{}\n\n\
Diff:\n```diff\n{}\n```",
            context,
            diff_for_prompt
        )
    } else {
        format!(
            "Deep review the following git diff. Be thorough and check for:\n\
1. Bugs and logic errors\n\
2. Security vulnerabilities\n\
3. Performance issues\n\
4. Code quality\n\
5. Documentation: Are docs up to date? Do new public APIs have docstrings? Does README need updates?\n\n\
Return ONLY a JSON object with fields: issues_found (bool), issues (array of short strings), summary (string).\n\
Never include secrets, credentials, personal data, or other sensitive data in the response.\n\n\
Diff:\n```diff\n{}\n```",
            diff_for_prompt
        )
    };

    // Use codex review with explicit model selection via stdin to avoid argv limits.
    let mut child = Command::new("codex")
        .args([
            "review",
            "-c",
            "model=\"gpt-5.1-codex-max\"",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to run codex - is it installed?")?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(prompt.as_bytes())
            .context("failed to write codex review prompt")?;
    }

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = mpsc::channel();
    let start = Instant::now();

    let reader_handle = std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().flatten() {
            let _ = tx.send(ReviewEvent::Line(line));
        }
        let _ = tx.send(ReviewEvent::Done);
    });

    let mut output_lines = Vec::new();
    let mut last_progress = Instant::now();
    let timeout = Duration::from_secs(commit_with_check_timeout_secs());
    let mut timed_out = false;
    loop {
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(ReviewEvent::Line(line)) => {
                println!("{}", line);
                output_lines.push(line);
                last_progress = Instant::now();
            }
            Ok(ReviewEvent::Done) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if last_progress.elapsed() >= Duration::from_secs(10) {
                    println!(
                        "Waiting on Codex review... ({}s elapsed)",
                        start.elapsed().as_secs()
                    );
                    last_progress = Instant::now();
                }
                if start.elapsed() >= timeout {
                    timed_out = true;
                    let _ = child.kill();
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    let _ = reader_handle.join();
    let status = child.wait()?;
    let stderr_output = read_stderr(stderr);

    if timed_out {
        if !stderr_output.trim().is_empty() {
            println!("{}", stderr_output.trim_end());
        }
        return Ok(ReviewResult {
            issues_found: false,
            issues: Vec::new(),
            summary: Some(format!(
                "Codex review timed out after {}s",
                timeout.as_secs()
            )),
            timed_out: true,
        });
    }

    if !status.success() {
        if !stderr_output.trim().is_empty() {
            println!("{}", stderr_output.trim_end());
        }
        println!("\nnotify: Codex review failed");
        bail!("Codex review failed");
    }

    let result = output_lines.join("\n");

    let review_json = parse_review_json(&result);
    let (issues_found, issues) = if let Some(ref parsed) = review_json {
        if let Some(summary) = parsed.summary.as_ref() {
            debug!(summary = summary.as_str(), "codex review summary");
        }
        (parsed.issues_found, parsed.issues.clone())
    } else if result.trim().is_empty() {
        (false, Vec::new())
    } else {
        debug!(review_output = result.as_str(), "codex review output");
        let lowered = result.to_lowercase();
        let has_issues = lowered.contains("bug")
            || lowered.contains("issue")
            || lowered.contains("problem")
            || lowered.contains("error")
            || lowered.contains("vulnerability")
            || lowered.contains("performance issue")
            || lowered.contains("memory leak");
        (has_issues, Vec::new())
    };

    Ok(ReviewResult {
        issues_found,
        issues,
        summary: review_json.and_then(|r| r.summary),
        timed_out: false,
    })
}

/// Run Codex to auto-fix issues in staged changes.
/// Returns true if fixes were applied.
fn run_codex_fix(prompt: &str) -> Result<bool> {
    // Use codex exec to apply fixes
    let mut child = Command::new("codex")
        .args([
            "exec",
            "--model",
            "gpt-5.1-codex-max",
            "-a",  // auto-edit mode
            prompt,
        ])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("failed to run codex - is it installed?")?;

    let status = child.wait()?;

    if !status.success() {
        bail!("codex exited with status {}", status);
    }

    // Check if any files changed
    let diff_output = Command::new("git")
        .args(["diff", "--name-only"])
        .output()
        .context("failed to check for changes")?;

    let has_changes = !diff_output.stdout.is_empty();
    Ok(has_changes)
}

fn ensure_git_repo() -> Result<()> {
    let output = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("failed to run git")?;

    if !output.success() {
        bail!("Not a git repository");
    }
    Ok(())
}

fn get_openai_key() -> Result<String> {
    std::env::var("OPENAI_API_KEY")
        .context("OPENAI_API_KEY environment variable not set")
}

fn git_run(args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .args(args)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;

    if !status.success() {
        bail!("git {} failed with status {}", args.join(" "), status);
    }
    Ok(())
}

/// Try to run a git command, returning Ok/Err without bailing.
fn git_try(args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;

    if !status.success() {
        bail!("git {} failed", args.join(" "));
    }
    Ok(())
}

fn git_capture(args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;

    if !output.status.success() {
        bail!("git {} failed", args.join(" "));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn truncate_diff(diff: &str) -> (String, bool) {
    if diff.len() <= MAX_DIFF_CHARS {
        (diff.to_string(), false)
    } else {
        let truncated = format!(
            "{}\n\n[Diff truncated to first {} characters]",
            &diff[..MAX_DIFF_CHARS],
            MAX_DIFF_CHARS
        );
        (truncated, true)
    }
}

fn truncate_context(context: &str, max_chars: usize) -> String {
    if context.len() <= max_chars {
        context.to_string()
    } else {
        format!(
            "{}\n\n[Context truncated to first {} characters]",
            &context[..max_chars],
            max_chars
        )
    }
}

fn generate_commit_message(
    api_key: &str,
    diff: &str,
    status: &str,
    truncated: bool,
) -> Result<String> {
    let mut user_prompt = String::from("Write a git commit message for the staged changes.\n\nGit diff:\n");
    user_prompt.push_str(diff);

    if truncated {
        user_prompt.push_str("\n\n[Diff truncated to fit within prompt]");
    }

    let status = status.trim();
    if !status.is_empty() {
        user_prompt.push_str("\n\nGit status --short:\n");
        user_prompt.push_str(status);
    }

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("failed to create HTTP client")?;

    let body = ChatRequest {
        model: MODEL.to_string(),
        messages: vec![
            Message {
                role: "system".to_string(),
                content: SYSTEM_PROMPT.to_string(),
            },
            Message {
                role: "user".to_string(),
                content: user_prompt,
            },
        ],
        temperature: 0.3,
    };

    let resp = client
        .post("https://api.openai.com/v1/chat/completions")
        .header("Authorization", format!("Bearer {}", api_key))
        .json(&body)
        .send()
        .context("failed to call OpenAI API")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        bail!("OpenAI API error {}: {}", status, text);
    }

    let parsed: ChatResponse = resp.json().context("failed to parse OpenAI response")?;

    let message = parsed
        .choices
        .first()
        .and_then(|c| c.message.as_ref())
        .map(|m| m.content.trim().to_string())
        .unwrap_or_default();

    if message.is_empty() {
        bail!("OpenAI returned empty commit message");
    }

    // Trim quotes if wrapped
    Ok(trim_quotes(&message))
}

fn trim_quotes(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 {
        let first = s.chars().next().unwrap();
        let last = s.chars().last().unwrap();
        if (first == '"' && last == '"') || (first == '\'' && last == '\'') {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

fn capture_staged_snapshot() -> Result<StagedSnapshot> {
    let staged_diff = git_capture(&["diff", "--cached"])?;
    if staged_diff.trim().is_empty() {
        return Ok(StagedSnapshot { patch_path: None });
    }

    let mut file = NamedTempFile::new().context("failed to create temp file for staged diff")?;
    file.write_all(staged_diff.as_bytes())
        .context("failed to write staged diff snapshot")?;
    let path = file
        .into_temp_path()
        .keep()
        .context("failed to persist staged diff snapshot")?;

    Ok(StagedSnapshot {
        patch_path: Some(path),
    })
}

fn restore_staged_snapshot(snapshot: &StagedSnapshot) -> Result<()> {
    let _ = git_try(&["reset", "HEAD"]);
    if let Some(path) = &snapshot.patch_path {
        let path_str = path
            .to_str()
            .context("failed to convert staged snapshot path to string")?;
        let _ = git_try(&["apply", "--cached", path_str]);
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

fn cleanup_staged_snapshot(snapshot: &StagedSnapshot) {
    if let Some(path) = &snapshot.patch_path {
        let _ = std::fs::remove_file(path);
    }
}

fn parse_review_json(output: &str) -> Option<ReviewJson> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(parsed) = serde_json::from_str::<ReviewJson>(trimmed) {
        return Some(parsed);
    }

    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    if end <= start {
        return None;
    }
    let candidate = &trimmed[start..=end];
    serde_json::from_str::<ReviewJson>(candidate).ok()
}

fn read_stderr(stderr: std::process::ChildStderr) -> String {
    use std::io::Read;
    let mut buf = String::new();
    let _ = std::io::BufReader::new(stderr).read_to_string(&mut buf);
    buf
}

enum ReviewEvent {
    Line(String),
    Done,
}

fn should_show_review_context() -> bool {
    std::env::var("FLOW_SHOW_REVIEW_CONTEXT")
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

fn split_paragraphs(message: &str) -> Vec<String> {
    let mut paragraphs = Vec::new();
    let mut current = Vec::new();

    for line in message.lines() {
        if line.trim().is_empty() {
            if !current.is_empty() {
                paragraphs.push(current.join("\n"));
                current.clear();
            }
        } else {
            current.push(line.trim_end());
        }
    }

    if !current.is_empty() {
        paragraphs.push(current.join("\n"));
    }

    paragraphs
}

fn delegate_to_hub(push: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;

    // Build the command to run using the current executable path
    let push_flag = if push { "" } else { " --no-push" };
    let flow_bin = std::env::current_exe()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "flow".to_string());
    let command = format!("{} commit --sync{}", flow_bin, push_flag);

    let url = format!("http://{}:{}/tasks/run", HUB_HOST, HUB_PORT);
    let client = Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("failed to create HTTP client")?;

    let payload = json!({
        "task": {
            "name": "commit",
            "command": command,
            "dependencies": {
                "commands": [],
                "flox": [],
            },
        },
        "cwd": cwd.to_string_lossy(),
        "flow_version": env!("CARGO_PKG_VERSION"),
    });

    let resp = client
        .post(&url)
        .json(&payload)
        .send()
        .context("failed to submit commit to hub")?;

    if resp.status().is_success() {
        // Parse response to get task_id
        let body: serde_json::Value = resp.json().unwrap_or_default();
        if let Some(task_id) = body.get("task_id").and_then(|v| v.as_str()) {
            println!("Delegated commit to hub");
            println!("  View logs: f logs --task-id {}", task_id);
            println!("  Stream logs: f logs --task-id {} --follow", task_id);
        } else {
            println!("Delegated commit to hub");
        }
        Ok(())
    } else {
        let body = resp.text().unwrap_or_default();
        bail!("hub returned error: {}", body);
    }
}

fn delegate_to_hub_with_check(push: bool, include_context: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;

    // Build the command to run using the current executable path
    let push_flag = if push { "" } else { " --no-push" };
    let context_flag = if include_context { "" } else { " --no-context" };
    let flow_bin = std::env::current_exe()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "flow".to_string());
    let command = format!(
        "{} commitWithCheck --sync{}{}",
        flow_bin, push_flag, context_flag
    );

    let url = format!("http://{}:{}/tasks/run", HUB_HOST, HUB_PORT);
    let client = Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("failed to create HTTP client")?;

    let payload = json!({
        "task": {
            "name": "commitWithCheck",
            "command": command,
            "dependencies": {
                "commands": [],
                "flox": [],
            },
        },
        "cwd": cwd.to_string_lossy(),
        "flow_version": env!("CARGO_PKG_VERSION"),
    });

    let resp = client
        .post(&url)
        .json(&payload)
        .send()
        .context("failed to submit commitWithCheck to hub")?;

    if resp.status().is_success() {
        // Parse response to get task_id
        let body: serde_json::Value = resp.json().unwrap_or_default();
        if let Some(task_id) = body.get("task_id").and_then(|v| v.as_str()) {
            println!("Delegated commitWithCheck to hub");
            println!("  View logs: f logs --task-id {}", task_id);
            println!("  Stream logs: f logs --task-id {} --follow", task_id);
        } else {
            println!("Delegated commitWithCheck to hub");
        }
        Ok(())
    } else {
        let body = resp.text().unwrap_or_default();
        bail!("hub returned error: {}", body);
    }
}
