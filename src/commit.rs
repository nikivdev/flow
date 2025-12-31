//! AI-powered git commit command using OpenAI.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{self, Write};
use std::net::IpAddr;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
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
const HUB_HOST: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1));
const HUB_PORT: u16 = 9050;

const SYSTEM_PROMPT: &str = "You are an expert software engineer who writes clear, concise git commit messages. Use imperative mood, keep the subject line under 72 characters, and include an optional body with bullet points if helpful. Never wrap the message in quotes. Never include secrets, credentials, or file contents from .env files, environment variables, keys, or other sensitive data—even if they appear in the diff.";

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum ReviewModelArg {
    /// Use Claude Opus 1 for review.
    ClaudeOpus,
    /// Use Codex high-capacity review (gpt-5.1-codex-max).
    CodexHigh,
    /// Use Codex mini review model (gpt-5.1-codex-mini).
    CodexMini,
}

impl ReviewModelArg {
    fn as_arg(&self) -> &'static str {
        match self {
            ReviewModelArg::ClaudeOpus => "claude-opus",
            ReviewModelArg::CodexHigh => "codex-high",
            ReviewModelArg::CodexMini => "codex-mini",
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub enum CodexModel {
    High,
    Mini,
}

impl CodexModel {
    fn as_codex_arg(&self) -> &'static str {
        match self {
            CodexModel::High => "gpt-5.1-codex-max",
            CodexModel::Mini => "gpt-5.1-codex-mini",
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub enum ClaudeModel {
    Sonnet,
    Opus,
}

impl ClaudeModel {
    fn as_claude_arg(&self) -> &'static str {
        match self {
            ClaudeModel::Sonnet => "claude-sonnet-4-20250514",
            ClaudeModel::Opus => "claude-opus-1",
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub enum ReviewSelection {
    Codex(CodexModel),
    Claude(ClaudeModel),
}

impl ReviewSelection {
    fn is_claude(&self) -> bool {
        matches!(self, ReviewSelection::Claude(_))
    }

    fn review_model_arg(&self) -> Option<ReviewModelArg> {
        match self {
            ReviewSelection::Codex(CodexModel::High) => Some(ReviewModelArg::CodexHigh),
            ReviewSelection::Codex(CodexModel::Mini) => Some(ReviewModelArg::CodexMini),
            ReviewSelection::Claude(ClaudeModel::Opus) => Some(ReviewModelArg::ClaudeOpus),
            ReviewSelection::Claude(ClaudeModel::Sonnet) => None,
        }
    }

    fn model_label(&self) -> &'static str {
        match self {
            ReviewSelection::Codex(model) => model.as_codex_arg(),
            ReviewSelection::Claude(model) => model.as_claude_arg(),
        }
    }
}

pub fn resolve_review_selection(
    use_claude: bool,
    override_model: Option<ReviewModelArg>,
) -> ReviewSelection {
    if let Some(model) = override_model {
        return match model {
            ReviewModelArg::ClaudeOpus => ReviewSelection::Claude(ClaudeModel::Opus),
            ReviewModelArg::CodexHigh => ReviewSelection::Codex(CodexModel::High),
            ReviewModelArg::CodexMini => ReviewSelection::Codex(CodexModel::Mini),
        };
    }

    if use_claude {
        ReviewSelection::Claude(ClaudeModel::Sonnet)
    } else {
        ReviewSelection::Codex(CodexModel::High)
    }
}

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
    let diff = git_capture(&["diff", "--cached"]).or_else(|_| git_capture(&["diff"]))?;

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
            println!(
                "Context length: {} chars, {} lines\n",
                context.len(),
                context.lines().count()
            );
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
    debug!(
        truncated = truncated,
        prompt_len = diff_for_prompt.len(),
        "prepared diff for prompt"
    );

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
    debug!(
        paragraphs = paragraphs.len(),
        "split message into paragraphs"
    );
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

    // Sync to gitedit if enabled
    let cwd = std::env::current_dir().unwrap_or_default();
    if gitedit_mirror_enabled() {
        sync_to_gitedit(&cwd, "commit", &[], None);
    }

    Ok(())
}

/// Run commit with code review: stage, review with Codex or Claude, generate message, commit, push.
/// If hub is running, delegates to it for async execution.
pub fn run_with_check(
    push: bool,
    include_context: bool,
    review_selection: ReviewSelection,
    author_message: Option<&str>,
    max_tokens: usize,
) -> Result<()> {
    if commit_with_check_async_enabled() && hub::hub_healthy(HUB_HOST, HUB_PORT) {
        return delegate_to_hub_with_check(
            "commitWithCheck",
            push,
            include_context,
            review_selection,
            author_message,
            max_tokens,
        );
    }

    run_with_check_sync(
        push,
        include_context,
        review_selection,
        author_message,
        max_tokens,
        false,
    )
}

/// Run commitWithCheck and always sync AI sessions to GitEdit (ignores config).
pub fn run_with_check_with_gitedit(
    push: bool,
    include_context: bool,
    review_selection: ReviewSelection,
    author_message: Option<&str>,
    max_tokens: usize,
) -> Result<()> {
    if commit_with_check_async_enabled() && hub::hub_healthy(HUB_HOST, HUB_PORT) {
        return delegate_to_hub_with_check(
            "commitWithCheckWithGitedit",
            push,
            include_context,
            review_selection,
            author_message,
            max_tokens,
        );
    }

    run_with_check_sync(
        push,
        include_context,
        review_selection,
        author_message,
        max_tokens,
        true,
    )
}

fn commit_with_check_async_enabled() -> bool {
    let cwd = std::env::current_dir().ok();

    if let Some(cwd) = cwd {
        let local_config = cwd.join("flow.toml");
        if local_config.exists() {
            if let Ok(cfg) = config::load(&local_config) {
                return cfg.options.commit_with_check_async.unwrap_or(true);
            }
            return true;
        }
    }

    let global_config = config::default_config_path();
    if global_config.exists() {
        if let Ok(cfg) = config::load(&global_config) {
            return cfg.options.commit_with_check_async.unwrap_or(true);
        }
    }

    true
}

fn commit_with_check_use_repo_root() -> bool {
    let cwd = std::env::current_dir().ok();

    if let Some(cwd) = cwd {
        let local_config = cwd.join("flow.toml");
        if local_config.exists() {
            if let Ok(cfg) = config::load(&local_config) {
                return cfg.options.commit_with_check_use_repo_root.unwrap_or(true);
            }
            return true;
        }
    }

    let global_config = config::default_config_path();
    if global_config.exists() {
        if let Ok(cfg) = config::load(&global_config) {
            return cfg.options.commit_with_check_use_repo_root.unwrap_or(true);
        }
    }

    true
}

fn resolve_commit_with_check_root() -> Result<std::path::PathBuf> {
    if !commit_with_check_use_repo_root() {
        return std::env::current_dir().context("failed to get current directory");
    }

    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("failed to run git rev-parse --show-toplevel")?;

    if !output.status.success() {
        bail!("failed to resolve git repo root");
    }

    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if root.is_empty() {
        bail!("git repo root was empty");
    }

    Ok(std::path::PathBuf::from(root))
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

fn prompt_yes_no(message: &str) -> Result<bool> {
    print!("{} [y/N]: ", message);
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

/// Run commit with code review synchronously (called directly or by hub).
/// If `include_context` is true, AI session context is passed for better understanding.
/// `review_selection` determines whether Claude or Codex runs and which model is used.
/// If `author_message` is provided, it's appended to the commit message.
pub fn run_with_check_sync(
    push: bool,
    include_context: bool,
    review_selection: ReviewSelection,
    author_message: Option<&str>,
    max_tokens: usize,
    force_gitedit: bool,
) -> Result<()> {
    // Convert tokens to chars (roughly 4 chars per token)
    let max_context = max_tokens * 4;
    info!(
        push = push,
        include_context = include_context,
        review_model = review_selection.model_label(),
        max_tokens = max_tokens,
        "starting commit with check workflow"
    );

    // Ensure we're in a git repo
    ensure_git_repo()?;

    let repo_root = resolve_commit_with_check_root()?;

    // Capture current staged changes so we can restore if we cancel.
    let staged_snapshot = capture_staged_snapshot_in(&repo_root)?;

    // Stage all changes
    print!("Staging changes... ");
    io::stdout().flush()?;
    git_run_in(&repo_root, &["add", "."])?;
    println!("done");

    // Get diff
    let diff = git_capture_in(&repo_root, &["diff", "--cached"])?;
    if diff.trim().is_empty() {
        println!("\nnotify: No staged changes to commit");
        bail!("No staged changes to commit");
    }

    // Get AI session context since last checkpoint (if enabled)
    let session_context = if include_context {
        ai::get_context_since_checkpoint_for_path(&repo_root)
            .ok()
            .flatten()
            .map(|context| truncate_context(&context, max_context))
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

    // Run code review (Codex or Claude)
    if review_selection.is_claude() {
        println!("\nRunning Claude code review...");
    } else {
        println!("\nRunning Codex code review...");
    }
    println!("Model: {}", review_selection.model_label());
    if session_context.is_some() {
        println!("(with AI session context)");
    }
    println!("────────────────────────────────────────");

    let review = match review_selection {
        ReviewSelection::Claude(model) => {
            run_claude_review(&diff, session_context.as_deref(), &repo_root, model)
        }
        ReviewSelection::Codex(model) => {
            run_codex_review(&diff, session_context.as_deref(), &repo_root, model)
        }
    };
    let review = match review {
        Ok(review) => review,
        Err(err) => {
            restore_staged_snapshot_in(&repo_root, &staged_snapshot)?;
            return Err(err);
        }
    };

    println!("────────────────────────────────────────\n");

    // Log review result for async tracking
    let context_chars = session_context.as_ref().map(|c| c.len()).unwrap_or(0);
    ai::log_review_result(
        &repo_root,
        review.issues_found,
        &review.issues,
        context_chars,
        0, // TODO: track actual review time
    );

    if review.timed_out {
        println!(
            "⚠ Review timed out after {}s, proceeding anyway",
            commit_with_check_timeout_secs()
        );
    }

    // Show review results (informational only, never blocks)
    if review.issues_found {
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

            // Send notification for critical issues (secrets, security)
            let critical_issues: Vec<_> = review
                .issues
                .iter()
                .filter(|i| {
                    let lower = i.to_lowercase();
                    lower.contains("secret")
                        || lower.contains(".env")
                        || lower.contains("credential")
                        || lower.contains("api key")
                        || lower.contains("password")
                        || lower.contains("token")
                        || lower.contains("security")
                        || lower.contains("vulnerability")
                })
                .collect();

            if !critical_issues.is_empty() {
                let alert_msg = format!(
                    "⚠️ Review found {} critical issue(s): {}",
                    critical_issues.len(),
                    critical_issues
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join("; ")
                );
                // Truncate if too long
                let alert_msg = if alert_msg.len() > 200 {
                    format!("{}...", &alert_msg[..200])
                } else {
                    alert_msg
                };
                let _ = notify::send_warning(&alert_msg);
                // Also try to POST to 1focus
                send_to_1focus(&repo_root, &review.issues, review.summary.as_deref());
            }
        }
        println!("Proceeding with commit...");
    } else if !review.timed_out {
        println!("✓ Review passed");
    }

    // Continue with normal commit flow
    let api_key = get_openai_key()?;

    // Get status
    let status = git_capture_in(&repo_root, &["status", "--short"]).unwrap_or_default();

    // Truncate diff if needed
    let (diff_for_prompt, truncated) = truncate_diff(&diff);

    // Generate commit message
    print!("Generating commit message... ");
    io::stdout().flush()?;
    let message = generate_commit_message(&api_key, &diff_for_prompt, &status, truncated)?;
    println!("done\n");

    let mut gitedit_sessions: Vec<ai::GitEditSessionData> = Vec::new();
    let mut gitedit_session_hash: Option<String> = None;

    let gitedit_enabled =
        force_gitedit || gitedit_mirror_enabled_for_commit_with_check(&repo_root);

    if gitedit_enabled {
        match ai::get_sessions_for_gitedit(&repo_root) {
            Ok(sessions) => {
                if !sessions.is_empty() {
                    gitedit_session_hash = gitedit_sessions_hash(&sessions);
                    gitedit_sessions = sessions;
                }
            }
            Err(err) => {
                debug!("failed to collect AI sessions for gitedit: {}", err);
            }
        }
    }

    // Append author note if provided
    let mut full_message = if let Some(note) = author_message {
        format!("{}\n\nauthor: {}", message, note)
    } else {
        message
    };

    if let Some(hash) = gitedit_session_hash.as_deref() {
        full_message = format!("{}\n\nGitEdit-AI-Hash: {}", full_message, hash);
    }

    // Show the message
    println!("Commit message:");
    println!("────────────────────────────────────────");
    println!("{}", full_message);
    println!("────────────────────────────────────────\n");

    // Commit
    let paragraphs = split_paragraphs(&full_message);
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
        let now = chrono::Utc::now().to_rfc3339();
        let (session_id, last_ts) = match ai::get_last_entry_timestamp_for_path(&repo_root) {
            Ok(Some((session_id, last_ts))) => (Some(session_id), Some(last_ts)),
            Ok(None) => (None, Some(now.clone())),
            Err(_) => (None, Some(now.clone())),
        };
        let checkpoint = ai::CommitCheckpoint {
            timestamp: now,
            session_id,
            last_entry_timestamp: last_ts,
        };
        if let Err(e) = ai::save_checkpoint(&repo_root, checkpoint) {
            debug!("failed to save commit checkpoint: {}", e);
        } else {
            debug!("saved commit checkpoint");
        }
    }

    // Sync to gitedit if enabled
    let should_sync = if force_gitedit {
        true
    } else {
        push && gitedit_enabled
    };

    if should_sync {
        sync_to_gitedit(
            &repo_root,
            "commit_with_check",
            &gitedit_sessions,
            gitedit_session_hash.as_deref(),
        );
    }

    Ok(())
}

/// Run Codex to review staged changes for bugs and performance issues.
fn run_codex_review(
    diff: &str,
    session_context: Option<&str>,
    workdir: &std::path::Path,
    model: CodexModel,
) -> Result<ReviewResult> {
    use std::io::{BufRead, BufReader};
    use std::sync::mpsc;
    use std::time::Instant;

    let (diff_for_prompt, _truncated) = truncate_diff(diff);

    // Build compact review prompt optimized for speed/cost
    let prompt = if let Some(context) = session_context {
        format!(
            "Review diff for bugs, security, perf issues. Return JSON: {{\"issues_found\":bool,\"issues\":[\"...\"],\"summary\":\"...\"}}\n\
Context:{}\n\
```diff\n{}\n```",
            context, diff_for_prompt
        )
    } else {
        format!(
            "Review diff for bugs, security, perf issues. Return JSON: {{\"issues_found\":bool,\"issues\":[\"...\"],\"summary\":\"...\"}}\n\
```diff\n{}\n```",
            diff_for_prompt
        )
    };

    let model_arg = format!("model=\"{}\"", model.as_codex_arg());

    // Use codex review with explicit model selection via stdin to avoid argv limits.
    let mut child = Command::new("codex")
        .args(["review", "-c", &model_arg, "-"])
        .current_dir(workdir)
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

    let tx_stdout = tx.clone();
    let reader_handle = std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().flatten() {
            let _ = tx_stdout.send(ReviewEvent::Line(line));
        }
        let _ = tx_stdout.send(ReviewEvent::StdoutDone);
    });

    let tx_stderr = tx.clone();
    let stderr_handle = std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines().flatten() {
            let _ = tx_stderr.send(ReviewEvent::StderrLine(line));
        }
        let _ = tx_stderr.send(ReviewEvent::StderrDone);
    });

    let mut output_lines = Vec::new();
    let mut stderr_lines = Vec::new();
    let mut last_progress = Instant::now();
    let timeout = Duration::from_secs(commit_with_check_timeout_secs());
    let mut deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let mut done_count = 0;
    loop {
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(ReviewEvent::Line(line)) => {
                println!("{}", line);
                output_lines.push(line);
                last_progress = Instant::now();
            }
            Ok(ReviewEvent::StderrLine(line)) => {
                if !line.trim().is_empty() {
                    println!("codex: {}", line);
                }
                stderr_lines.push(line);
            }
            Ok(ReviewEvent::StdoutDone) | Ok(ReviewEvent::StderrDone) => {
                done_count += 1;
                if done_count >= 2 {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if last_progress.elapsed() >= Duration::from_secs(10) {
                    println!(
                        "Waiting on Codex review... ({}s elapsed, no output yet)",
                        start.elapsed().as_secs()
                    );
                    last_progress = Instant::now();
                }
                if Instant::now() >= deadline {
                    if prompt_yes_no("Codex review is taking longer than expected. Keep waiting?")?
                    {
                        deadline = Instant::now() + timeout;
                    } else {
                        timed_out = true;
                        let _ = child.kill();
                        break;
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    let _ = reader_handle.join();
    let _ = stderr_handle.join();
    let status = child.wait()?;
    let stderr_output = stderr_lines.join("\n");

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

/// Run Claude Code SDK to review staged changes for bugs and performance issues.
fn run_claude_review(
    diff: &str,
    session_context: Option<&str>,
    workdir: &std::path::Path,
    model: ClaudeModel,
) -> Result<ReviewResult> {
    use std::io::{BufRead, BufReader};
    use std::sync::mpsc;
    use std::time::Instant;

    let (diff_for_prompt, _truncated) = truncate_diff(diff);

    // Build compact review prompt optimized for speed/cost
    let prompt = if let Some(context) = session_context {
        format!(
            "Review diff for bugs, security, perf issues. Return JSON: {{\"issues_found\":bool,\"issues\":[\"...\"],\"summary\":\"...\"}}\n\
Context:{}\n\
```diff\n{}\n```",
            context, diff_for_prompt
        )
    } else {
        format!(
            "Review diff for bugs, security, perf issues. Return JSON: {{\"issues_found\":bool,\"issues\":[\"...\"],\"summary\":\"...\"}}\n\
```diff\n{}\n```",
            diff_for_prompt
        )
    };

    // Use claude CLI with print mode, piping prompt via stdin to avoid arg length limits
    let model_arg = model.as_claude_arg();

    let mut child = Command::new("claude")
        .args(["-p", "--model", model_arg])
        .current_dir(workdir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to run claude - is Claude Code SDK installed?")?;

    // Write prompt to stdin
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(prompt.as_bytes())
            .context("failed to write prompt to claude stdin")?;
    }

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = mpsc::channel();
    let start = Instant::now();

    let tx_stdout = tx.clone();
    let reader_handle = std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().flatten() {
            let _ = tx_stdout.send(ReviewEvent::Line(line));
        }
        let _ = tx_stdout.send(ReviewEvent::StdoutDone);
    });

    let tx_stderr = tx.clone();
    let stderr_handle = std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines().flatten() {
            let _ = tx_stderr.send(ReviewEvent::StderrLine(line));
        }
        let _ = tx_stderr.send(ReviewEvent::StderrDone);
    });

    let mut output_lines = Vec::new();
    let mut stderr_lines = Vec::new();
    let mut last_progress = Instant::now();
    let timeout = Duration::from_secs(commit_with_check_timeout_secs());
    let mut deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let mut done_count = 0;
    loop {
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(ReviewEvent::Line(line)) => {
                println!("{}", line);
                output_lines.push(line);
                last_progress = Instant::now();
            }
            Ok(ReviewEvent::StderrLine(line)) => {
                if !line.trim().is_empty() {
                    println!("claude: {}", line);
                }
                stderr_lines.push(line);
            }
            Ok(ReviewEvent::StdoutDone) | Ok(ReviewEvent::StderrDone) => {
                done_count += 1;
                if done_count >= 2 {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if last_progress.elapsed() >= Duration::from_secs(10) {
                    println!(
                        "Waiting on Claude review... ({}s elapsed, no output yet)",
                        start.elapsed().as_secs()
                    );
                    last_progress = Instant::now();
                }
                if Instant::now() >= deadline {
                    if prompt_yes_no("Claude review is taking longer than expected. Keep waiting?")?
                    {
                        deadline = Instant::now() + timeout;
                    } else {
                        timed_out = true;
                        let _ = child.kill();
                        break;
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    let _ = reader_handle.join();
    let _ = stderr_handle.join();
    let status = child.wait()?;
    let stderr_output = stderr_lines.join("\n");

    if timed_out {
        if !stderr_output.trim().is_empty() {
            println!("{}", stderr_output.trim_end());
        }
        return Ok(ReviewResult {
            issues_found: false,
            issues: Vec::new(),
            summary: Some(format!(
                "Claude review timed out after {}s",
                timeout.as_secs()
            )),
            timed_out: true,
        });
    }

    if !status.success() {
        if !stderr_output.trim().is_empty() {
            println!("{}", stderr_output.trim_end());
        }
        println!("\nnotify: Claude review failed");
        bail!("Claude review failed");
    }

    let result = output_lines.join("\n");

    let review_json = parse_review_json(&result);
    let (issues_found, issues) = if let Some(ref parsed) = review_json {
        if let Some(summary) = parsed.summary.as_ref() {
            debug!(summary = summary.as_str(), "claude review summary");
        }
        (parsed.issues_found, parsed.issues.clone())
    } else if result.trim().is_empty() {
        (false, Vec::new())
    } else {
        debug!(review_output = result.as_str(), "claude review output");
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
    std::env::var("OPENAI_API_KEY").context("OPENAI_API_KEY environment variable not set")
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

fn git_run_in(workdir: &std::path::Path, args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .current_dir(workdir)
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

fn git_try_in(workdir: &std::path::Path, args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .current_dir(workdir)
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

fn git_capture_in(workdir: &std::path::Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .current_dir(workdir)
        .args(args)
        .output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;

    if !output.status.success() {
        bail!("git {} failed", args.join(" "));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Find the largest valid UTF-8 char boundary at or before `pos`.
fn floor_char_boundary(s: &str, pos: usize) -> usize {
    let mut end = pos.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn truncate_diff(diff: &str) -> (String, bool) {
    if diff.len() <= MAX_DIFF_CHARS {
        (diff.to_string(), false)
    } else {
        let end = floor_char_boundary(diff, MAX_DIFF_CHARS);
        let truncated = format!(
            "{}\n\n[Diff truncated to first {} characters]",
            &diff[..end],
            end
        );
        (truncated, true)
    }
}

fn truncate_context(context: &str, max_chars: usize) -> String {
    if context.len() <= max_chars {
        context.to_string()
    } else {
        let end = floor_char_boundary(context, max_chars);
        format!(
            "{}\n\n[Context truncated to first {} characters]",
            &context[..end],
            end
        )
    }
}

fn generate_commit_message(
    api_key: &str,
    diff: &str,
    status: &str,
    truncated: bool,
) -> Result<String> {
    let mut user_prompt =
        String::from("Write a git commit message for the staged changes.\n\nGit diff:\n");
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
        .timeout(std::time::Duration::from_secs(60))
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

    // Retry logic for transient failures
    const MAX_RETRIES: u32 = 3;
    let mut last_error = None;

    for attempt in 0..MAX_RETRIES {
        if attempt > 0 {
            let delay = Duration::from_secs(2u64.pow(attempt));
            print!("Retrying in {}s... ", delay.as_secs());
            io::stdout().flush().ok();
            std::thread::sleep(delay);
        }

        match client
            .post("https://api.openai.com/v1/chat/completions")
            .header("Authorization", format!("Bearer {}", api_key))
            .json(&body)
            .send()
        {
            Ok(resp) => {
                if !resp.status().is_success() {
                    let status = resp.status();
                    let text = resp.text().unwrap_or_default();
                    // Don't retry client errors (4xx)
                    if status.is_client_error() {
                        bail!("OpenAI API error {}: {}", status, text);
                    }
                    last_error = Some(format!("OpenAI API error {}: {}", status, text));
                    continue;
                }

                let parsed: ChatResponse =
                    resp.json().context("failed to parse OpenAI response")?;

                let message = parsed
                    .choices
                    .first()
                    .and_then(|c| c.message.as_ref())
                    .map(|m| m.content.trim().to_string())
                    .unwrap_or_default();

                if message.is_empty() {
                    bail!("OpenAI returned empty commit message");
                }

                return Ok(trim_quotes(&message));
            }
            Err(e) => {
                last_error = Some(format!("failed to call OpenAI API: {}", e));
                if attempt < MAX_RETRIES - 1 {
                    println!("API call failed, will retry...");
                }
            }
        }
    }

    bail!(
        "{}",
        last_error.unwrap_or_else(|| "OpenAI API failed after retries".to_string())
    )
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

fn capture_staged_snapshot_in(workdir: &std::path::Path) -> Result<StagedSnapshot> {
    let staged_diff = git_capture_in(workdir, &["diff", "--cached"])?;
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

fn restore_staged_snapshot_in(workdir: &std::path::Path, snapshot: &StagedSnapshot) -> Result<()> {
    let _ = git_try_in(workdir, &["reset", "HEAD"]);
    if let Some(path) = &snapshot.patch_path {
        let path_str = path
            .to_str()
            .context("failed to convert staged snapshot path to string")?;
        let _ = git_try_in(workdir, &["apply", "--cached", path_str]);
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

/// Send critical review issues to 1focus for reactive display.
fn send_to_1focus(project_path: &std::path::Path, issues: &[String], summary: Option<&str>) {
    // Try production worker first, then local
    let endpoints = [
        "https://1f-worker.nikiv.workers.dev/api/v1/events", // Production worker
        "http://localhost:8787/api/v1/events",               // Local dev
    ];

    let project_name = project_path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let payload = json!({
        "type": "review_issue",
        "project": project_name,
        "issues": issues,
        "summary": summary,
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });

    let client = match Client::builder().timeout(Duration::from_secs(2)).build() {
        Ok(c) => c,
        Err(_) => return,
    };

    for endpoint in &endpoints {
        if client.post(*endpoint).json(&payload).send().is_ok() {
            debug!("Sent review issues to {}", endpoint);
            return;
        }
    }
}

enum ReviewEvent {
    Line(String),
    StderrLine(String),
    StdoutDone,
    StderrDone,
}

fn should_show_review_context() -> bool {
    std::env::var("FLOW_SHOW_REVIEW_CONTEXT")
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

/// Check if gitedit mirroring is enabled in flow.toml.
fn gitedit_mirror_enabled() -> bool {
    let cwd = std::env::current_dir().ok();

    if let Some(cwd) = cwd {
        let local_config = cwd.join("flow.toml");
        if local_config.exists() {
            if let Ok(cfg) = config::load(&local_config) {
                return cfg.options.gitedit_mirror.unwrap_or(false);
            }
        }
    }

    false
}

/// Check if gitedit mirroring is enabled for commitWithCheck in flow.toml.
fn gitedit_mirror_enabled_for_commit_with_check(repo_root: &std::path::Path) -> bool {
    let local_config = repo_root.join("flow.toml");
    if local_config.exists() {
        if let Ok(cfg) = config::load(&local_config) {
            if let Some(value) = cfg.options.commit_with_check_gitedit_mirror {
                return value;
            }
            return cfg.options.gitedit_mirror.unwrap_or(false);
        }
    }

    false
}

/// Get the gitedit API URL from config or use default.
fn gitedit_api_url(repo_root: &std::path::Path) -> String {
    let local_config = repo_root.join("flow.toml");
    if local_config.exists() {
        if let Ok(cfg) = config::load(&local_config) {
            if let Some(url) = cfg.options.gitedit_url {
                return url;
            }
        }
    }

    "https://gitedit.dev".to_string()
}

fn gitedit_repo_override(repo_root: &std::path::Path) -> Option<(String, String)> {
    let local_config = repo_root.join("flow.toml");
    if !local_config.exists() {
        return None;
    }

    let cfg = config::load(&local_config).ok()?;
    let raw = cfg.options.gitedit_repo_full_name?;
    let mut value = raw.trim();

    if let Some(rest) = value.strip_prefix("gh/") {
        value = rest;
    }
    if let Some(idx) = value.find("github.com/") {
        value = &value[idx + "github.com/".len()..];
    }
    if let Some(rest) = value.strip_suffix(".git") {
        value = rest;
    }

    let mut parts = value.split('/').filter(|s| !s.is_empty());
    let owner = parts.next()?.to_string();
    let repo = parts.next()?.to_string();
    Some((owner, repo))
}

/// Sync commit to gitedit.dev for mirroring.
fn sync_to_gitedit(
    repo_root: &std::path::Path,
    event: &str,
    ai_sessions: &[ai::GitEditSessionData],
    session_hash: Option<&str>,
) {
    let (owner, repo) = if let Some((owner, repo)) = gitedit_repo_override(repo_root) {
        (owner, repo)
    } else {
        // Get remote origin URL to extract owner/repo
        let remote_url = match git_capture_in(repo_root, &["remote", "get-url", "origin"]) {
            Ok(url) => url.trim().to_string(),
            Err(_) => {
                debug!("No git remote found, skipping gitedit sync");
                return;
            }
        };

        // Parse owner/repo from remote URL
        // Supports: git@github.com:owner/repo.git, https://github.com/owner/repo.git
        match parse_github_remote(&remote_url) {
            Some((o, r)) => (o, r),
            None => {
                debug!("Could not parse GitHub remote URL: {}", remote_url);
                return;
            }
        }
    };

    // Get current commit SHA
    let commit_sha = match git_capture_in(repo_root, &["rev-parse", "HEAD"]) {
        Ok(sha) => sha.trim().to_string(),
        Err(_) => {
            debug!("Could not get commit SHA");
            return;
        }
    };

    // Get current branch
    let branch = git_capture_in(repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .ok()
        .map(|b| b.trim().to_string());
    let ref_name = branch
        .as_ref()
        .map(|name| format!("refs/heads/{}", name));

    // Get commit message
    let commit_message = git_capture_in(repo_root, &["log", "-1", "--format=%B"])
        .ok()
        .map(|m| m.trim().to_string());

    // Get author info
    let author_name = git_capture_in(repo_root, &["log", "-1", "--format=%an"])
        .ok()
        .map(|n| n.trim().to_string());
    let author_email = git_capture_in(repo_root, &["log", "-1", "--format=%ae"])
        .ok()
        .map(|e| e.trim().to_string());

    let session_count = ai_sessions.len();
    let ai_sessions_json: Vec<serde_json::Value> = ai_sessions
        .iter()
        .map(|s| {
            json!({
                "session_id": s.session_id,
                "provider": s.provider,
                "started_at": s.started_at,
                "last_activity_at": s.last_activity_at,
                "exchange_count": s.exchanges.len(),
                "context_summary": s.context_summary,
                "exchanges": s.exchanges.iter().map(|e| json!({
                    "user_message": e.user_message,
                    "assistant_message": e.assistant_message,
                    "timestamp": e.timestamp,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();

    let base_url = gitedit_api_url(repo_root);
    let base_url = base_url.trim_end_matches('/').to_string();
    let api_url = format!("{}/api/mirrors/sync", base_url);
    let view_url = format!("{}/{}/{}", base_url, owner, repo);

    let payload = json!({
        "owner": owner,
        "repo": repo,
        "commit_sha": commit_sha,
        "branch": branch,
        "ref": ref_name,
        "event": event,
        "source": "flow-cli",
        "commit_message": commit_message,
        "author_name": author_name,
        "author_email": author_email,
        "session_hash": session_hash,
        "ai_sessions": ai_sessions_json,
    });

    let client = match Client::builder().timeout(Duration::from_secs(10)).build() {
        Ok(c) => c,
        Err(_) => return,
    };

    match client.post(&api_url).json(&payload).send() {
        Ok(resp) if resp.status().is_success() => {
            if session_count > 0 {
                println!(
                    "✓ Synced to {} ({} AI session{})",
                    view_url,
                    session_count,
                    if session_count == 1 { "" } else { "s" }
                );
            } else {
                println!("✓ Synced to {}", view_url);
            }
            debug!("Gitedit sync successful");
        }
        Ok(resp) => {
            debug!("Gitedit sync failed: HTTP {}", resp.status());
        }
        Err(e) => {
            debug!("Gitedit sync error: {}", e);
        }
    }
}

fn gitedit_sessions_hash(sessions: &[ai::GitEditSessionData]) -> Option<String> {
    if sessions.is_empty() {
        return None;
    }

    let serialized = serde_json::to_string(sessions).ok()?;
    let mut hasher = DefaultHasher::new();
    serialized.hash(&mut hasher);
    Some(format!("{:016x}", hasher.finish()))
}

/// Parse owner and repo from a GitHub remote URL.
fn parse_github_remote(url: &str) -> Option<(String, String)> {
    let url = url.trim();

    // SSH format: git@github.com:owner/repo.git
    if url.starts_with("git@github.com:") {
        let path = url.strip_prefix("git@github.com:")?;
        let path = path.strip_suffix(".git").unwrap_or(path);
        let parts: Vec<&str> = path.split('/').collect();
        if parts.len() >= 2 {
            return Some((parts[0].to_string(), parts[1].to_string()));
        }
    }

    // HTTPS format: https://github.com/owner/repo.git
    if url.contains("github.com/") {
        let idx = url.find("github.com/")?;
        let path = &url[idx + 11..];
        let path = path.strip_suffix(".git").unwrap_or(path);
        let parts: Vec<&str> = path.split('/').collect();
        if parts.len() >= 2 {
            return Some((parts[0].to_string(), parts[1].to_string()));
        }
    }

    None
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

fn delegate_to_hub_with_check(
    command_name: &str,
    push: bool,
    include_context: bool,
    review_selection: ReviewSelection,
    author_message: Option<&str>,
    max_tokens: usize,
) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;

    // Build the command to run using the current executable path
    let push_flag = if push { "" } else { " --no-push" };
    let context_flag = if include_context { "" } else { " --no-context" };
    let claude_flag = if review_selection.is_claude() {
        " --claude"
    } else {
        ""
    };
    let message_flag = author_message
        .map(|m| format!(" --message {:?}", m))
        .unwrap_or_default();
    let review_model_flag = review_selection
        .review_model_arg()
        .map(|arg| format!(" --review-model {}", arg.as_arg()))
        .unwrap_or_default();
    let flow_bin = std::env::current_exe()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "flow".to_string());
    let command = format!(
        "{} {} --sync{}{}{}{}{} --tokens {}",
        flow_bin,
        command_name,
        push_flag,
        context_flag,
        claude_flag,
        review_model_flag,
        message_flag,
        max_tokens
    );

    let url = format!("http://{}:{}/tasks/run", HUB_HOST, HUB_PORT);
    let client = Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("failed to create HTTP client")?;

    let payload = json!({
        "task": {
            "name": command_name,
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
            println!("Delegated {} to hub", command_name);
            println!("  View logs: f logs --task-id {}", task_id);
            println!("  Stream logs: f logs --task-id {} --follow", task_id);
        } else {
            println!("Delegated {} to hub", command_name);
        }
        Ok(())
    } else {
        let body = resp.text().unwrap_or_default();
        bail!("hub returned error: {}", body);
    }
}
