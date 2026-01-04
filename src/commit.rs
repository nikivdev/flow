//! AI-powered git commit command using OpenAI.

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{self, Write};
use std::net::IpAddr;
use std::env;
use std::path::Path;
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

/// Patterns for files that likely contain secrets and shouldn't be committed.
const SENSITIVE_PATTERNS: &[&str] = &[
    ".env",
    ".env.local",
    ".env.production",
    ".env.development",
    ".env.staging",
    ".env.host",
    "credentials.json",
    "secrets.json",
    "service-account.json",
    ".pem",
    ".key",
    ".p12",
    ".pfx",
    ".keystore",
    "id_rsa",
    "id_ed25519",
    "id_ecdsa",
    "id_dsa",
    ".npmrc",
    ".pypirc",
    ".netrc",
    "htpasswd",
    ".htpasswd",
    "shadow",
    "passwd",
];

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

#[derive(Clone, Debug)]
pub enum ReviewSelection {
    Codex(CodexModel),
    Claude(ClaudeModel),
    Opencode { model: String },
}

impl ReviewSelection {
    fn is_claude(&self) -> bool {
        matches!(self, ReviewSelection::Claude(_))
    }

    fn is_codex(&self) -> bool {
        matches!(self, ReviewSelection::Codex(_))
    }

    fn is_opencode(&self) -> bool {
        matches!(self, ReviewSelection::Opencode { .. })
    }

    fn review_model_arg(&self) -> Option<ReviewModelArg> {
        match self {
            ReviewSelection::Codex(CodexModel::High) => Some(ReviewModelArg::CodexHigh),
            ReviewSelection::Codex(CodexModel::Mini) => Some(ReviewModelArg::CodexMini),
            ReviewSelection::Claude(ClaudeModel::Opus) => Some(ReviewModelArg::ClaudeOpus),
            ReviewSelection::Claude(ClaudeModel::Sonnet) => None,
            ReviewSelection::Opencode { .. } => None,
        }
    }

    fn model_label(&self) -> String {
        match self {
            ReviewSelection::Codex(model) => model.as_codex_arg().to_string(),
            ReviewSelection::Claude(model) => model.as_claude_arg().to_string(),
            ReviewSelection::Opencode { model } => model.clone(),
        }
    }
}

/// Check staged files for potentially sensitive content and warn the user.
/// Returns list of sensitive files found.
fn check_sensitive_files(repo_root: &Path) -> Vec<String> {
    let output = Command::new("git")
        .args(["diff", "--cached", "--name-only"])
        .current_dir(repo_root)
        .output();

    let Ok(output) = output else {
        return Vec::new();
    };

    if !output.status.success() {
        return Vec::new();
    }

    let files = String::from_utf8_lossy(&output.stdout);
    let mut sensitive = Vec::new();

    for file in files.lines() {
        let file_lower = file.to_lowercase();
        let file_name = Path::new(file)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(file)
            .to_lowercase();

        if file_name.starts_with(".env") {
            sensitive.push(file.to_string());
            continue;
        }

        for pattern in SENSITIVE_PATTERNS {
            let pattern_lower = pattern.to_lowercase();
            // Check if filename matches or ends with pattern
            if file_name == pattern_lower
                || file_name.ends_with(&pattern_lower)
                || file_lower.contains(&format!("/{}", pattern_lower))
            {
                sensitive.push(file.to_string());
                break;
            }
        }
    }

    sensitive
}

/// Warn about sensitive files and optionally abort.
fn warn_sensitive_files(files: &[String]) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }

    if env::var("FLOW_ALLOW_SENSITIVE_COMMIT").ok().as_deref() == Some("1") {
        return Ok(());
    }

    println!("\n⚠️  Warning: Potentially sensitive files detected:");
    for file in files {
        println!("   - {}", file);
    }
    println!();
    println!("These files may contain secrets. Consider:");
    println!("   - Adding them to .gitignore");
    println!("   - Using `git reset HEAD <file>` to unstage");
    println!();

    bail!("Refusing to commit sensitive files. Set FLOW_ALLOW_SENSITIVE_COMMIT=1 to override.")
}

/// Threshold for "large" file changes (lines added + removed).
const LARGE_DIFF_THRESHOLD: usize = 500;

/// Check for files with unusually large diffs.
/// Returns list of (filename, lines_changed) for files over threshold.
fn check_large_diffs(repo_root: &Path) -> Vec<(String, usize)> {
    let output = Command::new("git")
        .args(["diff", "--cached", "--numstat"])
        .current_dir(repo_root)
        .output();

    let Ok(output) = output else {
        return Vec::new();
    };

    if !output.status.success() {
        return Vec::new();
    }

    let stats = String::from_utf8_lossy(&output.stdout);
    let mut large_files = Vec::new();

    for line in stats.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() >= 3 {
            // Format: added<tab>removed<tab>filename
            // Binary files show "-" for added/removed
            let added: usize = parts[0].parse().unwrap_or(0);
            let removed: usize = parts[1].parse().unwrap_or(0);
            let filename = parts[2].to_string();
            let total = added + removed;

            if total >= LARGE_DIFF_THRESHOLD {
                large_files.push((filename, total));
            }
        }
    }

    // Sort by size descending
    large_files.sort_by(|a, b| b.1.cmp(&a.1));
    large_files
}

/// Warn about files with large diffs.
fn warn_large_diffs(files: &[(String, usize)]) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }

    println!("⚠️  Warning: Files with large diffs ({}+ lines):", LARGE_DIFF_THRESHOLD);
    for (file, lines) in files {
        println!("   - {} ({} lines)", file, lines);
    }
    println!();
    println!("These might be generated/lock files. Consider:");
    println!("   - Adding them to .gitignore if generated");
    println!("   - Using `git reset HEAD <file>` to unstage");
    println!();

    Ok(())
}

/// Check TypeScript config for commit settings first.
pub fn resolve_review_selection_from_config() -> Option<ReviewSelection> {
    let ts_config = config::load_ts_config()?;
    let commit_config = ts_config.flow?.commit?;

    let tool = commit_config.tool.as_deref()?;
    let model = commit_config.model.clone();

    match tool {
        "opencode" => {
            let model = model.unwrap_or_else(|| "opencode/minimax-m2.1-free".to_string());
            Some(ReviewSelection::Opencode { model })
        }
        "claude" => {
            let model_enum = match model.as_deref() {
                Some("opus") | Some("claude-opus") => ClaudeModel::Opus,
                _ => ClaudeModel::Sonnet,
            };
            Some(ReviewSelection::Claude(model_enum))
        }
        "codex" => {
            let model_enum = match model.as_deref() {
                Some("mini") | Some("codex-mini") => CodexModel::Mini,
                _ => CodexModel::High,
            };
            Some(ReviewSelection::Codex(model_enum))
        }
        _ => None,
    }
}

pub fn resolve_review_selection(
    use_claude: bool,
    override_model: Option<ReviewModelArg>,
) -> ReviewSelection {
    // Check TypeScript config first
    if let Some(selection) = resolve_review_selection_from_config() {
        return selection;
    }

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

/// New default: Claude is default, --codex flag to use Codex
pub fn resolve_review_selection_v2(
    use_codex: bool,
    override_model: Option<ReviewModelArg>,
) -> ReviewSelection {
    // Check TypeScript config first
    if let Some(selection) = resolve_review_selection_from_config() {
        return selection;
    }

    if let Some(model) = override_model {
        return match model {
            ReviewModelArg::ClaudeOpus => ReviewSelection::Claude(ClaudeModel::Opus),
            ReviewModelArg::CodexHigh => ReviewSelection::Codex(CodexModel::High),
            ReviewModelArg::CodexMini => ReviewSelection::Codex(CodexModel::Mini),
        };
    }

    if use_codex {
        ReviewSelection::Codex(CodexModel::High)
    } else {
        // Default: Claude Sonnet
        ReviewSelection::Claude(ClaudeModel::Sonnet)
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

#[derive(Debug, Serialize)]
struct RemoteReviewRequest {
    diff: String,
    context: Option<String>,
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    review_instructions: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RemoteReviewResponse {
    output: String,
    #[serde(default)]
    stderr: String,
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

    // Check for sensitive files before proceeding
    let cwd = std::env::current_dir()?;
    let sensitive_files = check_sensitive_files(&cwd);
    warn_sensitive_files(&sensitive_files)?;

    // Check for files with large diffs
    let large_diffs = check_large_diffs(&cwd);
    warn_large_diffs(&large_diffs)?;

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
        sync_to_gitedit(&cwd, "commit", &[], None, None);
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
            "commit",  // CLI command name
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
    // Check TypeScript config first
    if let Some(ts_config) = config::load_ts_config() {
        if let Some(flow) = ts_config.flow {
            if let Some(commit) = flow.commit {
                if let Some(async_enabled) = commit.async_enabled {
                    return async_enabled;
                }
            }
        }
    }

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

fn commit_with_check_review_url() -> Option<String> {
    if let Ok(url) = env::var("FLOW_REVIEW_URL") {
        let trimmed = url.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    let cwd = std::env::current_dir().ok();
    if let Some(cwd) = cwd {
        let local_config = cwd.join("flow.toml");
        if local_config.exists() {
            if let Ok(cfg) = config::load(&local_config) {
                if let Some(url) = cfg.options.commit_with_check_review_url {
                    let trimmed = url.trim().to_string();
                    if !trimmed.is_empty() {
                        return Some(trimmed);
                    }
                }
            }
        }
    }

    let global_config = config::default_config_path();
    if global_config.exists() {
        if let Ok(cfg) = config::load(&global_config) {
            if let Some(url) = cfg.options.commit_with_check_review_url {
                let trimmed = url.trim().to_string();
                if !trimmed.is_empty() {
                    return Some(trimmed);
                }
            }
        }
    }

    None
}

fn commit_with_check_review_token() -> Option<String> {
    if let Ok(token) = env::var("FLOW_REVIEW_TOKEN") {
        let trimmed = token.trim().to_string();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }

    let cwd = std::env::current_dir().ok();
    if let Some(cwd) = cwd {
        let local_config = cwd.join("flow.toml");
        if local_config.exists() {
            if let Ok(cfg) = config::load(&local_config) {
                if let Some(token) = cfg.options.commit_with_check_review_token {
                    let trimmed = token.trim().to_string();
                    if !trimmed.is_empty() {
                        return Some(trimmed);
                    }
                }
            }
        }
    }

    let global_config = config::default_config_path();
    if global_config.exists() {
        if let Ok(cfg) = config::load(&global_config) {
            if let Some(token) = cfg.options.commit_with_check_review_token {
                let trimmed = token.trim().to_string();
                if !trimmed.is_empty() {
                    return Some(trimmed);
                }
            }
        }
    }

    None
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

    // Run pre-commit fixers if configured
    if let Ok(fixed) = run_fixers(&repo_root) {
        if fixed {
            println!();
        }
    }

    // Stage all changes
    print!("Staging changes... ");
    io::stdout().flush()?;
    git_run_in(&repo_root, &["add", "."])?;
    println!("done");

    // Check for sensitive files before proceeding
    let sensitive_files = check_sensitive_files(&repo_root);
    warn_sensitive_files(&sensitive_files)?;

    // Check for files with large diffs
    let large_diffs = check_large_diffs(&repo_root);
    warn_large_diffs(&large_diffs)?;

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

    // Get custom review instructions from [commit] config
    let review_instructions = get_review_instructions(&repo_root);

    // Run code review
    if review_selection.is_claude() {
        println!("\nRunning Claude code review...");
    } else if review_selection.is_opencode() {
        println!("\nRunning opencode review...");
    } else {
        println!("\nRunning Codex code review...");
    }
    println!("Model: {}", review_selection.model_label());
    if session_context.is_some() {
        println!("(with AI session context)");
    }
    if review_instructions.is_some() {
        println!("(with custom review instructions)");
    }
    println!("────────────────────────────────────────");

    let review = match &review_selection {
        ReviewSelection::Claude(model) => {
            run_claude_review(&diff, session_context.as_deref(), review_instructions.as_deref(), &repo_root, *model)
        }
        ReviewSelection::Codex(model) => {
            run_codex_review(&diff, session_context.as_deref(), review_instructions.as_deref(), &repo_root, *model)
        }
        ReviewSelection::Opencode { model } => {
            run_opencode_review(&diff, session_context.as_deref(), review_instructions.as_deref(), &repo_root, model)
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

    // Generate commit message (use opencode if that's the review tool)
    print!("Generating commit message... ");
    io::stdout().flush()?;
    let message = if let ReviewSelection::Opencode { ref model } = review_selection {
        generate_commit_message_opencode(&diff_for_prompt, &status, truncated, model)?
    } else {
        generate_commit_message(&api_key, &diff_for_prompt, &status, truncated)?
    };
    println!("done\n");

    let mut gitedit_sessions: Vec<ai::GitEditSessionData> = Vec::new();
    let mut gitedit_session_hash: Option<String> = None;

    let gitedit_enabled =
        force_gitedit || gitedit_mirror_enabled_for_commit_with_check(&repo_root);

    if gitedit_enabled {
        match ai::get_sessions_for_gitedit(&repo_root) {
            Ok(sessions) => {
                if !sessions.is_empty() {
                    // Get owner/repo for the hash
                    if let Some((owner, repo)) = get_gitedit_project(&repo_root) {
                        gitedit_session_hash = gitedit_sessions_hash(&owner, &repo, &sessions);
                    }
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
        full_message = format!("{}\n\ngitedit.dev/{}", full_message, hash);
    }

    // Show the message
    println!("Commit message:");
    println!("────────────────────────────────────────");
    println!("{}", full_message);
    println!("────────────────────────────────────────\n");

    // Check if docs need updating (reminder for AI assistant)
    let docs_dir = repo_root.join(".ai/docs");
    if docs_dir.exists() {
        let has_new_commands = diff.contains("pub enum Commands")
            || diff.contains("Subcommand")
            || diff.contains("#[command(");
        let has_new_features = diff.contains("pub fn run")
            || diff.contains("pub async fn")
            || diff.lines().any(|l| l.starts_with("+pub mod"));

        if has_new_commands || has_new_features {
            println!("📝 Docs may need updating (.ai/docs/)");
        }
    }

    // Commit
    let paragraphs = split_paragraphs(&full_message);
    let mut args = vec!["commit"];
    for p in &paragraphs {
        args.push("-m");
        args.push(p);
    }
    git_run(&args)?;
    println!("✓ Committed");

    if let Ok(commit_sha) = git_capture_in(&repo_root, &["rev-parse", "HEAD"]) {
        let branch = git_capture_in(&repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])
            .unwrap_or_else(|_| "unknown".to_string());
        let reviewer = if review_selection.is_claude() {
            "claude"
        } else {
            "codex"
        };
        ai::log_commit_review(
            &repo_root,
            commit_sha.trim(),
            branch.trim(),
            &full_message,
            &review_selection.model_label(),
            reviewer,
            review.issues_found,
            &review.issues,
            review.summary.as_deref(),
            review.timed_out,
            context_chars,
        );
    } else {
        debug!("failed to capture commit SHA for review log");
    }

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
        // Build review data for gitedit
        let review_data = GitEditReviewData {
            diff: Some(diff.clone()),
            issues_found: review.issues_found,
            issues: review.issues.clone(),
            summary: review.summary.clone(),
            reviewer: Some(
                if review_selection.is_claude() {
                    "claude".to_string()
                } else {
                    "codex".to_string()
                },
            ),
        };

        sync_to_gitedit(
            &repo_root,
            "commit_with_check",
            &gitedit_sessions,
            gitedit_session_hash.as_deref(),
            Some(&review_data),
        );
    }

    Ok(())
}

/// Run Codex to review staged changes for bugs and performance issues.
fn run_codex_review(
    diff: &str,
    session_context: Option<&str>,
    review_instructions: Option<&str>,
    workdir: &std::path::Path,
    model: CodexModel,
) -> Result<ReviewResult> {
    use std::io::{BufRead, BufReader};
    use std::sync::mpsc;
    use std::time::Instant;

    let (diff_for_prompt, _truncated) = truncate_diff(diff);

    // Build compact review prompt optimized for speed/cost
    let mut prompt = String::from("Review diff for bugs, security, perf issues. Return JSON: {\"issues_found\":bool,\"issues\":[\"...\"],\"summary\":\"...\"}\n");

    // Add custom review instructions if provided
    if let Some(instructions) = review_instructions {
        prompt.push_str(&format!("\nAdditional review instructions:\n{}\n", instructions));
    }

    // Add session context if provided
    if let Some(context) = session_context {
        prompt.push_str(&format!("\nContext:\n{}\n", context));
    }

    prompt.push_str(&format!("```diff\n{}\n```", diff_for_prompt));

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

fn normalize_review_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    if trimmed.ends_with("/review") {
        trimmed.to_string()
    } else {
        format!("{}/review", trimmed)
    }
}

fn run_remote_claude_review(
    diff: &str,
    session_context: Option<&str>,
    review_instructions: Option<&str>,
    model: ClaudeModel,
) -> Result<ReviewResult> {
    let url = match commit_with_check_review_url() {
        Some(url) => url,
        None => bail!("remote review URL not configured"),
    };

    let review_url = normalize_review_url(&url);
    let payload = RemoteReviewRequest {
        diff: diff.to_string(),
        context: session_context.map(|value| value.to_string()),
        model: model.as_claude_arg().to_string(),
        review_instructions: review_instructions.map(|v| v.to_string()),
    };

    let client = Client::builder()
        .timeout(Duration::from_secs(commit_with_check_timeout_secs()))
        .build()
        .context("failed to create HTTP client for remote review")?;

    let mut request = client.post(&review_url).json(&payload);
    if let Some(token) = commit_with_check_review_token() {
        request = request.bearer_auth(token);
    }

    let response = request
        .send()
        .context("failed to send remote review request")?;

    if !response.status().is_success() {
        bail!("remote review failed: HTTP {}", response.status());
    }

    let payload: RemoteReviewResponse = response
        .json()
        .context("failed to parse remote review response")?;

    if !payload.stderr.trim().is_empty() {
        debug!(stderr = payload.stderr.as_str(), "remote claude stderr");
    }

    let result = payload.output;
    let review_json = parse_review_json(&result);
    let (issues_found, issues) = if let Some(ref parsed) = review_json {
        if let Some(summary) = parsed.summary.as_ref() {
            debug!(summary = summary.as_str(), "remote claude review summary");
        }
        (parsed.issues_found, parsed.issues.clone())
    } else if result.trim().is_empty() {
        (false, Vec::new())
    } else {
        debug!(review_output = result.as_str(), "remote claude review output");
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
    review_instructions: Option<&str>,
    workdir: &std::path::Path,
    model: ClaudeModel,
) -> Result<ReviewResult> {
    if commit_with_check_review_url().is_some() {
        match run_remote_claude_review(diff, session_context, review_instructions, model) {
            Ok(review) => return Ok(review),
            Err(err) => {
                println!("⚠ Remote review failed: {}", err);
                println!("  Falling back to local Claude review...");
            }
        }
    }

    let local_review = (|| -> Result<ReviewResult> {
        use std::io::{BufRead, BufReader};
        use std::sync::mpsc;
        use std::time::Instant;

        let (diff_for_prompt, _truncated) = truncate_diff(diff);

        // Build compact review prompt optimized for speed/cost
        let mut prompt = String::from("Review diff for bugs, security, perf issues. Return JSON: {\"issues_found\":bool,\"issues\":[\"...\"],\"summary\":\"...\"}\n");

        // Add custom review instructions if provided
        if let Some(instructions) = review_instructions {
            prompt.push_str(&format!("\nAdditional review instructions:\n{}\n", instructions));
        }

        // Add session context if provided
        if let Some(context) = session_context {
            prompt.push_str(&format!("\nContext:\n{}\n", context));
        }

        prompt.push_str(&format!("```diff\n{}\n```", diff_for_prompt));

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
    })();

    match local_review {
        Ok(review) => Ok(review),
        Err(err) => {
            println!("⚠ Local Claude review failed: {}", err);
            println!("  Proceeding without review.");
            Ok(ReviewResult {
                issues_found: false,
                issues: Vec::new(),
                summary: Some(format!("Claude review failed: {}", err)),
                timed_out: false,
            })
        }
    }
}

/// Run opencode to review staged changes for bugs and performance issues.
fn run_opencode_review(
    diff: &str,
    session_context: Option<&str>,
    review_instructions: Option<&str>,
    workdir: &std::path::Path,
    model: &str,
) -> Result<ReviewResult> {
    use std::io::{BufRead, BufReader};

    let (diff_for_prompt, _truncated) = truncate_diff(diff);

    // Build review prompt
    let mut prompt = String::from(
        "Review this git diff for bugs, security issues, and performance problems. \
         Return a JSON object with this exact format: \
         {\"issues_found\": true/false, \"issues\": [\"issue 1\", \"issue 2\"], \"summary\": \"brief summary\"}\n\n",
    );

    if let Some(instructions) = review_instructions {
        prompt.push_str(&format!("Additional review instructions:\n{}\n\n", instructions));
    }

    if let Some(context) = session_context {
        prompt.push_str(&format!("Context:\n{}\n\n", context));
    }

    prompt.push_str(&format!("```diff\n{}\n```", diff_for_prompt));

    // Run opencode with the specified model
    let mut child = Command::new("opencode")
        .args(["run", "--model", model, &prompt])
        .current_dir(workdir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to run opencode - is it installed?")?;

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    // Read output with timeout
    let reader = BufReader::new(stdout);
    let mut output_lines = Vec::new();
    for line in reader.lines().flatten() {
        print!("{}\n", line);
        output_lines.push(line);
    }

    // Also capture stderr
    let stderr_reader = BufReader::new(stderr);
    for line in stderr_reader.lines().flatten() {
        debug!("opencode stderr: {}", line);
    }

    let status = child.wait()?;
    if !status.success() {
        debug!("opencode exited with non-zero status: {:?}", status.code());
    }

    let output = output_lines.join("\n");

    // Try to parse JSON from output
    let review_json = parse_review_json(&output);
    let (issues_found, issues) = if let Some(ref json) = review_json {
        (json.issues_found, json.issues.clone())
    } else {
        // Fallback: check for issue keywords
        let lowered = output.to_lowercase();
        let has_issues = lowered.contains("bug")
            || lowered.contains("issue")
            || lowered.contains("error")
            || lowered.contains("problem")
            || lowered.contains("security")
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

/// Generate commit message using opencode with a specified model.
#[allow(dead_code)]
fn generate_commit_message_opencode(
    diff: &str,
    status: &str,
    truncated: bool,
    model: &str,
) -> Result<String> {
    let mut prompt = String::from(
        "You are an expert software engineer. Write a clear, concise git commit message for these changes.\n\n\
         Guidelines:\n\
         - Use imperative mood (\"Add feature\" not \"Added feature\")\n\
         - First line: concise summary of WHAT and WHY (under 72 chars)\n\
         - Focus on the semantic meaning and purpose, not just listing changed files/functions\n\
         - If multiple logical changes, use bullet points in body\n\
         - Never include secrets or sensitive data\n\n\
         Git diff:\n",
    );
    prompt.push_str(diff);

    if truncated {
        prompt.push_str("\n\n[Diff truncated]");
    }

    let status = status.trim();
    if !status.is_empty() {
        prompt.push_str("\n\nGit status:\n");
        prompt.push_str(status);
    }

    prompt.push_str("\n\nWrite only the commit message, nothing else:");

    let output = Command::new("opencode")
        .args(["run", "--model", model, &prompt])
        .output()
        .context("failed to run opencode for commit message")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("opencode failed: {}", stderr.trim());
    }

    let message = String::from_utf8_lossy(&output.stdout)
        .trim()
        .to_string();

    if message.is_empty() {
        bail!("opencode returned empty commit message");
    }

    Ok(trim_quotes(&message))
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

/// Data from AI code review to sync to gitedit.
#[derive(Debug, Clone, Default)]
pub struct GitEditReviewData {
    pub diff: Option<String>,
    pub issues_found: bool,
    pub issues: Vec<String>,
    pub summary: Option<String>,
    pub reviewer: Option<String>, // "claude" or "codex"
}

/// Sync commit to gitedit.dev for mirroring.
fn sync_to_gitedit(
    repo_root: &std::path::Path,
    event: &str,
    ai_sessions: &[ai::GitEditSessionData],
    session_hash: Option<&str>,
    review_data: Option<&GitEditReviewData>,
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

    // Build review data if present
    let review_json = review_data.map(|r| {
        json!({
            "diff": r.diff,
            "issues_found": r.issues_found,
            "issues": r.issues,
            "summary": r.summary,
            "reviewer": r.reviewer,
        })
    });

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
        "review": review_json,
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

fn gitedit_sessions_hash(
    owner: &str,
    repo: &str,
    sessions: &[ai::GitEditSessionData],
) -> Option<String> {
    if sessions.is_empty() {
        return None;
    }

    // Hash includes owner/repo so the URL uniquely identifies the project
    let serialized = serde_json::to_string(sessions).ok()?;
    let mut hasher = DefaultHasher::new();
    owner.hash(&mut hasher);
    repo.hash(&mut hasher);
    serialized.hash(&mut hasher);
    Some(format!("{:016x}", hasher.finish()))
}

/// Get owner/repo from git remote or gitedit override.
fn get_gitedit_project(repo_root: &std::path::Path) -> Option<(String, String)> {
    // Check for override first
    if let Some((owner, repo)) = gitedit_repo_override(repo_root) {
        return Some((owner, repo));
    }

    // Get from git remote
    let remote_url = git_capture_in(repo_root, &["remote", "get-url", "origin"]).ok()?;
    parse_github_remote(remote_url.trim())
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
    let repo_root = resolve_commit_with_check_root()?;

    // Generate early gitedit hash from session IDs + owner/repo
    let early_gitedit_url = generate_early_gitedit_url(&repo_root);

    // Build the command to run using the current executable path
    let push_flag = if push { "" } else { " --no-push" };
    let context_flag = if include_context { " --context" } else { "" };
    let codex_flag = if review_selection.is_codex() {
        " --codex"
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
        codex_flag,
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
            if let Some(gitedit_url) = early_gitedit_url {
                println!("  GitEdit: {}", gitedit_url);
            }
        } else {
            println!("Delegated {} to hub", command_name);
        }
        Ok(())
    } else {
        let body = resp.text().unwrap_or_default();
        bail!("hub returned error: {}", body);
    }
}

/// Generate gitedit URL early from session IDs (before full data load).
fn generate_early_gitedit_url(repo_root: &std::path::Path) -> Option<String> {
    // Get owner/repo
    let (owner, repo) = get_gitedit_project(repo_root)?;

    // Get session IDs and checkpoint for hashing
    let (session_ids, checkpoint_ts) = ai::get_session_ids_for_hash(&repo_root.to_path_buf()).ok()?;

    if session_ids.is_empty() {
        return None;
    }

    // Generate hash from owner/repo + session IDs + checkpoint
    let mut hasher = DefaultHasher::new();
    owner.hash(&mut hasher);
    repo.hash(&mut hasher);
    for sid in &session_ids {
        sid.hash(&mut hasher);
    }
    if let Some(ts) = &checkpoint_ts {
        ts.hash(&mut hasher);
    }
    let hash = format!("{:016x}", hasher.finish());

    let base_url = gitedit_api_url(repo_root);
    let base_url = base_url.trim_end_matches('/');
    Some(format!("{}/{}", base_url, hash))
}

// ─────────────────────────────────────────────────────────────
// Pre-commit fixers
// ─────────────────────────────────────────────────────────────

/// Run pre-commit fixers from [commit] config.
pub fn run_fixers(repo_root: &Path) -> Result<bool> {
    let config_path = repo_root.join("flow.toml");
    let config = if config_path.exists() {
        config::load(&config_path)?
    } else {
        return Ok(false);
    };

    let commit_cfg = match &config.commit {
        Some(c) if !c.fixers.is_empty() => c,
        _ => return Ok(false),
    };

    let mut any_fixed = false;

    for fixer in &commit_cfg.fixers {
        match run_fixer(repo_root, fixer) {
            Ok(fixed) => {
                if fixed {
                    any_fixed = true;
                }
            }
            Err(e) => {
                eprintln!("Fixer '{}' failed: {}", fixer, e);
            }
        }
    }

    Ok(any_fixed)
}

/// Run a single fixer. Returns true if any files were modified.
fn run_fixer(repo_root: &Path, fixer: &str) -> Result<bool> {
    // Custom command: "cmd:prettier --write"
    if let Some(cmd) = fixer.strip_prefix("cmd:") {
        return run_action_script(repo_root, cmd);
    }

    // Check for script in .ai/actions/
    let action_path = repo_root.join(".ai/actions").join(fixer);
    if action_path.exists() {
        return run_action_script(repo_root, action_path.to_str().unwrap_or(fixer));
    }

    // Fallback to built-in fixers
    match fixer {
        "mdx-comments" => fix_mdx_comments(repo_root),
        "trailing-whitespace" => fix_trailing_whitespace(repo_root),
        "end-of-file" => fix_end_of_file(repo_root),
        _ => {
            debug!("Unknown fixer and no .ai/actions/{} script found", fixer);
            Ok(false)
        }
    }
}

/// Run an action script from .ai/actions/ or a custom command.
fn run_action_script(repo_root: &Path, cmd: &str) -> Result<bool> {
    let display_name = cmd.strip_prefix(".ai/actions/").unwrap_or(cmd);
    println!("Running: {}", display_name);

    let status = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(repo_root)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;

    Ok(status.success())
}

/// Fix MDX comments: convert <!-- --> to {/* */}
fn fix_mdx_comments(repo_root: &Path) -> Result<bool> {
    // Quick check: any HTML comments in MDX files?
    let check = Command::new("git")
        .args(["grep", "-l", "<!--", "--", "*.mdx", "**/*.mdx"])
        .current_dir(repo_root)
        .output()?;

    let files_with_issues: Vec<_> = String::from_utf8_lossy(&check.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| repo_root.join(l))
        .collect();

    if files_with_issues.is_empty() {
        return Ok(false);
    }

    let mut fixed_any = false;
    for file in files_with_issues {
        if let Ok(content) = fs::read_to_string(&file) {
            let fixed = fix_html_comments_to_jsx(&content);
            if fixed != content {
                fs::write(&file, &fixed)?;
                println!("  Fixed MDX comments: {}", file.display());
                fixed_any = true;
            }
        }
    }

    if fixed_any {
        println!("✓ Fixed MDX comments");
    }

    Ok(fixed_any)
}

/// Convert HTML comments to JSX comments in MDX content.
fn fix_html_comments_to_jsx(content: &str) -> String {
    let mut result = String::with_capacity(content.len());
    let mut chars = content.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '<' && chars.peek() == Some(&'!') {
            // Potential HTML comment
            let mut buf = String::from("<");
            buf.push(chars.next().unwrap()); // !

            // Check for --
            if chars.peek() == Some(&'-') {
                buf.push(chars.next().unwrap()); // first -
                if chars.peek() == Some(&'-') {
                    buf.push(chars.next().unwrap()); // second -

                    // Found <!--, now collect until -->
                    let mut comment_content = String::new();
                    loop {
                        match chars.next() {
                            Some('-') => {
                                if chars.peek() == Some(&'-') {
                                    chars.next(); // consume second -
                                    if chars.peek() == Some(&'>') {
                                        chars.next(); // consume >
                                        // Found -->, convert to JSX comment
                                        result.push_str("{/* ");
                                        result.push_str(comment_content.trim());
                                        result.push_str(" */}");
                                        break;
                                    } else {
                                        comment_content.push_str("--");
                                    }
                                } else {
                                    comment_content.push('-');
                                }
                            }
                            Some(ch) => comment_content.push(ch),
                            None => {
                                // Unclosed comment, keep original
                                result.push_str(&buf);
                                result.push_str(&comment_content);
                                break;
                            }
                        }
                    }
                    continue;
                }
            }
            result.push_str(&buf);
        } else {
            result.push(c);
        }
    }

    result
}

/// Fix trailing whitespace in text files.
fn fix_trailing_whitespace(repo_root: &Path) -> Result<bool> {
    // Quick check: any trailing whitespace in working directory changes?
    let check = Command::new("git")
        .args(["diff", "--check"])
        .current_dir(repo_root)
        .output()?;

    // --check exits non-zero and outputs lines if there's trailing whitespace
    if check.stdout.is_empty() {
        return Ok(false);
    }

    let mut fixed_any = false;

    // Get modified/new text files (unstaged)
    let output = Command::new("git")
        .args(["diff", "--name-only", "--diff-filter=ACMR"])
        .current_dir(repo_root)
        .output()?;

    let files: Vec<_> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| repo_root.join(l))
        .collect();

    for file in files {
        if !file.exists() || is_binary(&file) {
            continue;
        }
        if let Ok(content) = fs::read_to_string(&file) {
            let fixed: String = content
                .lines()
                .map(|line| line.trim_end())
                .collect::<Vec<_>>()
                .join("\n");

            // Preserve original line ending
            let fixed = if content.ends_with('\n') && !fixed.ends_with('\n') {
                format!("{}\n", fixed)
            } else {
                fixed
            };

            if fixed != content {
                fs::write(&file, &fixed)?;
                println!("  Trimmed whitespace: {}", file.display());
                fixed_any = true;
            }
        }
    }

    if fixed_any {
        println!("✓ Fixed trailing whitespace");
    }

    Ok(fixed_any)
}

/// Ensure files end with a newline.
fn fix_end_of_file(repo_root: &Path) -> Result<bool> {
    // Quick check: any files missing final newline in working directory?
    let check = Command::new("git")
        .args(["diff"])
        .current_dir(repo_root)
        .output()?;

    let diff_output = String::from_utf8_lossy(&check.stdout);
    if !diff_output.contains("\\ No newline at end of file") {
        return Ok(false);
    }

    let mut fixed_any = false;

    let output = Command::new("git")
        .args(["diff", "--name-only", "--diff-filter=ACMR"])
        .current_dir(repo_root)
        .output()?;

    let files: Vec<_> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| repo_root.join(l))
        .collect();

    for file in files {
        if !file.exists() || is_binary(&file) {
            continue;
        }
        if let Ok(content) = fs::read_to_string(&file) {
            if !content.is_empty() && !content.ends_with('\n') {
                fs::write(&file, format!("{}\n", content))?;
                println!("  Added newline: {}", file.display());
                fixed_any = true;
            }
        }
    }

    if fixed_any {
        println!("✓ Fixed end of file newlines");
    }

    Ok(fixed_any)
}

/// Simple binary file detection.
fn is_binary(path: &Path) -> bool {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    matches!(
        ext,
        "png" | "jpg" | "jpeg" | "gif" | "ico" | "webp" | "svg"
            | "woff" | "woff2" | "ttf" | "otf" | "eot"
            | "zip" | "tar" | "gz" | "rar" | "7z"
            | "pdf" | "doc" | "docx" | "xls" | "xlsx"
            | "exe" | "dll" | "so" | "dylib"
            | "mp3" | "mp4" | "wav" | "avi" | "mov"
    )
}

/// Get review instructions from [commit] config or .ai/ folder.
pub fn get_review_instructions(repo_root: &Path) -> Option<String> {
    // Check config first
    let config_path = repo_root.join("flow.toml");
    if let Ok(config) = config::load(&config_path) {
        if let Some(commit_cfg) = config.commit.as_ref() {
            // Try inline instructions
            if let Some(instructions) = &commit_cfg.review_instructions {
                return Some(instructions.clone());
            }

            // Try loading from configured file
            if let Some(file_path) = &commit_cfg.review_instructions_file {
                let full_path = repo_root.join(file_path);
                if let Ok(content) = fs::read_to_string(full_path) {
                    return Some(content);
                }
            }
        }
    }

    // Auto-discover from .ai/ folder (no config needed)
    let candidates = [
        ".ai/review.md",
        ".ai/commit-review.md",
        ".ai/instructions.md",
    ];

    for candidate in candidates {
        let path = repo_root.join(candidate);
        if let Ok(content) = fs::read_to_string(&path) {
            return Some(content);
        }
    }

    None
}
