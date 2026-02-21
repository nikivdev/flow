//! AI-powered git commit command using OpenAI.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, hash_map::DefaultHasher};
use std::env;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{self, IsTerminal, Read, Seek, SeekFrom, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use clap::ValueEnum;
use flow_commit_scan::scan_diff_for_secrets;
use regex::Regex;
use reqwest::StatusCode;
use reqwest::blocking::Client;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha1::{Digest, Sha1};
use tempfile::{Builder as TempBuilder, NamedTempFile, TempDir};
use tracing::{debug, info};
use uuid::Uuid;

use crate::ai;
use crate::cli::{CommitQueueAction, CommitQueueCommand, DaemonAction, PrOpts};
use crate::config;
use crate::daemon;
use crate::env as flow_env;
use crate::features;
use crate::git_guard;
use crate::gitignore_policy;
use crate::hub;
use crate::notify;
use crate::setup;
use crate::skills;
use crate::supervisor;
use crate::todo;
use crate::undo;
use crate::vcs;

const MODEL: &str = "gpt-4.1-nano";
const MAX_DIFF_CHARS: usize = 12_000;
const HUB_HOST: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1));
const HUB_PORT: u16 = 9050;
const DEFAULT_OPENROUTER_REVIEW_MODEL: &str = "arcee-ai/trinity-large-preview:free";
const DEFAULT_OPENCODE_MODEL: &str = "opencode/minimax-m2.1-free";
const DEFAULT_RISE_MODEL: &str = "zai:glm-4.7";
const DEFAULT_GLM5_RISE_MODEL: &str = "zai:glm-5";

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

#[derive(Copy, Clone, Debug)]
pub struct CommitQueueMode {
    pub enabled: bool,
    pub override_flag: Option<bool>,
    pub open_review: bool,
}

impl CommitQueueMode {
    pub fn with_open_review(mut self, open_review: bool) -> Self {
        self.open_review = open_review;
        self
    }
}

#[derive(Copy, Clone, Debug, Default)]
pub struct CommitGateOverrides {
    pub skip_quality: bool,
    pub skip_docs: bool,
    pub skip_tests: bool,
}

#[derive(Clone, Debug)]
struct CommitTestingPolicy {
    mode: String,
    runner: String,
    bun_repo_strict: bool,
    require_related_tests: bool,
    ai_scratch_test_dir: String,
    run_ai_scratch_tests: bool,
    allow_ai_scratch_to_satisfy_gate: bool,
    max_local_gate_seconds: u64,
}

#[derive(Clone, Debug, Default)]
struct CommitSkillGatePolicy {
    mode: String,
    required: Vec<String>,
    min_version: HashMap<String, u32>,
}

#[derive(Clone, Debug, Default)]
struct SkillGateReport {
    pass: bool,
    mode: String,
    override_flag: Option<String>,
    required_skills: Vec<String>,
    missing_skills: Vec<String>,
    version_failures: Vec<String>,
    loaded_versions: HashMap<String, u32>,
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
    Rise { model: String },
    Kimi { model: Option<String> },
    OpenRouter { model: String },
}

impl ReviewSelection {
    fn is_codex(&self) -> bool {
        matches!(self, ReviewSelection::Codex(_))
    }

    fn is_openrouter(&self) -> bool {
        matches!(self, ReviewSelection::OpenRouter { .. })
    }

    fn review_model_arg(&self) -> Option<ReviewModelArg> {
        match self {
            ReviewSelection::Codex(CodexModel::High) => Some(ReviewModelArg::CodexHigh),
            ReviewSelection::Codex(CodexModel::Mini) => Some(ReviewModelArg::CodexMini),
            ReviewSelection::Claude(ClaudeModel::Opus) => Some(ReviewModelArg::ClaudeOpus),
            ReviewSelection::Claude(ClaudeModel::Sonnet) => None,
            ReviewSelection::Opencode { .. } => None,
            ReviewSelection::Rise { .. } => None,
            ReviewSelection::Kimi { .. } => None,
            ReviewSelection::OpenRouter { .. } => None,
        }
    }

    fn model_label(&self) -> String {
        match self {
            ReviewSelection::Codex(model) => model.as_codex_arg().to_string(),
            ReviewSelection::Claude(model) => model.as_claude_arg().to_string(),
            ReviewSelection::Opencode { model } => model.clone(),
            ReviewSelection::Rise { model } => format!("rise:{}", model),
            ReviewSelection::Kimi { model } => match model.as_deref() {
                Some(model) if !model.trim().is_empty() => format!("kimi:{}", model),
                _ => "kimi".to_string(),
            },
            ReviewSelection::OpenRouter { model } => openrouter_model_label(model),
        }
    }
}

fn review_tool_label(selection: &ReviewSelection) -> &'static str {
    match selection {
        ReviewSelection::Claude(_) => "Claude",
        ReviewSelection::Codex(_) => "Codex",
        ReviewSelection::Opencode { .. } => "opencode",
        ReviewSelection::OpenRouter { .. } => "OpenRouter",
        ReviewSelection::Rise { .. } => "Rise AI",
        ReviewSelection::Kimi { .. } => "Kimi",
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

        // Check for .env files, but allow .env.example and .env.sample (safe templates)
        if file_name.starts_with(".env") {
            if file_name.ends_with(".example") || file_name.ends_with(".sample") {
                continue;
            }
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

/// Warn about secrets found in diff and optionally abort.
fn warn_secrets_in_diff(
    repo_root: &Path,
    findings: &[(String, usize, String, String)],
) -> Result<()> {
    if findings.is_empty() {
        return Ok(());
    }

    if env::var("FLOW_ALLOW_SECRET_COMMIT").ok().as_deref() == Some("1") {
        println!(
            "\n⚠️  Warning: Potential secrets detected but FLOW_ALLOW_SECRET_COMMIT=1, continuing..."
        );
        return Ok(());
    }

    println!();
    print_secret_findings("🔐 Potential secrets detected in staged changes:", findings);
    println!();
    println!("If these are false positives (examples, placeholders, tests), you can:");
    println!("   - Set FLOW_ALLOW_SECRET_COMMIT=1 to override for this commit");
    println!(
        "   - Mark the line with '# flow:secret:ignore' (or add it on the line above to ignore the next line)"
    );
    println!("   - Use placeholder values like 'xxx' for example secrets");
    println!("   - Re-stage files if you recently edited them: git add <file>");
    println!();

    let mut unstaged_files: Vec<&str> = Vec::new();
    for (file, _, _, _) in findings {
        if has_unstaged_changes(repo_root, file) {
            unstaged_files.push(file);
        }
    }

    if !unstaged_files.is_empty() {
        println!("ℹ️  Staged content differs from working tree for:");
        for file in &unstaged_files {
            println!("   - {}", file);
        }
        println!("   Re-run: git add <file> to update the staged diff.");
        println!();
    }

    let agent_name =
        env::var("FLOW_FIX_COMMIT_AGENT").unwrap_or_else(|_| "fix-f-commit".to_string());
    let agent_enabled = agent_name.trim().to_lowercase() != "off";
    let hive_available = which::which("hive").is_ok();
    let ai_available = which::which("ai").is_ok();
    let interactive = io::stdin().is_terminal();
    let mut current_findings = findings.to_vec();

    let rescan_after_fix = |findings: &mut Vec<(String, usize, String, String)>| -> Result<()> {
        git_run_in(repo_root, &["add", "."])?;
        ensure_no_internal_staged(repo_root)?;
        ensure_no_unwanted_staged(repo_root)?;
        gitignore_policy::enforce_staged_policy(repo_root)?;
        *findings = scan_diff_for_secrets(repo_root);
        Ok(())
    };

    if interactive && agent_enabled && hive_available {
        let task = build_fix_f_commit_task(&current_findings);
        println!("Running fix-f-commit agent (hive)...");
        if let Err(err) = run_fix_f_commit_agent(repo_root, &agent_name, &task) {
            eprintln!("⚠ Failed to run fix-f-commit agent: {err}");
            eprintln!(
                "  Create the agent at ~/.config/flow/agents/fix-f-commit.md or ~/.hive/agents/fix-f-commit/spec.md"
            );
            eprintln!();
        }
        rescan_after_fix(&mut current_findings)?;
        if current_findings.is_empty() {
            if prompt_yes_no_default_yes(
                "Secret scan is clean after auto-fix. Continue with commit?",
            )? {
                return Ok(());
            }
            bail!("Commit aborted after auto-fix. Review changes and retry.");
        }
    } else if !agent_enabled {
        eprintln!("ℹ️  fix-f-commit agent disabled via FLOW_FIX_COMMIT_AGENT=off");
    } else if !hive_available {
        eprintln!("ℹ️  hive not found; skipping fix-f-commit agent");
    }

    if interactive && !current_findings.is_empty() && ai_available {
        if prompt_yes_no_default_yes("Run auto-fix with ai?")? {
            let task = build_fix_f_commit_task(&current_findings);
            println!("Running auto-fix with ai...");
            if let Err(err) = run_fix_f_commit_ai(repo_root, &task) {
                eprintln!("⚠ Failed to run ai auto-fix: {err}");
            }
            rescan_after_fix(&mut current_findings)?;
            if current_findings.is_empty() {
                if prompt_yes_no_default_yes(
                    "Secret scan is clean after auto-fix. Continue with commit?",
                )? {
                    return Ok(());
                }
                bail!("Commit aborted after auto-fix. Review changes and retry.");
            }
        }
    }

    if current_findings != findings {
        print_secret_findings(
            "🔐 Potential secrets still detected in staged changes:",
            &current_findings,
        );
        println!();
    }

    let task = build_fix_f_commit_task(&current_findings);
    if !task.trim().is_empty() {
        eprintln!("Suggested prompt (copy/paste into your model):");
        eprintln!("────────────────────────────────────────");
        eprintln!("{}", task);
        eprintln!("────────────────────────────────────────");
    }

    bail!("Refusing to commit potential secrets. Review the findings above.")
}

fn should_run_sync_for_secret_fixes(repo_root: &Path) -> Result<bool> {
    if !io::stdin().is_terminal() {
        return Ok(false);
    }
    if env::var("FLOW_ALLOW_SECRET_COMMIT").ok().as_deref() == Some("1") {
        return Ok(false);
    }

    let agent_name =
        env::var("FLOW_FIX_COMMIT_AGENT").unwrap_or_else(|_| "fix-f-commit".to_string());
    let hive_enabled = agent_name.trim().to_lowercase() != "off" && which::which("hive").is_ok();
    let ai_available = which::which("ai").is_ok();
    if !hive_enabled && !ai_available {
        return Ok(false);
    }

    git_run(&["add", "."])?;
    ensure_no_internal_staged(repo_root)?;
    ensure_no_unwanted_staged(repo_root)?;
    gitignore_policy::enforce_staged_policy(repo_root)?;

    Ok(!scan_diff_for_secrets(repo_root).is_empty())
}

fn run_fix_f_commit_agent(repo_root: &Path, agent: &str, task: &str) -> Result<()> {
    if which::which("hive").is_err() {
        bail!("hive not found in PATH");
    }

    let mut cmd = Command::new("hive");
    cmd.args(["agent", &agent, task])
        .current_dir(repo_root)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .envs(resolve_hive_env());

    let status = cmd.status().context("failed to run hive agent")?;

    if !status.success() {
        bail!("hive agent '{}' failed", agent);
    }

    Ok(())
}

fn run_fix_f_commit_ai(repo_root: &Path, task: &str) -> Result<()> {
    if which::which("ai").is_err() {
        bail!("ai not found in PATH");
    }

    let status = Command::new("ai")
        .args(["--prompt", task])
        .current_dir(repo_root)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to run ai")?;

    if !status.success() {
        bail!("ai auto-fix failed");
    }

    Ok(())
}

fn build_fix_f_commit_task(findings: &[(String, usize, String, String)]) -> String {
    let mut summary = String::new();
    for (file, line, pattern, matched) in findings {
        summary.push_str(&format!(
            "- {}:{} — {} ({})\n",
            file, line, pattern, matched
        ));
    }

    let task = format!(
        "Fix f commit secret detection.\n\n\
Findings:\n{summary}\n\
Please remove or mask real secrets, replace with placeholders if needed, \
and update .gitignore or docs/examples so the commit passes the secret scan. \
If the match is a false positive, prefer marking the flagged line with `flow:secret:ignore` (for example: `# flow:secret:ignore`). \
If you must keep the pattern but want it to pass the scanner, use 'xxx' placeholders.\n\
After fixing, restage changes."
    );

    sanitize_hive_task(&task)
}

fn print_secret_findings(header: &str, findings: &[(String, usize, String, String)]) {
    println!("{}", header);
    for (file, line, pattern, matched) in findings {
        println!("   {}:{} - {} ({})", file, line, pattern, matched);
    }
}

fn has_unstaged_changes(repo_root: &Path, file: &str) -> bool {
    let output = Command::new("git")
        .args(["diff", "--name-only", "--", file])
        .current_dir(repo_root)
        .output();

    let Ok(output) = output else {
        return false;
    };
    if !output.status.success() {
        return false;
    }

    let output = String::from_utf8_lossy(&output.stdout);
    output.lines().any(|line| line.trim() == file)
}

fn sanitize_hive_task(task: &str) -> String {
    let mut cleaned = String::with_capacity(task.len());
    for ch in task.chars() {
        match ch {
            '"' => cleaned.push('\''),
            '\n' | '\r' | '\t' => cleaned.push(' '),
            _ => cleaned.push(ch),
        }
    }
    cleaned
}

fn resolve_hive_env() -> Vec<(String, String)> {
    let mut vars = Vec::new();

    if std::env::var("CEREBRAS_API_KEY")
        .map(|v| v.trim().is_empty())
        .unwrap_or(true)
    {
        if is_local_env_backend() {
            if let Ok(store) =
                crate::env::fetch_personal_env_vars(&["CEREBRAS_API_KEY".to_string()])
            {
                if let Some(value) = store.get("CEREBRAS_API_KEY") {
                    if !value.trim().is_empty() {
                        vars.push(("CEREBRAS_API_KEY".to_string(), value.to_string()));
                    }
                }
            }
        }
    }

    vars
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

    println!(
        "⚠️  Warning: Files with large diffs ({}+ lines):",
        LARGE_DIFF_THRESHOLD
    );
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

/// Check TypeScript config for review settings first, then fall back to commit settings.
pub fn resolve_review_selection_from_config() -> Option<ReviewSelection> {
    let ts_config = config::load_ts_config()?;
    let flow_config = ts_config.flow?;

    // Check review config first (takes precedence)
    let (tool, model) = if let Some(ref review_config) = flow_config.review {
        if let Some(ref tool) = review_config.tool {
            (tool.as_str(), review_config.model.clone())
        } else if let Some(ref commit_config) = flow_config.commit {
            // Fall back to commit config
            (commit_config.tool.as_deref()?, commit_config.model.clone())
        } else {
            return None;
        }
    } else if let Some(ref commit_config) = flow_config.commit {
        // No review config, use commit config
        (commit_config.tool.as_deref()?, commit_config.model.clone())
    } else {
        return None;
    };

    match tool {
        "opencode" => {
            let model = model.unwrap_or_else(|| DEFAULT_OPENCODE_MODEL.to_string());
            Some(ReviewSelection::Opencode { model })
        }
        "openrouter" => {
            let model = model.unwrap_or_else(|| DEFAULT_OPENROUTER_REVIEW_MODEL.to_string());
            Some(ReviewSelection::OpenRouter { model })
        }
        "rise" => {
            let model = model.unwrap_or_else(|| DEFAULT_RISE_MODEL.to_string());
            Some(ReviewSelection::Rise { model })
        }
        "glm5" | "glm-5" | "glm" => {
            let model = model.unwrap_or_else(|| DEFAULT_GLM5_RISE_MODEL.to_string());
            Some(ReviewSelection::Rise { model })
        }
        "kimi" => Some(ReviewSelection::Kimi { model }),
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

fn parse_boolish(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn load_ts_commit_config() -> Option<config::TsCommitConfig> {
    config::load_ts_config()
        .and_then(|cfg| cfg.flow)
        .and_then(|flow| flow.commit)
}

fn load_local_commit_config(repo_root: &Path) -> Option<config::CommitConfig> {
    let local = repo_root.join("flow.toml");
    if !local.exists() {
        return None;
    }
    config::load(&local).ok().and_then(|cfg| cfg.commit)
}

fn load_global_commit_config() -> Option<config::CommitConfig> {
    let global = config::default_config_path();
    if !global.exists() {
        return None;
    }
    config::load(&global).ok().and_then(|cfg| cfg.commit)
}

pub fn commit_quick_default_enabled() -> bool {
    if let Ok(value) = env::var("FLOW_COMMIT_QUICK_DEFAULT") {
        if let Some(parsed) = parse_boolish(&value) {
            return parsed;
        }
    }

    if let Some(ts) = load_ts_commit_config() {
        if let Some(enabled) = ts.quick_default {
            return enabled;
        }
    }

    let repo_root = git_root_or_cwd();
    if let Some(local) = load_local_commit_config(&repo_root) {
        if let Some(enabled) = local.quick_default {
            return enabled;
        }
    }

    if let Some(global) = load_global_commit_config() {
        if let Some(enabled) = global.quick_default {
            return enabled;
        }
    }

    true
}

fn commit_review_fail_open_enabled(repo_root: &Path) -> bool {
    if let Ok(value) = env::var("FLOW_COMMIT_REVIEW_FAIL_OPEN") {
        if let Some(parsed) = parse_boolish(&value) {
            return parsed;
        }
    }

    if let Some(ts) = load_ts_commit_config() {
        if let Some(enabled) = ts.review_fail_open {
            return enabled;
        }
    }
    if let Some(local) = load_local_commit_config(repo_root) {
        if let Some(enabled) = local.review_fail_open {
            return enabled;
        }
    }
    if let Some(global) = load_global_commit_config() {
        if let Some(enabled) = global.review_fail_open {
            return enabled;
        }
    }

    true
}

fn commit_message_fail_open_enabled(repo_root: &Path) -> bool {
    if let Ok(value) = env::var("FLOW_COMMIT_MESSAGE_FAIL_OPEN") {
        if let Some(parsed) = parse_boolish(&value) {
            return parsed;
        }
    }

    if let Some(ts) = load_ts_commit_config() {
        if let Some(enabled) = ts.message_fail_open {
            return enabled;
        }
    }
    if let Some(local) = load_local_commit_config(repo_root) {
        if let Some(enabled) = local.message_fail_open {
            return enabled;
        }
    }
    if let Some(global) = load_global_commit_config() {
        if let Some(enabled) = global.message_fail_open {
            return enabled;
        }
    }

    true
}

fn parse_review_selection_spec(spec: &str) -> Option<ReviewSelection> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower == "codex" || lower == "codex-high" {
        return Some(ReviewSelection::Codex(CodexModel::High));
    }
    if lower == "codex-mini" || lower == "codex:mini" || lower == "codex-mini-review" {
        return Some(ReviewSelection::Codex(CodexModel::Mini));
    }
    if lower == "claude" || lower == "claude-sonnet" {
        return Some(ReviewSelection::Claude(ClaudeModel::Sonnet));
    }
    if lower == "claude-opus" || lower == "claude:opus" {
        return Some(ReviewSelection::Claude(ClaudeModel::Opus));
    }
    if lower == "kimi" {
        return Some(ReviewSelection::Kimi { model: None });
    }
    if let Some(model) = trimmed
        .strip_prefix("openrouter:")
        .or_else(|| trimmed.strip_prefix("openrouter/"))
    {
        let model = if model.trim().is_empty() {
            DEFAULT_OPENROUTER_REVIEW_MODEL.to_string()
        } else {
            model.trim().to_string()
        };
        return Some(ReviewSelection::OpenRouter { model });
    }
    if lower == "openrouter" {
        return Some(ReviewSelection::OpenRouter {
            model: DEFAULT_OPENROUTER_REVIEW_MODEL.to_string(),
        });
    }
    if let Some(model) = trimmed
        .strip_prefix("rise:")
        .or_else(|| trimmed.strip_prefix("rise/"))
    {
        let model = if model.trim().is_empty() {
            DEFAULT_RISE_MODEL.to_string()
        } else {
            model.trim().to_string()
        };
        return Some(ReviewSelection::Rise { model });
    }
    if lower == "rise" {
        return Some(ReviewSelection::Rise {
            model: DEFAULT_RISE_MODEL.to_string(),
        });
    }
    if lower == "glm5" || lower == "glm-5" || lower == "glm" {
        return Some(ReviewSelection::Rise {
            model: DEFAULT_GLM5_RISE_MODEL.to_string(),
        });
    }
    if let Some(model) = trimmed
        .strip_prefix("glm5:")
        .or_else(|| trimmed.strip_prefix("glm5/"))
        .or_else(|| trimmed.strip_prefix("glm-5:"))
        .or_else(|| trimmed.strip_prefix("glm-5/"))
    {
        let model = if model.trim().is_empty() {
            DEFAULT_GLM5_RISE_MODEL.to_string()
        } else {
            model.trim().to_string()
        };
        return Some(ReviewSelection::Rise { model });
    }
    if let Some(model) = trimmed
        .strip_prefix("opencode:")
        .or_else(|| trimmed.strip_prefix("opencode/"))
    {
        let model = if model.trim().is_empty() {
            DEFAULT_OPENCODE_MODEL.to_string()
        } else {
            model.trim().to_string()
        };
        return Some(ReviewSelection::Opencode { model });
    }
    if lower == "opencode" {
        return Some(ReviewSelection::Opencode {
            model: DEFAULT_OPENCODE_MODEL.to_string(),
        });
    }
    None
}

fn commit_review_fallback_specs(repo_root: &Path) -> Vec<String> {
    if let Ok(raw) = env::var("FLOW_COMMIT_REVIEW_FALLBACKS") {
        let parsed = raw
            .split([',', '\n'])
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .collect::<Vec<_>>();
        if !parsed.is_empty() {
            return parsed;
        }
    }

    if let Some(ts) = load_ts_commit_config() {
        if let Some(v) = ts.review_fallbacks {
            let parsed = v
                .into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>();
            if !parsed.is_empty() {
                return parsed;
            }
        }
    }
    if let Some(local) = load_local_commit_config(repo_root) {
        if let Some(v) = local.review_fallbacks {
            let parsed = v
                .into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>();
            if !parsed.is_empty() {
                return parsed;
            }
        }
    }
    if let Some(global) = load_global_commit_config() {
        if let Some(v) = global.review_fallbacks {
            let parsed = v
                .into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>();
            if !parsed.is_empty() {
                return parsed;
            }
        }
    }

    vec![
        "openrouter".to_string(),
        "claude".to_string(),
        "codex-high".to_string(),
    ]
}

fn review_attempts_for_selection(
    repo_root: &Path,
    primary: &ReviewSelection,
    prefer_codex_over_openrouter: bool,
) -> Vec<ReviewSelection> {
    let mut attempts: Vec<ReviewSelection> = Vec::new();
    if prefer_codex_over_openrouter {
        attempts.push(ReviewSelection::Codex(CodexModel::High));
    }
    attempts.push(primary.clone());

    for spec in commit_review_fallback_specs(repo_root) {
        if let Some(selection) = parse_review_selection_spec(&spec) {
            attempts.push(selection);
        }
    }

    let mut seen = HashSet::new();
    let mut deduped = Vec::new();
    for attempt in attempts {
        let key = attempt.model_label();
        if seen.insert(key) {
            deduped.push(attempt);
        }
    }
    deduped
}

#[derive(Debug, Clone)]
enum CommitMessageSelection {
    Kimi { model: Option<String> },
    Claude,
    Opencode { model: String },
    OpenRouter { model: String },
    Rise { model: String },
    Remote,
    OpenAi,
    Heuristic,
}

impl CommitMessageSelection {
    fn key(&self) -> String {
        match self {
            CommitMessageSelection::Kimi { model } => match model.as_deref() {
                Some(model) if !model.trim().is_empty() => format!("kimi:{}", model.trim()),
                _ => "kimi".to_string(),
            },
            CommitMessageSelection::Claude => "claude".to_string(),
            CommitMessageSelection::Opencode { model } => format!("opencode:{}", model.trim()),
            CommitMessageSelection::OpenRouter { model } => {
                format!("openrouter:{}", openrouter_model_id(model))
            }
            CommitMessageSelection::Rise { model } => format!("rise:{}", model.trim()),
            CommitMessageSelection::Remote => "remote".to_string(),
            CommitMessageSelection::OpenAi => "openai".to_string(),
            CommitMessageSelection::Heuristic => "heuristic".to_string(),
        }
    }

    fn label(&self) -> String {
        match self {
            CommitMessageSelection::Kimi { .. } => "Kimi".to_string(),
            CommitMessageSelection::Claude => "Claude".to_string(),
            CommitMessageSelection::Opencode { .. } => "opencode".to_string(),
            CommitMessageSelection::OpenRouter { .. } => "OpenRouter".to_string(),
            CommitMessageSelection::Rise { .. } => "Rise".to_string(),
            CommitMessageSelection::Remote => "myflow".to_string(),
            CommitMessageSelection::OpenAi => "OpenAI".to_string(),
            CommitMessageSelection::Heuristic => "deterministic fallback".to_string(),
        }
    }
}

fn parse_commit_message_selection_spec(spec: &str) -> Option<CommitMessageSelection> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return None;
    }

    let lower = trimmed.to_ascii_lowercase();
    if lower == "remote" || lower == "myflow" || lower == "flow" {
        return Some(CommitMessageSelection::Remote);
    }
    if lower == "openai" {
        return Some(CommitMessageSelection::OpenAi);
    }
    if lower == "heuristic" || lower == "fallback" || lower == "local" {
        return Some(CommitMessageSelection::Heuristic);
    }
    if lower == "claude" {
        return Some(CommitMessageSelection::Claude);
    }
    if lower == "kimi" {
        return Some(CommitMessageSelection::Kimi { model: None });
    }

    if let Some(model) = trimmed
        .strip_prefix("kimi:")
        .or_else(|| trimmed.strip_prefix("kimi/"))
    {
        let model = model.trim();
        return Some(CommitMessageSelection::Kimi {
            model: if model.is_empty() {
                None
            } else {
                Some(model.to_string())
            },
        });
    }

    if let Some(model) = trimmed
        .strip_prefix("openrouter:")
        .or_else(|| trimmed.strip_prefix("openrouter/"))
    {
        let model = if model.trim().is_empty() {
            DEFAULT_OPENROUTER_REVIEW_MODEL.to_string()
        } else {
            model.trim().to_string()
        };
        return Some(CommitMessageSelection::OpenRouter { model });
    }
    if lower == "openrouter" {
        return Some(CommitMessageSelection::OpenRouter {
            model: DEFAULT_OPENROUTER_REVIEW_MODEL.to_string(),
        });
    }

    if let Some(model) = trimmed
        .strip_prefix("opencode:")
        .or_else(|| trimmed.strip_prefix("opencode/"))
    {
        let model = if model.trim().is_empty() {
            DEFAULT_OPENCODE_MODEL.to_string()
        } else {
            model.trim().to_string()
        };
        return Some(CommitMessageSelection::Opencode { model });
    }
    if lower == "opencode" {
        return Some(CommitMessageSelection::Opencode {
            model: DEFAULT_OPENCODE_MODEL.to_string(),
        });
    }

    if let Some(model) = trimmed
        .strip_prefix("rise:")
        .or_else(|| trimmed.strip_prefix("rise/"))
    {
        let model = if model.trim().is_empty() {
            DEFAULT_RISE_MODEL.to_string()
        } else {
            model.trim().to_string()
        };
        return Some(CommitMessageSelection::Rise { model });
    }
    if lower == "rise" {
        return Some(CommitMessageSelection::Rise {
            model: DEFAULT_RISE_MODEL.to_string(),
        });
    }
    if lower == "glm5" || lower == "glm-5" || lower == "glm" {
        return Some(CommitMessageSelection::Rise {
            model: DEFAULT_GLM5_RISE_MODEL.to_string(),
        });
    }
    if let Some(model) = trimmed
        .strip_prefix("glm5:")
        .or_else(|| trimmed.strip_prefix("glm5/"))
        .or_else(|| trimmed.strip_prefix("glm-5:"))
        .or_else(|| trimmed.strip_prefix("glm-5/"))
    {
        let model = if model.trim().is_empty() {
            DEFAULT_GLM5_RISE_MODEL.to_string()
        } else {
            model.trim().to_string()
        };
        return Some(CommitMessageSelection::Rise { model });
    }

    None
}

fn parse_commit_message_selection_with_model(
    tool: &str,
    model: Option<String>,
) -> Option<CommitMessageSelection> {
    let tool_trimmed = tool.trim();
    if tool_trimmed.is_empty() {
        return None;
    }
    let model_trimmed = model.and_then(|m| {
        let trimmed = m.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    });

    match tool_trimmed.to_ascii_lowercase().as_str() {
        "kimi" => Some(CommitMessageSelection::Kimi {
            model: model_trimmed,
        }),
        "claude" => Some(CommitMessageSelection::Claude),
        "openrouter" => Some(CommitMessageSelection::OpenRouter {
            model: model_trimmed.unwrap_or_else(|| DEFAULT_OPENROUTER_REVIEW_MODEL.to_string()),
        }),
        "opencode" => Some(CommitMessageSelection::Opencode {
            model: model_trimmed.unwrap_or_else(|| DEFAULT_OPENCODE_MODEL.to_string()),
        }),
        "rise" => Some(CommitMessageSelection::Rise {
            model: model_trimmed.unwrap_or_else(|| DEFAULT_RISE_MODEL.to_string()),
        }),
        "glm5" | "glm-5" | "glm" => Some(CommitMessageSelection::Rise {
            model: model_trimmed.unwrap_or_else(|| DEFAULT_GLM5_RISE_MODEL.to_string()),
        }),
        "remote" | "myflow" | "flow" => Some(CommitMessageSelection::Remote),
        "openai" => Some(CommitMessageSelection::OpenAi),
        "heuristic" | "fallback" | "local" => Some(CommitMessageSelection::Heuristic),
        _ => parse_commit_message_selection_spec(tool_trimmed),
    }
}

fn commit_message_fallback_specs(repo_root: &Path) -> Vec<String> {
    if let Ok(raw) = env::var("FLOW_COMMIT_MESSAGE_FALLBACKS") {
        let parsed = raw
            .split([',', '\n'])
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .collect::<Vec<_>>();
        if !parsed.is_empty() {
            return parsed;
        }
    }

    if let Some(ts) = load_ts_commit_config() {
        if let Some(v) = ts.message_fallbacks {
            let parsed = v
                .into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>();
            if !parsed.is_empty() {
                return parsed;
            }
        }
    }
    if let Some(local) = load_local_commit_config(repo_root) {
        if let Some(v) = local.message_fallbacks {
            let parsed = v
                .into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>();
            if !parsed.is_empty() {
                return parsed;
            }
        }
    }
    if let Some(global) = load_global_commit_config() {
        if let Some(v) = global.message_fallbacks {
            let parsed = v
                .into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>();
            if !parsed.is_empty() {
                return parsed;
            }
        }
    }

    vec![
        "remote".to_string(),
        "openai".to_string(),
        "openrouter".to_string(),
    ]
}

fn review_selection_to_message_selection(
    review_selection: &ReviewSelection,
) -> Option<CommitMessageSelection> {
    match review_selection {
        ReviewSelection::Claude(_) => Some(CommitMessageSelection::Claude),
        ReviewSelection::Opencode { model } => Some(CommitMessageSelection::Opencode {
            model: model.clone(),
        }),
        ReviewSelection::OpenRouter { model } => Some(CommitMessageSelection::OpenRouter {
            model: model.clone(),
        }),
        ReviewSelection::Rise { model } => Some(CommitMessageSelection::Rise {
            model: model.clone(),
        }),
        ReviewSelection::Kimi { model } => Some(CommitMessageSelection::Kimi {
            model: model.clone(),
        }),
        ReviewSelection::Codex(_) => None,
    }
}

fn commit_message_attempts(
    repo_root: &Path,
    review_selection: Option<&ReviewSelection>,
    override_selection: Option<&CommitMessageSelection>,
) -> Vec<CommitMessageSelection> {
    let mut attempts: Vec<CommitMessageSelection> = Vec::new();

    if let Some(selection) = override_selection {
        attempts.push(selection.clone());
    } else if let Some(review_selection) = review_selection {
        if let Some(selection) = review_selection_to_message_selection(review_selection) {
            attempts.push(selection);
        }
    }

    for spec in commit_message_fallback_specs(repo_root) {
        if let Some(selection) = parse_commit_message_selection_spec(&spec) {
            attempts.push(selection);
        }
    }

    let mut seen = HashSet::new();
    let mut deduped = Vec::new();
    for attempt in attempts {
        let key = attempt.key();
        if seen.insert(key) {
            deduped.push(attempt);
        }
    }
    deduped
}

#[derive(Debug, Deserialize)]
struct ReviewJson {
    issues_found: bool,
    #[serde(default)]
    issues: Vec<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    future_tasks: Vec<String>,
    #[serde(default)]
    quality: Option<QualityResult>,
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

#[derive(Debug, Deserialize)]
struct RemoteCommitMessageResponse {
    message: String,
}

#[derive(Debug)]
struct ReviewResult {
    issues_found: bool,
    issues: Vec<String>,
    summary: Option<String>,
    future_tasks: Vec<String>,
    timed_out: bool,
    quality: Option<QualityResult>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub(crate) struct QualityResult {
    pub(crate) features_touched: Vec<FeatureTouched>,
    pub(crate) new_features: Vec<NewFeature>,
    pub(crate) test_coverage: String,
    pub(crate) doc_coverage: String,
    pub(crate) gate_pass: bool,
    pub(crate) gate_failures: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub(crate) struct FeatureTouched {
    pub(crate) name: String,
    pub(crate) action: String,
    pub(crate) description: String,
    pub(crate) files_changed: Vec<String>,
    pub(crate) has_tests: bool,
    pub(crate) test_files: Vec<String>,
    pub(crate) doc_current: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub(crate) struct NewFeature {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) files: Vec<String>,
    pub(crate) doc_content: String,
}

#[derive(Debug)]
struct StagedSnapshot {
    patch_path: Option<std::path::PathBuf>,
}

#[derive(Debug, Serialize)]
struct UnhashCommitMetadata {
    repo: String,
    repo_root: String,
    branch: String,
    created_at: String,
    commit_message: String,
    author_message: Option<String>,
    include_context: bool,
    context_chars: Option<usize>,
    review_model: Option<String>,
    review_instructions: Option<String>,
    review_issues: Vec<String>,
    review_summary: Option<String>,
    review_future_tasks: Vec<String>,
    review_timed_out: bool,
    gitedit_session_hash: Option<String>,
    session_count: usize,
}

#[derive(Debug, Serialize)]
struct UnhashReviewPayload {
    issues_found: bool,
    issues: Vec<String>,
    summary: Option<String>,
    future_tasks: Vec<String>,
    timed_out: bool,
    model: Option<String>,
    reviewer: Option<String>,
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

fn parse_rise_output(text: &str) -> Result<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        bail!("Rise daemon returned empty response");
    }

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if let Some(err) = value.get("error") {
            let code = err
                .get("code")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let message = err
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            bail!("Rise daemon error: {} ({})", message, code);
        }
    }

    if let Ok(response) = serde_json::from_str::<ChatResponse>(text) {
        if let Some(output) = response
            .choices
            .first()
            .and_then(|c| c.message.as_ref())
            .map(|m| m.content.clone())
        {
            if !output.trim().is_empty() {
                return Ok(output);
            }
        }
    }

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
        if let Some(content) = value
            .get("assistant")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
        {
            if !content.trim().is_empty() {
                return Ok(content);
            }
        }

        if let Some(content) = value
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
        {
            if !content.trim().is_empty() {
                return Ok(content);
            }
        }
    }

    Ok(trimmed.to_string())
}

fn is_rise_auth_error(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed.contains("Authorization Token Missing")
        || trimmed.contains("\"code\":\"1001\"")
        || trimmed.contains("\"code\":1001")
}

fn rise_provider_from_model(model: &str) -> Option<String> {
    let trimmed = model.trim();
    if trimmed.is_empty() {
        return None;
    }
    let stripped = trimmed.strip_prefix("rise:").unwrap_or(trimmed);
    let provider = stripped.split(':').next().unwrap_or("").trim();
    if provider.is_empty() {
        return None;
    }
    Some(provider.to_ascii_lowercase())
}

fn rise_provider_env_key(provider: &str) -> Option<&'static str> {
    match provider {
        "zai" => Some("ZAI_API_KEY"),
        "xai" => Some("XAI_API_KEY"),
        "cerebras" => Some("CEREBRAS_API_KEY"),
        "deepseek" => Some("DEEPSEEK_API_KEY"),
        "openai" => Some("OPENAI_API_KEY"),
        _ => None,
    }
}

fn is_local_env_backend() -> bool {
    if let Some(backend) = crate::config::preferred_env_backend() {
        return backend.eq_ignore_ascii_case("local");
    }

    match std::env::var("FLOW_ENV_BACKEND")
        .ok()
        .map(|v| v.to_ascii_lowercase())
        .as_deref()
    {
        Some("local") => true,
        Some("cloud") | Some("remote") => false,
        _ => std::env::var("FLOW_ENV_LOCAL")
            .ok()
            .map(|v| {
                let v = v.to_ascii_lowercase();
                v == "1" || v == "true" || v == "yes"
            })
            .unwrap_or(false),
    }
}

fn rise_auth_error_message(model: &str) -> String {
    let Some(provider) = rise_provider_from_model(model) else {
        return "Rise daemon error: Authorization Token Missing (1001).".to_string();
    };
    let Some(env_key) = rise_provider_env_key(&provider) else {
        return format!(
            "Rise daemon error: Authorization Token Missing (1001). Missing auth for provider '{}'.",
            provider
        );
    };

    let has_env = std::env::var(env_key)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    let has_store = if is_local_env_backend() {
        crate::env::fetch_personal_env_vars(&[env_key.to_string()])
            .ok()
            .and_then(|vars| vars.get(env_key).cloned())
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
    } else {
        false
    };

    let mut message = format!(
        "Rise daemon error: Authorization Token Missing (1001). Missing {} for provider '{}'.",
        env_key, provider
    );
    if has_store || has_env {
        message.push_str(" Restart the Rise daemon so it picks up the key.");
    } else {
        message.push_str(&format!(
            " Set it in Flow env store: f env set --personal {}=... then restart the Rise daemon.",
            env_key
        ));
    }
    message
}

fn rise_url() -> String {
    std::env::var("ZERG_AI_URL")
        .or_else(|_| std::env::var("FLOW_RISE_URL"))
        .or_else(|_| std::env::var("RISE_URL"))
        .unwrap_or_else(|_| "http://localhost:7654/v1/chat/completions".to_string())
}

fn rise_health_url(rise_url: &str) -> Option<String> {
    let trimmed = rise_url.trim_end_matches('/');
    let idx = trimmed.find("/v1/")?;
    Some(format!("{}/health", &trimmed[..idx]))
}

fn wait_for_rise_ready(client: &Client, rise_url: &str) {
    let Some(health_url) = rise_health_url(rise_url) else {
        return;
    };
    for _ in 0..12 {
        match client.get(&health_url).send() {
            Ok(resp) if resp.status().is_success() => return,
            _ => std::thread::sleep(Duration::from_millis(350)),
        }
    }
}

fn try_start_rise_daemon() -> Result<()> {
    let action = DaemonAction::Start {
        name: "rise".to_string(),
    };
    if supervisor::try_handle_daemon_action(&action, None)? {
        return Ok(());
    }
    daemon::start_daemon_with_path("rise", None)
}

fn try_restart_rise_daemon() -> Result<()> {
    let action = DaemonAction::Restart {
        name: "rise".to_string(),
    };
    if supervisor::try_handle_daemon_action(&action, None)? {
        return Ok(());
    }
    daemon::stop_daemon_with_path("rise", None).ok();
    daemon::start_daemon_with_path("rise", None)
}

fn send_rise_request_text(
    client: &Client,
    rise_url: &str,
    body: &ChatRequest,
    model: &str,
) -> Result<String> {
    let resp = send_rise_request(client, rise_url, body)?;
    if !resp.status().is_success() {
        let error_text = resp.text().unwrap_or_else(|_| "unknown error".to_string());
        if is_rise_auth_error(&error_text) {
            info!("Rise auth error; attempting daemon restart...");
            if let Err(err) = try_restart_rise_daemon() {
                bail!(
                    "{} (restart failed: {})",
                    rise_auth_error_message(model),
                    err
                );
            }
            std::thread::sleep(Duration::from_millis(500));
            wait_for_rise_ready(client, rise_url);
            let resp = send_rise_request(client, rise_url, body)?;
            if !resp.status().is_success() {
                let error_text = resp.text().unwrap_or_else(|_| "unknown error".to_string());
                bail!("Rise daemon error: {}", error_text);
            }
            let text = resp.text().context("failed to read Rise response")?;
            if is_rise_auth_error(&text) {
                bail!(rise_auth_error_message(model));
            }
            return Ok(text);
        }
        bail!("Rise daemon error: {}", error_text);
    }

    let text = resp.text().context("failed to read Rise response")?;
    if is_rise_auth_error(&text) {
        info!("Rise auth error; attempting daemon restart...");
        if let Err(err) = try_restart_rise_daemon() {
            bail!(
                "{} (restart failed: {})",
                rise_auth_error_message(model),
                err
            );
        }
        std::thread::sleep(Duration::from_millis(500));
        wait_for_rise_ready(client, rise_url);
        let resp = send_rise_request(client, rise_url, body)?;
        if !resp.status().is_success() {
            let error_text = resp.text().unwrap_or_else(|_| "unknown error".to_string());
            bail!("Rise daemon error: {}", error_text);
        }
        let text = resp.text().context("failed to read Rise response")?;
        if is_rise_auth_error(&text) {
            bail!(rise_auth_error_message(model));
        }
        return Ok(text);
    }

    Ok(text)
}

fn send_rise_request(
    client: &Client,
    rise_url: &str,
    body: &ChatRequest,
) -> Result<reqwest::blocking::Response> {
    match client.post(rise_url).json(body).send() {
        Ok(resp) => Ok(resp),
        Err(err) => {
            if err.is_connect() {
                info!("Rise daemon unreachable; attempting auto-start...");
                if let Err(start_err) = try_start_rise_daemon() {
                    return Err(err).with_context(|| {
                        format!(
                            "failed to reach Rise daemon at {}. Auto-start failed: {}",
                            rise_url, start_err
                        )
                    });
                }
                std::thread::sleep(Duration::from_millis(500));
                wait_for_rise_ready(client, rise_url);
                return client.post(rise_url).json(body).send().with_context(|| {
                    format!(
                        "failed to reach Rise daemon at {} after auto-start. Start with: f rise",
                        rise_url
                    )
                });
            }

            Err(err).with_context(|| {
                format!(
                    "failed to reach Rise daemon at {}. Start with: f rise",
                    rise_url
                )
            })
        }
    }
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
pub fn run(
    push: bool,
    queue: CommitQueueMode,
    include_unhash: bool,
    stage_paths: &[String],
) -> Result<()> {
    let _git_capture_cache_scope = GitCaptureCacheScope::begin();

    // Check if hub is running - if so, delegate
    if hub::hub_healthy(HUB_HOST, HUB_PORT) {
        ensure_git_repo()?;
        let repo_root = git_root_or_cwd();
        ensure_commit_setup(&repo_root)?;
        git_guard::ensure_clean_for_commit(&repo_root)?;
        if should_run_sync_for_secret_fixes(&repo_root)? {
            return run_sync(push, queue, include_unhash, stage_paths);
        }
        return delegate_to_hub(push, queue, include_unhash, stage_paths);
    }

    run_sync(push, queue, include_unhash, stage_paths)
}

fn save_commit_checkpoint_for_repo(repo_root: &Path) {
    let now = chrono::Utc::now().to_rfc3339();
    let (session_id, last_ts) =
        match ai::get_last_entry_timestamp_for_path(&repo_root.to_path_buf()) {
            Ok(Some((session_id, last_ts))) => (Some(session_id), Some(last_ts)),
            Ok(None) => (None, Some(now.clone())),
            Err(err) => {
                debug!(
                    "failed to resolve latest session timestamp for checkpoint: {}",
                    err
                );
                (None, Some(now.clone()))
            }
        };
    let checkpoint = ai::CommitCheckpoint {
        timestamp: now,
        session_id,
        last_entry_timestamp: last_ts,
    };
    if let Err(err) = ai::save_checkpoint(&repo_root.to_path_buf(), checkpoint) {
        debug!("failed to save commit checkpoint: {}", err);
    }
}

fn git_commit_timestamp_iso(repo_root: &Path, rev: &str) -> Option<String> {
    git_capture_in(repo_root, &["show", "-s", "--format=%cI", rev])
        .ok()
        .map(|ts| ts.trim().to_string())
        .filter(|ts| !ts.is_empty())
}

#[derive(Debug, Clone)]
struct MyflowSessionWindow {
    mode: String,
    since_ts: Option<String>,
    until_ts: Option<String>,
    collected_at: String,
}

impl MyflowSessionWindow {
    fn new(mode: &str, since_ts: Option<String>, until_ts: Option<String>) -> Self {
        Self {
            mode: mode.to_string(),
            since_ts,
            until_ts,
            collected_at: chrono::Utc::now().to_rfc3339(),
        }
    }
}

fn collect_sync_sessions_for_commit_with_window(
    repo_root: &Path,
) -> (Vec<ai::GitEditSessionData>, MyflowSessionWindow) {
    let until_ts = git_commit_timestamp_iso(repo_root, "HEAD");
    let since_ts = git_commit_timestamp_iso(repo_root, "HEAD~1");

    if until_ts.is_some() {
        match ai::get_sessions_for_gitedit_between(
            &repo_root.to_path_buf(),
            since_ts.as_deref(),
            until_ts.as_deref(),
        ) {
            Ok(sessions) => {
                return (
                    sessions,
                    MyflowSessionWindow::new("commit_window", since_ts, until_ts),
                );
            }
            Err(err) => {
                debug!(
                    "failed to collect AI sessions in commit timestamp window (since={:?}, until={:?}): {}",
                    since_ts, until_ts, err
                );
            }
        }
    }

    match ai::get_sessions_for_gitedit(&repo_root.to_path_buf()) {
        Ok(sessions) => (
            sessions,
            MyflowSessionWindow::new("checkpoint_fallback", since_ts, until_ts),
        ),
        Err(err) => {
            debug!(
                "failed to collect AI sessions using checkpoint fallback: {}",
                err
            );
            (
                Vec::new(),
                MyflowSessionWindow::new("checkpoint_fallback", since_ts, until_ts),
            )
        }
    }
}

fn collect_sync_sessions_for_pending_commit_with_window(
    repo_root: &Path,
) -> (Vec<ai::GitEditSessionData>, MyflowSessionWindow) {
    // commit-with-check calls this before creating the new commit; use HEAD as the lower
    // bound and include everything after it so current-cycle AI exchanges are not dropped.
    let since_ts = git_commit_timestamp_iso(repo_root, "HEAD");

    if since_ts.is_some() {
        match ai::get_sessions_for_gitedit_between(
            &repo_root.to_path_buf(),
            since_ts.as_deref(),
            None,
        ) {
            Ok(sessions) => {
                return (
                    sessions,
                    MyflowSessionWindow::new("pending_window", since_ts, None),
                );
            }
            Err(err) => {
                debug!(
                    "failed to collect AI sessions for pending commit window (since={:?}): {}",
                    since_ts, err
                );
            }
        }
    }

    match ai::get_sessions_for_gitedit(&repo_root.to_path_buf()) {
        Ok(sessions) => (
            sessions,
            MyflowSessionWindow::new("checkpoint_fallback", since_ts, None),
        ),
        Err(err) => {
            debug!(
                "failed to collect AI sessions using checkpoint fallback: {}",
                err
            );
            (
                Vec::new(),
                MyflowSessionWindow::new("checkpoint_fallback", since_ts, None),
            )
        }
    }
}

/// Run commit synchronously (called directly or by hub).
pub fn run_sync(
    push: bool,
    queue: CommitQueueMode,
    include_unhash: bool,
    stage_paths: &[String],
) -> Result<()> {
    let _git_capture_cache_scope = GitCaptureCacheScope::begin();

    let queue_enabled = queue.enabled;
    let push = push && !queue_enabled;
    info!(
        push = push,
        queue = queue_enabled,
        "starting commit workflow"
    );

    // Ensure we're in a git repo
    ensure_git_repo()?;
    debug!("verified git repository");
    let repo_root = git_root_or_cwd();
    warn_if_commit_invoked_from_subdir(&repo_root);
    ensure_commit_setup(&repo_root)?;
    git_guard::ensure_clean_for_commit(&repo_root)?;

    let commit_message_override = resolve_commit_message_override(&repo_root);
    debug!(
        has_override = commit_message_override.is_some(),
        "resolved commit message override"
    );

    stage_changes_for_commit(&repo_root, stage_paths)?;
    debug!(paths = stage_paths.len(), "staged changes");
    ensure_no_internal_staged(&repo_root)?;
    ensure_no_unwanted_staged(&repo_root)?;
    gitignore_policy::enforce_staged_policy(&repo_root)?;

    // Check for sensitive files before proceeding
    let sensitive_files = check_sensitive_files(&repo_root);
    warn_sensitive_files(&sensitive_files)?;

    // Scan diff content for hardcoded secrets
    let secret_findings = scan_diff_for_secrets(&repo_root);
    warn_secrets_in_diff(&repo_root, &secret_findings)?;

    // Check for files with large diffs
    let large_diffs = check_large_diffs(&repo_root);
    warn_large_diffs(&large_diffs)?;

    // Get diff
    let diff = git_capture_in(&repo_root, &["diff", "--cached"])?;
    if diff.trim().is_empty() {
        println!("\nnotify: No staged changes to commit");
        print_pending_queue_review_hint(&repo_root);
        bail!("No staged changes to commit");
    }
    debug!(diff_len = diff.len(), "got cached diff");

    // Get status
    let status = git_capture_in(&repo_root, &["status", "--short"]).unwrap_or_default();
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
    let mut message = generate_commit_message_with_fallbacks(
        &repo_root,
        None,
        commit_message_override.as_ref(),
        &diff_for_prompt,
        &status,
        truncated,
    )?;
    println!("done\n");
    debug!(message_len = message.len(), "got commit message");

    if include_unhash && unhash_capture_enabled() {
        if let Some(unhash_hash) = capture_unhash_bundle(
            &repo_root,
            &diff,
            Some(&status),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            &message,
            None,
            false,
        ) {
            message = format!("{}\n\nunhash.sh/{}", message, unhash_hash);
        }
    }

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

    log_commit_event_for_repo(&repo_root, &message, "commit", None, None);

    if queue_enabled {
        match queue_commit_for_review(&repo_root, &message, None, None, None, Vec::new()) {
            Ok(sha) => {
                print_queue_instructions(&repo_root, &sha);
                if queue.open_review {
                    open_review_in_rise(&repo_root, &sha);
                }
            }
            Err(err) => println!("⚠ Failed to queue commit for review: {}", err),
        }
    }

    // Push if requested
    let mut pushed = false;
    if push {
        let push_remote = config::preferred_git_remote_for_repo(&repo_root);
        let push_branch = git_capture(&["rev-parse", "--abbrev-ref", "HEAD"])
            .unwrap_or_else(|_| "HEAD".to_string())
            .trim()
            .to_string();
        print!("Pushing... ");
        io::stdout().flush()?;

        match git_push_try(&push_remote, &push_branch) {
            PushResult::Success => {
                println!("done");
                info!("pushed to remote");
                pushed = true;
            }
            PushResult::NoRemoteRepo => {
                println!("skipped (no remote repo)");
                info!("skipped push - remote repo does not exist");
            }
            PushResult::RemoteAhead => {
                // Push failed, likely remote has new commits
                println!("failed (remote ahead)");
                print!("Pulling with rebase... ");
                io::stdout().flush()?;

                match git_pull_rebase_try(&push_remote, &push_branch) {
                    Ok(_) => {
                        println!("done");
                        print!("Pushing... ");
                        io::stdout().flush()?;
                        git_push_run(&push_remote, &push_branch)?;
                        println!("done");
                        info!("pulled and pushed to remote");
                        pushed = true;
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

    // Record undo action
    record_undo_action(&repo_root, pushed, Some(&message));

    // Sync mirrors with AI sessions since previous checkpoint.
    let cwd = std::env::current_dir().unwrap_or_default();
    let sync_gitedit = gitedit_globally_enabled() && gitedit_mirror_enabled_for_commit(&repo_root);
    let sync_myflow = myflow_mirror_enabled(&repo_root);
    let (sync_sessions, sync_window) = if sync_gitedit || sync_myflow {
        let (sessions, window) = collect_sync_sessions_for_commit_with_window(&repo_root);
        (sessions, Some(window))
    } else {
        (Vec::new(), None)
    };
    if sync_gitedit {
        sync_to_gitedit(&cwd, "commit", &sync_sessions, None, None);
    }
    if sync_myflow {
        sync_to_myflow(
            &repo_root,
            "commit",
            &sync_sessions,
            sync_window.as_ref(),
            None,
            None,
        );
    }
    save_commit_checkpoint_for_repo(&repo_root);

    Ok(())
}

/// Run a fast commit with the provided message (no AI review).
pub fn run_fast(
    message: &str,
    push: bool,
    queue: CommitQueueMode,
    include_unhash: bool,
    stage_paths: &[String],
) -> Result<()> {
    let queue_enabled = queue.enabled;
    let push = push && !queue_enabled;
    ensure_git_repo()?;
    let repo_root = git_root_or_cwd();
    warn_if_commit_invoked_from_subdir(&repo_root);
    ensure_commit_setup(&repo_root)?;
    git_guard::ensure_clean_for_commit(&repo_root)?;

    // Run pre-commit fixers if configured (fast lint/format)
    if let Ok(fixed) = run_fixers(&repo_root) {
        if fixed {
            println!();
        }
    }

    stage_changes_for_commit(&repo_root, stage_paths)?;
    ensure_no_internal_staged(&repo_root)?;
    ensure_no_unwanted_staged(&repo_root)?;
    gitignore_policy::enforce_staged_policy(&repo_root)?;

    // Check for sensitive files before proceeding
    let cwd = std::env::current_dir()?;
    let sensitive_files = check_sensitive_files(&cwd);
    warn_sensitive_files(&sensitive_files)?;

    // Scan diff content for hardcoded secrets
    let secret_findings = scan_diff_for_secrets(&cwd);
    warn_secrets_in_diff(&repo_root, &secret_findings)?;

    // Ensure we actually have changes
    let diff = git_capture(&["diff", "--cached"])?;
    if diff.trim().is_empty() {
        println!("\nnotify: No staged changes to commit");
        print_pending_queue_review_hint(&repo_root);
        bail!("No staged changes to commit");
    }

    let status = git_capture(&["status", "--short"]).unwrap_or_default();
    let mut full_message = message.to_string();

    if include_unhash && unhash_capture_enabled() {
        if let Some(unhash_hash) = capture_unhash_bundle(
            &repo_root,
            &diff,
            Some(&status),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            &full_message,
            None,
            false,
        ) {
            full_message = format!("{}\n\nunhash.sh/{}", full_message, unhash_hash);
        }
    }

    ensure_no_unwanted_staged(&repo_root)?;
    gitignore_policy::enforce_staged_policy(&repo_root)?;

    // Commit
    git_run(&["commit", "-m", &full_message])?;
    println!("✓ Committed");

    log_commit_event_for_repo(&repo_root, &full_message, "commit", None, None);

    if queue_enabled {
        match queue_commit_for_review(&repo_root, &full_message, None, None, None, Vec::new()) {
            Ok(sha) => {
                print_queue_instructions(&repo_root, &sha);
                if queue.open_review {
                    open_review_in_rise(&repo_root, &sha);
                }
            }
            Err(err) => println!("⚠ Failed to queue commit for review: {}", err),
        }
    }

    // Push if requested
    let mut pushed = false;
    if push {
        let push_remote = config::preferred_git_remote_for_repo(&repo_root);
        let push_branch = git_capture(&["rev-parse", "--abbrev-ref", "HEAD"])
            .unwrap_or_else(|_| "HEAD".to_string())
            .trim()
            .to_string();
        print!("Pushing... ");
        io::stdout().flush()?;

        match git_push_try(&push_remote, &push_branch) {
            PushResult::Success => {
                println!("done");
                pushed = true;
            }
            PushResult::NoRemoteRepo => {
                println!("skipped (no remote repo)");
            }
            PushResult::RemoteAhead => {
                println!("failed (remote ahead)");
                print!("Pulling with rebase... ");
                io::stdout().flush()?;

                match git_pull_rebase_try(&push_remote, &push_branch) {
                    Ok(_) => {
                        println!("done");
                        print!("Pushing... ");
                        io::stdout().flush()?;
                        git_push_run(&push_remote, &push_branch)?;
                        println!("done");
                        pushed = true;
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

    // Record undo action
    record_undo_action(&repo_root, pushed, Some(&full_message));

    let sync_gitedit = gitedit_globally_enabled() && gitedit_mirror_enabled();
    let sync_myflow = myflow_mirror_enabled(&repo_root);
    let (sync_sessions, sync_window) = if sync_gitedit || sync_myflow {
        let (sessions, window) = collect_sync_sessions_for_commit_with_window(&repo_root);
        (sessions, Some(window))
    } else {
        (Vec::new(), None)
    };
    if sync_gitedit {
        sync_to_gitedit(&cwd, "commit", &sync_sessions, None, None);
    }
    if sync_myflow {
        sync_to_myflow(
            &repo_root,
            "commit",
            &sync_sessions,
            sync_window.as_ref(),
            None,
            None,
        );
    }
    save_commit_checkpoint_for_repo(&repo_root);

    Ok(())
}

/// Commit immediately and trigger Codex queue review in the background.
/// This gives a fast "commit now" UX while preserving deep review asynchronously.
pub fn run_quick_then_async_review(
    push: bool,
    queue: CommitQueueMode,
    include_unhash: bool,
    stage_paths: &[String],
    fast_message: Option<&str>,
) -> Result<()> {
    let explicit_no_queue = queue.override_flag == Some(false);

    if let Some(message) = fast_message {
        run_fast(message, push, queue, include_unhash, stage_paths)?;
    } else {
        run_sync(push, queue, include_unhash, stage_paths)?;
    }

    if explicit_no_queue {
        println!("Skipped async Codex review because --no-queue was requested.");
        return Ok(());
    }

    let repo_root = git_root_or_cwd();
    let commit_sha = git_capture_in(&repo_root, &["rev-parse", "--verify", "HEAD"])?
        .trim()
        .to_string();
    if commit_sha.is_empty() {
        bail!("failed to resolve HEAD commit after quick commit");
    }

    ensure_commit_queued_for_async_review(&repo_root, &commit_sha)?;

    match spawn_async_queue_review(&repo_root, &commit_sha) {
        Ok(()) => {
            println!(
                "Started async Codex review for {} (running in background).",
                short_sha(&commit_sha)
            );
            println!(
                "  Check status: f commit-queue show {}",
                short_sha(&commit_sha)
            );
        }
        Err(err) => {
            println!("⚠️ Failed to start async review automatically: {}", err);
            println!(
                "  Run manually: f commit-queue review {}",
                short_sha(&commit_sha)
            );
        }
    }

    Ok(())
}

fn ensure_commit_queued_for_async_review(repo_root: &Path, commit_sha: &str) -> Result<()> {
    if resolve_commit_queue_entry(repo_root, commit_sha).is_ok() {
        return Ok(());
    }

    let entry = queue_existing_commit_for_approval(repo_root, commit_sha, false)?;
    println!("Queued {} for async review.", short_sha(&entry.commit_sha));
    Ok(())
}

fn spawn_async_queue_review(repo_root: &Path, commit_sha: &str) -> Result<()> {
    let flow_bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("f"));

    let mut cmd = Command::new(flow_bin);
    cmd.current_dir(repo_root)
        .arg("commit-queue")
        .arg("review")
        .arg(commit_sha)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    cmd.spawn()
        .context("failed to spawn background queue review")?;
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
    queue: CommitQueueMode,
    include_unhash: bool,
    stage_paths: &[String],
    gate_overrides: CommitGateOverrides,
) -> Result<()> {
    let _git_capture_cache_scope = GitCaptureCacheScope::begin();

    if commit_with_check_async_enabled() && hub::hub_healthy(HUB_HOST, HUB_PORT) {
        ensure_git_repo()?;
        let repo_root = git_root_or_cwd();
        ensure_commit_setup(&repo_root)?;
        git_guard::ensure_clean_for_commit(&repo_root)?;
        if should_run_sync_for_secret_fixes(&repo_root)? {
            return run_with_check_sync(
                push,
                include_context,
                review_selection,
                author_message,
                max_tokens,
                false,
                queue,
                include_unhash,
                stage_paths,
                gate_overrides,
            );
        }
        return delegate_to_hub_with_check(
            "commitWithCheck",
            push,
            include_context,
            review_selection,
            author_message,
            max_tokens,
            queue,
            include_unhash,
            stage_paths,
            gate_overrides,
        );
    }

    run_with_check_sync(
        push,
        include_context,
        review_selection,
        author_message,
        max_tokens,
        false,
        queue,
        include_unhash,
        stage_paths,
        gate_overrides,
    )
}

/// Run commitWithCheck, honoring the global gitedit setting for sync/hash.
pub fn run_with_check_with_gitedit(
    push: bool,
    include_context: bool,
    review_selection: ReviewSelection,
    author_message: Option<&str>,
    max_tokens: usize,
    queue: CommitQueueMode,
    include_unhash: bool,
    stage_paths: &[String],
    gate_overrides: CommitGateOverrides,
) -> Result<()> {
    let _git_capture_cache_scope = GitCaptureCacheScope::begin();

    let force_gitedit = gitedit_globally_enabled();
    if commit_with_check_async_enabled() && hub::hub_healthy(HUB_HOST, HUB_PORT) {
        ensure_git_repo()?;
        let repo_root = git_root_or_cwd();
        ensure_commit_setup(&repo_root)?;
        git_guard::ensure_clean_for_commit(&repo_root)?;
        if should_run_sync_for_secret_fixes(&repo_root)? {
            return run_with_check_sync(
                push,
                include_context,
                review_selection,
                author_message,
                max_tokens,
                force_gitedit,
                queue,
                include_unhash,
                stage_paths,
                gate_overrides,
            );
        }
        return delegate_to_hub_with_check(
            "commit", // CLI command name
            push,
            include_context,
            review_selection,
            author_message,
            max_tokens,
            queue,
            include_unhash,
            stage_paths,
            gate_overrides,
        );
    }

    run_with_check_sync(
        push,
        include_context,
        review_selection,
        author_message,
        max_tokens,
        force_gitedit,
        queue,
        include_unhash,
        stage_paths,
        gate_overrides,
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

    let repo_root = git_root_or_cwd();
    let local_config = repo_root.join("flow.toml");
    if local_config.exists() {
        if let Ok(cfg) = config::load(&local_config) {
            return cfg.options.commit_with_check_async.unwrap_or(true);
        }
        return true;
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
    let repo_root = git_root_or_cwd();
    let local_config = repo_root.join("flow.toml");
    if local_config.exists() {
        if let Ok(cfg) = config::load(&local_config) {
            return cfg.options.commit_with_check_use_repo_root.unwrap_or(true);
        }
        return true;
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

const DEFAULT_COMMIT_WITH_CHECK_TIMEOUT_SECS: u64 = 300;
const MAX_COMMIT_WITH_CHECK_TIMEOUT_SECS: u64 = 3600;
const DEFAULT_COMMIT_WITH_CHECK_REVIEW_RETRIES: u32 = 2;
const MAX_COMMIT_WITH_CHECK_REVIEW_RETRIES: u32 = 5;
const DEFAULT_COMMIT_WITH_CHECK_RETRY_BACKOFF_SECS: u64 = 3;

fn commit_with_check_timeout_from_env() -> Option<u64> {
    for key in [
        "FLOW_COMMIT_WITH_CHECK_TIMEOUT_SECS",
        "FLOW_COMMIT_TIMEOUT_SECS",
    ] {
        if let Ok(value) = env::var(key) {
            if let Ok(parsed) = value.trim().parse::<u64>() {
                if parsed > 0 {
                    return Some(parsed.min(MAX_COMMIT_WITH_CHECK_TIMEOUT_SECS));
                }
            }
        }
    }
    None
}

fn commit_with_check_timeout_secs() -> u64 {
    if let Some(timeout) = commit_with_check_timeout_from_env() {
        return timeout;
    }

    let repo_root = git_root_or_cwd();
    let local_config = repo_root.join("flow.toml");
    if local_config.exists() {
        if let Ok(cfg) = config::load(&local_config) {
            return cfg
                .options
                .commit_with_check_timeout_secs
                .unwrap_or(DEFAULT_COMMIT_WITH_CHECK_TIMEOUT_SECS)
                .clamp(1, MAX_COMMIT_WITH_CHECK_TIMEOUT_SECS);
        }
        return DEFAULT_COMMIT_WITH_CHECK_TIMEOUT_SECS;
    }

    let global_config = config::default_config_path();
    if global_config.exists() {
        if let Ok(cfg) = config::load(&global_config) {
            return cfg
                .options
                .commit_with_check_timeout_secs
                .unwrap_or(DEFAULT_COMMIT_WITH_CHECK_TIMEOUT_SECS)
                .clamp(1, MAX_COMMIT_WITH_CHECK_TIMEOUT_SECS);
        }
    }

    DEFAULT_COMMIT_WITH_CHECK_TIMEOUT_SECS
}

fn commit_with_check_review_retries() -> u32 {
    for key in [
        "FLOW_COMMIT_WITH_CHECK_REVIEW_RETRIES",
        "FLOW_COMMIT_REVIEW_RETRIES",
    ] {
        if let Ok(value) = env::var(key) {
            if let Ok(parsed) = value.trim().parse::<u32>() {
                return parsed.min(MAX_COMMIT_WITH_CHECK_REVIEW_RETRIES);
            }
        }
    }

    let repo_root = git_root_or_cwd();
    let local_config = repo_root.join("flow.toml");
    if local_config.exists() {
        if let Ok(cfg) = config::load(&local_config) {
            return cfg
                .options
                .commit_with_check_review_retries
                .unwrap_or(DEFAULT_COMMIT_WITH_CHECK_REVIEW_RETRIES)
                .min(MAX_COMMIT_WITH_CHECK_REVIEW_RETRIES);
        }
        return DEFAULT_COMMIT_WITH_CHECK_REVIEW_RETRIES;
    }

    let global_config = config::default_config_path();
    if global_config.exists() {
        if let Ok(cfg) = config::load(&global_config) {
            return cfg
                .options
                .commit_with_check_review_retries
                .unwrap_or(DEFAULT_COMMIT_WITH_CHECK_REVIEW_RETRIES)
                .min(MAX_COMMIT_WITH_CHECK_REVIEW_RETRIES);
        }
    }

    DEFAULT_COMMIT_WITH_CHECK_REVIEW_RETRIES
}

fn commit_with_check_retry_backoff_secs(attempt: u32) -> u64 {
    let mut base = DEFAULT_COMMIT_WITH_CHECK_RETRY_BACKOFF_SECS;
    if let Ok(value) = env::var("FLOW_COMMIT_WITH_CHECK_RETRY_BACKOFF_SECS") {
        if let Ok(parsed) = value.trim().parse::<u64>() {
            if parsed > 0 {
                base = parsed.min(60);
            }
        }
    }
    base.saturating_mul(attempt as u64).min(120)
}

fn commit_with_check_review_url() -> Option<String> {
    if let Ok(url) = env::var("FLOW_REVIEW_URL") {
        let trimmed = url.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    let repo_root = git_root_or_cwd();
    let local_config = repo_root.join("flow.toml");
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

    if let Ok(Some(_token)) = crate::env::load_ai_auth_token() {
        if let Ok(api_url) = crate::env::load_ai_api_url() {
            let trimmed = api_url.trim().trim_end_matches('/').to_string();
            if !trimmed.is_empty() {
                return Some(format!("{}/api/ai", trimmed));
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

    let repo_root = git_root_or_cwd();
    let local_config = repo_root.join("flow.toml");
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

    if let Ok(Some(token)) = crate::env::load_ai_auth_token() {
        let trimmed = token.trim().to_string();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }

    None
}

pub fn resolve_commit_queue_mode(cli_queue: bool, cli_no_queue: bool) -> CommitQueueMode {
    if cli_no_queue {
        return CommitQueueMode {
            enabled: false,
            override_flag: Some(false),
            open_review: false,
        };
    }
    if cli_queue {
        return CommitQueueMode {
            enabled: true,
            override_flag: Some(true),
            open_review: false,
        };
    }

    CommitQueueMode {
        enabled: commit_queue_enabled_from_config(),
        override_flag: None,
        open_review: false,
    }
}

fn commit_queue_enabled_from_config() -> bool {
    if let Some(ts_config) = config::load_ts_config() {
        if let Some(flow) = ts_config.flow {
            if let Some(commit) = flow.commit {
                if let Some(queue_enabled) = commit.queue {
                    return queue_enabled;
                }
            }
        }
    }

    let repo_root = git_root_or_cwd();
    let local_config = repo_root.join("flow.toml");
    if local_config.exists() {
        if let Ok(cfg) = config::load(&local_config) {
            if let Some(commit) = cfg.commit {
                if let Some(queue_enabled) = commit.queue {
                    return queue_enabled;
                }
            }
        }
    }

    let global_config = config::default_config_path();
    if global_config.exists() {
        if let Ok(cfg) = config::load(&global_config) {
            if let Some(commit) = cfg.commit {
                if let Some(queue_enabled) = commit.queue {
                    return queue_enabled;
                }
            }
        }
    }

    true
}

fn commit_queue_on_issues_enabled(repo_root: &Path) -> bool {
    if let Some(ts_config) = config::load_ts_config() {
        if let Some(flow) = ts_config.flow {
            if let Some(commit) = flow.commit {
                if let Some(queue_on_issues) = commit.queue_on_issues {
                    return queue_on_issues;
                }
            }
        }
    }

    let local_config = repo_root.join("flow.toml");
    if local_config.exists() {
        if let Ok(cfg) = config::load(&local_config) {
            if let Some(commit) = cfg.commit {
                if let Some(queue_on_issues) = commit.queue_on_issues {
                    return queue_on_issues;
                }
            }
        }
    }

    let global_config = config::default_config_path();
    if global_config.exists() {
        if let Ok(cfg) = config::load(&global_config) {
            if let Some(commit) = cfg.commit {
                if let Some(queue_on_issues) = commit.queue_on_issues {
                    return queue_on_issues;
                }
            }
        }
    }

    false
}

fn prompt_yes_no(message: &str) -> Result<bool> {
    print!("{} [y/N]: ", message);
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

fn prompt_yes_no_default_yes(message: &str) -> Result<bool> {
    if !io::stdin().is_terminal() {
        return Ok(false);
    }
    print!("{} [Y/n]: ", message);
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_ascii_lowercase();
    if answer.is_empty() {
        return Ok(true);
    }
    Ok(answer == "y" || answer == "yes")
}

fn resolve_commit_testing_policy(repo_root: &Path) -> CommitTestingPolicy {
    let cfg = config::load_or_default(repo_root.join("flow.toml"));
    let maybe_testing = cfg.commit.and_then(|commit| commit.testing);
    let Some(testing) = maybe_testing else {
        if is_bun_repo_layout(repo_root) {
            return CommitTestingPolicy {
                mode: "warn".to_string(),
                runner: "bun".to_string(),
                bun_repo_strict: true,
                require_related_tests: true,
                ai_scratch_test_dir: ".ai/test".to_string(),
                run_ai_scratch_tests: true,
                allow_ai_scratch_to_satisfy_gate: false,
                max_local_gate_seconds: 15,
            };
        }
        return CommitTestingPolicy {
            mode: "off".to_string(),
            runner: "bun".to_string(),
            bun_repo_strict: true,
            require_related_tests: true,
            ai_scratch_test_dir: ".ai/test".to_string(),
            run_ai_scratch_tests: true,
            allow_ai_scratch_to_satisfy_gate: false,
            max_local_gate_seconds: 15,
        };
    };

    let mode = testing
        .mode
        .unwrap_or_else(|| "warn".to_string())
        .to_ascii_lowercase();
    let mode = match mode.as_str() {
        "warn" | "block" | "off" => mode,
        _ => "warn".to_string(),
    };

    CommitTestingPolicy {
        mode,
        runner: testing
            .runner
            .unwrap_or_else(|| "bun".to_string())
            .to_ascii_lowercase(),
        bun_repo_strict: testing.bun_repo_strict.unwrap_or(true),
        require_related_tests: testing.require_related_tests.unwrap_or(true),
        ai_scratch_test_dir: testing
            .ai_scratch_test_dir
            .unwrap_or_else(|| ".ai/test".to_string()),
        run_ai_scratch_tests: testing.run_ai_scratch_tests.unwrap_or(true),
        allow_ai_scratch_to_satisfy_gate: testing.allow_ai_scratch_to_satisfy_gate.unwrap_or(false),
        max_local_gate_seconds: testing.max_local_gate_seconds.unwrap_or(15),
    }
}

fn resolve_commit_skill_gate_policy(repo_root: &Path) -> CommitSkillGatePolicy {
    let cfg = config::load_or_default(repo_root.join("flow.toml"));
    let Some(skill_gate) = cfg.commit.and_then(|commit| commit.skill_gate) else {
        return CommitSkillGatePolicy {
            mode: "off".to_string(),
            required: Vec::new(),
            min_version: HashMap::new(),
        };
    };

    let mut required = skill_gate.required;
    required.retain(|name| !name.trim().is_empty());
    required.sort();
    required.dedup();

    let default_mode = if required.is_empty() { "off" } else { "warn" };
    let mode = skill_gate
        .mode
        .unwrap_or_else(|| default_mode.to_string())
        .to_ascii_lowercase();
    let mode = match mode.as_str() {
        "warn" | "block" | "off" => mode,
        _ => default_mode.to_string(),
    };

    CommitSkillGatePolicy {
        mode,
        required,
        min_version: skill_gate.min_version.unwrap_or_default(),
    }
}

fn run_required_skill_gate(
    repo_root: &Path,
    gate_overrides: CommitGateOverrides,
) -> Result<SkillGateReport> {
    if gate_overrides.skip_quality {
        return Ok(SkillGateReport {
            pass: true,
            mode: "off".to_string(),
            override_flag: Some("skip-quality".to_string()),
            ..SkillGateReport::default()
        });
    }

    let policy = resolve_commit_skill_gate_policy(repo_root);
    if policy.mode == "off" || policy.required.is_empty() {
        return Ok(SkillGateReport {
            pass: true,
            mode: policy.mode,
            required_skills: policy.required,
            ..SkillGateReport::default()
        });
    }

    let mut report = SkillGateReport {
        pass: true,
        mode: policy.mode.clone(),
        override_flag: None,
        required_skills: policy.required.clone(),
        missing_skills: Vec::new(),
        version_failures: Vec::new(),
        loaded_versions: HashMap::new(),
    };

    for skill_name in &policy.required {
        let skill_content = skills::read_skill_content_at(repo_root, skill_name)?;
        if skill_content.is_none() {
            report.missing_skills.push(skill_name.clone());
            continue;
        }

        if let Some(required_version) = policy.min_version.get(skill_name) {
            let local_version = skills::read_skill_version_at(repo_root, skill_name)?;
            match local_version {
                Some(version) => {
                    report.loaded_versions.insert(skill_name.clone(), version);
                    if version < *required_version {
                        report.version_failures.push(format!(
                            "{} has version {}, requires >= {}",
                            skill_name, version, required_version
                        ));
                    }
                }
                None => {
                    report.version_failures.push(format!(
                        "{} is missing frontmatter version (requires >= {})",
                        skill_name, required_version
                    ));
                }
            }
        } else if let Some(version) = skills::read_skill_version_at(repo_root, skill_name)? {
            report.loaded_versions.insert(skill_name.clone(), version);
        }
    }

    report.pass = report.missing_skills.is_empty() && report.version_failures.is_empty();
    if !report.pass {
        for missing in &report.missing_skills {
            eprintln!(
                "  skills: required skill '{}' is missing in .ai/skills/",
                missing
            );
        }
        for failure in &report.version_failures {
            eprintln!("  skills: {}", failure);
        }
        if policy.mode == "block" {
            bail!("Commit blocked by required skill gate");
        }
        eprintln!("  skills: warning only (mode=warn)");
    }

    Ok(report)
}

fn build_required_skills_prompt_context(
    repo_root: &Path,
    skill_report: &SkillGateReport,
) -> String {
    if skill_report.required_skills.is_empty() {
        return String::new();
    }

    let mut sections = Vec::new();
    for skill_name in &skill_report.required_skills {
        if let Ok(Some(content)) = skills::read_skill_content_at(repo_root, skill_name) {
            sections.push(format!("## Skill: {}\n{}", skill_name, content));
        }
    }
    if sections.is_empty() {
        return String::new();
    }
    format!(
        "\nRequired workflow skills for this repo. Follow these constraints while reviewing and generating output:\n\n{}\n",
        sections.join("\n\n")
    )
}

fn combine_review_instructions(
    custom: Option<&str>,
    required_skill_context: &str,
) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(custom) = custom {
        if !custom.trim().is_empty() {
            parts.push(custom.trim().to_string());
        }
    }
    if !required_skill_context.trim().is_empty() {
        parts.push(required_skill_context.trim().to_string());
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

// ---------------------------------------------------------------------------
// Invariant gate: check staged diff against [invariants] from flow.toml
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct InvariantFinding {
    severity: String, // "critical" | "warning" | "note"
    category: String, // "forbidden" | "deps" | "files" | "terminology"
    message: String,
    file: Option<String>,
}

#[derive(Debug, Default)]
struct InvariantGateReport {
    findings: Vec<InvariantFinding>,
}

impl InvariantGateReport {
    /// Build prompt context from findings + invariants for AI review injection.
    fn to_prompt_context(&self, inv: &config::InvariantsConfig) -> String {
        let mut parts = Vec::new();

        // Always inject invariants into prompt even if no findings.
        if let Some(style) = inv.architecture_style.as_deref() {
            parts.push(format!("Architecture: {}", style));
        }
        if !inv.non_negotiable.is_empty() {
            parts.push(format!(
                "Non-negotiable rules:\n{}",
                inv.non_negotiable
                    .iter()
                    .map(|r| format!("- {}", r))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        if !inv.terminology.is_empty() {
            let terms: Vec<String> = inv
                .terminology
                .iter()
                .map(|(k, v)| format!("- {}: {}", k, v))
                .collect();
            parts.push(format!(
                "Terminology (do not rename):\n{}",
                terms.join("\n")
            ));
        }

        if !self.findings.is_empty() {
            let finding_lines: Vec<String> = self
                .findings
                .iter()
                .map(|f| {
                    let loc = f.file.as_deref().unwrap_or("(repo)");
                    format!("  [{}] {} — {}", f.severity, loc, f.message)
                })
                .collect();
            parts.push(format!(
                "Invariant findings on staged files:\n{}",
                finding_lines.join("\n")
            ));
        }

        if parts.is_empty() {
            return String::new();
        }
        format!(
            "\n## Project Invariants (enforced by flow)\n\n{}\n",
            parts.join("\n\n")
        )
    }
}

fn resolve_invariants_config(repo_root: &Path) -> Option<config::InvariantsConfig> {
    let cfg = config::load_or_default(repo_root.join("flow.toml"));
    cfg.invariants
}

fn run_invariant_gate(
    repo_root: &Path,
    diff: &str,
    changed_files: &[String],
    gate_overrides: CommitGateOverrides,
) -> Result<InvariantGateReport> {
    if gate_overrides.skip_quality {
        return Ok(InvariantGateReport {
            findings: Vec::new(),
        });
    }

    let Some(inv) = resolve_invariants_config(repo_root) else {
        return Ok(InvariantGateReport {
            findings: Vec::new(),
        });
    };

    let mode = inv.mode.as_deref().unwrap_or("warn").to_ascii_lowercase();
    if mode == "off" {
        return Ok(InvariantGateReport {
            findings: Vec::new(),
        });
    }

    let mut findings: Vec<InvariantFinding> = Vec::new();

    // 1. Forbidden patterns in diff content.
    let skip_files = ["flow.toml"];
    for pattern in &inv.forbidden {
        let pat_lower = pattern.to_lowercase();
        let mut current_file: Option<String> = None;
        let mut skip_current = false;
        for line in diff.lines() {
            if let Some(file) = line.strip_prefix("+++ b/") {
                let file = file.trim().trim_matches('"');
                current_file = Some(file.to_string());
                skip_current = skip_files.iter().any(|s| file.ends_with(s));
                continue;
            }
            if current_file
                .as_deref()
                .is_some_and(|f| f.trim().trim_matches('"').ends_with("flow.toml"))
            {
                continue;
            }
            if skip_current {
                continue;
            }
            // Only check added lines (lines starting with +, excluding +++ header).
            if !line.starts_with('+') || line.starts_with("+++") {
                continue;
            }
            if line.to_lowercase().contains(&pat_lower) {
                findings.push(InvariantFinding {
                    severity: "warning".to_string(),
                    category: "forbidden".to_string(),
                    message: format!("Forbidden pattern '{}' in added line", pattern),
                    file: current_file.clone(),
                });
                break; // One finding per pattern is enough.
            }
        }
    }

    // 2. Dependency policy: check package.json changes for unapproved deps.
    if let Some(deps_config) = &inv.deps {
        let policy = deps_config.policy.as_deref().unwrap_or("approval_required");
        if policy == "approval_required" && !deps_config.approved.is_empty() {
            for file in changed_files {
                if file.ends_with("package.json") {
                    let full = repo_root.join(file);
                    if let Ok(contents) = fs::read_to_string(&full) {
                        check_unapproved_deps(
                            &contents,
                            &deps_config.approved,
                            file,
                            &mut findings,
                        );
                    }
                }
            }
        }
    }

    // 3. File size limits.
    if let Some(files_config) = &inv.files {
        if let Some(max_lines) = files_config.max_lines {
            for file in changed_files {
                let full = repo_root.join(file);
                if let Ok(contents) = fs::read_to_string(&full) {
                    let line_count = contents.lines().count() as u32;
                    if line_count > max_lines {
                        findings.push(InvariantFinding {
                            severity: "warning".to_string(),
                            category: "files".to_string(),
                            message: format!("File has {} lines (max {})", line_count, max_lines),
                            file: Some(file.clone()),
                        });
                    }
                }
            }
        }
    }

    let has_blocking = findings
        .iter()
        .any(|f| f.severity == "critical" || f.severity == "warning");

    // Print findings.
    if !findings.is_empty() {
        eprintln!();
        eprintln!("  invariants: {} finding(s)", findings.len());
        for f in &findings {
            let loc = f.file.as_deref().unwrap_or("(diff)");
            eprintln!(
                "    [{}:{}] {} — {}",
                f.severity, f.category, loc, f.message
            );
        }
    }

    let pass = !has_blocking;
    if !pass && mode == "block" {
        bail!(
            "Commit blocked by invariant gate ({} finding(s))",
            findings.len()
        );
    }
    if !pass {
        eprintln!("  invariants: warning only (mode=warn)");
    }

    Ok(InvariantGateReport { findings })
}

/// Check a package.json for dependencies not on the approved list.
fn check_unapproved_deps(
    package_json: &str,
    approved: &[String],
    file_path: &str,
    findings: &mut Vec<InvariantFinding>,
) {
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(package_json) else {
        return;
    };

    let dep_sections = ["dependencies", "devDependencies", "peerDependencies"];
    for section in &dep_sections {
        if let Some(deps) = parsed.get(section).and_then(|v| v.as_object()) {
            for dep_name in deps.keys() {
                if !approved.iter().any(|a| a == dep_name) {
                    findings.push(InvariantFinding {
                        severity: "warning".to_string(),
                        category: "deps".to_string(),
                        message: format!(
                            "'{}' in {} is not on the approved list",
                            dep_name, section
                        ),
                        file: Some(file_path.to_string()),
                    });
                }
            }
        }
    }
}

fn is_bun_repo_layout(repo_root: &Path) -> bool {
    if repo_root.join("build.zig").exists() && repo_root.join("src/bun.js").exists() {
        return true;
    }
    let agents_file = repo_root.join("AGENTS.md");
    if let Ok(contents) = fs::read_to_string(agents_file) {
        return contents.contains("This is the Bun repository");
    }
    false
}

fn looks_like_source_file_for_test_gate(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    let ext = Path::new(&normalized)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(
        ext.as_str(),
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" | "rs"
    )
}

fn is_test_file_path(path: &str) -> bool {
    let normalized = path.replace('\\', "/").to_ascii_lowercase();
    normalized.contains("/__tests__/")
        || normalized.ends_with(".test.js")
        || normalized.ends_with(".test.jsx")
        || normalized.ends_with(".test.ts")
        || normalized.ends_with(".test.tsx")
        || normalized.ends_with(".spec.js")
        || normalized.ends_with(".spec.jsx")
        || normalized.ends_with(".spec.ts")
        || normalized.ends_with(".spec.tsx")
        || normalized.ends_with("_test.rs")
}

fn normalize_rel_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn normalize_dir_path(path: &str) -> String {
    let mut normalized = path.replace('\\', "/");
    while normalized.starts_with("./") {
        normalized = normalized.trim_start_matches("./").to_string();
    }
    normalized.trim_end_matches('/').to_string()
}

fn path_is_within_dir(path: &str, dir: &str) -> bool {
    let normalized_path = normalize_dir_path(path);
    let normalized_dir = normalize_dir_path(dir);
    if normalized_dir.is_empty() {
        return false;
    }
    normalized_path == normalized_dir || normalized_path.starts_with(&(normalized_dir + "/"))
}

fn find_ai_scratch_tests(repo_root: &Path, scratch_dir: &str) -> Vec<String> {
    let scratch_dir = normalize_dir_path(scratch_dir);
    if scratch_dir.is_empty() {
        return Vec::new();
    }

    let scratch_root = repo_root.join(&scratch_dir);
    if !scratch_root.is_dir() {
        return Vec::new();
    }

    let mut out = HashSet::new();
    let mut stack = vec![scratch_root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if !path.is_file() {
                continue;
            }
            let Ok(rel) = path.strip_prefix(repo_root) else {
                continue;
            };
            let rel = normalize_rel_path(rel);
            if is_test_file_path(&rel) {
                out.insert(rel);
            }
        }
    }

    let mut tests: Vec<String> = out.into_iter().collect();
    tests.sort();
    tests
}

fn collect_candidate_js_test_paths(rel_path: &Path) -> Vec<PathBuf> {
    const JS_EXTS: &[&str] = &["ts", "tsx", "js", "jsx", "mjs", "cjs"];
    let mut out = Vec::new();
    let parent = rel_path.parent().unwrap_or_else(|| Path::new(""));
    let stem = rel_path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    if stem.is_empty() {
        return out;
    }

    let base_no_ext = parent.join(stem);
    for ext in JS_EXTS {
        let mut same_dir_test = base_no_ext.clone();
        same_dir_test.set_extension(format!("test.{}", ext));
        out.push(same_dir_test);

        let mut same_dir_spec = base_no_ext.clone();
        same_dir_spec.set_extension(format!("spec.{}", ext));
        out.push(same_dir_spec);

        let mut in_test_dir = PathBuf::from("test").join(&base_no_ext);
        in_test_dir.set_extension(format!("test.{}", ext));
        out.push(in_test_dir);

        let mut in_tests_dir = PathBuf::from("tests").join(&base_no_ext);
        in_tests_dir.set_extension(format!("test.{}", ext));
        out.push(in_tests_dir);
    }

    let tests_dir = parent.join("__tests__");
    if let Some(file_name) = rel_path.file_name() {
        out.push(tests_dir.join(file_name));
    }

    out
}

fn collect_candidate_rust_test_paths(rel_path: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let parent = rel_path.parent().unwrap_or_else(|| Path::new(""));
    let stem = rel_path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    if stem.is_empty() {
        return out;
    }

    out.push(parent.join(format!("{}_test.rs", stem)));
    out.push(PathBuf::from("tests").join(format!("{}.rs", stem)));
    out.push(PathBuf::from("tests").join(format!("{}_test.rs", stem)));

    let mut tests_rel = PathBuf::from("tests").join(rel_path);
    tests_rel.set_extension("rs");
    out.push(tests_rel);

    out
}

fn find_related_tests(
    repo_root: &Path,
    changed_files: &[String],
    ai_scratch_test_dir: &str,
) -> Vec<String> {
    let mut tests = HashSet::new();
    for changed in changed_files {
        let normalized = changed.replace('\\', "/");
        if path_is_within_dir(&normalized, ai_scratch_test_dir) {
            continue;
        }
        if is_test_file_path(&normalized) {
            tests.insert(normalized);
            continue;
        }
        if !looks_like_source_file_for_test_gate(&normalized) {
            continue;
        }

        let rel = Path::new(&normalized);
        let ext = rel
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let candidates = if ext == "rs" {
            collect_candidate_rust_test_paths(rel)
        } else {
            collect_candidate_js_test_paths(rel)
        };

        for candidate in candidates {
            if repo_root.join(&candidate).is_file() {
                let candidate = normalize_rel_path(&candidate);
                if !path_is_within_dir(&candidate, ai_scratch_test_dir) {
                    tests.insert(candidate);
                }
            }
        }
    }

    let mut out: Vec<String> = tests.into_iter().collect();
    out.sort();
    out
}

fn find_non_bun_test_tasks(repo_root: &Path, strict_bun_repo: bool) -> Vec<String> {
    let config_path = repo_root.join("flow.toml");
    if !config_path.exists() {
        return Vec::new();
    }
    let cfg = match config::load(&config_path) {
        Ok(cfg) => cfg,
        Err(_) => return Vec::new(),
    };

    let mut violations = Vec::new();
    for task in cfg.tasks {
        let name = task.name.to_ascii_lowercase();
        let cmd = task.command.to_ascii_lowercase();
        let looks_like_test_task = name.contains("test")
            || cmd.starts_with("test ")
            || cmd.contains(" test ")
            || cmd.contains("bun test")
            || cmd.contains("bun bd test");
        if !looks_like_test_task {
            continue;
        }

        if !cmd.contains("bun ") {
            violations.push(format!(
                "task '{}' must use bun: {}",
                task.name, task.command
            ));
            continue;
        }

        if strict_bun_repo && !cmd.contains("bun bd test") {
            violations.push(format!(
                "task '{}' must use `bun bd test` in Bun repo: {}",
                task.name, task.command
            ));
        }
    }

    violations
}

fn apply_testing_gate_failure(mode: &str, message: &str) -> Result<()> {
    eprintln!("  testing: {}", message);
    if mode == "block" {
        bail!("Commit blocked by testing gate");
    }
    eprintln!("  testing: warning only (mode=warn)");
    Ok(())
}

fn run_pre_commit_test_gate(
    repo_root: &Path,
    changed_files: &[String],
    gate_overrides: CommitGateOverrides,
) -> Result<()> {
    if gate_overrides.skip_quality || gate_overrides.skip_tests {
        if gate_overrides.skip_tests {
            println!("Skipping test gate due to --skip-tests");
        }
        return Ok(());
    }

    let policy = resolve_commit_testing_policy(repo_root);
    if policy.mode == "off" {
        return Ok(());
    }
    if policy.runner != "bun" {
        return apply_testing_gate_failure(
            &policy.mode,
            &format!(
                "unsupported test runner '{}'; only bun is currently supported",
                policy.runner
            ),
        );
    }

    let strict_bun_repo = policy.bun_repo_strict && is_bun_repo_layout(repo_root);
    let task_violations = find_non_bun_test_tasks(repo_root, strict_bun_repo);
    if !task_violations.is_empty() {
        return apply_testing_gate_failure(
            &policy.mode,
            &format!(
                "flow.toml test tasks are not Bun-compliant:\n    {}",
                task_violations.join("\n    ")
            ),
        );
    }

    let has_source_changes = changed_files
        .iter()
        .any(|p| looks_like_source_file_for_test_gate(p) && !is_test_file_path(p));
    if !has_source_changes {
        return Ok(());
    }

    let related_tests = find_related_tests(repo_root, changed_files, &policy.ai_scratch_test_dir);
    let run_bun_tests = |tests: &[String], label: &str| -> Result<()> {
        let mut args: Vec<String> = Vec::new();
        if strict_bun_repo {
            args.push("bd".to_string());
            args.push("test".to_string());
        } else {
            args.push("test".to_string());
        }
        args.extend(tests.iter().cloned());

        println!();
        println!("Running local test gate (bun) for {}...", label);
        println!("Command: bun {}", args.join(" "));

        let started_at = Instant::now();
        let status = match Command::new("bun")
            .args(args.iter().map(|s| s.as_str()))
            .current_dir(repo_root)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
        {
            Ok(status) => status,
            Err(err) => {
                return apply_testing_gate_failure(
                    &policy.mode,
                    &format!("failed to execute bun test gate: {}", err),
                );
            }
        };
        let elapsed = started_at.elapsed();

        if elapsed > Duration::from_secs(policy.max_local_gate_seconds) {
            eprintln!(
                "  testing: local gate exceeded target budget ({}s > {}s)",
                elapsed.as_secs(),
                policy.max_local_gate_seconds
            );
        }

        if !status.success() {
            return apply_testing_gate_failure(
                &policy.mode,
                &format!("bun tests failed for {} (exit status: {})", label, status),
            );
        }

        Ok(())
    };

    if related_tests.is_empty() {
        if policy.run_ai_scratch_tests {
            let scratch_tests = find_ai_scratch_tests(repo_root, &policy.ai_scratch_test_dir);
            if !scratch_tests.is_empty() {
                run_bun_tests(&scratch_tests, "AI scratch tests")?;
                println!(
                    "✓ AI scratch tests passed ({} test file{})",
                    scratch_tests.len(),
                    if scratch_tests.len() == 1 { "" } else { "s" }
                );

                if policy.allow_ai_scratch_to_satisfy_gate {
                    println!(
                        "✓ Test gate satisfied by AI scratch tests ({})",
                        policy.ai_scratch_test_dir
                    );
                    return Ok(());
                }
            }
        }

        if policy.require_related_tests {
            return apply_testing_gate_failure(
                &policy.mode,
                &format!(
                    "no related tracked test files detected for staged source changes (AI scratch dir: {}; set commit.testing.allow_ai_scratch_to_satisfy_gate=true to allow scratch-only satisfaction)",
                    policy.ai_scratch_test_dir
                ),
            );
        }
        return Ok(());
    }

    run_bun_tests(&related_tests, "related tracked tests")?;

    println!(
        "✓ Test gate passed ({} related tracked test file{})",
        related_tests.len(),
        if related_tests.len() == 1 { "" } else { "s" }
    );
    Ok(())
}

fn is_doc_gate_failure(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("doc") || m.contains("documentation")
}

fn is_test_gate_failure(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("test") || m.contains("coverage")
}

fn run_review_attempt(
    selection: &ReviewSelection,
    diff: &str,
    session_context: Option<&str>,
    review_instructions: Option<&str>,
    repo_root: &Path,
) -> Result<(ReviewResult, &'static str, String)> {
    match selection {
        ReviewSelection::Claude(model) => Ok((
            run_claude_review(
                diff,
                session_context,
                review_instructions,
                repo_root,
                *model,
            )?,
            "claude",
            model.as_claude_arg().to_string(),
        )),
        ReviewSelection::Codex(model) => Ok((
            run_codex_review(
                diff,
                session_context,
                review_instructions,
                repo_root,
                *model,
            )?,
            "codex",
            model.as_codex_arg().to_string(),
        )),
        ReviewSelection::Opencode { model } => Ok((
            run_opencode_review(diff, session_context, review_instructions, repo_root, model)?,
            "opencode",
            model.clone(),
        )),
        ReviewSelection::OpenRouter { model } => Ok((
            run_openrouter_review(diff, session_context, review_instructions, repo_root, model)?,
            "openrouter",
            openrouter_model_label(model),
        )),
        ReviewSelection::Rise { model } => Ok((
            run_rise_review(diff, session_context, review_instructions, repo_root, model)?,
            "rise",
            format!("rise:{}", model),
        )),
        ReviewSelection::Kimi { model } => Ok((
            run_kimi_review(
                diff,
                session_context,
                review_instructions,
                repo_root,
                model.as_deref(),
            )?,
            "kimi",
            match model.as_deref() {
                Some(model) if !model.trim().is_empty() => format!("kimi:{}", model),
                _ => "kimi".to_string(),
            },
        )),
    }
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
    queue: CommitQueueMode,
    include_unhash: bool,
    stage_paths: &[String],
    gate_overrides: CommitGateOverrides,
) -> Result<()> {
    let _git_capture_cache_scope = GitCaptureCacheScope::begin();

    let push_requested = push;
    let mut queue_enabled = queue.enabled;
    let prefer_codex_over_openrouter =
        review_selection.is_openrouter() && openrouter_review_should_use_codex();
    // Convert tokens to chars (roughly 4 chars per token)
    let max_context = max_tokens * 4;
    info!(
        push = push_requested && !queue_enabled,
        queue = queue_enabled,
        include_context = include_context,
        review_model = if prefer_codex_over_openrouter {
            CodexModel::High.as_codex_arg().to_string()
        } else {
            review_selection.model_label()
        },
        max_tokens = max_tokens,
        "starting commit with check workflow"
    );

    // Ensure we're in a git repo
    ensure_git_repo()?;

    let repo_root = resolve_commit_with_check_root()?;
    warn_if_commit_invoked_from_subdir(&repo_root);
    ensure_commit_setup(&repo_root)?;
    git_guard::ensure_clean_for_commit(&repo_root)?;

    // Capture current staged changes so we can restore if we cancel.
    let staged_snapshot = capture_staged_snapshot_in(&repo_root)?;

    // Run pre-commit fixers if configured
    if let Ok(fixed) = run_fixers(&repo_root) {
        if fixed {
            println!();
        }
    }

    stage_changes_for_commit(&repo_root, stage_paths)?;
    ensure_no_internal_staged(&repo_root)?;
    gitignore_policy::enforce_staged_policy(&repo_root)?;

    // Check for sensitive files before proceeding
    let sensitive_files = check_sensitive_files(&repo_root);
    warn_sensitive_files(&sensitive_files)?;

    // Scan diff content for hardcoded secrets
    let secret_findings = scan_diff_for_secrets(&repo_root);
    warn_secrets_in_diff(&repo_root, &secret_findings)?;

    // Check for files with large diffs
    let large_diffs = check_large_diffs(&repo_root);
    warn_large_diffs(&large_diffs)?;

    // Get diff
    let diff = git_capture_in(&repo_root, &["diff", "--cached"])?;
    if diff.trim().is_empty() {
        println!("\nnotify: No staged changes to commit");
        print_pending_queue_review_hint(&repo_root);
        bail!("No staged changes to commit");
    }
    let changed_files = changed_files_from_diff(&diff);

    // Enforce required workflow skills before review.
    let skill_gate_report = run_required_skill_gate(&repo_root, gate_overrides)?;

    // Fast feedback loop: run impacted tests with Bun before AI review.
    run_pre_commit_test_gate(&repo_root, &changed_files, gate_overrides)?;

    // Enforce project invariants (forbidden patterns, dep policy, file size).
    let invariant_report = run_invariant_gate(&repo_root, &diff, &changed_files, gate_overrides)?;

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

    // Merge [commit] review instructions with required skill + invariant instructions.
    let custom_review_instructions = get_review_instructions(&repo_root);
    let required_skill_context =
        build_required_skills_prompt_context(&repo_root, &skill_gate_report);
    let invariant_context = resolve_invariants_config(&repo_root)
        .map(|inv| invariant_report.to_prompt_context(&inv))
        .unwrap_or_default();
    let combined_extra = format!("{}{}", required_skill_context, invariant_context);
    let review_instructions =
        combine_review_instructions(custom_review_instructions.as_deref(), &combined_extra);

    // Run code review with configured fallbacks.
    let review_attempts =
        review_attempts_for_selection(&repo_root, &review_selection, prefer_codex_over_openrouter);
    let primary_review_attempt = review_attempts
        .first()
        .cloned()
        .unwrap_or_else(|| review_selection.clone());

    println!(
        "\nRunning {} review...",
        review_tool_label(&primary_review_attempt)
    );
    println!("Model: {}", primary_review_attempt.model_label());
    if session_context.is_some() {
        println!("(with AI session context)");
    }
    if custom_review_instructions.is_some()
        || !required_skill_context.is_empty()
        || !invariant_context.trim().is_empty()
    {
        println!("(with custom review instructions)");
    }
    println!("────────────────────────────────────────");

    let mut review_reviewer_label = "codex";
    let mut review_model_label = primary_review_attempt.model_label();
    let mut review_selection_used = primary_review_attempt.clone();
    let mut review_failures: Vec<String> = Vec::new();
    let mut review_result: Option<ReviewResult> = None;

    for (idx, attempt) in review_attempts.iter().enumerate() {
        if idx > 0 {
            println!("────────────────────────────────────────");
            println!(
                "Retrying review with fallback: {} ({})",
                review_tool_label(attempt),
                attempt.model_label()
            );
            println!("────────────────────────────────────────");
        }

        match run_review_attempt(
            attempt,
            &diff,
            session_context.as_deref(),
            review_instructions.as_deref(),
            &repo_root,
        ) {
            Ok((review, reviewer_label, model_label)) => {
                review_reviewer_label = reviewer_label;
                review_model_label = model_label;
                review_selection_used = attempt.clone();
                review_result = Some(review);
                break;
            }
            Err(err) => {
                let error_message = format!(
                    "{} ({}) failed: {}",
                    review_tool_label(attempt),
                    attempt.model_label(),
                    err
                );
                review_failures.push(error_message.clone());
                if idx + 1 < review_attempts.len() {
                    println!("⚠ {}. Trying next fallback...", error_message);
                }
            }
        }
    }

    let mut review_failed_open = false;
    let review = if let Some(review) = review_result {
        review
    } else if commit_review_fail_open_enabled(&repo_root) {
        review_failed_open = true;
        println!(
            "⚠ Review failed across all attempts; continuing because commit.review_fail_open = true."
        );
        if let Some(last_error) = review_failures.last() {
            println!("Last review error: {}", last_error);
        }
        ReviewResult {
            issues_found: false,
            issues: Vec::new(),
            summary: Some(format!(
                "Review unavailable; commit proceeded in fail-open mode after {} failed attempt(s).",
                review_failures.len()
            )),
            future_tasks: Vec::new(),
            timed_out: true,
            quality: None,
        }
    } else {
        restore_staged_snapshot_in(&repo_root, &staged_snapshot)?;
        if review_failures.is_empty() {
            bail!("review failed: no review attempts were available");
        }
        bail!("review failed:\n  {}", review_failures.join("\n  "));
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
        if review_failed_open {
            println!("⚠ Review unavailable after fallback attempts, proceeding anyway");
        } else {
            println!(
                "⚠ Review timed out after {}s, proceeding anyway",
                commit_with_check_timeout_secs()
            );
        }
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
                // Also try to POST to cloud
                send_to_cloud(&repo_root, &review.issues, review.summary.as_deref());
            }
        }
        println!("Proceeding with commit...");
    } else if !review.timed_out {
        if let Some(summary) = review.summary.as_ref() {
            if !summary.trim().is_empty() {
                println!("Summary: {}", summary.trim());
                println!();
            }
        }
        println!("✓ Review passed");
    }

    // ── Quality gate check ─────────────────────────────────────────
    if gate_overrides.skip_quality {
        println!("Skipping quality gates due to --skip-quality");
    } else if let Some(ref quality) = review.quality {
        let quality_config = config::load_or_default(repo_root.join("flow.toml"))
            .commit
            .and_then(|c| c.quality)
            .unwrap_or_default();
        let mode = quality_config.mode.as_deref().unwrap_or("warn");

        let mut gate_failures: Vec<String> = quality.gate_failures.clone();
        if gate_overrides.skip_docs {
            gate_failures.retain(|failure| !is_doc_gate_failure(failure));
        }
        if gate_overrides.skip_tests {
            gate_failures.retain(|failure| !is_test_gate_failure(failure));
        }

        if !gate_failures.is_empty() && mode != "off" {
            println!();
            for failure in &gate_failures {
                eprintln!("  quality: {}", failure);
            }

            if mode == "block" {
                eprintln!("\nCommit blocked by quality gates.");
                eprintln!("Fix the issues above, or override with: f commit --skip-quality");
                restore_staged_snapshot_in(&repo_root, &staged_snapshot)?;
                bail!("Quality gate blocked commit");
            } else {
                eprintln!("\nQuality warnings above. Proceeding with commit.");
            }
        }

        // Auto-generate/update feature docs if enabled
        let auto_docs = quality_config.auto_generate_docs.unwrap_or(true);
        if auto_docs && mode != "off" && !gate_overrides.skip_docs {
            let commit_sha_preview = git_capture_in(&repo_root, &["rev-parse", "--short", "HEAD"])
                .unwrap_or_else(|_| "unknown".to_string())
                .trim()
                .to_string();

            match features::apply_quality_results(&repo_root, quality, &commit_sha_preview) {
                Ok(actions) => {
                    for action in &actions {
                        println!("  feature docs: {}", action);
                    }
                    // Stage .ai/features/ changes
                    if !actions.is_empty() {
                        let _ = std::process::Command::new("git")
                            .args(["add", ".ai/features/"])
                            .current_dir(&repo_root)
                            .output();
                    }
                }
                Err(e) => {
                    eprintln!("  warning: failed to update feature docs: {}", e);
                }
            }
        }
    }

    if queue_enabled && queue.override_flag.is_none() && commit_queue_on_issues_enabled(&repo_root)
    {
        if review.issues_found || review.timed_out {
            println!("ℹ️  Review found issues; keeping commit queued for approval.");
        } else {
            println!("ℹ️  Review passed; skipping queue because commit.queue_on_issues = true.");
            queue_enabled = false;
        }
    }

    let push = push_requested && !queue_enabled;

    let review_run_id = flow_review_run_id(
        &repo_root,
        &diff,
        &review_model_label,
        review_reviewer_label,
    );

    // Continue with normal commit flow
    let commit_message_override = resolve_commit_message_override(&repo_root);

    // Get status
    let status = git_capture_in(&repo_root, &["status", "--short"]).unwrap_or_default();

    // Truncate diff if needed
    let (diff_for_prompt, truncated) = truncate_diff(&diff);

    // Generate commit message based on the review tool
    print!("Generating commit message... ");
    io::stdout().flush()?;
    let message = generate_commit_message_with_fallbacks(
        &repo_root,
        Some(&review_selection_used),
        commit_message_override.as_ref(),
        &diff_for_prompt,
        &status,
        truncated,
    )?;
    println!("done\n");

    // Best-effort: write a private review record into repo-local beads history for later triage.
    // This is written into `.beads/.br_history` inside the current repository.
    if let Err(err) = write_beads_commit_review_record(
        &repo_root,
        review_reviewer_label,
        &review_model_label,
        &review,
        Some(&message),
    ) {
        debug!(
            "failed to write commit review record to repo-local beads: {}",
            err
        );
    }

    let mut gitedit_sessions: Vec<ai::GitEditSessionData> = Vec::new();
    let mut gitedit_session_hash: Option<String> = None;

    let gitedit_mirror_enabled = if force_gitedit {
        gitedit_mirror_enabled_for_commit(&repo_root)
    } else {
        gitedit_mirror_enabled_for_commit_with_check(&repo_root)
    };
    let gitedit_enabled = gitedit_globally_enabled() && gitedit_mirror_enabled;
    let unhash_enabled = include_unhash && unhash_capture_enabled();
    let mut unhash_sessions: Vec<ai::GitEditSessionData> = Vec::new();
    let mut pending_sync_window: Option<MyflowSessionWindow> = None;

    if gitedit_enabled || unhash_enabled {
        let (sessions, window) = collect_sync_sessions_for_pending_commit_with_window(&repo_root);
        pending_sync_window = Some(window);
        if !sessions.is_empty() {
            if gitedit_enabled {
                if let Some((owner, repo)) = get_gitedit_project(&repo_root) {
                    gitedit_session_hash = gitedit_sessions_hash(&owner, &repo, &sessions);
                }
                gitedit_sessions = sessions.clone();
            }
            if unhash_enabled {
                unhash_sessions = sessions;
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

    if unhash_enabled {
        if let Some(unhash_hash) = capture_unhash_bundle(
            &repo_root,
            &diff,
            Some(&status),
            Some(&review),
            Some(&review_model_label),
            Some(review_reviewer_label),
            review_instructions.as_deref(),
            session_context.as_deref(),
            Some(&unhash_sessions),
            gitedit_session_hash.as_deref(),
            &full_message,
            author_message,
            include_context,
        ) {
            full_message = format!("{}\n\nunhash.sh/{}", full_message, unhash_hash);
        }
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

    ensure_no_internal_staged(&repo_root)?;
    ensure_no_unwanted_staged(&repo_root)?;
    gitignore_policy::enforce_staged_policy(&repo_root)?;

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
        ai::log_commit_review(
            &repo_root,
            commit_sha.trim(),
            branch.trim(),
            &full_message,
            &review_model_label,
            review_reviewer_label,
            review.issues_found,
            &review.issues,
            review.summary.as_deref(),
            review.timed_out,
            context_chars,
        );
    } else {
        debug!("failed to capture commit SHA for review log");
    }

    let review_summary = ai::CommitReviewSummary {
        model: review_model_label.clone(),
        reviewer: review_reviewer_label.to_string(),
        issues_found: review.issues_found,
        issues: review.issues.clone(),
        summary: review.summary.clone(),
        timed_out: review.timed_out,
    };
    let context_len = if context_chars > 0 {
        Some(context_chars)
    } else {
        None
    };
    log_commit_event_for_repo(
        &repo_root,
        &full_message,
        "commitWithCheck",
        Some(review_summary),
        context_len,
    );

    // Record review issues as project-scoped todos so they cannot be ignored.
    // This is best-effort: never block commits on todo persistence.
    let mut review_todo_ids: Vec<String> = Vec::new();
    let committed_sha = git_capture_in(&repo_root, &["rev-parse", "HEAD"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if let Some(commit_sha) = committed_sha.as_deref() {
        if !env_flag("FLOW_REVIEW_ISSUES_TODOS_DISABLE") {
            if review.issues_found && !review.issues.is_empty() {
                match todo::record_review_issues_as_todos(
                    &repo_root,
                    commit_sha,
                    &review.issues,
                    review.summary.as_deref(),
                    &review_model_label,
                ) {
                    Ok(ids) => {
                        if !ids.is_empty() {
                            println!("Added {} review issue todo(s) to .ai/todos", ids.len());
                        }
                        review_todo_ids.extend(ids);
                    }
                    Err(err) => println!("⚠ Failed to record review issues as todos: {}", err),
                }
            }

            if review.timed_out {
                let issue = format!(
                    "Re-run review: review timed out for commit {}",
                    short_sha(commit_sha)
                );
                match todo::record_review_issues_as_todos(
                    &repo_root,
                    commit_sha,
                    &vec![issue],
                    review.summary.as_deref(),
                    &review_model_label,
                ) {
                    Ok(ids) => {
                        if !ids.is_empty() {
                            println!("Added {} review todo(s) to .ai/todos", ids.len());
                        }
                        review_todo_ids.extend(ids);
                    }
                    Err(err) => println!("⚠ Failed to record review timeout todo: {}", err),
                }
            }
        }
    }

    // Record review outputs as ephemeral beads in beads_rust
    record_review_outputs_to_beads_rust(
        &repo_root,
        &review,
        review_reviewer_label,
        &review_model_label,
        committed_sha.as_deref(),
        &review_run_id,
    );

    let review_report_path = match write_commit_review_markdown_report(
        &repo_root,
        &review,
        review_reviewer_label,
        &review_model_label,
        committed_sha.as_deref(),
        &full_message,
        &review_run_id,
        &review_todo_ids,
    ) {
        Ok(path) => Some(path),
        Err(err) => {
            println!("⚠ Failed to write review report: {}", err);
            None
        }
    };

    if queue_enabled {
        match queue_commit_for_review(
            &repo_root,
            &full_message,
            Some(&review),
            Some(&review_model_label),
            Some(review_reviewer_label),
            review_todo_ids,
        ) {
            Ok(sha) => {
                print_queue_instructions(&repo_root, &sha);
                if queue.open_review {
                    open_review_in_rise(&repo_root, &sha);
                }
            }
            Err(err) => println!("⚠ Failed to queue commit for review: {}", err),
        }
    }

    // Push if requested
    let mut pushed = false;
    if push {
        let push_remote = config::preferred_git_remote_for_repo(&repo_root);
        let push_branch = git_capture(&["rev-parse", "--abbrev-ref", "HEAD"])
            .unwrap_or_else(|_| "HEAD".to_string())
            .trim()
            .to_string();
        print!("Pushing... ");
        io::stdout().flush()?;

        match git_push_try(&push_remote, &push_branch) {
            PushResult::Success => {
                println!("done");
                pushed = true;
            }
            PushResult::NoRemoteRepo => {
                println!("skipped (no remote repo)");
            }
            PushResult::RemoteAhead => {
                println!("failed (remote ahead)");
                print!("Pulling with rebase... ");
                io::stdout().flush()?;

                match git_pull_rebase_try(&push_remote, &push_branch) {
                    Ok(_) => {
                        println!("done");
                        print!("Pushing... ");
                        io::stdout().flush()?;
                        git_push_run(&push_remote, &push_branch)?;
                        println!("done");
                        pushed = true;
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

    // Record undo action (use full_message which contains the commit message)
    record_undo_action(&repo_root, pushed, Some(&full_message));

    cleanup_staged_snapshot(&staged_snapshot);

    // Advance checkpoint for all commit paths so syncs only include new exchanges.
    save_commit_checkpoint_for_repo(&repo_root);

    // Sync to gitedit if enabled
    let should_sync = if force_gitedit {
        gitedit_enabled
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
            reviewer: Some(review_reviewer_label.to_string()),
        };

        sync_to_gitedit(
            &repo_root,
            "commit_with_check",
            &gitedit_sessions,
            gitedit_session_hash.as_deref(),
            Some(&review_data),
        );

        // Also sync to myflow if enabled
        if myflow_mirror_enabled(&repo_root) {
            sync_to_myflow(
                &repo_root,
                "commit_with_check",
                &gitedit_sessions,
                pending_sync_window.as_ref(),
                Some(&review_data),
                Some(&skill_gate_report),
            );
        }
    } else if myflow_mirror_enabled(&repo_root) {
        // myflow sync even when gitedit sync is skipped
        let review_data = GitEditReviewData {
            diff: Some(diff.clone()),
            issues_found: review.issues_found,
            issues: review.issues.clone(),
            summary: review.summary.clone(),
            reviewer: Some(review_reviewer_label.to_string()),
        };
        // Get AI sessions for myflow even if gitedit didn't collect them
        let (myflow_sessions, myflow_window) =
            collect_sync_sessions_for_commit_with_window(&repo_root);
        sync_to_myflow(
            &repo_root,
            "commit_with_check",
            &myflow_sessions,
            Some(&myflow_window),
            Some(&review_data),
            Some(&skill_gate_report),
        );
    }

    if let Some(path) = review_report_path.as_ref() {
        println!("Review report: {}", path.display());
        println!("Run: f fix {}", path.display());
    }

    Ok(())
}

/// Write a JSON-RPC message to a writer (newline-delimited).
fn codex_write_msg(writer: &mut dyn Write, msg: &serde_json::Value) -> Result<()> {
    let mut line = serde_json::to_string(msg)?;
    line.push('\n');
    writer.write_all(line.as_bytes())?;
    writer.flush()?;
    Ok(())
}

enum CodexAppServerEvent {
    Line(String),
    ReadError(String),
    Closed,
}

enum CodexReadOutcome {
    Message(serde_json::Value),
    TimedOut,
}

fn codex_read_next_message(
    rx: &std::sync::mpsc::Receiver<CodexAppServerEvent>,
    deadline: std::time::Instant,
) -> Result<CodexReadOutcome> {
    use std::cmp;
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Instant;

    loop {
        let now = Instant::now();
        if now >= deadline {
            return Ok(CodexReadOutcome::TimedOut);
        }

        let wait = cmp::min(
            Duration::from_millis(250),
            deadline.saturating_duration_since(now),
        );
        match rx.recv_timeout(wait) {
            Ok(CodexAppServerEvent::Line(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                let msg: serde_json::Value = serde_json::from_str(&line)
                    .with_context(|| format!("invalid JSON from codex: {}", line))?;
                return Ok(CodexReadOutcome::Message(msg));
            }
            Ok(CodexAppServerEvent::ReadError(err)) => bail!("failed to read from codex: {}", err),
            Ok(CodexAppServerEvent::Closed) => bail!("codex app-server closed stdout unexpectedly"),
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => bail!("codex app-server reader disconnected"),
        }
    }
}

/// Read lines until a JSON-RPC response with the expected id arrives.
fn codex_read_response(
    rx: &std::sync::mpsc::Receiver<CodexAppServerEvent>,
    expected_id: u64,
    deadline: std::time::Instant,
) -> Result<serde_json::Value> {
    loop {
        let msg = match codex_read_next_message(rx, deadline)? {
            CodexReadOutcome::Message(msg) => msg,
            CodexReadOutcome::TimedOut => bail!("codex app-server response timed out"),
        };
        if msg.get("id").and_then(|id| id.as_u64()) == Some(expected_id) {
            if let Some(err) = msg.get("error") {
                bail!(
                    "codex error: {}",
                    err.get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("unknown error")
                );
            }
            return Ok(msg);
        }
    }
}

fn codex_read_response_with_notifications<F>(
    rx: &std::sync::mpsc::Receiver<CodexAppServerEvent>,
    expected_id: u64,
    deadline: std::time::Instant,
    mut on_notification: F,
) -> Result<serde_json::Value>
where
    F: FnMut(&serde_json::Value),
{
    loop {
        let msg = match codex_read_next_message(rx, deadline)? {
            CodexReadOutcome::Message(msg) => msg,
            CodexReadOutcome::TimedOut => bail!("codex app-server response timed out"),
        };

        if msg.get("method").is_some() && msg.get("id").is_none() {
            on_notification(&msg);
        }

        if msg.get("id").and_then(|id| id.as_u64()) == Some(expected_id) {
            if let Some(err) = msg.get("error") {
                bail!(
                    "codex error: {}",
                    err.get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("unknown error")
                );
            }
            return Ok(msg);
        }
    }
}

fn openrouter_review_should_use_codex() -> bool {
    // Default: true (use Codex /review when available) to improve commit-check quality.
    // Allow opt-out for cases where the user explicitly wants OpenRouter.
    match env::var("FLOW_OPENROUTER_REVIEW_USE_CODEX") {
        Ok(v) if v.trim() == "0" || v.trim().eq_ignore_ascii_case("false") => false,
        _ => true,
    }
}

fn beads_rust_history_dir(repo_root: &Path) -> PathBuf {
    repo_root
        .join(".beads")
        .join(".br_history")
        .join("flow_commit_reviews")
}

fn beads_rust_beads_dir(repo_root: &Path) -> PathBuf {
    repo_root.join(".beads")
}

fn flow_commit_reports_dir() -> Option<PathBuf> {
    if let Ok(value) = env::var("FLOW_COMMIT_REPORT_DIR") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    dirs::home_dir().map(|home| home.join(".flow").join("commits"))
}

fn write_commit_review_markdown_report(
    repo_root: &Path,
    review: &ReviewResult,
    reviewer: &str,
    model_label: &str,
    committed_sha: Option<&str>,
    commit_message: &str,
    review_run_id: &str,
    review_todo_ids: &[String],
) -> Result<PathBuf> {
    let Some(report_dir) = flow_commit_reports_dir() else {
        bail!("could not resolve commit report directory");
    };
    fs::create_dir_all(&report_dir)?;

    let project_name = flow_project_name(repo_root);
    let branch = git_capture_in(repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .unwrap_or_else(|_| "unknown".to_string())
        .trim()
        .to_string();
    let sha_short = committed_sha.map(short_sha).unwrap_or("unknown");
    let stamp = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
    let file_name = format!(
        "{}-{}-{}-{}.md",
        safe_label_value(&project_name),
        safe_label_value(&branch),
        sha_short,
        stamp
    );
    let path = report_dir.join(file_name);

    let mut md = String::new();
    md.push_str("# Flow Commit Review\n\n");
    md.push_str("- Generated: ");
    md.push_str(&chrono::Utc::now().to_rfc3339());
    md.push_str("\n- Project: ");
    md.push_str(&project_name);
    md.push_str("\n- Repo Root: ");
    md.push_str(&repo_root.display().to_string());
    md.push_str("\n- Branch: ");
    md.push_str(&branch);
    md.push_str("\n- Commit: ");
    md.push_str(sha_short);
    md.push_str("\n- Reviewer: ");
    md.push_str(reviewer);
    md.push_str("\n- Model: ");
    md.push_str(model_label);
    md.push_str("\n- Review Run ID: ");
    md.push_str(review_run_id);
    md.push_str("\n- Timed Out: ");
    md.push_str(if review.timed_out { "yes" } else { "no" });
    md.push_str("\n\n## Commit Message\n\n```text\n");
    md.push_str(commit_message.trim());
    md.push_str("\n```\n");

    if let Some(summary) = review
        .summary
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        md.push_str("\n## Summary\n\n");
        md.push_str(summary);
        md.push('\n');
    }

    md.push_str("\n## Issues\n\n");
    if review.issues.is_empty() {
        md.push_str("0. (none)\n");
    } else {
        for (idx, issue) in review.issues.iter().enumerate() {
            md.push_str(&(idx + 1).to_string());
            md.push_str(". ");
            md.push_str(issue.trim());
            md.push('\n');
        }
    }

    md.push_str("\n## Future Tasks\n\n");
    if review.future_tasks.is_empty() {
        md.push_str("0. (none)\n");
    } else {
        for (idx, task) in review.future_tasks.iter().enumerate() {
            md.push_str(&(idx + 1).to_string());
            md.push_str(". ");
            md.push_str(task.trim());
            md.push('\n');
        }
    }

    if !review_todo_ids.is_empty() {
        md.push_str("\n## Todo IDs\n\n");
        for todo_id in review_todo_ids {
            md.push_str("- ");
            md.push_str(todo_id.trim());
            md.push('\n');
        }
    }

    md.push_str("\n## Next Step\n\n```bash\nf fix ");
    md.push_str(&path.display().to_string());
    md.push_str("\n```\n");

    fs::write(&path, md).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

fn write_beads_commit_review_record(
    repo_root: &Path,
    reviewer: &str,
    model_label: &str,
    review: &ReviewResult,
    commit_message: Option<&str>,
) -> Result<()> {
    #[derive(Serialize)]
    struct BeadsCommitReviewRecord<'a> {
        timestamp: String,
        repo_root: String,
        repo_name: String,
        branch: String,
        reviewer: &'a str,
        model: &'a str,
        issues_found: bool,
        issues: Vec<String>,
        future_tasks: Vec<String>,
        summary: Option<String>,
        commit_message: Option<String>,
    }

    let dir = beads_rust_history_dir(repo_root);
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;

    let repo_name = repo_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("repo")
        .to_string();
    let branch = git_capture_in(repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .unwrap_or_else(|_| "unknown".to_string())
        .trim()
        .to_string();
    let ts = chrono::Utc::now().to_rfc3339();
    let stamp = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
    let safe_repo = repo_name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect::<String>();
    let safe_branch = branch
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect::<String>();

    let path = dir.join(format!(
        "review.{}.{}.{}.json",
        stamp, safe_repo, safe_branch
    ));

    let record = BeadsCommitReviewRecord {
        timestamp: ts,
        repo_root: repo_root.display().to_string(),
        repo_name,
        branch,
        reviewer,
        model: model_label,
        issues_found: review.issues_found,
        issues: review.issues.clone(),
        future_tasks: review.future_tasks.clone(),
        summary: review.summary.clone(),
        commit_message: commit_message.map(|s| s.to_string()),
    };
    let json = serde_json::to_string_pretty(&record)?;
    fs::write(&path, json)?;
    Ok(())
}

/// Run Codex app-server `review/start` to review staged changes.
///
/// Spawns `codex app-server` over stdio JSON-RPC, sends initialize handshake,
/// creates a thread, and uses the built-in `review/start` method which is
/// optimized for code review (structured findings, confidence scores, etc.).
fn run_codex_review(
    _diff: &str,
    session_context: Option<&str>,
    review_instructions: Option<&str>,
    workdir: &std::path::Path,
    model: CodexModel,
) -> Result<ReviewResult> {
    let max_attempts = commit_with_check_review_retries() + 1; // retries + initial attempt
    let mut last_timeout_secs = 0u64;

    for attempt in 1..=max_attempts {
        match run_codex_review_once(_diff, session_context, review_instructions, workdir, model) {
            Ok(result) if result.timed_out && attempt < max_attempts => {
                last_timeout_secs = commit_with_check_timeout_secs();
                let backoff_secs = commit_with_check_retry_backoff_secs(attempt);
                println!(
                    "⚠ Review timed out after {}s, retrying ({}/{}) in {}s...",
                    last_timeout_secs, attempt, max_attempts, backoff_secs
                );
                std::thread::sleep(Duration::from_secs(backoff_secs));
                continue;
            }
            other => return other,
        }
    }

    // Should not reach here, but just in case
    Ok(ReviewResult {
        issues_found: false,
        issues: Vec::new(),
        summary: Some(format!(
            "Codex review timed out after {}s (exhausted {} attempts)",
            last_timeout_secs, max_attempts
        )),
        future_tasks: Vec::new(),
        timed_out: true,
        quality: None,
    })
}

fn run_codex_review_once(
    _diff: &str,
    session_context: Option<&str>,
    review_instructions: Option<&str>,
    workdir: &std::path::Path,
    model: CodexModel,
) -> Result<ReviewResult> {
    use std::io::{BufRead, BufReader};
    use std::sync::mpsc;
    use std::time::Instant;

    let timeout = Duration::from_secs(commit_with_check_timeout_secs());

    let mut developer_instructions = String::new();
    if let Some(instructions) = review_instructions {
        if !instructions.trim().is_empty() {
            developer_instructions.push_str("Additional review instructions:\n");
            developer_instructions.push_str(instructions.trim());
            developer_instructions.push_str("\n\n");
        }
    }
    if let Some(ctx) = session_context {
        if !ctx.trim().is_empty() {
            developer_instructions.push_str("Context:\n");
            developer_instructions.push_str(ctx.trim());
            developer_instructions.push_str("\n\n");
        }
    }

    let codex_bin = configured_codex_bin_for_workdir(workdir);

    // Spawn codex app-server (JSON-RPC over stdio)
    let mut child = Command::new(&codex_bin)
        .arg("app-server")
        .current_dir(workdir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to run codex app-server - is codex installed?")?;

    let mut stdin = child.stdin.take().context("missing stdin")?;
    let stdout = child.stdout.take().context("missing stdout")?;
    let (line_tx, line_rx) = mpsc::channel::<CodexAppServerEvent>();
    let reader_handle = std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    if line_tx.send(CodexAppServerEvent::Line(line)).is_err() {
                        return;
                    }
                }
                Err(err) => {
                    let _ = line_tx.send(CodexAppServerEvent::ReadError(err.to_string()));
                    return;
                }
            }
        }
        let _ = line_tx.send(CodexAppServerEvent::Closed);
    });
    let handshake_deadline = Instant::now() + Duration::from_secs(15);

    // Step 1: Initialize handshake
    codex_write_msg(
        &mut stdin,
        &json!({
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": { "name": "flow", "title": "Flow CLI", "version": "0.1.0" },
                "capabilities": { "experimentalApi": true }
            }
        }),
    )?;
    let _init_resp = codex_read_response(&line_rx, 1, handshake_deadline)
        .context("codex app-server did not respond to initialize")?;

    // Step 2: Send initialized notification
    codex_write_msg(&mut stdin, &json!({ "method": "initialized" }))?;

    // Step 3: Start a thread
    let op_deadline = Instant::now() + timeout;
    codex_write_msg(
        &mut stdin,
        &json!({
            "id": 2,
            "method": "thread/start",
            "params": {
                "cwd": workdir.to_string_lossy(),
                "approvalPolicy": "never",
                "sandbox": "read-only",
                "model": model.as_codex_arg(),
                "developerInstructions": if developer_instructions.trim().is_empty() { serde_json::Value::Null } else { json!(developer_instructions.trim()) }
            }
        }),
    )?;
    let thread_resp = codex_read_response(&line_rx, 2, op_deadline)?;
    let thread_id = thread_resp
        .pointer("/result/threadId")
        .or_else(|| thread_resp.pointer("/result/thread/id"))
        .and_then(|v| v.as_str())
        .context("failed to get threadId from codex")?
        .to_string();

    // Step 4: Start review using review/start with appropriate target
    let target = json!({ "type": "uncommittedChanges" });

    codex_write_msg(
        &mut stdin,
        &json!({
            "id": 3,
            "method": "review/start",
            "params": {
                "threadId": thread_id,
                "target": target,
                "delivery": "inline"
            }
        }),
    )?;
    let _review_resp = codex_read_response(&line_rx, 3, op_deadline)?;

    // Step 5: Collect streaming events until we see the ExitedReviewMode item.
    let mut review_text: Option<String> = None;
    let mut timed_out = false;
    let review_start = Instant::now();
    let hard_cap = Duration::from_secs(commit_with_check_timeout_secs().saturating_mul(3));
    let hard_deadline = review_start + hard_cap;
    let mut idle_deadline = review_start + timeout;

    loop {
        let next_deadline = std::cmp::min(idle_deadline, hard_deadline);
        let msg = match codex_read_next_message(&line_rx, next_deadline)? {
            CodexReadOutcome::Message(msg) => msg,
            CodexReadOutcome::TimedOut => {
                timed_out = true;
                break;
            }
        };
        idle_deadline = Instant::now() + timeout;
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        match method {
            "item/completed" => {
                let thread_id_msg = msg.pointer("/params/threadId").and_then(|v| v.as_str());
                if thread_id_msg != Some(thread_id.as_str()) {
                    continue;
                }
                let item_type = msg.pointer("/params/item/type").and_then(|v| v.as_str());
                if item_type == Some("exitedReviewMode") {
                    if let Some(text) = msg.pointer("/params/item/review").and_then(|v| v.as_str())
                    {
                        review_text = Some(text.to_string());
                        break;
                    }
                }
            }
            "review/completed" => {
                let thread_id_msg = msg.pointer("/params/threadId").and_then(|v| v.as_str());
                if thread_id_msg != Some(thread_id.as_str()) {
                    continue;
                }
                if let Some(text) = msg
                    .pointer("/params/review")
                    .or_else(|| msg.pointer("/params/item/review"))
                    .and_then(|v| v.as_str())
                {
                    review_text = Some(text.to_string());
                    break;
                }
            }
            _ => {}
        }
    }

    let review_text = review_text.unwrap_or_default();

    if timed_out {
        // Best-effort cleanup
        let _ = codex_write_msg(
            &mut stdin,
            &json!({
                "id": 4,
                "method": "thread/archive",
                "params": { "threadId": thread_id }
            }),
        );
        drop(stdin);
        let _ = child.kill();
        let _ = child.wait();
        let _ = reader_handle.join();

        return Ok(ReviewResult {
            issues_found: false,
            issues: Vec::new(),
            summary: Some(format!(
                "Codex review timed out after {}s",
                review_start.elapsed().as_secs()
            )),
            future_tasks: Vec::new(),
            timed_out: true,
            quality: None,
        });
    }

    let result = review_text.trim().to_string();
    if !result.is_empty() {
        println!("{}", result);
    }

    // Codex review output is plain text. Convert it into the structured JSON
    // format expected by the rest of Flow via a small follow-up turn.
    let mut json_output = String::new();
    let conversion_deadline = Instant::now() + Duration::from_secs(60);
    let conversion_prompt = format!(
        "Convert the following code review into JSON ONLY with this exact schema: \
{{\"issues_found\": true/false, \"issues\": [\"...\"], \"summary\": \"...\", \"future_tasks\": [\"...\"]}}.\n\
Rules:\n\
- Put concrete, actionable problems in issues (include file paths/line hints when present).\n\
- future_tasks are optional follow-up improvements (max 3), not duplicates of issues.\n\
- If review contains no concrete issues, set issues_found=false and issues=[].\n\
Review:\n{}",
        result
    );

    codex_write_msg(
        &mut stdin,
        &json!({
            "id": 5,
            "method": "turn/start",
            "params": {
                "threadId": thread_id,
                "cwd": workdir.to_string_lossy(),
                "approvalPolicy": "never",
                "sandboxPolicy": { "type": "readOnly" },
                "input": [{ "type": "text", "text": conversion_prompt }]
            }
        }),
    )?;
    let _turn_resp =
        codex_read_response_with_notifications(&line_rx, 5, conversion_deadline, |msg| {
            let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
            if method != "item/agentMessage/delta" {
                return;
            }
            let thread_id_msg = msg.pointer("/params/threadId").and_then(|v| v.as_str());
            if thread_id_msg != Some(thread_id.as_str()) {
                return;
            }
            if let Some(delta) = msg.pointer("/params/delta").and_then(|v| v.as_str()) {
                json_output.push_str(delta);
            }
        })?;

    // Now stream until turn/completed for this thread, collecting agent deltas.
    loop {
        let msg = match codex_read_next_message(&line_rx, conversion_deadline)? {
            CodexReadOutcome::Message(msg) => msg,
            CodexReadOutcome::TimedOut => break,
        };
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        match method {
            "item/agentMessage/delta" => {
                let thread_id_msg = msg.pointer("/params/threadId").and_then(|v| v.as_str());
                if thread_id_msg != Some(thread_id.as_str()) {
                    continue;
                }
                if let Some(delta) = msg.pointer("/params/delta").and_then(|v| v.as_str()) {
                    json_output.push_str(delta);
                }
            }
            "turn/completed" => {
                let thread_id_msg = msg.pointer("/params/threadId").and_then(|v| v.as_str());
                if thread_id_msg == Some(thread_id.as_str()) {
                    break;
                }
            }
            _ => {}
        }
    }

    let json_output = json_output.trim().to_string();
    let mut review_json = parse_review_json(&json_output);
    let future_tasks = review_json
        .as_ref()
        .map(|parsed| normalize_future_tasks(&parsed.future_tasks))
        .unwrap_or_default();
    let summary = review_json.as_ref().and_then(|r| r.summary.clone());
    let quality = review_json.as_mut().and_then(|r| r.quality.take());
    let (issues_found, issues) = if let Some(ref parsed) = review_json {
        (parsed.issues_found, parsed.issues.clone())
    } else if result.is_empty() {
        (false, Vec::new())
    } else {
        // Fallback: parse bullet items from Codex's rendered review text.
        let mut issues = Vec::new();
        for line in result.lines() {
            let trimmed = line.trim_start();
            if let Some(rest) = trimmed.strip_prefix("- ") {
                let t = rest.trim();
                if !t.is_empty() {
                    issues.push(t.to_string());
                }
            }
        }
        (!issues.is_empty(), issues)
    };

    // Cleanup: archive thread and kill process
    let _ = codex_write_msg(
        &mut stdin,
        &json!({
            "id": 4,
            "method": "thread/archive",
            "params": { "threadId": thread_id }
        }),
    );
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    let _ = reader_handle.join();

    Ok(ReviewResult {
        issues_found,
        issues,
        summary,
        future_tasks,
        timed_out: false,
        quality,
    })
}

pub(crate) fn configured_codex_bin_for_workdir(workdir: &Path) -> String {
    if let Ok(value) = env::var("CODEX_BIN") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    let mut roots: Vec<PathBuf> = vec![workdir.to_path_buf()];
    if let Ok(repo_root) = git_capture_in(workdir, &["rev-parse", "--show-toplevel"]) {
        let trimmed = repo_root.trim();
        if !trimmed.is_empty() {
            let root = PathBuf::from(trimmed);
            if !roots.iter().any(|r| r == &root) {
                roots.push(root);
            }
        }
    }

    for root in roots {
        let cfg_path = root.join("flow.toml");
        if !cfg_path.exists() {
            continue;
        }
        if let Ok(cfg) = config::load(&cfg_path) {
            if let Some(bin) = cfg.options.codex_bin {
                let trimmed = bin.trim();
                if !trimmed.is_empty() {
                    return trimmed.to_string();
                }
            }
        }
    }

    let global_cfg = config::default_config_path();
    if global_cfg.exists() {
        if let Ok(cfg) = config::load(&global_cfg) {
            if let Some(bin) = cfg.options.codex_bin {
                let trimmed = bin.trim();
                if !trimmed.is_empty() {
                    return trimmed.to_string();
                }
            }
        }
    }

    "codex".to_string()
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

    let client = crate::http_client::blocking_with_timeout(Duration::from_secs(
        commit_with_check_timeout_secs(),
    ))
    .context("failed to create HTTP client for remote review")?;

    let mut request = client.post(&review_url).json(&payload);
    if let Some(token) = commit_with_check_review_token() {
        request = request.bearer_auth(token);
    }

    let response = request
        .send()
        .context("failed to send remote review request")?;

    if !response.status().is_success() {
        if response.status() == StatusCode::UNAUTHORIZED {
            bail!("remote review unauthorized. Run `f auth` to login.");
        }
        if response.status() == StatusCode::PAYMENT_REQUIRED {
            bail!("remote review requires an active subscription. Visit myflow to subscribe.");
        }
        bail!("remote review failed: HTTP {}", response.status());
    }

    let payload: RemoteReviewResponse = response
        .json()
        .context("failed to parse remote review response")?;

    if !payload.stderr.trim().is_empty() {
        debug!(stderr = payload.stderr.as_str(), "remote claude stderr");
    }

    let result = payload.output;
    let mut review_json = parse_review_json(&result);
    let future_tasks = review_json
        .as_ref()
        .map(|parsed| normalize_future_tasks(&parsed.future_tasks))
        .unwrap_or_default();
    let summary = review_json.as_ref().and_then(|r| r.summary.clone());
    let quality = review_json.as_mut().and_then(|r| r.quality.take());
    let (issues_found, issues) = if let Some(ref parsed) = review_json {
        if let Some(summary) = parsed.summary.as_ref() {
            debug!(summary = summary.as_str(), "remote claude review summary");
        }
        (parsed.issues_found, parsed.issues.clone())
    } else if result.trim().is_empty() {
        (false, Vec::new())
    } else {
        debug!(
            review_output = result.as_str(),
            "remote claude review output"
        );
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
        summary,
        future_tasks,
        timed_out: false,
        quality,
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
        let mut prompt = String::from(
            "Review diff for bugs, security, perf issues. Return JSON: {\"issues_found\":bool,\"issues\":[\"...\"],\"summary\":\"...\",\"future_tasks\":[\"...\"]}. future_tasks are optional follow-up improvements or optimizations (max 3), actionable, and not duplicates of issues; use [] if none.\n",
        );

        // Add quality assessment instructions if quality gates are enabled
        let quality_config = config::load_or_default(workdir.join("flow.toml"))
            .commit
            .and_then(|c| c.quality)
            .unwrap_or_default();
        let quality_mode = quality_config.mode.as_deref().unwrap_or("warn");
        if quality_mode != "off" {
            prompt.push_str(
                "\nAdditionally, analyze the diff for quality assessment. Add a \"quality\" object to your JSON response:\n\
                 {\"quality\":{\"features_touched\":[{\"name\":\"kebab-name\",\"action\":\"added|modified|fixed\",\"description\":\"one sentence\",\"files_changed\":[\"...\"],\"has_tests\":bool,\"test_files\":[\"...\"],\"doc_current\":bool}],\
                 \"new_features\":[{\"name\":\"kebab-name\",\"description\":\"one sentence\",\"files\":[\"...\"],\"doc_content\":\"# Title\\n\\nDescription...\"}],\
                 \"test_coverage\":\"full|partial|none\",\"doc_coverage\":\"full|partial|none\",\"gate_pass\":bool,\"gate_failures\":[\"...\"]}}\n\
                 A \"feature\" = a user-visible capability, API endpoint, or CLI command. Name features in kebab-case. \
                 gate_pass is false if new features lack tests or docs. gate_failures lists specific reasons.\n",
            );
            // Add features context (existing documented features) if available
            let features_ctx = crate::features::features_context_for_review(
                workdir,
                &changed_files_from_diff(diff),
            );
            if !features_ctx.is_empty() {
                prompt.push_str(&features_ctx);
            }
        }

        // Add custom review instructions if provided
        if let Some(instructions) = review_instructions {
            prompt.push_str(&format!(
                "\nAdditional review instructions:\n{}\n",
                instructions
            ));
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

        // Write prompt to stdin and explicitly close it
        {
            let mut stdin = child.stdin.take().context("failed to get stdin")?;
            stdin
                .write_all(prompt.as_bytes())
                .context("failed to write prompt to claude stdin")?;
            stdin.flush().context("failed to flush stdin")?;
            drop(stdin); // Explicitly close stdin to signal EOF
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
                        if prompt_yes_no(
                            "Claude review is taking longer than expected. Keep waiting?",
                        )? {
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
                future_tasks: Vec::new(),
                timed_out: true,
                quality: None,
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

        let mut review_json = parse_review_json(&result);
        let future_tasks = review_json
            .as_ref()
            .map(|parsed| normalize_future_tasks(&parsed.future_tasks))
            .unwrap_or_default();
        let summary = review_json.as_ref().and_then(|r| r.summary.clone());
        let quality = review_json.as_mut().and_then(|r| r.quality.take());
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
            summary,
            future_tasks,
            timed_out: false,
            quality,
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
                future_tasks: Vec::new(),
                timed_out: false,
                quality: None,
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
    use std::io::{BufRead, BufReader, Write};

    let (diff_for_prompt, _truncated) = truncate_diff(diff);

    // Write diff to a temp file in the working directory to avoid /tmp permission issues
    let diff_file = workdir.join(".flow_diff_review.tmp");
    {
        let mut f = std::fs::File::create(&diff_file).context("failed to create temp diff file")?;
        f.write_all(diff_for_prompt.as_bytes())
            .context("failed to write temp diff file")?;
    }

    // Build review prompt - explicitly say to output to stdout only
    let mut prompt = String::from(
        "Review the attached git diff file for bugs, security issues, and performance problems. \
         Output ONLY a JSON object to stdout with this exact format (do not write any files): \
         {\"issues_found\": true/false, \"issues\": [\"issue 1\", \"issue 2\"], \"summary\": \"brief summary\", \"future_tasks\": [\"optional follow-up\"]}. \
         future_tasks are optional improvements/optimizations (max 3), actionable, and not duplicates of issues; use [] if none.",
    );

    if let Some(instructions) = review_instructions {
        prompt.push_str(&format!(
            "\n\nAdditional review instructions:\n{}",
            instructions
        ));
    }

    if let Some(context) = session_context {
        prompt.push_str(&format!("\n\nContext:\n{}", context));
    }

    // Run opencode with the diff as an attached file
    let mut child = Command::new("opencode")
        .args([
            "run",
            "--model",
            model,
            "-f",
            diff_file.to_str().unwrap(),
            &prompt,
        ])
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
    let mut review_json = parse_review_json(&output);
    let future_tasks = review_json
        .as_ref()
        .map(|json| normalize_future_tasks(&json.future_tasks))
        .unwrap_or_default();
    let summary = review_json.as_ref().and_then(|r| r.summary.clone());
    let quality = review_json.as_mut().and_then(|r| r.quality.take());
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

    // Clean up temp file
    let _ = std::fs::remove_file(&diff_file);

    Ok(ReviewResult {
        issues_found,
        issues,
        summary,
        future_tasks,
        timed_out: false,
        quality,
    })
}

/// Run Kimi CLI to review staged changes for bugs and performance issues.
fn changed_files_from_diff(diff: &str) -> Vec<String> {
    let mut files = Vec::new();
    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            if path != "/dev/null" {
                files.push(path.to_string());
            }
        }
    }
    files.sort();
    files.dedup();
    files
}

fn issue_mentions_changed_file(issue: &str, files: &[String]) -> bool {
    for file in files {
        if issue.contains(file) {
            return true;
        }
        let with_b = format!("b/{}", file);
        if issue.contains(&with_b) {
            return true;
        }
        let with_dot = format!("./{}", file);
        if issue.contains(&with_dot) {
            return true;
        }
    }
    false
}

fn run_kimi_review(
    diff: &str,
    session_context: Option<&str>,
    review_instructions: Option<&str>,
    _workdir: &std::path::Path,
    model: Option<&str>,
) -> Result<ReviewResult> {
    use std::io::{BufRead, Read, Write};
    use std::sync::mpsc;
    use std::thread;

    let (diff_for_prompt, truncated) = truncate_diff(diff);

    let mut prompt = String::from(
        "Review this git diff for bugs, security issues, and performance problems. \
         Only report issues that are directly supported by this diff. \
         Each issue MUST include a file path and line number from the diff, in the format: \
         \"path/to/file:line - description (evidence: `exact diff line`)\". \
         Output ONLY a JSON object with this exact format: \
         {\"issues_found\": true/false, \"issues\": [\"issue 1\", \"issue 2\"], \"summary\": \"brief summary\", \"future_tasks\": [\"optional follow-up\"]}. \
         future_tasks are optional improvements/optimizations (max 3), actionable, and not duplicates of issues; use [] if none. \
         If you cannot find concrete issues in the diff, set issues_found=false and issues=[].\n\n\
         Git diff:\n",
    );
    prompt.push_str(&diff_for_prompt);

    if truncated {
        prompt.push_str("\n\n[Diff truncated]");
    }

    if let Some(instructions) = review_instructions {
        prompt.push_str(&format!(
            "\n\nAdditional review instructions:\n{}",
            instructions
        ));
    }

    if let Some(context) = session_context {
        prompt.push_str(&format!("\n\nContext:\n{}", context));
    }

    info!(
        model = model.unwrap_or("default"),
        prompt_len = prompt.len(),
        "calling kimi for code review"
    );

    let mut cmd = Command::new("kimi");
    cmd.args([
        "--print",
        "--input-format",
        "text",
        "--output-format",
        "stream-json",
    ]);
    if let Some(model) = model {
        if !model.trim().is_empty() {
            cmd.args(["--model", model]);
        }
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().context("failed to run kimi for review")?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(prompt.as_bytes())
            .context("failed to write prompt to kimi")?;
    }

    let stdout = child
        .stdout
        .take()
        .context("failed to capture kimi stdout")?;
    let stderr = child
        .stderr
        .take()
        .context("failed to capture kimi stderr")?;

    let (stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>();
    let (stderr_tx, stderr_rx) = mpsc::channel::<String>();

    let stdout_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::BufReader::new(stdout).read_to_end(&mut buf);
        let _ = stdout_tx.send(buf);
    });

    let stderr_handle = thread::spawn(move || {
        let mut collected = String::new();
        let reader = std::io::BufReader::new(stderr);
        for line in reader.lines().flatten() {
            // Stream stderr (progress/errors) to console
            if !line.trim().is_empty() {
                eprintln!("{}", line);
            }
            collected.push_str(&line);
            collected.push('\n');
        }
        let _ = stderr_tx.send(collected);
    });

    let status = child.wait().context("failed to wait for kimi")?;
    let _ = stdout_handle.join();
    let _ = stderr_handle.join();

    let stdout_bytes = stdout_rx.recv().unwrap_or_default();
    let stderr_text = stderr_rx.recv().unwrap_or_default();

    if !status.success() {
        let stdout_text = String::from_utf8_lossy(&stdout_bytes);
        let error_msg = if stderr_text.trim().is_empty() {
            stdout_text.trim()
        } else {
            stderr_text.trim()
        };
        bail!("kimi review failed: {}", error_msg);
    }

    let stdout_text = String::from_utf8_lossy(&stdout_bytes).trim().to_string();
    if stdout_text.is_empty() {
        bail!("kimi returned empty output");
    }

    // Parse the stream-json output from kimi
    // Format: {"role":"assistant","content":[{"type":"think","think":"..."},{"type":"text","text":"..."}]}
    let result = extract_kimi_text_content(&stdout_text).unwrap_or_else(|| stdout_text.clone());
    if result.is_empty() {
        bail!("kimi returned empty review output (no text content in response)");
    }

    // Try to parse JSON from output
    let mut review_json = parse_review_json(&result);
    let future_tasks = review_json
        .as_ref()
        .map(|json| normalize_future_tasks(&json.future_tasks))
        .unwrap_or_default();
    let mut summary = review_json.as_ref().and_then(|r| r.summary.clone());
    let quality = review_json.as_mut().and_then(|r| r.quality.take());
    let (mut issues_found, mut issues) = if let Some(ref json) = review_json {
        (json.issues_found, json.issues.clone())
    } else {
        let lowered = result.to_lowercase();
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

    let changed_files = changed_files_from_diff(diff);
    if !issues.is_empty() && !changed_files.is_empty() {
        let before = issues.len();
        issues.retain(|issue| issue_mentions_changed_file(issue, &changed_files));
        let dropped = before.saturating_sub(issues.len());
        if dropped > 0 {
            let note = format!(
                "Filtered {} unverified issue(s) that did not reference files in the diff.",
                dropped
            );
            let summary = match summary.take() {
                Some(existing) if !existing.is_empty() => format!("{} {}", existing, note),
                _ => note,
            };
            issues_found = !issues.is_empty();
            return Ok(ReviewResult {
                issues_found,
                issues,
                summary: Some(summary),
                future_tasks,
                timed_out: false,
                quality: quality.clone(),
            });
        }
    }

    if issues.is_empty() {
        issues_found = false;
    }

    Ok(ReviewResult {
        issues_found,
        issues,
        summary,
        future_tasks,
        timed_out: false,
        quality,
    })
}

fn run_openrouter_review(
    diff: &str,
    session_context: Option<&str>,
    review_instructions: Option<&str>,
    _workdir: &std::path::Path,
    model: &str,
) -> Result<ReviewResult> {
    let (diff_for_prompt, truncated) = truncate_diff(diff);

    let mut prompt = String::from(
        "Review this git diff for bugs, security issues, and performance problems. \
         Only report issues that are directly supported by this diff. \
         Each issue MUST include a file path and line number from the diff, in the format: \
         \"path/to/file:line - description (evidence: `exact diff line`)\". \
         Output ONLY a JSON object with this exact format: \
         {\"issues_found\": true/false, \"issues\": [\"issue 1\", \"issue 2\"], \"summary\": \"brief summary\", \"future_tasks\": [\"optional follow-up\"]}. \
         future_tasks are optional improvements/optimizations (max 3), actionable, and not duplicates of issues; use [] if none. \
         If you cannot find concrete issues in the diff, set issues_found=false and issues=[].\n\n\
         Git diff:\n",
    );
    prompt.push_str(&diff_for_prompt);

    if truncated {
        prompt.push_str("\n\n[Diff truncated]");
    }

    if let Some(instructions) = review_instructions {
        prompt.push_str(&format!(
            "\n\nAdditional review instructions:\n{}",
            instructions
        ));
    }

    if let Some(context) = session_context {
        prompt.push_str(&format!("\n\nContext:\n{}", context));
    }

    let api_key = openrouter_api_key()?;
    let model_id = openrouter_model_id(model);

    let client = openrouter_http_client(Duration::from_secs(120))?;

    let body = ChatRequest {
        model: model_id.to_string(),
        messages: vec![
            Message {
                role: "system".to_string(),
                content: "You are a code reviewer. Analyze code changes for bugs, security issues, and performance problems. Output JSON only.".to_string(),
            },
            Message {
                role: "user".to_string(),
                content: prompt,
            },
        ],
        temperature: 0.3,
    };

    info!(
        model = model_id,
        prompt_len = body.messages[1].content.len(),
        "calling OpenRouter for code review"
    );
    let start = std::time::Instant::now();

    let parsed: ChatResponse = openrouter_chat_completion_with_retry(&client, &api_key, &body)
        .context("OpenRouter request failed")?;

    info!(
        elapsed_ms = start.elapsed().as_millis() as u64,
        "OpenRouter responded"
    );

    let output = parsed
        .choices
        .first()
        .and_then(|c| c.message.as_ref())
        .map(|m| m.content.trim().to_string())
        .unwrap_or_default();

    if output.is_empty() {
        bail!("OpenRouter returned empty review output");
    }

    println!("{}", output);

    let mut review_json = parse_review_json(&output);
    let future_tasks = review_json
        .as_ref()
        .map(|json| normalize_future_tasks(&json.future_tasks))
        .unwrap_or_default();
    let mut summary = review_json.as_ref().and_then(|r| r.summary.clone());
    let quality = review_json.as_mut().and_then(|r| r.quality.take());
    let (mut issues_found, mut issues) = if let Some(ref json) = review_json {
        (json.issues_found, json.issues.clone())
    } else {
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

    let changed_files = changed_files_from_diff(diff);
    if !issues.is_empty() && !changed_files.is_empty() {
        let before = issues.len();
        issues.retain(|issue| issue_mentions_changed_file(issue, &changed_files));
        let dropped = before.saturating_sub(issues.len());
        if dropped > 0 {
            let note = format!(
                "Filtered {} unverified issue(s) that did not reference files in the diff.",
                dropped
            );
            let summary = match summary.take() {
                Some(existing) if !existing.is_empty() => format!("{} {}", existing, note),
                _ => note,
            };
            issues_found = !issues.is_empty();
            return Ok(ReviewResult {
                issues_found,
                issues,
                summary: Some(summary),
                future_tasks,
                timed_out: false,
                quality: quality.clone(),
            });
        }
    }

    if issues.is_empty() {
        issues_found = false;
    }

    Ok(ReviewResult {
        issues_found,
        issues,
        summary,
        future_tasks,
        timed_out: false,
        quality,
    })
}

const OPENROUTER_CHAT_URL: &str = "https://openrouter.ai/api/v1/chat/completions";

fn openrouter_http_client(timeout: Duration) -> Result<Client> {
    Client::builder()
        .timeout(timeout)
        // OpenRouter occasionally drops pooled connections mid-body, producing
        // `unexpected EOF during chunk size line`. Disabling idle pooling makes
        // these transient failures much rarer for CLI-style, low-QPS usage.
        .pool_max_idle_per_host(0)
        .build()
        .context("failed to create HTTP client")
}

fn openrouter_should_retry_error(err: &reqwest::Error) -> bool {
    if err.is_timeout() || err.is_connect() || err.is_body() {
        return true;
    }

    // reqwest/hyper doesn't expose a stable typed error for this; match common symptoms.
    let msg = err.to_string().to_lowercase();
    msg.contains("unexpected eof")
        || msg.contains("chunk size line")
        || msg.contains("connection closed")
        || msg.contains("incomplete message")
}

fn openrouter_retry_after(resp: &reqwest::blocking::Response) -> Option<Duration> {
    let value = resp.headers().get("retry-after")?.to_str().ok()?;
    // Spec also allows HTTP-date; we only handle integer seconds.
    let secs: u64 = value.trim().parse().ok()?;
    Some(Duration::from_secs(secs))
}

fn openrouter_chat_completion_with_retry(
    client: &Client,
    api_key: &str,
    body: &ChatRequest,
) -> Result<ChatResponse> {
    let max_attempts = 3usize;
    let mut backoff = Duration::from_millis(250);
    let mut last_err: Option<anyhow::Error> = None;

    for attempt in 1..=max_attempts {
        let resp = client
            .post(OPENROUTER_CHAT_URL)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("HTTP-Referer", "https://github.com/nikitavoloboev/flow")
            .header("Accept", "application/json")
            .json(body)
            .send();

        let resp = match resp {
            Ok(resp) => resp,
            Err(err) => {
                let retry = openrouter_should_retry_error(&err) && attempt < max_attempts;
                let err = anyhow::Error::new(err).context("failed to call OpenRouter API");
                if retry {
                    info!(
                        attempt = attempt,
                        max_attempts = max_attempts,
                        backoff_ms = backoff.as_millis() as u64,
                        "OpenRouter request error (transient), retrying"
                    );
                    last_err = Some(err);
                    std::thread::sleep(backoff);
                    backoff = backoff.saturating_mul(2);
                    continue;
                }
                return Err(err);
            }
        };

        let status = resp.status();
        let retry_after = openrouter_retry_after(&resp);
        let request_id = resp
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
            .or_else(|| {
                resp.headers()
                    .get("cf-ray")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string())
            });

        let body_bytes = match resp.bytes() {
            Ok(bytes) => bytes,
            Err(err) => {
                let retry = openrouter_should_retry_error(&err) && attempt < max_attempts;
                let mut err =
                    anyhow::Error::new(err).context("failed to read OpenRouter response body");
                if let Some(rid) = request_id.as_deref() {
                    err = err.context(format!("OpenRouter request id: {}", rid));
                }
                if retry {
                    info!(
                        attempt = attempt,
                        max_attempts = max_attempts,
                        backoff_ms = backoff.as_millis() as u64,
                        "OpenRouter body read error (transient), retrying"
                    );
                    last_err = Some(err);
                    std::thread::sleep(backoff);
                    backoff = backoff.saturating_mul(2);
                    continue;
                }
                return Err(err);
            }
        };

        if !status.is_success() {
            let is_retryable_status =
                status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
            let text = String::from_utf8_lossy(&body_bytes).trim().to_string();

            if is_retryable_status && attempt < max_attempts {
                let sleep_for = retry_after.unwrap_or(backoff);
                info!(
                    attempt = attempt,
                    max_attempts = max_attempts,
                    status = %status,
                    sleep_ms = sleep_for.as_millis() as u64,
                    "OpenRouter returned transient status, retrying"
                );
                last_err = Some(anyhow::anyhow!("OpenRouter API error {}: {}", status, text));
                std::thread::sleep(sleep_for);
                backoff = backoff.saturating_mul(2);
                continue;
            }

            let mut err = anyhow::anyhow!("OpenRouter API error {}: {}", status, text);
            if let Some(rid) = request_id.as_deref() {
                err = err.context(format!("OpenRouter request id: {}", rid));
            }
            return Err(err);
        }

        match serde_json::from_slice::<ChatResponse>(&body_bytes) {
            Ok(parsed) => return Ok(parsed),
            Err(err) => {
                let snippet = {
                    let s = String::from_utf8_lossy(&body_bytes);
                    let s = s.trim();
                    let max = 600usize;
                    if s.len() > max {
                        format!("{}...", &s[..max])
                    } else {
                        s.to_string()
                    }
                };
                let mut err = anyhow::Error::new(err)
                    .context("failed to decode OpenRouter JSON response")
                    .context(format!("response snippet: {}", snippet));
                if let Some(rid) = request_id.as_deref() {
                    err = err.context(format!("OpenRouter request id: {}", rid));
                }
                return Err(err);
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("OpenRouter request failed after retries")))
}

/// Run Rise daemon to review staged changes for bugs and performance issues.
fn run_rise_review(
    diff: &str,
    session_context: Option<&str>,
    review_instructions: Option<&str>,
    _workdir: &std::path::Path,
    model: &str,
) -> Result<ReviewResult> {
    let (diff_for_prompt, _truncated) = truncate_diff(diff);

    // Build review prompt
    let mut prompt = String::from(
        "Review this git diff for bugs, security issues, and performance problems. \
         Output ONLY a JSON object with this exact format: \
         {\"issues_found\": true/false, \"issues\": [\"issue 1\", \"issue 2\"], \"summary\": \"brief summary\", \"future_tasks\": [\"optional follow-up\"]}. \
         future_tasks are optional improvements/optimizations (max 3), actionable, and not duplicates of issues; use [] if none.\n\n\
         Git diff:\n",
    );
    prompt.push_str(&diff_for_prompt);

    if let Some(instructions) = review_instructions {
        prompt.push_str(&format!(
            "\n\nAdditional review instructions:\n{}",
            instructions
        ));
    }

    if let Some(context) = session_context {
        prompt.push_str(&format!("\n\nContext:\n{}", context));
    }

    let client = crate::http_client::blocking_with_timeout(std::time::Duration::from_secs(120))
        .context("failed to create HTTP client")?;

    let body = ChatRequest {
        model: model.to_string(),
        messages: vec![
            Message {
                role: "system".to_string(),
                content: "You are a code reviewer. Analyze code changes for bugs, security issues, and performance problems. Output JSON only.".to_string(),
            },
            Message {
                role: "user".to_string(),
                content: prompt,
            },
        ],
        temperature: 0.3,
    };

    info!(model = model, "calling Rise daemon for code review");
    let start = std::time::Instant::now();

    let rise_url = rise_url();
    let text = send_rise_request_text(&client, &rise_url, &body, model)?;

    info!(
        elapsed_ms = start.elapsed().as_millis() as u64,
        "Rise daemon responded"
    );
    let output = parse_rise_output(&text).context("failed to parse Rise response")?;

    println!("{}", output);

    // Try to parse JSON from output
    let mut review_json = parse_review_json(&output);
    let future_tasks = review_json
        .as_ref()
        .map(|json| normalize_future_tasks(&json.future_tasks))
        .unwrap_or_default();
    let summary = review_json.as_ref().and_then(|r| r.summary.clone());
    let quality = review_json.as_mut().and_then(|r| r.quality.take());
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
        summary,
        future_tasks,
        timed_out: false,
        quality,
    })
}

fn ensure_git_repo() -> Result<()> {
    let _ = vcs::ensure_jj_repo()?;
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

fn git_root_or_cwd() -> std::path::PathBuf {
    match git_capture(&["rev-parse", "--show-toplevel"]) {
        Ok(root) => std::path::PathBuf::from(root.trim()),
        Err(_) => std::env::current_dir().unwrap_or_default(),
    }
}

fn warn_if_commit_invoked_from_subdir(repo_root: &Path) {
    let Ok(cwd) = std::env::current_dir() else {
        return;
    };
    let cwd_norm = cwd.canonicalize().unwrap_or(cwd.clone());
    let root_norm = repo_root
        .canonicalize()
        .unwrap_or_else(|_| repo_root.to_path_buf());
    if cwd_norm == root_norm {
        return;
    }

    println!(
        "warning: commit invoked from subdirectory: {}",
        cwd.display()
    );
    println!(
        "warning: using git repo root for commit operations: {}",
        repo_root.display()
    );
}

fn ensure_commit_setup(repo_root: &Path) -> Result<()> {
    let ai_internal = repo_root.join(".ai").join("internal");
    fs::create_dir_all(&ai_internal)
        .with_context(|| format!("failed to create {}", ai_internal.display()))?;
    setup::add_gitignore_entry(repo_root, ".ai/internal/")?;
    Ok(())
}

fn ensure_no_internal_staged(repo_root: &Path) -> Result<()> {
    if env::var("FLOW_ALLOW_INTERNAL_COMMIT").as_deref() == Ok("1") {
        return Ok(());
    }
    let staged = internal_staged_paths(repo_root);
    if staged.is_empty() {
        return Ok(());
    }

    println!("Refusing to commit internal .ai files:");
    for path in staged {
        println!("  - {}", path);
    }
    println!("Remove these from staging or set FLOW_ALLOW_INTERNAL_COMMIT=1 to override.");
    bail!("Refusing to commit internal .ai files");
}

fn internal_staged_paths(repo_root: &Path) -> Vec<String> {
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
    files
        .lines()
        .filter(|path| {
            path.starts_with(".ai/internal/")
                || path == &".ai/internal"
                || (path.starts_with(".ai/todos/") && path.ends_with(".bike"))
        })
        .map(|path| path.to_string())
        .collect()
}

fn ensure_no_unwanted_staged(repo_root: &Path) -> Result<()> {
    if env::var("FLOW_ALLOW_UNWANTED_COMMIT")
        .ok()
        .map(|v| {
            let v = v.to_ascii_lowercase();
            v == "1" || v == "true" || v == "yes"
        })
        .unwrap_or(false)
    {
        return Ok(());
    }

    let staged = unwanted_staged_paths(repo_root);
    if staged.is_empty() {
        return Ok(());
    }

    let mut ignore_entries = HashSet::new();
    let mut saw_personal_tooling = false;

    for (path, reason) in &staged {
        println!("Refusing to commit generated file: {} ({})", path, reason);

        // Personal tooling entries belong in global gitignore, not project .gitignore.
        if path.starts_with(".beads/") || path == ".beads" {
            saw_personal_tooling = true;
            continue;
        }
        if path == ".rise" || path.starts_with(".rise/") || path.contains("/.rise/") {
            saw_personal_tooling = true;
            continue;
        }

        if path.ends_with(".pyc")
            || path.contains("/__pycache__/")
            || path.ends_with("/__pycache__")
        {
            ignore_entries.insert("__pycache__/");
            ignore_entries.insert("*.pyc");
        }
    }

    for entry in &ignore_entries {
        let _ = setup::add_gitignore_entry(repo_root, entry);
    }

    for (path, _) in &staged {
        let _ = git_run_in(repo_root, &["reset", "HEAD", "--", path]);
    }

    if !ignore_entries.is_empty() {
        println!("Added ignore rules for generated files and unstaged them.");
    } else {
        println!("Unstaged generated files.");
    }
    if saw_personal_tooling {
        println!(
            "Personal tooling paths (.beads/, .rise/) should be ignored globally, not in project .gitignore."
        );
        println!("Run `f gitignore policy-init` and `f gitignore fix` to clean existing repos.");
    }
    println!("Re-run `f commit` after verifying the changes.");
    println!("Set FLOW_ALLOW_UNWANTED_COMMIT=1 to override.");
    bail!("Refusing to commit generated files");
}
fn unwanted_staged_paths(repo_root: &Path) -> Vec<(String, String)> {
    let output = Command::new("git")
        .args(["diff", "--cached", "--name-status", "-z"])
        .current_dir(repo_root)
        .output();

    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }

    let mut out = Vec::new();
    let raw = String::from_utf8_lossy(&output.stdout);
    let parts: Vec<&str> = raw.split('\0').collect();
    let mut i = 0;
    while i < parts.len() {
        let status = parts[i];
        i += 1;
        if status.is_empty() {
            continue;
        }
        let path = if status.starts_with('R') || status.starts_with('C') {
            if i + 1 >= parts.len() {
                break;
            }
            let new_path = parts[i + 1];
            i += 2;
            new_path
        } else {
            if i >= parts.len() {
                break;
            }
            let path = parts[i];
            i += 1;
            path
        };

        if status.starts_with('D') {
            continue;
        }

        if let Some(reason) = unwanted_reason(path) {
            out.push((path.to_string(), reason.to_string()));
        }
    }
    out
}

fn unwanted_reason(path: &str) -> Option<&'static str> {
    if path == ".flow/deploy-log.json" || path.ends_with("/.flow/deploy-log.json") {
        return Some("flow deploy state");
    }
    if path == ".beads" || path.starts_with(".beads/") || path.contains("/.beads/") {
        return Some("beads metadata");
    }
    if path == ".rise" || path.starts_with(".rise/") || path.contains("/.rise/") {
        return Some("rise metadata");
    }
    if path.ends_with(".pyc") {
        return Some("python bytecode");
    }
    if path.ends_with("/__pycache__")
        || path.contains("/__pycache__/")
        || path.starts_with("__pycache__/")
    {
        return Some("python cache");
    }
    None
}

fn log_commit_event_for_repo(
    repo_root: &Path,
    message: &str,
    command: &str,
    review: Option<ai::CommitReviewSummary>,
    context_chars: Option<usize>,
) {
    let commit_sha = match git_capture_in(repo_root, &["rev-parse", "HEAD"]) {
        Ok(sha) => sha,
        Err(err) => {
            debug!("failed to capture commit SHA for commit log: {}", err);
            return;
        }
    };
    let branch = git_capture_in(repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .unwrap_or_else(|_| "unknown".to_string());
    let author_name = git_capture_in(repo_root, &["log", "-1", "--format=%an"])
        .unwrap_or_else(|_| "unknown".to_string());
    let author_email = git_capture_in(repo_root, &["log", "-1", "--format=%ae"])
        .unwrap_or_else(|_| "unknown".to_string());

    ai::log_commit_event(
        &repo_root.to_path_buf(),
        commit_sha.trim(),
        branch.trim(),
        message,
        author_name.trim(),
        author_email.trim(),
        command,
        review,
        context_chars,
    );
}

/// Record an undoable commit action.
/// Call this after a successful commit (and optionally push).
fn record_undo_action(repo_root: &Path, pushed: bool, message: Option<&str>) {
    // Get the current HEAD (after commit)
    let after_sha = match git_capture_in(repo_root, &["rev-parse", "HEAD"]) {
        Ok(sha) => sha.trim().to_string(),
        Err(_) => return,
    };

    // Get the parent commit (before commit)
    let before_sha = match git_capture_in(repo_root, &["rev-parse", "HEAD~1"]) {
        Ok(sha) => sha.trim().to_string(),
        Err(_) => return,
    };

    // Get current branch
    let branch = git_capture_in(repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .unwrap_or_else(|_| "unknown".to_string());

    let action_type = if pushed {
        undo::ActionType::CommitPush
    } else {
        undo::ActionType::Commit
    };
    let push_remote = config::preferred_git_remote_for_repo(repo_root);
    let remote_for_undo = if pushed {
        Some(push_remote.as_str())
    } else {
        None
    };

    if let Err(e) = undo::record_action(
        repo_root,
        action_type,
        &before_sha,
        &after_sha,
        branch.trim(),
        pushed,
        remote_for_undo,
        message,
    ) {
        debug!("failed to record undo action: {}", e);
    }
}

const COMMIT_QUEUE_DIR: &str = ".ai/internal/commit-queue";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CommitQueueEntry {
    version: u8,
    created_at: String,
    repo_root: String,
    branch: String,
    commit_sha: String,
    message: String,
    review_bookmark: Option<String>,
    #[serde(default)]
    review_completed: bool,
    #[serde(default)]
    review_issues_found: bool,
    #[serde(default)]
    review_timed_out: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    review_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    review_reviewer: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    review_todo_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pr_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pr_number: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pr_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pr_base: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    analysis: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    review: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    #[serde(skip)]
    record_path: Option<PathBuf>,
}

const RISE_REVIEW_DIR: &str = ".ai/internal/rise-review";
const EMPTY_TREE_HASH: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

#[derive(Debug, Serialize)]
struct RiseReviewFileEntry {
    status: String,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none", rename = "originalPath")]
    original_path: Option<String>,
}

#[derive(Debug, Serialize)]
struct RiseReviewSession {
    version: u8,
    #[serde(rename = "created_at")]
    created_at: String,
    #[serde(rename = "repoRoot")]
    repo_root: String,
    commit: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bookmark: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    analysis: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    review: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    files: Vec<RiseReviewFileEntry>,
}

fn short_sha(sha: &str) -> &str {
    if sha.len() <= 7 { sha } else { &sha[..7] }
}

fn commit_queue_dir(repo_root: &Path) -> PathBuf {
    repo_root.join(COMMIT_QUEUE_DIR)
}

fn commit_queue_entry_path(repo_root: &Path, sha: &str) -> PathBuf {
    commit_queue_dir(repo_root).join(format!("{}.json", sha))
}

fn write_commit_queue_entry(repo_root: &Path, entry: &CommitQueueEntry) -> Result<PathBuf> {
    let dir = commit_queue_dir(repo_root);
    fs::create_dir_all(&dir)?;
    let payload = serde_json::to_string_pretty(entry).context("serialize commit queue entry")?;
    let path = commit_queue_entry_path(repo_root, &entry.commit_sha);
    fs::write(&path, payload).context("write commit queue entry")?;
    Ok(path)
}

fn format_review_body(review: &ReviewResult) -> Option<String> {
    if review.issues.is_empty() {
        return None;
    }
    let mut out = String::new();
    for issue in &review.issues {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("- ");
        out.push_str(issue);
    }
    Some(out)
}

fn resolve_commit_parent(repo_root: &Path, commit_sha: &str) -> String {
    match git_capture_in(repo_root, &["rev-parse", &format!("{}^", commit_sha)]) {
        Ok(parent) => {
            let trimmed = parent.trim().to_string();
            if trimmed.is_empty() {
                EMPTY_TREE_HASH.to_string()
            } else {
                trimmed
            }
        }
        Err(_) => EMPTY_TREE_HASH.to_string(),
    }
}

fn resolve_commit_message(repo_root: &Path, entry: &CommitQueueEntry) -> Option<String> {
    if !entry.message.trim().is_empty() {
        return Some(entry.message.clone());
    }
    git_capture_in(repo_root, &["log", "-1", "--format=%B", &entry.commit_sha])
        .ok()
        .map(|message| message.trim().to_string())
        .filter(|message| !message.is_empty())
}

fn resolve_review_files(repo_root: &Path, commit_sha: &str) -> Vec<RiseReviewFileEntry> {
    let output = git_capture_in(
        repo_root,
        &[
            "diff-tree",
            "--root",
            "--no-commit-id",
            "--name-status",
            "-r",
            "-M",
            commit_sha,
        ],
    );

    let Ok(output) = output else {
        return Vec::new();
    };

    output
        .lines()
        .filter_map(|line| {
            if line.trim().is_empty() {
                return None;
            }
            let mut parts = line.split('\t');
            let status = parts.next().unwrap_or_default().trim().to_string();
            if status.starts_with('R') || status.starts_with('C') {
                let original = parts.next().unwrap_or_default().trim().to_string();
                let path = parts.next().unwrap_or_default().trim().to_string();
                if path.is_empty() {
                    return None;
                }
                return Some(RiseReviewFileEntry {
                    status,
                    path,
                    original_path: if original.is_empty() {
                        None
                    } else {
                        Some(original)
                    },
                });
            }
            let path = parts.next().unwrap_or_default().trim().to_string();
            if path.is_empty() {
                return None;
            }
            Some(RiseReviewFileEntry {
                status,
                path,
                original_path: None,
            })
        })
        .collect()
}

fn write_rise_review_session(repo_root: &Path, entry: &CommitQueueEntry) -> Result<PathBuf> {
    let review_dir = repo_root.join(RISE_REVIEW_DIR);
    fs::create_dir_all(&review_dir)
        .with_context(|| format!("failed to create {}", review_dir.display()))?;

    let session = RiseReviewSession {
        version: 1,
        created_at: entry.created_at.clone(),
        repo_root: entry.repo_root.clone(),
        commit: entry.commit_sha.clone(),
        parent: Some(resolve_commit_parent(repo_root, &entry.commit_sha)),
        bookmark: entry.review_bookmark.clone(),
        branch: Some(entry.branch.clone()),
        message: resolve_commit_message(repo_root, entry),
        analysis: entry.analysis.clone(),
        review: entry.review.clone(),
        summary: entry.summary.clone(),
        files: resolve_review_files(repo_root, &entry.commit_sha),
    };

    let path = review_dir.join(format!("review-{}.json", entry.commit_sha));
    let payload =
        serde_json::to_string_pretty(&session).context("serialize rise review session")?;
    fs::write(&path, payload).context("write rise review session")?;
    Ok(path)
}

fn rise_review_path(repo_root: &Path, commit_sha: &str) -> PathBuf {
    repo_root
        .join(RISE_REVIEW_DIR)
        .join(format!("review-{}.json", commit_sha))
}

fn delete_rise_review_session(repo_root: &Path, commit_sha: &str) {
    let path = rise_review_path(repo_root, commit_sha);
    if path.exists() {
        let _ = fs::remove_file(path);
    }
}

fn git_is_ancestor(repo_root: &Path, ancestor: &str, descendant: &str) -> bool {
    Command::new("git")
        .current_dir(repo_root)
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn load_commit_queue_entries(repo_root: &Path) -> Result<Vec<CommitQueueEntry>> {
    let dir = commit_queue_dir(repo_root);
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut entries = Vec::new();
    for entry in fs::read_dir(&dir).context("read commit queue directory")? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let content = fs::read_to_string(&path).unwrap_or_default();
        match serde_json::from_str::<CommitQueueEntry>(&content) {
            Ok(mut parsed) => {
                parsed.record_path = Some(path);
                entries.push(parsed);
            }
            Err(err) => debug!(path = %path.display(), error = %err, "invalid commit queue entry"),
        }
    }
    entries.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    Ok(entries)
}

fn resolve_commit_queue_entry(repo_root: &Path, hash: &str) -> Result<CommitQueueEntry> {
    let entries = load_commit_queue_entries(repo_root)?;
    let matches: Vec<_> = entries
        .into_iter()
        .filter(|entry| commit_queue_entry_matches(entry, hash))
        .collect();

    match matches.len() {
        0 => bail!("No queued commit matches {}", hash),
        1 => Ok(matches.into_iter().next().unwrap()),
        _ => bail!("Multiple queued commits match {}. Use a longer hash.", hash),
    }
}

fn resolve_git_commit_sha(repo_root: &Path, hash: &str) -> Result<String> {
    let rev = format!("{hash}^{{commit}}");
    let sha = git_capture_in(repo_root, &["rev-parse", "--verify", &rev])
        .with_context(|| format!("{hash} is not a valid git commit"))?;
    let trimmed = sha.trim();
    if trimmed.is_empty() {
        bail!("{hash} is not a valid git commit");
    }
    Ok(trimmed.to_string())
}

fn queue_existing_commit_for_approval(
    repo_root: &Path,
    hash: &str,
    mark_reviewed: bool,
) -> Result<CommitQueueEntry> {
    let commit_sha = resolve_git_commit_sha(repo_root, hash)?;
    let branch = git_capture_in(repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .unwrap_or_else(|_| "unknown".to_string())
        .trim()
        .to_string();
    let message = git_capture_in(repo_root, &["log", "-1", "--format=%s", &commit_sha])
        .unwrap_or_default()
        .trim()
        .to_string();
    let review_bookmark = create_review_bookmark(repo_root, &commit_sha, &branch).ok();

    let mut entry = CommitQueueEntry {
        version: 2,
        created_at: chrono::Utc::now().to_rfc3339(),
        repo_root: repo_root.display().to_string(),
        branch,
        commit_sha: commit_sha.clone(),
        message,
        review_bookmark,
        review_completed: mark_reviewed,
        review_issues_found: false,
        review_timed_out: !mark_reviewed,
        review_model: if mark_reviewed {
            Some("manual-codex".to_string())
        } else {
            None
        },
        review_reviewer: if mark_reviewed {
            Some("codex".to_string())
        } else {
            None
        },
        review_todo_ids: Vec::new(),
        pr_url: None,
        pr_number: None,
        pr_head: None,
        pr_base: None,
        analysis: None,
        review: None,
        summary: if mark_reviewed {
            Some("Manually reviewed with Codex; approved for push.".to_string())
        } else {
            Some("Queued from git history without review metadata.".to_string())
        },
        record_path: None,
    };

    let path = write_commit_queue_entry(repo_root, &entry)?;
    entry.record_path = Some(path);
    if let Err(err) = write_rise_review_session(repo_root, &entry) {
        debug!("failed to write rise review session: {}", err);
    }
    Ok(entry)
}

fn remove_commit_queue_entry_by_entry(repo_root: &Path, entry: &CommitQueueEntry) -> Result<()> {
    if let Some(path) = entry.record_path.as_ref() {
        if path.exists() {
            fs::remove_file(path).context("remove commit queue entry")?;
        }
    }
    let path = commit_queue_entry_path(repo_root, &entry.commit_sha);
    if path.exists() {
        fs::remove_file(&path).context("remove commit queue entry")?;
    }
    delete_rise_review_session(repo_root, &entry.commit_sha);
    Ok(())
}

fn queue_commit_for_review(
    repo_root: &Path,
    message: &str,
    review: Option<&ReviewResult>,
    review_model: Option<&str>,
    review_reviewer: Option<&str>,
    review_todo_ids: Vec<String>,
) -> Result<String> {
    let commit_sha = git_capture_in(repo_root, &["rev-parse", "HEAD"])?
        .trim()
        .to_string();
    let branch = git_capture_in(repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .unwrap_or_else(|_| "unknown".to_string())
        .trim()
        .to_string();
    let review_bookmark = create_review_bookmark(repo_root, &commit_sha, &branch).ok();
    let summary = review
        .and_then(|value| value.summary.clone())
        .and_then(|value| {
            let trimmed = value.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        });
    let review_body = review.and_then(format_review_body);

    let entry = CommitQueueEntry {
        version: 2,
        created_at: chrono::Utc::now().to_rfc3339(),
        repo_root: repo_root.display().to_string(),
        branch,
        commit_sha: commit_sha.clone(),
        message: message.to_string(),
        review_bookmark,
        review_completed: review.is_some(),
        review_issues_found: review.map(|r| r.issues_found).unwrap_or(false),
        review_timed_out: review.map(|r| r.timed_out).unwrap_or(false),
        review_model: review_model.map(|s| s.to_string()),
        review_reviewer: review_reviewer.map(|s| s.to_string()),
        review_todo_ids,
        pr_url: None,
        pr_number: None,
        pr_head: None,
        pr_base: None,
        analysis: None,
        review: review_body,
        summary,
        record_path: None,
    };

    let path = write_commit_queue_entry(repo_root, &entry)?;
    let _ = path;
    if let Err(err) = write_rise_review_session(repo_root, &entry) {
        debug!("failed to write rise review session: {}", err);
    }
    Ok(commit_sha)
}

fn open_review_in_rise(repo_root: &Path, commit_sha: &str) {
    // Prefer rise-app (VS Code fork) because it has the best multi-file diff UX.
    // Fall back to `rise review open` if rise-app isn't installed.
    let (cmd, args): (String, Vec<String>) = if let Ok(rise_app_path) = which::which("rise-app") {
        // Ensure review file exists, then open it explicitly.
        let review_file = rise_review_path(repo_root, commit_sha);
        if !review_file.exists() {
            // Best-effort recreate; failures here shouldn't block.
            if let Ok(entry) = resolve_commit_queue_entry(repo_root, commit_sha) {
                let _ = write_rise_review_session(repo_root, &entry);
            }
        }

        // Some installations place the JS wrapper directly on PATH without a shebang.
        // In that case, execute it with node.
        let launch_with_node = fs::read(&rise_app_path)
            .ok()
            .and_then(|bytes| {
                bytes
                    .get(0..128)
                    .map(|chunk| String::from_utf8_lossy(chunk).to_string())
            })
            .map(|head| {
                !head.starts_with("#!") && (head.starts_with("/*") || head.starts_with("//"))
            })
            .unwrap_or(false);

        if launch_with_node {
            (
                "node".to_string(),
                vec![
                    rise_app_path.display().to_string(),
                    "review".to_string(),
                    "--review-file".to_string(),
                    review_file.display().to_string(),
                ],
            )
        } else {
            (
                rise_app_path.display().to_string(),
                vec![
                    "review".to_string(),
                    "--review-file".to_string(),
                    review_file.display().to_string(),
                ],
            )
        }
    } else if which::which("rise").is_ok() {
        (
            "rise".to_string(),
            vec![
                "review".to_string(),
                "open".to_string(),
                "--queue".to_string(),
                commit_sha.to_string(),
            ],
        )
    } else {
        println!("Rise not found on PATH; skipping review open.");
        return;
    };

    let status = Command::new(&cmd)
        .args(&args)
        .current_dir(repo_root)
        .status();

    match status {
        Ok(status) => {
            if !status.success() {
                println!("⚠ Failed to open review (exit {}).", status);
            }
        }
        Err(err) => println!("⚠ Failed to run review opener: {}", err),
    }
}

pub fn open_latest_queue_review() -> Result<()> {
    ensure_git_repo()?;
    let repo_root = git_root_or_cwd();
    let _ = refresh_commit_queue(&repo_root);
    let mut entries = load_commit_queue_entries(&repo_root)?;
    if entries.is_empty() {
        bail!("Commit queue is empty.");
    }
    let entry = entries.pop().unwrap();
    println!(
        "Opening latest queued commit {} in Rise...",
        short_sha(&entry.commit_sha)
    );
    open_review_in_rise(&repo_root, &entry.commit_sha);
    Ok(())
}

fn latest_review_report_for_commit(repo_root: &Path, commit_sha: &str) -> Option<PathBuf> {
    let report_dir = flow_commit_reports_dir()?;
    let sha_short = short_sha(commit_sha);
    let project_slug = safe_label_value(&flow_project_name(repo_root));
    let branch = git_capture_in(repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .unwrap_or_else(|_| "unknown".to_string())
        .trim()
        .to_string();
    let branch_slug = safe_label_value(&branch);
    let strict_prefix = format!("{project_slug}-{branch_slug}-{sha_short}-");

    let mut strict_matches: Vec<PathBuf> = Vec::new();
    let mut fallback_matches: Vec<PathBuf> = Vec::new();
    for entry in fs::read_dir(&report_dir).ok()? {
        let path = entry.ok()?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if file_name.starts_with(&strict_prefix) {
            strict_matches.push(path);
        } else if file_name.contains(&format!("-{sha_short}-")) {
            fallback_matches.push(path);
        }
    }

    strict_matches.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    fallback_matches.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

    strict_matches.pop().or_else(|| fallback_matches.pop())
}

fn queued_review_counts_excluding(
    repo_root: &Path,
    excluded_commit_sha: &str,
) -> Result<(usize, usize, String)> {
    let entries = load_commit_queue_entries(repo_root)?;
    let current_branch = git_capture_in(repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .unwrap_or_else(|_| "unknown".to_string())
        .trim()
        .to_string();

    let mut total_other = 0usize;
    let mut branch_other = 0usize;
    for entry in entries {
        if entry.commit_sha == excluded_commit_sha {
            continue;
        }
        total_other += 1;
        if entry.branch.trim() == current_branch {
            branch_other += 1;
        }
    }

    Ok((branch_other, total_other, current_branch))
}

fn print_other_queued_review_count(repo_root: &Path, commit_sha: &str) {
    let Ok((branch_other, total_other, current_branch)) =
        queued_review_counts_excluding(repo_root, commit_sha)
    else {
        return;
    };

    if total_other == 0 {
        println!("No other queued commits pending review.");
        return;
    }

    println!(
        "{} other queued commit(s) pending review ({} on current branch {}).",
        total_other, branch_other, current_branch
    );
}

fn copy_text_to_clipboard(text: &str) -> Result<bool> {
    if std::env::var("FLOW_NO_CLIPBOARD").is_ok() || !std::io::stdin().is_terminal() {
        return Ok(false);
    }

    #[cfg(target_os = "macos")]
    {
        let mut child = Command::new("pbcopy")
            .stdin(Stdio::piped())
            .spawn()
            .context("failed to spawn pbcopy")?;

        if let Some(stdin) = child.stdin.as_mut() {
            stdin.write_all(text.as_bytes())?;
        }

        child.wait()?;
        return Ok(true);
    }

    #[cfg(target_os = "linux")]
    {
        let result = Command::new("xclip")
            .arg("-selection")
            .arg("clipboard")
            .stdin(Stdio::piped())
            .spawn();

        let mut child = match result {
            Ok(c) => c,
            Err(_) => Command::new("xsel")
                .arg("--clipboard")
                .arg("--input")
                .stdin(Stdio::piped())
                .spawn()
                .context("failed to spawn xclip or xsel")?,
        };

        if let Some(stdin) = child.stdin.as_mut() {
            stdin.write_all(text.as_bytes())?;
        }

        child.wait()?;
        return Ok(true);
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        bail!("clipboard not supported on this platform");
    }
}

fn build_review_prompt_payload(
    repo_root: &Path,
    entry: &CommitQueueEntry,
    report_path: Option<&Path>,
) -> String {
    let (branch_other, total_other, current_branch) = queued_review_counts_excluding(
        repo_root,
        &entry.commit_sha,
    )
    .unwrap_or((0, 0, "unknown".to_string()));
    let mut out = String::new();
    out.push_str("here is commit i want you to address fully\n\n");
    out.push_str(&format!(
        "Repo: {}\nBranch: {}\nQueued commit: {}",
        repo_root.display(),
        entry.branch.trim(),
        short_sha(&entry.commit_sha)
    ));
    if let Some(bookmark) = entry
        .review_bookmark
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        out.push_str(&format!("\nReview bookmark: {}", bookmark));
    }
    out.push_str("\n\nCommands:\n");
    out.push_str(&format!(
        "f commit-queue show {}\n",
        short_sha(&entry.commit_sha)
    ));
    out.push_str(&format!(
        "f commit-queue approve {}\n",
        short_sha(&entry.commit_sha)
    ));
    out.push_str("f commit-queue approve --all\n");

    if let Some(path) = report_path {
        out.push_str(&format!("\nReview report: {}\n", path.display()));
        out.push_str(&format!("Run: f fix {}\n", path.display()));
    }

    out.push_str("\nCommit message:\n");
    out.push_str("────────────────────────────────────────\n");
    out.push_str(entry.message.trim_end());
    out.push_str("\n────────────────────────────────────────\n");

    if let Some(summary) = entry
        .summary
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        out.push_str("\nReview summary:\n");
        out.push_str(summary);
        out.push('\n');
    }

    if let Some(review) = entry
        .review
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        out.push_str("\nReview findings:\n");
        out.push_str(review);
        out.push('\n');
    }

    if let Some(path) = report_path {
        if let Ok(markdown) = fs::read_to_string(path) {
            let trimmed = markdown.trim();
            if !trimmed.is_empty() {
                out.push_str("\nReview report markdown:\n");
                out.push_str(trimmed);
                out.push('\n');
            }
        }
    }

    if total_other == 0 {
        out.push_str("\nOther queued commits pending review: 0\n");
    } else {
        out.push_str(&format!(
            "\nOther queued commits pending review: {} ({} on current branch {})\n",
            total_other, branch_other, current_branch
        ));
    }

    out.push_str("\naddress this so we can push\n");
    out
}

pub fn copy_review_prompt(hash: Option<&str>) -> Result<()> {
    ensure_git_repo()?;
    let repo_root = git_root_or_cwd();
    let _ = refresh_commit_queue(&repo_root);

    let mut entry = if let Some(hash) = hash {
        resolve_commit_queue_entry(&repo_root, hash)?
    } else {
        let mut entries = load_commit_queue_entries(&repo_root)?;
        if entries.is_empty() {
            bail!("Commit queue is empty.");
        }
        entries.pop().unwrap()
    };
    let _ = refresh_queue_entry_commit(&repo_root, &mut entry);

    let report_path = latest_review_report_for_commit(&repo_root, &entry.commit_sha);
    let payload = build_review_prompt_payload(&repo_root, &entry, report_path.as_deref());

    match copy_text_to_clipboard(&payload) {
        Ok(true) => println!(
            "Copied review prompt for {} to clipboard.",
            short_sha(&entry.commit_sha)
        ),
        Ok(false) => {
            println!("Clipboard copy skipped (non-interactive shell or FLOW_NO_CLIPBOARD).")
        }
        Err(err) => println!("⚠ Failed to copy review prompt to clipboard: {}", err),
    }

    println!("────────────────────────────────────────");
    println!("{}", payload.trim_end());
    println!("────────────────────────────────────────");
    Ok(())
}

fn print_queue_instructions(repo_root: &Path, commit_sha: &str) {
    println!("Queued commit {} for review.", short_sha(commit_sha));
    println!("  f commit-queue list");
    println!("  f commit-queue show {}", short_sha(commit_sha));
    println!(
        "  When review passes: f commit-queue approve {}",
        short_sha(commit_sha)
    );
    println!("  When all pass: f commit-queue approve --all");
    println!("  f review copy {}", short_sha(commit_sha));
    print_other_queued_review_count(repo_root, commit_sha);
}

fn queue_review_status_label(entry: &CommitQueueEntry) -> &'static str {
    let issues_present = entry.review_issues_found
        || entry
            .review
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
    if entry.review_timed_out {
        "review timed out"
    } else if issues_present {
        "review issues"
    } else if entry.version >= 2 && !entry.review_completed {
        "review pending"
    } else {
        "review clean"
    }
}

fn print_pending_queue_review_hint(repo_root: &Path) {
    let mut entries = match load_commit_queue_entries(repo_root) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    if entries.is_empty() {
        return;
    }

    for entry in &mut entries {
        let _ = refresh_queue_entry_commit(repo_root, entry);
    }

    let current_branch = git_capture_in(repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .unwrap_or_else(|_| "unknown".to_string())
        .trim()
        .to_string();

    let mut scoped_entries: Vec<&CommitQueueEntry> = entries
        .iter()
        .filter(|entry| entry.branch.trim() == current_branch)
        .collect();
    let scoped_to_branch = !scoped_entries.is_empty();
    if !scoped_to_branch {
        scoped_entries = entries.iter().collect();
    }

    println!();
    if scoped_to_branch {
        println!(
            "Queued commits pending review on branch {}:",
            current_branch
        );
    } else {
        println!("Queued commits pending review (all branches):");
    }

    let max_display = 5usize;
    for entry in scoped_entries.iter().take(max_display) {
        println!(
            "  - {}  {}  {}",
            short_sha(&entry.commit_sha),
            format_queue_created_at(&entry.created_at),
            queue_review_status_label(entry)
        );
    }
    if scoped_entries.len() > max_display {
        println!("  ... and {} more", scoped_entries.len() - max_display);
    }

    println!("Next:");
    println!("  f commit-queue list");
    println!("  f commit-queue approve --all");
}

fn approve_all_queued_commits(
    repo_root: &Path,
    force: bool,
    allow_issues: bool,
    allow_unreviewed: bool,
) -> Result<()> {
    git_guard::ensure_clean_for_push(repo_root)?;
    let mut entries = load_commit_queue_entries(repo_root)?;
    if entries.is_empty() {
        println!("No queued commits.");
        return Ok(());
    }

    for entry in &mut entries {
        let _ = refresh_queue_entry_commit(repo_root, entry);
    }

    let current_branch = git_capture_in(repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .unwrap_or_else(|_| "unknown".to_string());
    let current_branch = current_branch.trim().to_string();

    let mut candidates = Vec::new();
    let mut skipped_branch = Vec::new();
    for entry in entries {
        if !force && entry.branch.trim() != current_branch {
            skipped_branch.push(entry);
        } else {
            candidates.push(entry);
        }
    }

    if candidates.is_empty() {
        if skipped_branch.is_empty() {
            println!("No queued commits to approve.");
        } else {
            println!(
                "No queued commits on branch {}. {} queued commit(s) are on other branches.",
                current_branch,
                skipped_branch.len()
            );
        }
        return Ok(());
    }

    if !force {
        let mut bad_issues: Vec<String> = Vec::new();
        let mut bad_unreviewed: Vec<String> = Vec::new();
        for entry in &candidates {
            let issues_present = entry.review_issues_found
                || entry
                    .review
                    .as_deref()
                    .map(|s| !s.trim().is_empty())
                    .unwrap_or(false);
            let unreviewed =
                (entry.version >= 2 && !entry.review_completed) || entry.review_timed_out;
            if issues_present && !allow_issues {
                bad_issues.push(short_sha(&entry.commit_sha).to_string());
            }
            if unreviewed && !allow_unreviewed {
                bad_unreviewed.push(short_sha(&entry.commit_sha).to_string());
            }
        }

        if !bad_unreviewed.is_empty() {
            bail!(
                "Some queued commits do not have a clean review (timed out/missing): {}. Re-run review or use --allow-unreviewed.",
                bad_unreviewed.join(", ")
            );
        }
        if !bad_issues.is_empty() {
            bail!(
                "Some queued commits have review issues: {}. Fix them or use --allow-issues.",
                bad_issues.join(", ")
            );
        }
    }

    let head_sha = git_capture_in(repo_root, &["rev-parse", "HEAD"])?;
    let head_sha = head_sha.trim().to_string();
    ensure_safe_upstream_for_commit_queue_push(repo_root, &head_sha, force)?;

    if git_try_in(repo_root, &["fetch", "--quiet"]).is_ok() {
        if let Ok(counts) = git_capture_in(
            repo_root,
            &["rev-list", "--left-right", "--count", "@{u}...HEAD"],
        ) {
            let parts: Vec<&str> = counts.split_whitespace().collect();
            if parts.len() == 2 {
                let behind = parts[0].parse::<u64>().unwrap_or(0);
                if behind > 0 && !force {
                    bail!(
                        "Remote is ahead by {} commit(s). Run `f sync` or rebase, then re-approve.",
                        behind
                    );
                }
            }
        }
    }

    let before_sha = git_capture_in(repo_root, &["rev-parse", "@{u}"]).ok();
    let push_remote = config::preferred_git_remote_for_repo(repo_root);
    let push_branch = current_branch.trim().to_string();

    print!("Pushing... ");
    io::stdout().flush()?;
    let mut pushed = false;
    match git_push_try_in(repo_root, &push_remote, &push_branch) {
        PushResult::Success => {
            println!("done");
            pushed = true;
        }
        PushResult::NoRemoteRepo => {
            println!("skipped (no remote repo)");
        }
        PushResult::RemoteAhead => {
            println!("failed (remote ahead)");
            print!("Pulling with rebase... ");
            io::stdout().flush()?;
            match git_pull_rebase_try_in(repo_root, &push_remote, &push_branch) {
                Ok(_) => {
                    println!("done");
                    print!("Pushing... ");
                    io::stdout().flush()?;
                    git_push_run_in(repo_root, &push_remote, &push_branch)?;
                    println!("done");
                    pushed = true;
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

    if pushed {
        if let (Some(before_sha), Ok(after_sha)) = (
            before_sha,
            git_capture_in(repo_root, &["rev-parse", "HEAD"]),
        ) {
            let branch = current_branch.as_str();
            let before_sha = before_sha.trim();
            let after_sha = after_sha.trim();
            let _ = undo::record_action(
                repo_root,
                undo::ActionType::Push,
                before_sha,
                after_sha,
                branch,
                true,
                Some(push_remote.as_str()),
                None,
            );
        }

        let head_sha = git_capture_in(repo_root, &["rev-parse", "HEAD"]).unwrap_or_default();
        let head_sha = head_sha.trim();
        let mut approved = 0;
        let mut skipped = 0;

        for entry in &candidates {
            if git_is_ancestor(repo_root, &entry.commit_sha, head_sha) {
                if let Some(bookmark) = entry.review_bookmark.as_ref() {
                    delete_review_bookmark(repo_root, bookmark);
                }
                remove_commit_queue_entry_by_entry(repo_root, entry)?;
                if let Ok(done) =
                    todo::complete_review_timeout_todos(repo_root, &entry.review_todo_ids)
                {
                    if done > 0 {
                        println!("Auto-completed {} review follow-up todo(s).", done);
                    }
                }
                approved += 1;
            } else {
                println!(
                    "Skipped queued commit {} (not reachable from HEAD)",
                    short_sha(&entry.commit_sha)
                );
                skipped += 1;
            }
        }

        if !skipped_branch.is_empty() {
            println!(
                "Skipped {} queued commit(s) on other branches.",
                skipped_branch.len()
            );
        }

        println!(
            "✓ Approved and pushed {} queued commit(s){}",
            approved,
            if skipped > 0 { " (some skipped)" } else { "" }
        );
    }

    Ok(())
}

fn commit_queue_entry_matches(entry: &CommitQueueEntry, hash: &str) -> bool {
    if entry.commit_sha.starts_with(hash) {
        return true;
    }
    if let Some(path) = entry.record_path.as_ref() {
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            return stem.starts_with(hash);
        }
    }
    false
}

fn refresh_queue_entry_commit(repo_root: &Path, entry: &mut CommitQueueEntry) -> Result<bool> {
    let Some(bookmark) = entry.review_bookmark.as_deref() else {
        return Ok(false);
    };
    let Some(jj_root) = vcs::jj_root_if_exists(repo_root) else {
        return Ok(false);
    };
    let Ok(output) = jj_capture_in(
        &jj_root,
        &["log", "-r", bookmark, "--no-graph", "-T", "commit_id"],
    ) else {
        return Ok(false);
    };
    let new_sha = output
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    if new_sha.is_empty() || new_sha == entry.commit_sha {
        return Ok(false);
    }

    let old_sha = entry.commit_sha.clone();
    let old_path = entry
        .record_path
        .clone()
        .unwrap_or_else(|| commit_queue_entry_path(repo_root, &entry.commit_sha));
    entry.commit_sha = new_sha;
    let new_path = write_commit_queue_entry(repo_root, entry)?;
    if old_path != new_path && old_path.exists() {
        let _ = fs::remove_file(&old_path);
    }
    entry.record_path = Some(new_path);
    delete_rise_review_session(repo_root, &old_sha);
    if let Err(err) = write_rise_review_session(repo_root, entry) {
        debug!("failed to refresh rise review session: {}", err);
    }
    Ok(true)
}

fn current_upstream_ref(repo_root: &Path) -> Option<String> {
    git_capture_in(
        repo_root,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    )
    .ok()
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
}

fn is_ephemeral_upstream_ref(upstream: &str) -> bool {
    upstream.starts_with("origin/jj/keep/")
        || upstream.starts_with("origin/review/")
        || upstream.contains("/jj/keep/")
        || upstream.contains("/review/")
}

fn find_best_pr_upstream_candidate(repo_root: &Path, head_sha: &str) -> Option<String> {
    let refs = git_capture_in(
        repo_root,
        &[
            "for-each-ref",
            "--format=%(refname:short)",
            "refs/remotes/origin/pr/",
        ],
    )
    .ok()?;

    let mut best: Option<(u64, String)> = None;
    for candidate in refs.lines().map(str::trim).filter(|s| !s.is_empty()) {
        if !git_is_ancestor(repo_root, candidate, head_sha) {
            continue;
        }
        let distance = git_capture_in(
            repo_root,
            &["rev-list", "--count", &format!("{candidate}..{head_sha}")],
        )
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(u64::MAX);
        match &best {
            Some((best_distance, _)) if *best_distance <= distance => {}
            _ => best = Some((distance, candidate.to_string())),
        }
    }
    best.map(|(_, candidate)| candidate)
}

fn ensure_safe_upstream_for_commit_queue_push(
    repo_root: &Path,
    head_sha: &str,
    force: bool,
) -> Result<()> {
    let upstream = current_upstream_ref(repo_root);

    if let Some(upstream) = upstream {
        if is_ephemeral_upstream_ref(&upstream) && !force {
            if let Some(candidate) = find_best_pr_upstream_candidate(repo_root, head_sha) {
                if candidate != upstream {
                    println!(
                        "Upstream {} looks ephemeral. Retargeting push upstream to {}.",
                        upstream, candidate
                    );
                    git_run_in(repo_root, &["branch", "--set-upstream-to", &candidate])?;
                }
            } else {
                bail!(
                    "Current upstream {} looks ephemeral and no origin/pr/* candidate was found. Set upstream explicitly to your PR branch, or re-run with --force.",
                    upstream
                );
            }
        }
        return Ok(());
    }

    if force {
        return Ok(());
    }

    if let Some(candidate) = find_best_pr_upstream_candidate(repo_root, head_sha) {
        println!(
            "No upstream configured. Using {} as push upstream.",
            candidate
        );
        git_run_in(repo_root, &["branch", "--set-upstream-to", &candidate])?;
        return Ok(());
    }

    bail!(
        "No upstream configured and no origin/pr/* candidate found. Set upstream to your PR branch first, then re-run."
    );
}

pub fn commit_queue_has_entries(repo_root: &Path) -> bool {
    let dir = commit_queue_dir(repo_root);
    if !dir.exists() {
        return false;
    }
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .any(|entry| entry.path().extension().and_then(|s| s.to_str()) == Some("json"))
        })
        .unwrap_or(false)
}

pub fn commit_queue_has_entries_on_branch(repo_root: &Path, branch: &str) -> bool {
    let target = branch.trim();
    if target.is_empty() {
        return commit_queue_has_entries(repo_root);
    }
    load_commit_queue_entries(repo_root)
        .map(|entries| entries.iter().any(|entry| entry.branch.trim() == target))
        .unwrap_or_else(|_| commit_queue_has_entries(repo_root))
}

pub fn commit_queue_has_entries_reachable_from_head(repo_root: &Path) -> bool {
    let head = match git_capture_in(repo_root, &["rev-parse", "HEAD"]) {
        Ok(value) => value.trim().to_string(),
        Err(_) => return commit_queue_has_entries(repo_root),
    };
    if head.is_empty() {
        return commit_queue_has_entries(repo_root);
    }

    load_commit_queue_entries(repo_root)
        .map(|entries| {
            entries
                .iter()
                .any(|entry| git_is_ancestor(repo_root, &entry.commit_sha, &head))
        })
        .unwrap_or_else(|_| commit_queue_has_entries(repo_root))
}

pub fn refresh_commit_queue(repo_root: &Path) -> Result<usize> {
    let mut entries = load_commit_queue_entries(repo_root)?;
    let mut updated = 0;
    for entry in &mut entries {
        if refresh_queue_entry_commit(repo_root, entry)? {
            updated += 1;
        }
    }
    Ok(updated)
}

fn queued_commit_patch(repo_root: &Path, commit_sha: &str) -> Result<String> {
    git_capture_in(
        repo_root,
        &["show", "--format=", "--patch", "--no-color", commit_sha],
    )
}

fn with_temp_worktree_for_commit<T, F>(repo_root: &Path, commit_sha: &str, f: F) -> Result<T>
where
    F: FnOnce(&Path) -> Result<T>,
{
    let tmp = TempDir::new().context("create temp worktree dir")?;
    let worktree_path = tmp.path().join("repo");
    let worktree_str = worktree_path.to_string_lossy().to_string();

    git_run_in(
        repo_root,
        &["worktree", "add", "--detach", &worktree_str, commit_sha],
    )?;

    let result = f(&worktree_path);

    if let Err(err) = git_run_in(repo_root, &["worktree", "remove", "--force", &worktree_str]) {
        debug!(
            worktree = %worktree_str,
            error = %err,
            "failed to remove temp worktree for queue review"
        );
    }

    result
}

fn run_codex_review_for_queued_commit(
    repo_root: &Path,
    commit_sha: &str,
    review_instructions: Option<&str>,
) -> Result<(ReviewResult, String)> {
    let diff = queued_commit_patch(repo_root, commit_sha)?;
    let review = with_temp_worktree_for_commit(repo_root, commit_sha, |worktree| {
        let parent = git_capture_in(worktree, &["rev-parse", "HEAD^"])
            .context("queued root commit review is not supported yet")?;
        git_run_in(worktree, &["reset", "--mixed", parent.trim()])?;
        run_codex_review(&diff, None, review_instructions, worktree, CodexModel::High)
    })?;
    Ok((review, diff))
}

fn append_unique_ids(dest: &mut Vec<String>, ids: Vec<String>) {
    let mut seen: HashSet<String> = dest.iter().cloned().collect();
    for id in ids {
        if seen.insert(id.clone()) {
            dest.push(id);
        }
    }
}

fn review_queue_entry_with_codex(
    repo_root: &Path,
    entry: &mut CommitQueueEntry,
    review_instructions: Option<&str>,
) -> Result<()> {
    let (review, diff) =
        run_codex_review_for_queued_commit(repo_root, &entry.commit_sha, review_instructions)?;

    let model_label = CodexModel::High.as_codex_arg();
    let reviewer_label = "codex";

    let mut review_todo_ids = entry.review_todo_ids.clone();
    if !env_flag("FLOW_REVIEW_ISSUES_TODOS_DISABLE") {
        if review.issues_found && !review.issues.is_empty() {
            let ids = todo::record_review_issues_as_todos(
                repo_root,
                &entry.commit_sha,
                &review.issues,
                review.summary.as_deref(),
                model_label,
            )?;
            append_unique_ids(&mut review_todo_ids, ids);
        }
        if review.timed_out {
            let issue = format!(
                "Re-run review: review timed out for commit {}",
                short_sha(&entry.commit_sha)
            );
            let ids = todo::record_review_issues_as_todos(
                repo_root,
                &entry.commit_sha,
                &vec![issue],
                review.summary.as_deref(),
                model_label,
            )?;
            append_unique_ids(&mut review_todo_ids, ids);
        } else {
            let _ = todo::complete_review_timeout_todos(repo_root, &review_todo_ids);
        }
    }

    let review_run_id = flow_review_run_id(repo_root, &diff, model_label, reviewer_label);
    record_review_outputs_to_beads_rust(
        repo_root,
        &review,
        reviewer_label,
        model_label,
        Some(&entry.commit_sha),
        &review_run_id,
    );

    entry.review_completed = true;
    entry.review_issues_found = review.issues_found;
    entry.review_timed_out = review.timed_out;
    entry.review_model = Some(model_label.to_string());
    entry.review_reviewer = Some(reviewer_label.to_string());
    entry.review_todo_ids = review_todo_ids;
    entry.review = format_review_body(&review);
    entry.summary = review.summary.as_ref().and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    });

    let path = write_commit_queue_entry(repo_root, entry)?;
    entry.record_path = Some(path);
    let _ = write_rise_review_session(repo_root, entry);
    maybe_sync_queue_review_to_mirrors(repo_root, entry, &diff, &review, reviewer_label);
    Ok(())
}

/// Mirror queued-review results to myflow/gitedit when the reviewed commit is the current HEAD.
/// This keeps async `f commit --quick` reviews visible in mirrors without risking wrong SHA syncs
/// when users review arbitrary queued commits from other branches.
fn maybe_sync_queue_review_to_mirrors(
    repo_root: &Path,
    entry: &CommitQueueEntry,
    diff: &str,
    review: &ReviewResult,
    reviewer_label: &str,
) {
    let head_sha = match git_capture_in(repo_root, &["rev-parse", "HEAD"]) {
        Ok(sha) => sha.trim().to_string(),
        Err(err) => {
            debug!(
                error = %err,
                "skipping queue review mirror sync: failed to resolve HEAD"
            );
            return;
        }
    };
    if head_sha != entry.commit_sha {
        debug!(
            queue_commit = %entry.commit_sha,
            head_commit = %head_sha,
            "skipping queue review mirror sync: reviewed commit is not HEAD"
        );
        return;
    }

    let sync_gitedit = gitedit_globally_enabled() && gitedit_mirror_enabled_for_commit(repo_root);
    let sync_myflow = myflow_mirror_enabled(repo_root);
    if !sync_gitedit && !sync_myflow {
        return;
    }

    let (sync_sessions, sync_window) = collect_sync_sessions_for_commit_with_window(repo_root);
    let review_data = GitEditReviewData {
        diff: Some(diff.to_string()),
        issues_found: review.issues_found,
        issues: review.issues.clone(),
        summary: review.summary.clone(),
        reviewer: Some(reviewer_label.to_string()),
    };

    if sync_gitedit {
        sync_to_gitedit(
            repo_root,
            "commit_queue_review",
            &sync_sessions,
            None,
            Some(&review_data),
        );
    }
    if sync_myflow {
        sync_to_myflow(
            repo_root,
            "commit_queue_review",
            &sync_sessions,
            Some(&sync_window),
            Some(&review_data),
            None,
        );
    }
}

fn queue_flag_for_command(queue: CommitQueueMode) -> String {
    if queue.enabled {
        " --queue".to_string()
    } else if queue.override_flag == Some(false) {
        " --no-queue".to_string()
    } else {
        String::new()
    }
}

fn review_flag_for_command(queue: CommitQueueMode) -> String {
    if queue.open_review {
        " --review".to_string()
    } else {
        String::new()
    }
}

fn review_bookmark_prefix(repo_root: &Path) -> Option<String> {
    let local_config = repo_root.join("flow.toml");
    if local_config.exists() {
        if let Ok(cfg) = config::load(&local_config) {
            if let Some(jj_cfg) = cfg.jj {
                if let Some(prefix) = jj_cfg.review_prefix {
                    let trimmed = prefix.trim();
                    if !trimmed.is_empty() {
                        return Some(trimmed.to_string());
                    } else {
                        return None;
                    }
                }
            }
        }
    }

    let global_config = config::default_config_path();
    if global_config.exists() {
        if let Ok(cfg) = config::load(&global_config) {
            if let Some(jj_cfg) = cfg.jj {
                if let Some(prefix) = jj_cfg.review_prefix {
                    let trimmed = prefix.trim();
                    if !trimmed.is_empty() {
                        return Some(trimmed.to_string());
                    } else {
                        return None;
                    }
                }
            }
        }
    }

    Some("review".to_string())
}

fn sanitize_review_branch(branch: &str) -> String {
    let mut out = String::new();
    for ch in branch.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else if ch == '/' || ch == '.' {
            out.push('-');
        }
    }
    if out.is_empty() {
        "branch".to_string()
    } else {
        out
    }
}

fn create_review_bookmark(repo_root: &Path, commit_sha: &str, branch: &str) -> Result<String> {
    if env_flag("FLOW_COMMIT_QUEUE_JJ_DISABLE") {
        bail!("FLOW_COMMIT_QUEUE_JJ_DISABLE=1");
    }
    let Some(prefix) = review_bookmark_prefix(repo_root) else {
        bail!("review prefix disabled");
    };
    let Some(jj_root) = vcs::jj_root_if_exists(repo_root) else {
        println!("ℹ️  jj workspace not found; skipping review bookmark creation.");
        bail!("jj workspace not available");
    };
    let branch_slug = sanitize_review_branch(branch);
    let base = format!("{}/{}-{}", prefix, branch_slug, short_sha(commit_sha));
    let mut name = base.clone();
    let mut index = 1;
    while jj_bookmark_exists(&jj_root, &name) {
        name = format!("{}-{}", base, index);
        index += 1;
        if index > 50 {
            bail!("too many review bookmarks with base {}", base);
        }
    }

    if let Err(err) = jj_run_in(&jj_root, &["bookmark", "create", &name, "-r", commit_sha]) {
        let msg = err.to_string().to_lowercase();
        if msg.contains("commit not found")
            || msg.contains("current working-copy commit not found")
            || msg.contains("failed to load short-prefixes index")
            || msg.contains("unexpected error from store")
            || msg.contains("failed to check out a commit")
        {
            println!("⚠️  jj workspace appears corrupted; skipping review bookmark creation.");
            println!(
                "   Fix: `jj git import` (or if still broken: `rm -rf .jj && jj git init --colocate`)"
            );
            bail!("jj workspace corrupted");
        }
        return Err(err);
    }
    println!("Queued review bookmark {}", name);
    Ok(name)
}

fn delete_review_bookmark(repo_root: &Path, bookmark: &str) {
    if let Some(jj_root) = vcs::jj_root_if_exists(repo_root) {
        let _ = jj_run_in(&jj_root, &["bookmark", "delete", bookmark]);
    }
}

fn jj_bookmark_exists(repo_root: &Path, name: &str) -> bool {
    let output = jj_capture_in(repo_root, &["bookmark", "list"]).unwrap_or_default();
    for line in output.lines() {
        let trimmed = line.trim_start().trim_start_matches('*').trim();
        let Some((token, _rest)) = trimmed.split_once(' ') else {
            continue;
        };
        if token == name {
            return true;
        }
    }
    false
}

fn jj_run_in(repo_root: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new(jj_bin())
        .current_dir(repo_root)
        .args(args)
        .output()
        .with_context(|| format!("failed to run jj {}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let msg = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        bail!("jj {} failed: {}", args.join(" "), msg);
    }
    Ok(())
}

fn jj_capture_in(repo_root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new(jj_bin())
        .current_dir(repo_root)
        .args(args)
        .output()
        .with_context(|| format!("failed to run jj {}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let msg = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        bail!("jj {} failed: {}", args.join(" "), msg);
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn jj_bin() -> String {
    env::var("FLOW_JJ_BIN")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "jj".to_string())
}

fn ensure_gh_available() -> Result<()> {
    let status = Command::new("gh")
        .args(["--version"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("failed to run `gh` (GitHub CLI)")?;
    if !status.success() {
        bail!("`gh` is installed but not working");
    }
    Ok(())
}

fn gh_capture_in(repo_root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("gh")
        .current_dir(repo_root)
        .args(args)
        .output()
        .with_context(|| format!("failed to run gh {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "gh {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn github_repo_from_remote_url(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }

    // https://github.com/owner/repo(.git)
    if let Some(rest) = trimmed.strip_prefix("https://github.com/") {
        return Some(rest.trim_end_matches(".git").to_string());
    }

    // git@github.com:owner/repo(.git)
    if let Some(rest) = trimmed.strip_prefix("git@github.com:") {
        return Some(rest.trim_end_matches(".git").to_string());
    }

    None
}

fn resolve_github_repo(repo_root: &Path) -> Result<String> {
    // First try origin URL.
    if let Ok(url) = git_capture_in(repo_root, &["remote", "get-url", "origin"]) {
        if let Some(repo) = github_repo_from_remote_url(&url) {
            return Ok(repo);
        }
    }

    // Fallback: ask `gh` (works for GitHub Enterprise too if authenticated).
    let repo = gh_capture_in(
        repo_root,
        &[
            "repo",
            "view",
            "--json",
            "nameWithOwner",
            "-q",
            ".nameWithOwner",
        ],
    )
    .context("failed to resolve GitHub repo for current directory")?;
    let repo = repo.trim();
    if repo.is_empty() {
        bail!(
            "unable to determine GitHub repo (origin URL not GitHub, and `gh repo view` returned empty)"
        );
    }
    Ok(repo.to_string())
}

fn sanitize_ref_component(input: &str) -> String {
    let mut out = String::new();
    let mut last_sep = false;
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
            out.push(ch);
            last_sep = false;
        } else if !last_sep {
            out.push('-');
            last_sep = true;
        }
    }
    out.trim_matches('-').to_string()
}

fn default_pr_head(entry: &CommitQueueEntry) -> String {
    if let Some(head) = entry
        .pr_head
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        return head.to_string();
    }
    if let Some(bookmark) = entry
        .review_bookmark
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        return bookmark.to_string();
    }
    // Fallback if jj bookmark wasn't created for some reason.
    format!(
        "pr/{}-{}",
        sanitize_ref_component(&entry.branch),
        short_sha(&entry.commit_sha)
    )
}

fn ensure_pr_head_pushed(repo_root: &Path, head: &str, commit_sha: &str) -> Result<String> {
    // Prefer jj bookmarks when available.
    if which::which("jj").is_ok() {
        // Ensure bookmark points at the commit, then push it.
        // If jj is unhealthy (store/index/template issues), fall back to git push.
        let jj_result = (|| -> Result<()> {
            let set_output = Command::new("jj")
                .current_dir(repo_root)
                .args([
                    "bookmark",
                    "set",
                    head,
                    "-r",
                    commit_sha,
                    "--allow-backwards",
                ])
                .output()
                .context("failed to run jj bookmark set for PR head")?;
            if !set_output.status.success() {
                let stderr = String::from_utf8_lossy(&set_output.stderr);
                let stdout = String::from_utf8_lossy(&set_output.stdout);
                bail!(
                    "jj bookmark set failed: {}",
                    format!("{}\n{}", stderr.trim(), stdout.trim()).trim()
                );
            }

            // We often push a brand new review/pr bookmark as the PR head.
            let push_output = Command::new("jj")
                .current_dir(repo_root)
                .args(["git", "push", "--bookmark", head, "--allow-new"])
                .output()
                .context("failed to run jj git push for PR head")?;
            if !push_output.status.success() {
                let stderr = String::from_utf8_lossy(&push_output.stderr);
                let stdout = String::from_utf8_lossy(&push_output.stdout);
                bail!(
                    "jj git push failed: {}",
                    format!("{}\n{}", stderr.trim(), stdout.trim()).trim()
                );
            }

            Ok(())
        })();
        if jj_result.is_ok() {
            // jj push uses the repo's configured/default git remote.
            // Keep plain branch head; gh can resolve this for same-repo pushes.
            return Ok(head.to_string());
        }
        let jj_error = jj_result.unwrap_err().to_string();
        let concise = jj_error
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("jj failed");
        eprintln!(
            "⚠️  jj bookmark push failed ({}). Falling back to git branch push for PR head.",
            concise
        );
    }

    // Fallback: push commit directly to a branch ref.
    // Try likely writable remotes first to support fork/upstream setups.
    let head_refspec = format!("{}:refs/heads/{}", commit_sha, head);
    let remotes = pr_push_remote_candidates(repo_root);
    if remotes.is_empty() {
        bail!("No git remotes configured; cannot push PR head {}", head);
    }

    let mut failures: Vec<String> = Vec::new();
    for remote in remotes {
        let push_output = Command::new("git")
            .current_dir(repo_root)
            .args(["push", "-u", &remote, &head_refspec])
            .output()
            .with_context(|| format!("failed to run git push for remote {remote}"))?;
        if push_output.status.success() {
            return Ok(pr_head_selector_for_remote(repo_root, &remote, head));
        }

        let push_stderr = String::from_utf8_lossy(&push_output.stderr)
            .trim()
            .to_string();
        let push_stdout = String::from_utf8_lossy(&push_output.stdout)
            .trim()
            .to_string();

        // Branch exists/diverged: retry safely with force-with-lease on the same remote.
        let force_output = Command::new("git")
            .current_dir(repo_root)
            .args(["push", "--force-with-lease", &remote, &head_refspec])
            .output()
            .with_context(|| format!("failed to run git force push for remote {remote}"))?;
        if force_output.status.success() {
            return Ok(pr_head_selector_for_remote(repo_root, &remote, head));
        }

        let force_stderr = String::from_utf8_lossy(&force_output.stderr)
            .trim()
            .to_string();
        failures.push(format!(
            "{remote}: push='{}' force='{}'{}",
            push_stderr,
            force_stderr,
            if push_stdout.is_empty() {
                String::new()
            } else {
                format!(" stdout='{}'", push_stdout)
            }
        ));
    }

    bail!(
        "failed to push PR head {} to any remote:\n{}",
        head,
        failures.join("\n")
    );
}

fn pr_push_remote_candidates(repo_root: &Path) -> Vec<String> {
    let mut remotes: Vec<String> = git_capture_in(repo_root, &["remote"])
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();

    remotes.sort_by_key(|r| match r.as_str() {
        "fork" => 0u8,
        "origin" => 1u8,
        "upstream" => 3u8,
        _ => 2u8,
    });
    remotes
}

fn pr_head_selector_for_remote(repo_root: &Path, remote: &str, head: &str) -> String {
    let Some(url) = git_capture_in(repo_root, &["remote", "get-url", remote])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        return head.to_string();
    };

    if let Some((owner, _repo)) = parse_github_remote(&url) {
        return format!("{owner}:{head}");
    }

    head.to_string()
}

fn extract_pr_url(text: &str) -> Option<String> {
    let re = Regex::new(r"https://github\\.com/[^/\\s]+/[^/\\s]+/pull/\\d+").ok()?;
    re.find(text).map(|m| m.as_str().to_string())
}

fn pr_number_from_url(url: &str) -> Option<u64> {
    let parts: Vec<&str> = url.trim_end_matches('/').split('/').collect();
    parts.last()?.parse().ok()
}

fn split_head_selector(head: &str) -> (Option<&str>, &str) {
    let trimmed = head.trim();
    if let Some((owner, branch)) = trimmed.split_once(':') {
        let owner = owner.trim();
        let branch = branch.trim();
        if !owner.is_empty() && !branch.is_empty() {
            return (Some(owner), branch);
        }
    }
    (None, trimmed)
}

fn gh_find_open_pr_by_head(
    repo_root: &Path,
    repo: &str,
    head: &str,
) -> Result<Option<(u64, String)>> {
    #[derive(Deserialize)]
    struct HeadOwner {
        login: String,
    }

    #[derive(Deserialize)]
    struct PrListItem {
        number: u64,
        url: String,
        #[serde(rename = "headRefName")]
        head_ref_name: String,
        #[serde(rename = "headRepositoryOwner")]
        head_repository_owner: Option<HeadOwner>,
    }

    let (owner_filter, branch) = split_head_selector(head);
    if branch.is_empty() {
        return Ok(None);
    }

    // gh --head matches by branch name; owner qualification must be filtered client-side.
    let out = gh_capture_in(
        repo_root,
        &[
            "pr",
            "list",
            "--repo",
            repo,
            "--head",
            branch,
            "--state",
            "open",
            "--json",
            "number,url,headRefName,headRepositoryOwner",
        ],
    )
    .unwrap_or_default();

    let prs: Vec<PrListItem> = serde_json::from_str(out.trim()).unwrap_or_default();
    for pr in prs {
        if pr.head_ref_name != branch {
            continue;
        }
        if let Some(owner) = owner_filter {
            let login = pr
                .head_repository_owner
                .as_ref()
                .map(|o| o.login.as_str())
                .unwrap_or_default();
            if !login.eq_ignore_ascii_case(owner) {
                continue;
            }
        }
        return Ok(Some((pr.number, pr.url)));
    }

    Ok(None)
}

fn gh_create_pr(
    repo_root: &Path,
    repo: &str,
    head: &str,
    base: &str,
    title: &str,
    body: &str,
    draft: bool,
) -> Result<(u64, String)> {
    let normalized_body = normalize_markdown_linebreaks(body);
    let mut args: Vec<&str> = vec![
        "pr",
        "create",
        "--repo",
        repo,
        "--head",
        head,
        "--base",
        base,
        "--title",
        title,
        "--body",
        &normalized_body,
    ];
    if draft {
        args.push("--draft");
    }

    let output = Command::new("gh")
        .current_dir(repo_root)
        .args(&args)
        .output()
        .with_context(|| format!("failed to run gh {}", args.join(" ")))?;

    // gh can fail with "already exists" and still include the PR URL in stderr.
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    if !output.status.success() {
        if let Some(url) = extract_pr_url(&combined) {
            let number = pr_number_from_url(&url)
                .ok_or_else(|| anyhow::anyhow!("failed to parse PR number from URL {}", url))?;
            return Ok((number, url));
        }
        bail!(
            "gh {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    // gh typically prints the PR URL, but some versions/configs can produce no stdout.
    if let Some(url) = extract_pr_url(&combined) {
        let number = pr_number_from_url(&url)
            .ok_or_else(|| anyhow::anyhow!("failed to parse PR number from URL {}", url))?;
        return Ok((number, url));
    }

    if let Some(found) = gh_find_open_pr_by_head(repo_root, repo, head)? {
        return Ok(found);
    }

    bail!(
        "failed to determine PR URL after creation (gh output had no URL and PR lookup by head returned empty)"
    );
}

fn open_in_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let status = Command::new("open").arg(url).status()?;
        if !status.success() {
            bail!("failed to open browser");
        }
        return Ok(());
    }

    #[cfg(not(target_os = "macos"))]
    {
        let status = Command::new("xdg-open").arg(url).status()?;
        if !status.success() {
            bail!("failed to open browser");
        }
        Ok(())
    }
}

fn commit_message_title_body(message: &str) -> (String, String) {
    let mut lines = message.lines();
    let title = lines.next().unwrap_or("no title").trim().to_string();
    let rest = lines.collect::<Vec<_>>().join("\n").trim().to_string();
    (title, rest)
}

fn normalize_markdown_linebreaks(text: &str) -> String {
    let trimmed = text.trim();
    // Guardrail: if body has escaped line breaks but no real newlines, decode it.
    // This prevents malformed PR bodies like "Summary\\n- item" on GitHub.
    if !trimmed.contains('\n') && trimmed.contains("\\n") {
        return trimmed.replace("\\r\\n", "\n").replace("\\n", "\n");
    }
    trimmed.to_string()
}

pub fn run_commit_queue(cmd: CommitQueueCommand) -> Result<()> {
    ensure_git_repo()?;
    let repo_root = git_root_or_cwd();
    ensure_commit_setup(&repo_root)?;

    let action = cmd.action.unwrap_or(CommitQueueAction::List);
    match action {
        CommitQueueAction::List => {
            let entries = load_commit_queue_entries(&repo_root)?;
            if entries.is_empty() {
                println!("No queued commits.");
                return Ok(());
            }
            println!("Queued commits:");
            for mut entry in entries {
                let _ = refresh_queue_entry_commit(&repo_root, &mut entry);
                let subject = entry.message.lines().next().unwrap_or("no message").trim();
                let created_at = format_queue_created_at(&entry.created_at);
                let bookmark = entry
                    .review_bookmark
                    .as_ref()
                    .map(|b| format!(" {}", b))
                    .unwrap_or_default();
                println!(
                    "  {}  {}  {}  {}{}",
                    short_sha(&entry.commit_sha),
                    entry.branch,
                    created_at,
                    subject,
                    bookmark
                );
            }
        }
        CommitQueueAction::Show { hash } => {
            let mut entry = resolve_commit_queue_entry(&repo_root, &hash)?;
            let _ = refresh_queue_entry_commit(&repo_root, &mut entry);
            println!("Commit: {}", entry.commit_sha);
            println!("Branch: {}", entry.branch);
            println!("Queued: {}", entry.created_at);
            if let Some(bookmark) = entry.review_bookmark.as_ref() {
                println!("Review bookmark: {}", bookmark);
            }
            println!();
            println!("Message:");
            println!("────────────────────────────────────────");
            println!("{}", entry.message.trim_end());
            println!("────────────────────────────────────────");
            let issues_present = entry.review_issues_found
                || entry
                    .review
                    .as_deref()
                    .map(|s| !s.trim().is_empty())
                    .unwrap_or(false);
            if entry.review_timed_out {
                println!();
                println!("Review: timed out or failed");
            }
            if issues_present {
                if let Some(body) = entry
                    .review
                    .as_deref()
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                {
                    println!();
                    println!("Review issues:");
                    println!("{}", body);
                }
            }
            if !entry.review_todo_ids.is_empty() {
                println!();
                println!("Todos: {}", entry.review_todo_ids.join(", "));
            }
            if let Ok(stat) = git_capture_in(
                &repo_root,
                &["show", "--stat", "--format=", &entry.commit_sha],
            ) {
                if !stat.trim().is_empty() {
                    println!();
                    println!("{}", stat.trim_end());
                }
            }
            println!();
            println!("Open diff UI:");
            println!("  f commit-queue open {}", short_sha(&entry.commit_sha));
            println!("Print diff:");
            println!("  f commit-queue diff {}", short_sha(&entry.commit_sha));
        }
        CommitQueueAction::Open { hash } => {
            let mut entry = resolve_commit_queue_entry(&repo_root, &hash)?;
            let _ = refresh_queue_entry_commit(&repo_root, &mut entry);
            // Ensure the review session exists (Rise UI expects a review session file).
            let _ = write_rise_review_session(&repo_root, &entry);
            println!(
                "Opening queued commit {} in Rise app...",
                short_sha(&entry.commit_sha)
            );
            open_review_in_rise(&repo_root, &entry.commit_sha);
        }
        CommitQueueAction::Diff { hash } => {
            let mut entry = resolve_commit_queue_entry(&repo_root, &hash)?;
            let _ = refresh_queue_entry_commit(&repo_root, &mut entry);
            // Print a full patch (user can pipe to less -R).
            let patch = git_capture_in(
                &repo_root,
                &[
                    "show",
                    "--color=always",
                    "--patch",
                    "--format=fuller",
                    &entry.commit_sha,
                ],
            )?;
            // Avoid panicking on SIGPIPE (e.g. `... | head`).
            if let Err(err) = io::stdout().write_all(patch.trim_end().as_bytes()) {
                if err.kind() != io::ErrorKind::BrokenPipe {
                    return Err(err).context("failed to write diff to stdout");
                }
                return Ok(());
            }
            if let Err(err) = io::stdout().write_all(b"\n") {
                if err.kind() != io::ErrorKind::BrokenPipe {
                    return Err(err).context("failed to write diff newline to stdout");
                }
            }
        }
        CommitQueueAction::Review { hashes, all } => {
            let mut entries = load_commit_queue_entries(&repo_root)?;
            if entries.is_empty() {
                println!("No queued commits.");
                return Ok(());
            }
            for entry in &mut entries {
                let _ = refresh_queue_entry_commit(&repo_root, entry);
            }

            let mut targets: Vec<CommitQueueEntry> = Vec::new();
            if !hashes.is_empty() {
                for hash in hashes {
                    let matches: Vec<CommitQueueEntry> = entries
                        .iter()
                        .filter(|entry| commit_queue_entry_matches(entry, &hash))
                        .cloned()
                        .collect();
                    match matches.len() {
                        0 => bail!("No queued commit matches {}", hash),
                        1 => targets.push(matches[0].clone()),
                        _ => bail!("Multiple queued commits match {}. Use a longer hash.", hash),
                    }
                }
            } else if all {
                targets = entries;
            } else {
                let current_branch =
                    git_capture_in(&repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])
                        .unwrap_or_else(|_| "unknown".to_string());
                targets = entries
                    .into_iter()
                    .filter(|entry| entry.branch.trim() == current_branch.trim())
                    .collect();
            }

            if targets.is_empty() {
                println!("No queued commits selected for review.");
                return Ok(());
            }

            let review_instructions = get_review_instructions(&repo_root);
            let mut clean = 0usize;
            let mut with_issues = 0usize;
            let mut timed_out = 0usize;
            let mut failed = 0usize;

            for mut entry in targets {
                println!(
                    "==> Reviewing queued commit {} ({}) with Codex...",
                    short_sha(&entry.commit_sha),
                    entry.branch
                );
                match review_queue_entry_with_codex(
                    &repo_root,
                    &mut entry,
                    review_instructions.as_deref(),
                ) {
                    Ok(()) => {
                        if entry.review_timed_out {
                            timed_out += 1;
                            println!(
                                "  ⚠ Review timed out again for {}",
                                short_sha(&entry.commit_sha)
                            );
                        } else if entry.review_issues_found {
                            with_issues += 1;
                            println!(
                                "  ⚠ Review found issue(s) for {}",
                                short_sha(&entry.commit_sha)
                            );
                        } else {
                            clean += 1;
                            println!("  ✓ Review clean for {}", short_sha(&entry.commit_sha));
                        }
                        if !entry.review_todo_ids.is_empty() {
                            match todo::count_open_todos(&repo_root, &entry.review_todo_ids) {
                                Ok(open) => {
                                    if open > 0 {
                                        println!(
                                            "  ↳ {} open review todo(s): {}",
                                            open,
                                            entry.review_todo_ids.join(", ")
                                        );
                                    } else {
                                        println!("  ↳ review todos accounted for");
                                    }
                                }
                                Err(err) => println!("  ↳ todo status check failed: {}", err),
                            }
                        }
                    }
                    Err(err) => {
                        failed += 1;
                        println!(
                            "  ✗ Failed to review {}: {}",
                            short_sha(&entry.commit_sha),
                            err
                        );
                    }
                }
            }

            println!(
                "Review refresh summary: clean={}, issues={}, timed_out={}, failed={}",
                clean, with_issues, timed_out, failed
            );

            if failed > 0 {
                bail!("Some queued commit reviews failed. Resolve errors and re-run.");
            }
        }
        CommitQueueAction::Approve {
            all,
            hash,
            queue_if_missing,
            mark_reviewed,
            force,
            allow_issues,
            allow_unreviewed,
        } => {
            if all {
                if hash.is_some() {
                    bail!(
                        "--all cannot be combined with HASH. Use `f commit-queue approve --all`."
                    );
                }
                if queue_if_missing {
                    eprintln!("note: --queue-if-missing is ignored when using --all");
                }
                if mark_reviewed {
                    eprintln!("note: --mark-reviewed is ignored when using --all");
                }
                return approve_all_queued_commits(
                    &repo_root,
                    force,
                    allow_issues,
                    allow_unreviewed,
                );
            }

            git_guard::ensure_clean_for_push(&repo_root)?;
            let auto_mode = hash.is_none();
            let target_hash = match hash {
                Some(value) => value,
                None => git_capture_in(&repo_root, &["rev-parse", "--verify", "HEAD"])?
                    .trim()
                    .to_string(),
            };
            let effective_queue_if_missing = queue_if_missing || auto_mode;
            let effective_mark_reviewed = mark_reviewed || auto_mode;
            let effective_allow_unreviewed = allow_unreviewed || auto_mode;

            let mut entry = match resolve_commit_queue_entry(&repo_root, &target_hash) {
                Ok(entry) => entry,
                Err(err) => {
                    let no_match = err
                        .to_string()
                        .starts_with(&format!("No queued commit matches {}", target_hash));
                    if effective_queue_if_missing && no_match {
                        let entry = queue_existing_commit_for_approval(
                            &repo_root,
                            &target_hash,
                            effective_mark_reviewed,
                        )?;
                        println!(
                            "Queued {} from git history for approval{}.",
                            short_sha(&entry.commit_sha),
                            if effective_mark_reviewed {
                                " (marked manually reviewed)"
                            } else {
                                ""
                            }
                        );
                        entry
                    } else {
                        return Err(err);
                    }
                }
            };
            let _ = refresh_queue_entry_commit(&repo_root, &mut entry);

            let issues_present = entry.review_issues_found
                || entry
                    .review
                    .as_deref()
                    .map(|s| !s.trim().is_empty())
                    .unwrap_or(false);
            let unreviewed = entry.version >= 2 && !entry.review_completed;

            if issues_present && !allow_issues && !force {
                bail!(
                    "Queued commit {} has review issues. Fix them, or re-run with --allow-issues.",
                    short_sha(&entry.commit_sha)
                );
            }
            if unreviewed && !effective_allow_unreviewed && !force {
                bail!(
                    "Queued commit {} does not have a clean review (missing). Re-run review, or re-run with --allow-unreviewed.",
                    short_sha(&entry.commit_sha)
                );
            }
            if entry.review_timed_out && !force {
                eprintln!(
                    "note: review timed out for {}; approving anyway (re-run `f commit-queue review {}` if you want a full review)",
                    short_sha(&entry.commit_sha),
                    short_sha(&entry.commit_sha)
                );
            }

            let head_sha = git_capture_in(&repo_root, &["rev-parse", "HEAD"])?;
            let head_sha = head_sha.trim();
            if head_sha != entry.commit_sha && !force {
                bail!(
                    "Queued commit {} is not at HEAD (current HEAD is {}). Checkout the commit or re-run with --force.",
                    short_sha(&entry.commit_sha),
                    short_sha(head_sha)
                );
            }

            let current_branch = git_capture_in(&repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])
                .unwrap_or_else(|_| "unknown".to_string());
            if current_branch.trim() != entry.branch && !force {
                bail!(
                    "Queued commit was created on branch {} but current branch is {}. Checkout the branch or re-run with --force.",
                    entry.branch,
                    current_branch.trim()
                );
            }

            ensure_safe_upstream_for_commit_queue_push(&repo_root, head_sha, force)?;

            if git_try_in(&repo_root, &["fetch", "--quiet"]).is_ok() {
                if let Ok(counts) = git_capture_in(
                    &repo_root,
                    &["rev-list", "--left-right", "--count", "@{u}...HEAD"],
                ) {
                    let parts: Vec<&str> = counts.split_whitespace().collect();
                    if parts.len() == 2 {
                        let behind = parts[0].parse::<u64>().unwrap_or(0);
                        if behind > 0 && !force {
                            bail!(
                                "Remote is ahead by {} commit(s). Run `f sync` or rebase, then re-approve.",
                                behind
                            );
                        }
                    }
                }
            }

            let before_sha = git_capture_in(&repo_root, &["rev-parse", "@{u}"]).ok();
            let push_remote = config::preferred_git_remote_for_repo(&repo_root);
            let push_branch = current_branch.trim().to_string();

            print!("Pushing... ");
            io::stdout().flush()?;
            let mut pushed = false;
            match git_push_try_in(&repo_root, &push_remote, &push_branch) {
                PushResult::Success => {
                    println!("done");
                    pushed = true;
                }
                PushResult::NoRemoteRepo => {
                    println!("skipped (no remote repo)");
                }
                PushResult::RemoteAhead => {
                    println!("failed (remote ahead)");
                    print!("Pulling with rebase... ");
                    io::stdout().flush()?;
                    match git_pull_rebase_try_in(&repo_root, &push_remote, &push_branch) {
                        Ok(_) => {
                            println!("done");
                            print!("Pushing... ");
                            io::stdout().flush()?;
                            git_push_run_in(&repo_root, &push_remote, &push_branch)?;
                            println!("done");
                            pushed = true;
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

            if pushed {
                if let (Some(before_sha), Ok(after_sha)) = (
                    before_sha,
                    git_capture_in(&repo_root, &["rev-parse", "HEAD"]),
                ) {
                    let branch = current_branch.trim();
                    let before_sha = before_sha.trim();
                    let after_sha = after_sha.trim();
                    let _ = undo::record_action(
                        &repo_root,
                        undo::ActionType::Push,
                        before_sha,
                        after_sha,
                        branch,
                        true,
                        Some(push_remote.as_str()),
                        Some(&entry.message),
                    );
                }
                if let Some(bookmark) = entry.review_bookmark.as_ref() {
                    delete_review_bookmark(&repo_root, bookmark);
                }
                remove_commit_queue_entry_by_entry(&repo_root, &entry)?;
                if let Ok(done) =
                    todo::complete_review_timeout_todos(&repo_root, &entry.review_todo_ids)
                {
                    if done > 0 {
                        println!("Auto-completed {} review follow-up todo(s).", done);
                    }
                }
                println!("✓ Approved and pushed {}", short_sha(&entry.commit_sha));
            }
        }
        CommitQueueAction::ApproveAll {
            force,
            allow_issues,
            allow_unreviewed,
        } => approve_all_queued_commits(&repo_root, force, allow_issues, allow_unreviewed)?,
        CommitQueueAction::Drop { hash } => {
            let mut entry = resolve_commit_queue_entry(&repo_root, &hash)?;
            let _ = refresh_queue_entry_commit(&repo_root, &mut entry);
            if let Some(bookmark) = entry.review_bookmark.as_ref() {
                delete_review_bookmark(&repo_root, bookmark);
            }
            remove_commit_queue_entry_by_entry(&repo_root, &entry)?;
            println!("Dropped queued commit {}", short_sha(&entry.commit_sha));
        }
        CommitQueueAction::PrCreate {
            hash,
            base,
            draft,
            open,
        } => {
            ensure_gh_available()?;
            let repo = resolve_github_repo(&repo_root)?;

            let mut entry = resolve_commit_queue_entry(&repo_root, &hash)?;
            let _ = refresh_queue_entry_commit(&repo_root, &mut entry);

            let head = default_pr_head(&entry);
            let gh_head = ensure_pr_head_pushed(&repo_root, &head, &entry.commit_sha)?;

            let (number, url) =
                if let Some(found) = gh_find_open_pr_by_head(&repo_root, &repo, &gh_head)? {
                    found
                } else {
                    let (title, body_rest) = commit_message_title_body(&entry.message);
                    let mut body = String::new();
                    if !body_rest.is_empty() {
                        body.push_str(&body_rest);
                        body.push_str("\n\n");
                    }
                    if let Some(summary) = entry
                        .summary
                        .as_deref()
                        .map(|s| s.trim())
                        .filter(|s| !s.is_empty())
                    {
                        body.push_str("Review summary:\n");
                        body.push_str(summary);
                        body.push('\n');
                    }
                    gh_create_pr(
                        &repo_root,
                        &repo,
                        &gh_head,
                        &base,
                        &title,
                        body.trim(),
                        draft,
                    )?
                };

            entry.pr_number = Some(number);
            entry.pr_url = Some(url.clone());
            entry.pr_head = Some(head.clone());
            entry.pr_base = Some(base.clone());
            let _ = write_commit_queue_entry(&repo_root, &entry);

            println!("PR: {}", url);
            if open {
                let _ = open_in_browser(&url);
            }
        }
        CommitQueueAction::PrOpen { hash, base } => {
            ensure_gh_available()?;
            let repo = resolve_github_repo(&repo_root)?;

            let mut entry = resolve_commit_queue_entry(&repo_root, &hash)?;
            let _ = refresh_queue_entry_commit(&repo_root, &mut entry);

            let head = default_pr_head(&entry);
            let url = if let Some(url) = entry
                .pr_url
                .as_deref()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
            {
                url.to_string()
            } else if let Some((_n, url)) = gh_find_open_pr_by_head(&repo_root, &repo, &head)? {
                url
            } else {
                // Create it if missing (as draft).
                let gh_head = ensure_pr_head_pushed(&repo_root, &head, &entry.commit_sha)?;
                let (title, body_rest) = commit_message_title_body(&entry.message);
                let (number, url) =
                    if let Some(found) = gh_find_open_pr_by_head(&repo_root, &repo, &gh_head)? {
                        found
                    } else {
                        gh_create_pr(
                            &repo_root,
                            &repo,
                            &gh_head,
                            &base,
                            &title,
                            body_rest.trim(),
                            true,
                        )?
                    };
                entry.pr_number = Some(number);
                entry.pr_url = Some(url.clone());
                entry.pr_head = Some(head.clone());
                entry.pr_base = Some(base.clone());
                let _ = write_commit_queue_entry(&repo_root, &entry);
                url
            };

            println!("{}", url);
            let _ = open_in_browser(&url);
        }
    }

    Ok(())
}

pub fn run_pr(opts: PrOpts) -> Result<()> {
    ensure_git_repo()?;
    let repo_root = git_root_or_cwd();
    ensure_commit_setup(&repo_root)?;

    let args = normalize_pr_args(&opts.args);
    if let Some(feedback) = parse_pr_feedback_args(&args)? {
        return run_pr_feedback(&repo_root, feedback);
    }

    match args.as_slice() {
        // Convenience: `f pr open` opens the PR for the current branch (or queued commit) without
        // creating a new commit.
        [a] if a == "open" => return run_pr_open(&repo_root, &opts),
        // Convenience: `f pr open edit` opens a local markdown file in Zed Preview and syncs PR
        // title/body on save.
        [a, b] if a == "open" && b == "edit" => return run_pr_open_edit(&repo_root, &opts),
        _ => {}
    }

    if !opts.paths.is_empty() && (opts.no_commit || opts.hash.is_some()) {
        bail!("--path cannot be used with --no-commit or --hash");
    }

    let should_commit = !opts.no_commit && opts.hash.is_none();
    if should_commit {
        let queue = resolve_commit_queue_mode(true, false);
        let review_selection = resolve_review_selection_v2(false, None);
        let message = if args.is_empty() {
            None
        } else {
            Some(args.join(" "))
        };
        run_with_check_sync(
            true,
            false,
            review_selection,
            message.as_deref(),
            1000,
            false,
            queue,
            false,
            &opts.paths,
            CommitGateOverrides::default(),
        )?;
    }

    let hash = if let Some(hash) = opts.hash {
        hash
    } else {
        let _ = refresh_commit_queue(&repo_root);
        let mut entries = load_commit_queue_entries(&repo_root)?;
        let Some(entry) = entries.pop() else {
            bail!(
                "Commit queue is empty. Run `f pr \"message\"` or queue a commit first with `f commit --queue`."
            );
        };
        entry.commit_sha
    };

    run_commit_queue(CommitQueueCommand {
        action: Some(CommitQueueAction::PrCreate {
            hash,
            base: opts.base,
            draft: opts.draft,
            open: !opts.no_open,
        }),
    })
}

fn run_pr_open(repo_root: &Path, opts: &PrOpts) -> Result<()> {
    ensure_gh_available()?;
    let repo = resolve_github_repo(repo_root)?;

    // Prefer opening based on the current git branch name (most intuitive UX).
    let branch = git_capture_in(repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .unwrap_or_else(|_| "HEAD".to_string())
        .trim()
        .to_string();
    if !branch.is_empty() && branch != "HEAD" {
        if let Some((_n, url)) = gh_find_open_pr_by_head(repo_root, &repo, &branch)? {
            println!("PR: {}", url);
            if !opts.no_open {
                let _ = open_in_browser(&url);
            }
            return Ok(());
        }
    }

    // Fallback: open based on queued commit (by explicit hash, by HEAD SHA, or latest entry).
    let hash = if let Some(hash) = opts.hash.clone() {
        hash
    } else {
        let head_sha = git_capture_in(repo_root, &["rev-parse", "HEAD"])
            .unwrap_or_default()
            .trim()
            .to_string();
        let _ = refresh_commit_queue(repo_root);
        let mut entries = load_commit_queue_entries(repo_root)?;
        if entries.is_empty() {
            bail!("No PR found for current branch and commit queue is empty.");
        }
        if !head_sha.is_empty() {
            if let Some(entry) = entries.iter().rev().find(|e| e.commit_sha == head_sha) {
                entry.commit_sha.clone()
            } else {
                entries.pop().unwrap().commit_sha
            }
        } else {
            entries.pop().unwrap().commit_sha
        }
    };

    // Reuse the commit queue PR-open behavior (creates draft if missing).
    run_commit_queue(CommitQueueCommand {
        action: Some(CommitQueueAction::PrOpen {
            hash,
            base: opts.base.clone(),
        }),
    })
}

fn normalize_pr_args(args: &[String]) -> Vec<String> {
    let mut normalized = Vec::new();
    for a in args {
        let t = a.trim();
        if !t.is_empty() {
            normalized.push(t.to_string());
        }
    }
    normalized
}

#[derive(Debug, Clone)]
struct PrFeedbackCommand {
    selector: Option<String>,
    record_todos: bool,
}

#[derive(Debug, Clone)]
struct PrFeedbackItem {
    external_ref: String,
    source: &'static str,
    author: String,
    body: String,
    url: String,
    path: Option<String>,
    line: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct GhApiUser {
    login: String,
}

#[derive(Debug, Deserialize)]
struct GhPrFeedbackSummary {
    number: u64,
    url: String,
}

#[derive(Debug, Deserialize)]
struct GhPrReviewComment {
    id: u64,
    #[serde(default)]
    body: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    line: Option<u64>,
    #[serde(default)]
    in_reply_to_id: Option<u64>,
    user: GhApiUser,
}

#[derive(Debug, Deserialize)]
struct GhIssueComment {
    id: u64,
    #[serde(default)]
    body: String,
    #[serde(default)]
    html_url: String,
    user: GhApiUser,
}

#[derive(Debug, Deserialize)]
struct GhReview {
    id: u64,
    #[serde(default)]
    body: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    html_url: String,
    user: GhApiUser,
}

fn parse_pr_feedback_args(args: &[String]) -> Result<Option<PrFeedbackCommand>> {
    if args.first().map(|s| s.as_str()) != Some("feedback") {
        return Ok(None);
    }

    let mut selector: Option<String> = None;
    let mut record_todos = false;
    for token in args.iter().skip(1) {
        match token.as_str() {
            "--todo" | "todo" => record_todos = true,
            "--help" | "-h" => {
                return Ok(Some(PrFeedbackCommand {
                    selector: Some("--help".to_string()),
                    record_todos: false,
                }));
            }
            _ if token.starts_with("--") => {
                bail!("unknown `f pr feedback` option: {token}");
            }
            _ => {
                if selector.is_some() {
                    bail!("multiple PR selectors provided. Use exactly one selector.");
                }
                selector = Some(token.clone());
            }
        }
    }

    Ok(Some(PrFeedbackCommand {
        selector,
        record_todos,
    }))
}

fn parse_github_pr_url(input: &str) -> Option<(String, u64)> {
    let trimmed = input.trim().trim_end_matches('/');
    let prefix = "https://github.com/";
    let rest = trimmed.strip_prefix(prefix)?;
    let mut parts = rest.split('/');
    let owner = parts.next()?.trim();
    let repo = parts.next()?.trim();
    let kind = parts.next()?.trim();
    let number = parts.next()?.trim();
    if owner.is_empty() || repo.is_empty() || kind != "pull" {
        return None;
    }
    let number = number.parse::<u64>().ok()?;
    Some((format!("{owner}/{repo}"), number))
}

fn resolve_current_pr_for_feedback(repo_root: &Path, repo: &str) -> Result<(u64, String)> {
    let branch = git_capture_in(repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .unwrap_or_else(|_| "HEAD".to_string())
        .trim()
        .to_string();
    if !branch.is_empty() && branch != "HEAD" {
        if let Some((number, url)) = gh_find_open_pr_by_head(repo_root, repo, &branch)? {
            return Ok((number, url));
        }
    }

    let out = gh_capture_in(
        repo_root,
        &["pr", "view", "--repo", repo, "--json", "number,url"],
    )?;
    let parsed: GhPrFeedbackSummary = serde_json::from_str(out.trim())
        .context("failed to parse gh pr view output while resolving current PR")?;
    Ok((parsed.number, parsed.url))
}

fn gh_api_json_in<T: DeserializeOwned>(repo_root: &Path, endpoint: &str) -> Result<T> {
    let out = gh_capture_in(repo_root, &["api", endpoint])?;
    serde_json::from_str(out.trim())
        .with_context(|| format!("failed to parse GitHub API response for `{endpoint}`"))
}

fn pr_feedback_external_ref(repo: &str, pr_number: u64, source: &str, source_id: u64) -> String {
    let mut hasher = Sha1::new();
    hasher.update(repo.as_bytes());
    hasher.update(b":");
    hasher.update(pr_number.to_string().as_bytes());
    hasher.update(b":");
    hasher.update(source.as_bytes());
    hasher.update(b":");
    hasher.update(source_id.to_string().as_bytes());
    let hex = hex::encode(hasher.finalize());
    let short = hex.get(..12).unwrap_or(&hex);
    format!("flow-pr-feedback-{short}")
}

fn compact_single_line(text: &str, max_chars: usize) -> String {
    let first = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .replace('\t', " ");
    if first.chars().count() <= max_chars {
        return first;
    }
    let mut out = String::new();
    for (idx, ch) in first.chars().enumerate() {
        if idx >= max_chars.saturating_sub(3) {
            out.push_str("...");
            break;
        }
        out.push(ch);
    }
    out
}

fn pr_feedback_todo_title(pr_number: u64, item: &PrFeedbackItem) -> String {
    let snippet = compact_single_line(&item.body, 90);
    let mut title = format!("PR #{pr_number} {}: {}", item.source, snippet);
    if title.trim().is_empty() {
        title = format!("PR #{pr_number} {} feedback", item.source);
    }
    title
}

fn feedback_location_label(item: &PrFeedbackItem) -> Option<String> {
    match (item.path.as_deref(), item.line) {
        (Some(path), Some(line)) => Some(format!("{path}:{line}")),
        (Some(path), None) => Some(path.to_string()),
        _ => None,
    }
}

fn format_review_state_counts(reviews: &[GhReview]) -> String {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for review in reviews {
        let key = if review.state.trim().is_empty() {
            "UNKNOWN".to_string()
        } else {
            review.state.trim().to_ascii_uppercase()
        };
        *counts.entry(key).or_insert(0) += 1;
    }
    if counts.is_empty() {
        return "none".to_string();
    }
    let mut entries: Vec<(String, usize)> = counts.into_iter().collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
        .into_iter()
        .map(|(state, count)| format!("{state}:{count}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn record_pr_feedback_todos(
    repo_root: &Path,
    repo: &str,
    pr_number: u64,
    items: &[PrFeedbackItem],
) -> Result<Vec<String>> {
    let (path, mut todos) = todo::load_items_at_root(repo_root)?;
    let mut existing_refs = HashSet::new();
    for todo_item in &todos {
        if let Some(ext) = todo_item
            .external_ref
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            existing_refs.insert(ext.to_string());
        }
    }

    let mut created = Vec::new();
    let now = chrono::Utc::now().to_rfc3339();
    for item in items {
        if existing_refs.contains(&item.external_ref) {
            continue;
        }
        let id = Uuid::new_v4().simple().to_string();
        let mut note = String::new();
        note.push_str("Source: GitHub PR feedback\n");
        note.push_str("Repo: ");
        note.push_str(repo);
        note.push('\n');
        note.push_str("PR: ");
        note.push_str(&pr_number.to_string());
        note.push('\n');
        note.push_str("Type: ");
        note.push_str(item.source);
        note.push('\n');
        note.push_str("Author: ");
        note.push_str(&item.author);
        note.push('\n');
        if let Some(location) = feedback_location_label(item) {
            note.push_str("Location: ");
            note.push_str(&location);
            note.push('\n');
        }
        note.push_str("Link: ");
        note.push_str(&item.url);
        note.push('\n');
        note.push('\n');
        note.push_str(item.body.trim());

        todos.push(todo::TodoItem {
            id: id.clone(),
            title: pr_feedback_todo_title(pr_number, item),
            status: "pending".to_string(),
            created_at: now.clone(),
            updated_at: None,
            note: Some(note),
            session: None,
            external_ref: Some(item.external_ref.clone()),
            priority: Some(todo::parse_priority_from_issue(&item.body)),
        });
        existing_refs.insert(item.external_ref.clone());
        created.push(id);
    }

    if !created.is_empty() {
        todo::save_items(&path, &todos)?;
    }

    Ok(created)
}

fn write_pr_feedback_snapshot(
    repo_root: &Path,
    repo: &str,
    pr_number: u64,
    pr_url: &str,
    items: &[PrFeedbackItem],
) -> Result<PathBuf> {
    let dir = repo_root.join(".ai").join("reviews");
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("pr-feedback-{pr_number}.md"));

    let mut out = String::new();
    out.push_str("# PR Feedback\n\n");
    out.push_str("- Repo: `");
    out.push_str(repo);
    out.push_str("`\n");
    out.push_str("- PR: #");
    out.push_str(&pr_number.to_string());
    out.push('\n');
    out.push_str("- URL: ");
    out.push_str(pr_url);
    out.push('\n');
    out.push_str("- Generated: ");
    out.push_str(&chrono::Utc::now().to_rfc3339());
    out.push('\n');
    out.push('\n');

    if items.is_empty() {
        out.push_str("No actionable text feedback found.\n");
    } else {
        out.push_str("## Actionable Items\n\n");
        for (idx, item) in items.iter().enumerate() {
            out.push_str(&(idx + 1).to_string());
            out.push_str(". [");
            out.push_str(item.source);
            out.push_str("] ");
            out.push_str(&item.author);
            if let Some(location) = feedback_location_label(item) {
                out.push_str(" (");
                out.push_str(&location);
                out.push(')');
            }
            out.push('\n');
            out.push_str("   ");
            out.push_str(item.body.trim());
            out.push('\n');
            out.push_str("   ");
            out.push_str(&item.url);
            out.push('\n');
        }
    }

    fs::write(&path, out)?;
    Ok(path)
}

fn run_pr_feedback(repo_root: &Path, cmd: PrFeedbackCommand) -> Result<()> {
    ensure_gh_available()?;

    if let Some(selector) = cmd.selector.as_deref() {
        if selector == "--help" || selector == "-h" {
            println!("Usage: f pr feedback [<pr-number|pr-url>] [--todo]");
            println!("Examples:");
            println!("  f pr feedback");
            println!("  f pr feedback 8");
            println!("  f pr feedback https://github.com/owner/repo/pull/8 --todo");
            return Ok(());
        }
    }

    let (repo, pr_number, pr_url) = if let Some(selector) = cmd.selector.as_deref() {
        if let Some((repo, pr_number)) = parse_github_pr_url(selector) {
            let pr_url = format!("https://github.com/{repo}/pull/{pr_number}");
            (repo, pr_number, pr_url)
        } else {
            let trimmed = selector.trim().trim_start_matches('#');
            let pr_number = trimmed.parse::<u64>().with_context(|| {
                format!("invalid PR selector `{selector}`; expected number or URL")
            })?;
            let repo = resolve_github_repo(repo_root)?;
            let pr_url = format!("https://github.com/{repo}/pull/{pr_number}");
            (repo, pr_number, pr_url)
        }
    } else {
        let repo = resolve_github_repo(repo_root)?;
        let (pr_number, pr_url) = resolve_current_pr_for_feedback(repo_root, &repo).with_context(
            || "failed to resolve current PR. Pass an explicit selector: `f pr feedback <number>`",
        )?;
        (repo, pr_number, pr_url)
    };

    let reviews_endpoint = format!("repos/{repo}/pulls/{pr_number}/reviews?per_page=100");
    let review_comments_endpoint = format!("repos/{repo}/pulls/{pr_number}/comments?per_page=100");
    let issue_comments_endpoint = format!("repos/{repo}/issues/{pr_number}/comments?per_page=100");

    let reviews: Vec<GhReview> = gh_api_json_in(repo_root, &reviews_endpoint)?;
    let review_comments: Vec<GhPrReviewComment> =
        gh_api_json_in(repo_root, &review_comments_endpoint)?;
    let issue_comments: Vec<GhIssueComment> = gh_api_json_in(repo_root, &issue_comments_endpoint)?;

    let mut items: Vec<PrFeedbackItem> = Vec::new();
    for comment in &review_comments {
        if comment.in_reply_to_id.is_some() {
            continue;
        }
        let body = comment.body.trim();
        if body.is_empty() {
            continue;
        }
        items.push(PrFeedbackItem {
            external_ref: pr_feedback_external_ref(&repo, pr_number, "review-comment", comment.id),
            source: "review-comment",
            author: comment.user.login.clone(),
            body: body.to_string(),
            url: comment.html_url.trim().to_string(),
            path: comment.path.clone(),
            line: comment.line,
        });
    }
    for comment in &issue_comments {
        let body = comment.body.trim();
        if body.is_empty() {
            continue;
        }
        items.push(PrFeedbackItem {
            external_ref: pr_feedback_external_ref(&repo, pr_number, "issue-comment", comment.id),
            source: "issue-comment",
            author: comment.user.login.clone(),
            body: body.to_string(),
            url: comment.html_url.trim().to_string(),
            path: None,
            line: None,
        });
    }
    for review in &reviews {
        let body = review.body.trim();
        if body.is_empty() {
            continue;
        }
        items.push(PrFeedbackItem {
            external_ref: pr_feedback_external_ref(&repo, pr_number, "review", review.id),
            source: "review",
            author: review.user.login.clone(),
            body: body.to_string(),
            url: review.html_url.trim().to_string(),
            path: None,
            line: None,
        });
    }

    println!("PR feedback: {repo}#{pr_number}");
    println!("URL: {pr_url}");
    println!(
        "Reviews: {} ({})",
        reviews.len(),
        format_review_state_counts(&reviews)
    );
    println!("Review comments: {}", review_comments.len());
    println!("Issue comments: {}", issue_comments.len());

    let snapshot_path = write_pr_feedback_snapshot(repo_root, &repo, pr_number, &pr_url, &items)?;
    println!("Snapshot: {}", snapshot_path.display());

    if items.is_empty() {
        println!("No actionable text feedback found.");
        return Ok(());
    }

    println!();
    println!("Actionable items ({}):", items.len());
    for (idx, item) in items.iter().enumerate() {
        let preview = compact_single_line(&item.body, 120);
        if let Some(location) = feedback_location_label(item) {
            println!("{}. [{}] {} {}", idx + 1, item.source, location, preview);
        } else {
            println!("{}. [{}] {}", idx + 1, item.source, preview);
        }
        println!("   by {}  {}", item.author, item.url);
    }

    if cmd.record_todos {
        let created = record_pr_feedback_todos(repo_root, &repo, pr_number, &items)?;
        if created.is_empty() {
            println!("Todos: no new todos created (all feedback already tracked).");
        } else {
            println!("Todos: created {} item(s).", created.len());
            println!("Use `f todo list` to review them.");
        }
    } else {
        println!("Tip: rerun with `--todo` to record these items into `.ai/todos/todos.json`.");
    }

    Ok(())
}

#[derive(Debug, Clone)]
struct GhPrView {
    title: String,
    body: String,
}

fn gh_pr_view(repo_root: &Path, repo: &str, number: u64) -> Result<GhPrView> {
    #[derive(serde::Deserialize)]
    struct GhPrJson {
        title: String,
        body: String,
    }

    let out = gh_capture_in(
        repo_root,
        &[
            "pr",
            "view",
            &number.to_string(),
            "--repo",
            repo,
            "--json",
            "title,body",
        ],
    )?;
    let parsed: GhPrJson = serde_json::from_str(out.trim())
        .with_context(|| format!("failed to parse gh pr view JSON for #{number}"))?;
    Ok(GhPrView {
        title: parsed.title,
        body: parsed.body,
    })
}

fn flow_project_name(repo_root: &Path) -> String {
    let flow_toml = repo_root.join("flow.toml");
    if flow_toml.exists() {
        if let Ok(cfg) = crate::config::load(&flow_toml) {
            if let Some(name) = cfg
                .project_name
                .as_deref()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
            {
                return name.to_string();
            }
        }
    }

    repo_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project")
        .to_string()
}

fn open_in_zed_preview(path: &Path) -> Result<()> {
    // Prefer Zed Preview if installed, otherwise fall back to Zed.
    let try_open = |app: &str| -> Result<()> {
        Command::new("open")
            .args(["-a", app])
            .arg(path)
            .status()
            .with_context(|| format!("failed to open {app}"))?;
        Ok(())
    };

    try_open("/Applications/Zed Preview.app").or_else(|_| try_open("/Applications/Zed.app"))
}

fn parse_pr_edit_markdown(text: &str) -> Result<(String, String)> {
    // Expected shape:
    //   # Title
    //   <one line title>
    //
    //   # Description
    //   <markdown body...>
    let mut title: Option<String> = None;
    let mut desc_lines: Vec<String> = Vec::new();

    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let l = line.trim_end();
        if l.trim() == "# Title" {
            // Consume subsequent blank lines then read the first non-empty line as the title.
            while let Some(nl) = lines.peek() {
                if nl.trim().is_empty() {
                    lines.next();
                } else {
                    break;
                }
            }
            if let Some(nl) = lines.peek() {
                let t = nl.trim();
                if !t.is_empty() {
                    title = Some(t.to_string());
                }
            }
            continue;
        }
        if l.trim() == "# Description" {
            // Skip leading blank lines in description.
            while let Some(nl) = lines.peek() {
                if nl.trim().is_empty() {
                    lines.next();
                } else {
                    break;
                }
            }
            // Collect remainder verbatim.
            for rest in lines {
                desc_lines.push(rest.to_string());
            }
            break;
        }
    }

    let title = title.unwrap_or_default().trim().to_string();
    if title.is_empty() {
        bail!("missing PR title in edit file (expected a non-empty line under `# Title`)");
    }
    let body = desc_lines.join("\n").trim_end().to_string();
    Ok((title, body))
}

fn render_pr_edit_markdown(title: &str, body: &str) -> String {
    let mut out = String::new();
    out.push_str("# Title\n\n");
    out.push_str(title.trim());
    out.push_str("\n\n# Description\n\n");
    out.push_str(body.trim_end());
    out.push('\n');
    out
}

fn render_pr_edit_markdown_with_frontmatter(
    repo: &str,
    number: u64,
    title: &str,
    body: &str,
) -> String {
    let mut out = String::new();
    out.push_str("---\n");
    out.push_str("repo: ");
    out.push_str(repo.trim());
    out.push('\n');
    out.push_str("pr: ");
    out.push_str(&number.to_string());
    out.push_str("\n---\n\n");
    out.push_str(&render_pr_edit_markdown(title, body));
    out
}

fn strip_existing_frontmatter(text: &str) -> &str {
    // If the file starts with a YAML frontmatter block, strip it so we can replace/insert ours.
    // Frontmatter:
    //   ---
    //   ...
    //   ---
    let mut lines = text.lines();
    let Some(first) = lines.next() else {
        return text;
    };
    if first.trim() != "---" {
        return text;
    }
    let mut idx = first.len() + 1; // include newline
    for line in lines {
        idx += line.len() + 1;
        if line.trim() == "---" {
            break;
        }
    }
    // Skip trailing blank line(s) after frontmatter.
    let remainder = &text[idx..];
    remainder.trim_start_matches('\n')
}

fn ensure_pr_edit_frontmatter(path: &Path, repo: &str, number: u64) -> Result<()> {
    use std::fs;
    let existing = fs::read_to_string(path).unwrap_or_default();
    let remainder = strip_existing_frontmatter(&existing);
    let rendered = format!(
        "---\nrepo: {}\npr: {}\n---\n\n{}",
        repo.trim(),
        number,
        remainder.trim_start()
    );
    if rendered != existing {
        fs::write(path, rendered)?;
    }
    Ok(())
}

fn gh_pr_edit(repo_root: &Path, repo: &str, number: u64, title: &str, body: &str) -> Result<()> {
    use std::fs;

    let tmp_dir = std::env::temp_dir().join("flow-pr-edit");
    let _ = fs::create_dir_all(&tmp_dir);
    let patch_path = tmp_dir.join(format!("pr-{number}.patch.json"));
    let normalized_body = normalize_markdown_linebreaks(body);
    let payload = serde_json::json!({
        "title": title,
        "body": normalized_body,
    });
    fs::write(&patch_path, serde_json::to_string(&payload)?)?;

    // Use the REST API instead of `gh pr edit` to avoid GitHub GraphQL breaking changes.
    let endpoint = format!("repos/{repo}/pulls/{number}");
    let output = Command::new("gh")
        .current_dir(repo_root)
        .arg("api")
        .arg("-X")
        .arg("PATCH")
        .arg(endpoint)
        .arg("--input")
        .arg(&patch_path)
        .arg("--silent")
        .output()
        .context("failed to run gh api (PATCH pull request)")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        bail!("failed to update PR via GitHub API:\n{stdout}\n{stderr}");
    }
    Ok(())
}

fn resolve_pr_for_open(repo_root: &Path, opts: &PrOpts) -> Result<(String, u64, String)> {
    ensure_gh_available()?;
    let repo = resolve_github_repo(repo_root)?;

    // Prefer opening based on the current git branch name (most intuitive UX).
    let branch = git_capture_in(repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .unwrap_or_else(|_| "HEAD".to_string())
        .trim()
        .to_string();
    if !branch.is_empty() && branch != "HEAD" {
        if let Some((n, url)) = gh_find_open_pr_by_head(repo_root, &repo, &branch)? {
            return Ok((repo, n, url));
        }
    }

    // Fallback: open based on queued commit (by explicit hash, by HEAD SHA, or latest entry).
    let hash = if let Some(hash) = opts.hash.clone() {
        hash
    } else {
        let head_sha = git_capture_in(repo_root, &["rev-parse", "HEAD"])
            .unwrap_or_default()
            .trim()
            .to_string();
        let _ = refresh_commit_queue(repo_root);
        let mut entries = load_commit_queue_entries(repo_root)?;
        if entries.is_empty() {
            bail!("No PR found for current branch and commit queue is empty.");
        }
        if !head_sha.is_empty() {
            if let Some(entry) = entries.iter().rev().find(|e| e.commit_sha == head_sha) {
                entry.commit_sha.clone()
            } else {
                entries.pop().unwrap().commit_sha
            }
        } else {
            entries.pop().unwrap().commit_sha
        }
    };

    let mut entry = resolve_commit_queue_entry(repo_root, &hash)?;
    let _ = refresh_queue_entry_commit(repo_root, &mut entry);

    let head = default_pr_head(&entry);
    let (number, url) = if let Some(url) = entry
        .pr_url
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
    {
        let n = entry
            .pr_number
            .or_else(|| pr_number_from_url(&url))
            .unwrap_or(0);
        if n > 0 {
            (n, url)
        } else if let Some((n, u)) = gh_find_open_pr_by_head(repo_root, &repo, &head)? {
            (n, u)
        } else {
            // If URL exists but we can't parse number or find by head, re-create is risky; just fail.
            bail!("found PR url in queue entry but could not resolve PR number");
        }
    } else if let Some((n, u)) = gh_find_open_pr_by_head(repo_root, &repo, &head)? {
        (n, u)
    } else {
        // Create it if missing (as draft).
        let gh_head = ensure_pr_head_pushed(repo_root, &head, &entry.commit_sha)?;
        let (title, body_rest) = commit_message_title_body(&entry.message);
        let (n, u) = if let Some(found) = gh_find_open_pr_by_head(repo_root, &repo, &gh_head)? {
            found
        } else {
            gh_create_pr(
                repo_root,
                &repo,
                &gh_head,
                &opts.base,
                &title,
                body_rest.trim(),
                true,
            )?
        };
        entry.pr_number = Some(n);
        entry.pr_url = Some(u.clone());
        entry.pr_head = Some(head.clone());
        entry.pr_base = Some(opts.base.clone());
        let _ = write_commit_queue_entry(repo_root, &entry);
        (n, u)
    };

    Ok((repo, number, url))
}

fn run_pr_open_edit(repo_root: &Path, opts: &PrOpts) -> Result<()> {
    use ::notify::RecursiveMode;
    use notify_debouncer_mini::new_debouncer;
    use std::fs;
    use std::sync::mpsc;
    use std::time::Duration;

    let (repo, number, url) = resolve_pr_for_open(repo_root, opts)?;
    let current = gh_pr_view(repo_root, &repo, number)?;

    let project = flow_project_name(repo_root);
    let home = dirs::home_dir().context("could not resolve home directory")?;
    let edit_dir = home.join(".flow").join("pr-edit");
    fs::create_dir_all(&edit_dir)?;
    let edit_path = edit_dir.join(format!("{project}-{number}.md"));

    if !edit_path.exists() {
        let rendered =
            render_pr_edit_markdown_with_frontmatter(&repo, number, &current.title, &current.body);
        fs::write(&edit_path, rendered)?;
    } else {
        // Backfill frontmatter for older files so the always-on daemon can sync them.
        let _ = ensure_pr_edit_frontmatter(&edit_path, &repo, number);
    }

    // Register a sidecar mapping too (useful if users delete the frontmatter).
    let _ = crate::pr_edit::index_upsert_file(&edit_path, &repo, number);

    println!("PR: {url}");
    if !opts.no_open {
        let _ = open_in_browser(&url);
    }

    open_in_zed_preview(&edit_path)?;
    println!(
        "Editing {} (save to sync to GitHub, Ctrl-C to stop)",
        edit_path.display()
    );

    // Seed hash so the initial file creation/open doesn't immediately trigger an API update.
    let mut last_hash: Option<String> = fs::read_to_string(&edit_path).ok().map(|text| {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        use std::hash::Hash;
        use std::hash::Hasher;
        text.hash(&mut hasher);
        format!("{:x}", hasher.finish())
    });
    let (event_tx, event_rx) = mpsc::channel();
    let mut debouncer = new_debouncer(Duration::from_millis(250), event_tx)
        .context("failed to initialize file watcher")?;
    debouncer
        .watcher()
        .watch(
            edit_path.parent().unwrap_or(repo_root).as_ref(),
            RecursiveMode::NonRecursive,
        )
        .with_context(|| format!("failed to watch {}", edit_path.display()))?;

    loop {
        match event_rx.recv() {
            Ok(Ok(events)) => {
                let touched = events.iter().any(|e| e.path == edit_path);
                if !touched {
                    continue;
                }
                let Ok(text) = fs::read_to_string(&edit_path) else {
                    continue;
                };
                // Lightweight dedupe to avoid re-sending on editor temp writes.
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                use std::hash::Hash;
                use std::hash::Hasher;
                text.hash(&mut hasher);
                let h = format!("{:x}", hasher.finish());
                if last_hash.as_deref() == Some(&h) {
                    continue;
                }
                last_hash = Some(h);

                match parse_pr_edit_markdown(&text) {
                    Ok((title, body)) => {
                        if let Err(err) = gh_pr_edit(repo_root, &repo, number, &title, &body) {
                            eprintln!("Failed to update PR #{number}: {err:#}");
                        } else {
                            println!("✓ Updated PR #{number}");
                        }
                    }
                    Err(err) => {
                        eprintln!("Skipped update: {err:#}");
                    }
                }
            }
            Ok(Err(err)) => {
                eprintln!("watcher error: {err:?}");
            }
            Err(_) => break,
        }
    }

    Ok(())
}

fn format_queue_created_at(ts: &str) -> String {
    if ts.trim().is_empty() {
        return "unknown".to_string();
    }

    let parsed = chrono::DateTime::parse_from_rfc3339(ts).or_else(|_| {
        chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%S%.fZ")
            .map(|dt| dt.and_utc().fixed_offset())
    });

    let Ok(dt) = parsed else {
        return ts.to_string();
    };

    let now = chrono::Utc::now();
    let duration = now.signed_duration_since(dt);
    let seconds = duration.num_seconds();
    if seconds < 0 {
        return "just now".to_string();
    }

    let minutes = duration.num_minutes();
    let hours = duration.num_hours();
    let days = duration.num_days();
    let weeks = days / 7;

    if seconds < 60 {
        "just now".to_string()
    } else if minutes < 60 {
        format!("{}m ago", minutes)
    } else if hours < 24 {
        format!("{}h ago", hours)
    } else if days == 1 {
        "yesterday".to_string()
    } else if days < 7 {
        format!("{}d ago", days)
    } else if weeks < 4 {
        format!("{}w ago", weeks)
    } else {
        dt.format("%b %d").to_string()
    }
}

fn get_openai_key() -> Result<String> {
    std::env::var("OPENAI_API_KEY").context("OPENAI_API_KEY environment variable not set")
}

#[derive(Debug, Clone)]
enum CommitMessageProvider {
    OpenAi { api_key: String },
    Remote { api_url: String, token: String },
}

#[derive(Debug, Clone)]
enum CommitMessageOverride {
    Selection(CommitMessageSelection),
}

fn parse_commit_message_override(
    tool: &str,
    model: Option<String>,
) -> Option<CommitMessageOverride> {
    parse_commit_message_selection_with_model(tool, model).map(CommitMessageOverride::Selection)
}

fn resolve_commit_message_override(repo_root: &Path) -> Option<CommitMessageOverride> {
    // TypeScript config has highest priority.
    if let Some(ts_config) = config::load_ts_config() {
        if let Some(flow) = ts_config.flow {
            if let Some(commit) = flow.commit {
                if let Some(tool) = commit.message_tool {
                    return parse_commit_message_override(&tool, commit.message_model);
                }
            }
        }
    }

    // Local flow.toml
    let local_config = repo_root.join("flow.toml");
    if local_config.exists() {
        if let Ok(cfg) = config::load(&local_config) {
            if let Some(commit_cfg) = cfg.commit.as_ref() {
                if let Some(tool) = commit_cfg.message_tool.as_deref() {
                    return parse_commit_message_override(tool, commit_cfg.message_model.clone());
                }
            }
        }
    }

    // Global flow config
    let global_config = config::default_config_path();
    if global_config.exists() {
        if let Ok(cfg) = config::load(&global_config) {
            if let Some(commit_cfg) = cfg.commit.as_ref() {
                if let Some(tool) = commit_cfg.message_tool.as_deref() {
                    return parse_commit_message_override(tool, commit_cfg.message_model.clone());
                }
            }
        }
    }

    None
}

fn resolve_commit_message_providers() -> Vec<CommitMessageProvider> {
    let mut providers = Vec::new();

    if let Ok(Some(token)) = crate::env::load_ai_auth_token() {
        if let Ok(api_url) = crate::env::load_ai_api_url() {
            let trimmed_url = api_url.trim().trim_end_matches('/').to_string();
            if !trimmed_url.is_empty() {
                providers.push(CommitMessageProvider::Remote {
                    api_url: trimmed_url,
                    token,
                });
            }
        }
    }

    if let Ok(api_key) = get_openai_key() {
        let trimmed = api_key.trim().to_string();
        if !trimmed.is_empty() {
            providers.push(CommitMessageProvider::OpenAi { api_key: trimmed });
        }
    }

    providers
}

fn commit_message_from_provider(
    provider: &CommitMessageProvider,
    diff: &str,
    status: &str,
    truncated: bool,
) -> Result<String> {
    let message = match provider {
        CommitMessageProvider::OpenAi { api_key } => {
            generate_commit_message(api_key, diff, status, truncated)
        }
        CommitMessageProvider::Remote { api_url, token } => {
            generate_commit_message_remote(api_url, token, diff, status, truncated)
        }
    }?;
    Ok(sanitize_commit_message(&message))
}

fn commit_message_from_selection(
    selection: &CommitMessageSelection,
    providers: &[CommitMessageProvider],
    diff: &str,
    status: &str,
    truncated: bool,
) -> Result<String> {
    match selection {
        CommitMessageSelection::Kimi { model } => {
            generate_commit_message_kimi(diff, status, truncated, model.as_deref())
        }
        CommitMessageSelection::Claude => generate_commit_message_claude(diff, status, truncated),
        CommitMessageSelection::Opencode { model } => {
            generate_commit_message_opencode(diff, status, truncated, model)
        }
        CommitMessageSelection::OpenRouter { model } => {
            generate_commit_message_openrouter(diff, status, truncated, model)
        }
        CommitMessageSelection::Rise { model } => {
            generate_commit_message_rise(diff, status, truncated, model)
        }
        CommitMessageSelection::Remote => {
            let provider = providers
                .iter()
                .find(|provider| matches!(provider, CommitMessageProvider::Remote { .. }))
                .ok_or_else(|| anyhow!("myflow provider unavailable; run `f auth`"))?;
            commit_message_from_provider(provider, diff, status, truncated)
        }
        CommitMessageSelection::OpenAi => {
            let provider = providers
                .iter()
                .find(|provider| matches!(provider, CommitMessageProvider::OpenAi { .. }))
                .ok_or_else(|| anyhow!("OPENAI_API_KEY is not configured"))?;
            commit_message_from_provider(provider, diff, status, truncated)
        }
        CommitMessageSelection::Heuristic => Ok(build_deterministic_commit_message(diff)),
    }
}

fn truncate_commit_subject(subject: &str) -> String {
    if subject.chars().count() <= 72 {
        return subject.to_string();
    }
    let mut truncated: String = subject.chars().take(69).collect();
    while truncated.ends_with(' ') {
        truncated.pop();
    }
    format!("{}...", truncated)
}

fn build_deterministic_commit_message(diff: &str) -> String {
    let mut files = changed_files_from_diff(diff);
    files.sort();
    files.dedup();

    let subject = if files.is_empty() {
        "Update project files".to_string()
    } else if files.len() == 1 {
        format!("Update {}", files[0])
    } else {
        format!("Update {} files", files.len())
    };
    let subject = truncate_commit_subject(&subject);

    if files.is_empty() {
        return subject;
    }

    let mut lines = Vec::new();
    for file in files.iter().take(3) {
        lines.push(format!("- {}", file));
    }
    if files.len() > 3 {
        lines.push(format!("- and {} more files", files.len() - 3));
    }

    if lines.is_empty() {
        subject
    } else {
        format!("{}\n\n{}", subject, lines.join("\n"))
    }
}

fn generate_commit_message_with_fallbacks(
    repo_root: &Path,
    review_selection: Option<&ReviewSelection>,
    commit_message_override: Option<&CommitMessageOverride>,
    diff: &str,
    status: &str,
    truncated: bool,
) -> Result<String> {
    let providers = resolve_commit_message_providers();
    let override_selection = commit_message_override.map(|override_tool| match override_tool {
        CommitMessageOverride::Selection(selection) => selection,
    });
    let attempts = commit_message_attempts(repo_root, review_selection, override_selection);

    let mut errors: Vec<String> = Vec::new();
    for (idx, selection) in attempts.iter().enumerate() {
        match commit_message_from_selection(selection, &providers, diff, status, truncated) {
            Ok(message) => {
                let sanitized = sanitize_commit_message(&message);
                if sanitized.trim().is_empty() {
                    errors.push(format!(
                        "{} returned an empty commit message",
                        selection.key()
                    ));
                    continue;
                }
                if idx > 0 {
                    println!(
                        "✓ Commit message fallback succeeded via {}",
                        selection.label()
                    );
                }
                return Ok(sanitized);
            }
            Err(err) => {
                if idx + 1 < attempts.len() {
                    println!(
                        "⚠ {} commit message failed: {}. Trying next fallback...",
                        selection.label(),
                        err
                    );
                }
                errors.push(format!("{}: {}", selection.key(), err));
            }
        }
    }

    if commit_message_fail_open_enabled(repo_root) {
        println!("⚠ Commit message generation failed; using deterministic fallback message.");
        return Ok(build_deterministic_commit_message(diff));
    }

    if errors.is_empty() {
        bail!(
            "commit message generation failed: no valid tools/providers configured for this repo"
        );
    }

    bail!(
        "commit message generation failed:\n  {}",
        errors.join("\n  ")
    )
}

fn sanitize_commit_message(message: &str) -> String {
    let filtered: Vec<&str> = message
        .lines()
        .filter(|line| !line.trim().contains("[Image #"))
        .collect();

    let cleaned = filtered.join("\n").trim().to_string();
    if cleaned.is_empty() {
        return message.trim().to_string();
    }
    cleaned
}

fn generate_commit_message_kimi(
    diff: &str,
    status: &str,
    truncated: bool,
    model: Option<&str>,
) -> Result<String> {
    let mut prompt = String::from(
        "Write a git commit message for these changes. Output ONLY the commit message, nothing else.\n\n\
         Guidelines:\n\
         - Use imperative mood (\"Add feature\" not \"Added feature\")\n\
         - First line: concise summary under 72 chars\n\
         - Focus on WHAT and WHY, not just listing files\n\
         - Never include secrets or credentials\n\n\
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

    info!(
        model = model.unwrap_or("default"),
        prompt_len = prompt.len(),
        "calling kimi for commit message"
    );

    let mut cmd = Command::new("kimi");
    cmd.args(["--quiet"]);
    if let Some(model) = model {
        if !model.trim().is_empty() {
            cmd.args(["--model", model]);
        }
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .context("failed to run kimi for commit message")?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(prompt.as_bytes())
            .context("failed to write prompt to kimi")?;
    }

    let output = child
        .wait_with_output()
        .context("failed to wait for kimi output")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let error_msg = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        bail!("kimi failed: {}", error_msg);
    }

    let message = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if message.is_empty() {
        bail!("kimi returned empty commit message");
    }

    Ok(message)
}

fn git_run(args: &[&str]) -> Result<()> {
    let mut cmd = Command::new("git");
    if args.first() == Some(&"commit") {
        cmd.env("FLOW_COMMIT", "1");
        if entire_enabled() {
            cmd.env("ENTIRE_TEST_TTY", "1");
        }
    }
    let status = cmd
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
    let mut cmd = Command::new("git");
    if args.first() == Some(&"commit") {
        cmd.env("FLOW_COMMIT", "1");
        if entire_enabled() {
            cmd.env("ENTIRE_TEST_TTY", "1");
        }
    }
    let status = cmd
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

/// Push result indicating success, remote ahead, or no remote repo.
enum PushResult {
    Success,
    RemoteAhead,
    NoRemoteRepo,
}

fn branch_is_detached(branch: &str) -> bool {
    let trimmed = branch.trim();
    trimmed.is_empty() || trimmed == "HEAD"
}

fn git_push_args<'a>(remote: &'a str, branch: &'a str) -> Vec<&'a str> {
    if branch_is_detached(branch) {
        vec!["push", remote, "HEAD"]
    } else {
        vec!["push", "-u", remote, branch.trim()]
    }
}

fn git_pull_rebase_args<'a>(remote: &'a str, branch: &'a str) -> Vec<&'a str> {
    if branch_is_detached(branch) {
        vec!["pull", "--rebase"]
    } else {
        vec!["pull", "--rebase", remote, branch.trim()]
    }
}

fn git_push_run(remote: &str, branch: &str) -> Result<()> {
    let args = git_push_args(remote, branch);
    git_run(&args)
}

fn git_push_run_in(workdir: &std::path::Path, remote: &str, branch: &str) -> Result<()> {
    let args = git_push_args(remote, branch);
    git_run_in(workdir, &args)
}

fn git_pull_rebase_try(remote: &str, branch: &str) -> Result<()> {
    let args = git_pull_rebase_args(remote, branch);
    git_try(&args)
}

fn git_pull_rebase_try_in(workdir: &std::path::Path, remote: &str, branch: &str) -> Result<()> {
    let args = git_pull_rebase_args(remote, branch);
    git_try_in(workdir, &args)
}

/// Try to push and detect if failure is due to missing remote repo.
fn git_push_try(remote: &str, branch: &str) -> PushResult {
    let args = git_push_args(remote, branch);
    let output = Command::new("git").args(args).output().ok();

    let Some(output) = output else {
        return PushResult::RemoteAhead;
    };

    if output.status.success() {
        return PushResult::Success;
    }

    let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
    if stderr.contains("repository not found")
        || stderr.contains("does not exist")
        || stderr.contains("could not read from remote")
    {
        PushResult::NoRemoteRepo
    } else {
        PushResult::RemoteAhead
    }
}

fn git_push_try_in(workdir: &std::path::Path, remote: &str, branch: &str) -> PushResult {
    let args = git_push_args(remote, branch);
    let output = Command::new("git")
        .current_dir(workdir)
        .args(args)
        .output()
        .ok();

    let Some(output) = output else {
        return PushResult::RemoteAhead;
    };

    if output.status.success() {
        return PushResult::Success;
    }

    let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
    if stderr.contains("repository not found")
        || stderr.contains("does not exist")
        || stderr.contains("could not read from remote")
    {
        PushResult::NoRemoteRepo
    } else {
        PushResult::RemoteAhead
    }
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

#[derive(Default)]
struct GitCaptureCacheState {
    depth: usize,
    entries: HashMap<String, String>,
}

thread_local! {
    static GIT_CAPTURE_CACHE: RefCell<GitCaptureCacheState> = RefCell::new(GitCaptureCacheState::default());
}

struct GitCaptureCacheScope;

impl GitCaptureCacheScope {
    fn begin() -> Self {
        GIT_CAPTURE_CACHE.with(|state| {
            let mut state = state.borrow_mut();
            if state.depth == 0 {
                state.entries.clear();
            }
            state.depth += 1;
        });
        Self
    }
}

impl Drop for GitCaptureCacheScope {
    fn drop(&mut self) {
        GIT_CAPTURE_CACHE.with(|state| {
            let mut state = state.borrow_mut();
            state.depth = state.depth.saturating_sub(1);
            if state.depth == 0 {
                state.entries.clear();
            }
        });
    }
}

fn git_capture_cacheable(args: &[&str]) -> bool {
    args == ["rev-parse", "--show-toplevel"]
        || args == ["rev-parse", "--git-dir"]
        || (args.len() == 3 && args[0] == "remote" && args[1] == "get-url")
}

fn git_capture_cache_key(workdir: Option<&std::path::Path>, args: &[&str]) -> Option<String> {
    if !git_capture_cacheable(args) {
        return None;
    }

    let cwd = workdir
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    Some(format!("{cwd}|{}", args.join("\x1f")))
}

fn git_capture_cached_lookup(key: &str) -> Option<String> {
    GIT_CAPTURE_CACHE.with(|state| {
        let state = state.borrow();
        if state.depth == 0 {
            return None;
        }
        state.entries.get(key).cloned()
    })
}

fn git_capture_cached_store(key: String, value: String) {
    GIT_CAPTURE_CACHE.with(|state| {
        let mut state = state.borrow_mut();
        if state.depth > 0 {
            state.entries.insert(key, value);
        }
    });
}

fn git_capture(args: &[&str]) -> Result<String> {
    if let Some(key) = git_capture_cache_key(None, args) {
        if let Some(cached) = git_capture_cached_lookup(&key) {
            return Ok(cached);
        }
    }

    let output = Command::new("git")
        .args(args)
        .output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;

    if !output.status.success() {
        bail!("git {} failed", args.join(" "));
    }

    let out = String::from_utf8_lossy(&output.stdout).to_string();
    if let Some(key) = git_capture_cache_key(None, args) {
        git_capture_cached_store(key, out.clone());
    }
    Ok(out)
}

fn git_capture_in(workdir: &std::path::Path, args: &[&str]) -> Result<String> {
    if let Some(key) = git_capture_cache_key(Some(workdir), args) {
        if let Some(cached) = git_capture_cached_lookup(&key) {
            return Ok(cached);
        }
    }

    let output = Command::new("git")
        .current_dir(workdir)
        .args(args)
        .output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;

    if !output.status.success() {
        bail!("git {} failed", args.join(" "));
    }

    let out = String::from_utf8_lossy(&output.stdout).to_string();
    if let Some(key) = git_capture_cache_key(Some(workdir), args) {
        git_capture_cached_store(key, out.clone());
    }
    Ok(out)
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

/// Generate commit message using opencode or OpenRouter directly.
#[allow(dead_code)]
fn generate_commit_message_opencode(
    diff: &str,
    status: &str,
    truncated: bool,
    model: &str,
) -> Result<String> {
    // For OpenRouter models, call API directly to avoid tool use issues
    if model.starts_with("openrouter/") {
        return generate_commit_message_openrouter(diff, status, truncated, model);
    }

    // For zen models (and others), use opencode run with --print flag
    generate_commit_message_opencode_run(diff, status, truncated, model)
}

/// Generate commit message using opencode run command.
fn generate_commit_message_opencode_run(
    diff: &str,
    status: &str,
    truncated: bool,
    model: &str,
) -> Result<String> {
    let mut prompt = String::from(
        "Write a git commit message for these changes. Output ONLY the commit message, nothing else.\n\n\
         Guidelines:\n\
         - Use imperative mood (\"Add feature\" not \"Added feature\")\n\
         - First line: concise summary under 72 chars\n\
         - Focus on WHAT and WHY, not just listing files\n\n\
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

    info!(
        model = model,
        prompt_len = prompt.len(),
        "calling opencode run for commit message"
    );
    let start = std::time::Instant::now();

    // Use --format json to get output in non-interactive mode
    let output = Command::new("opencode")
        .args(["run", "--model", model, "--format", "json", &prompt])
        .output()
        .context("failed to run opencode for commit message")?;

    info!(
        elapsed_ms = start.elapsed().as_millis() as u64,
        success = output.status.success(),
        stdout_len = output.stdout.len(),
        stderr_len = output.stderr.len(),
        "opencode run completed"
    );

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let error_msg = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        bail!("opencode failed: {}", error_msg);
    }

    // Parse JSON lines to extract text content
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut message = String::new();
    for line in stdout.lines() {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(line) {
            if json.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(text) = json
                    .get("part")
                    .and_then(|p| p.get("text"))
                    .and_then(|t| t.as_str())
                {
                    message.push_str(text);
                }
            }
        }
    }

    let message = message.trim().to_string();
    if message.is_empty() {
        bail!("opencode returned empty commit message");
    }

    Ok(trim_quotes(&message))
}

/// Generate commit message using Claude Code CLI.
fn generate_commit_message_claude(diff: &str, status: &str, truncated: bool) -> Result<String> {
    let mut prompt = String::from(
        "Write a git commit message for these changes. Output ONLY the commit message, nothing else.\n\n\
         Guidelines:\n\
         - Use imperative mood (\"Add feature\" not \"Added feature\")\n\
         - First line: concise summary under 72 chars\n\
         - Focus on WHAT and WHY, not just listing files\n\n\
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

    let output = Command::new("claude")
        .args(["-p", &prompt])
        .output()
        .context("failed to run claude for commit message")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("claude failed: {}", stderr.trim());
    }

    let message = String::from_utf8_lossy(&output.stdout).trim().to_string();

    if message.is_empty() {
        bail!("claude returned empty commit message");
    }

    Ok(trim_quotes(&message))
}

/// Generate commit message using Rise daemon (local AI proxy).
fn generate_commit_message_rise(
    diff: &str,
    status: &str,
    truncated: bool,
    model: &str,
) -> Result<String> {
    let mut user_prompt =
        String::from("Write a git commit message for the staged changes.\n\nGit diff:\n");
    user_prompt.push_str(diff);

    if truncated {
        user_prompt.push_str("\n\n[Diff truncated]");
    }

    let status = status.trim();
    if !status.is_empty() {
        user_prompt.push_str("\n\nGit status:\n");
        user_prompt.push_str(status);
    }

    let client = crate::http_client::blocking_with_timeout(std::time::Duration::from_secs(120))
        .context("failed to create HTTP client")?;

    let body = ChatRequest {
        model: model.to_string(),
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

    info!(model = model, "calling Rise daemon for commit message");
    let start = std::time::Instant::now();

    let rise_url = rise_url();
    let text = send_rise_request_text(&client, &rise_url, &body, model)?;

    info!(
        elapsed_ms = start.elapsed().as_millis() as u64,
        "Rise daemon responded"
    );
    let message = parse_rise_output(&text).context("failed to parse Rise response")?;

    let message = message.trim().to_string();
    if message.is_empty() {
        bail!("Rise daemon returned empty commit message");
    }

    Ok(trim_quotes(&message))
}

/// Generate commit message using OpenRouter API directly.
fn generate_commit_message_openrouter(
    diff: &str,
    status: &str,
    truncated: bool,
    model: &str,
) -> Result<String> {
    let api_key = openrouter_api_key()?;
    let model_id = openrouter_model_id(model);

    let mut user_prompt =
        String::from("Write a git commit message for the staged changes.\n\nGit diff:\n");
    user_prompt.push_str(diff);

    if truncated {
        user_prompt.push_str("\n\n[Diff truncated]");
    }

    let status = status.trim();
    if !status.is_empty() {
        user_prompt.push_str("\n\nGit status:\n");
        user_prompt.push_str(status);
    }

    let client = openrouter_http_client(Duration::from_secs(60))?;

    let body = ChatRequest {
        model: model_id.to_string(),
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

    let parsed: ChatResponse = openrouter_chat_completion_with_retry(&client, &api_key, &body)
        .context("OpenRouter request failed")?;

    let message = parsed
        .choices
        .first()
        .and_then(|c| c.message.as_ref())
        .map(|m| m.content.trim().to_string())
        .unwrap_or_default();

    if message.is_empty() {
        bail!("OpenRouter returned empty commit message");
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

    let client = crate::http_client::blocking_with_timeout(std::time::Duration::from_secs(60))
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

fn generate_commit_message_remote(
    api_url: &str,
    token: &str,
    diff: &str,
    status: &str,
    truncated: bool,
) -> Result<String> {
    let trimmed = api_url.trim().trim_end_matches('/');
    let url = format!("{}/api/ai/commit-message", trimmed);

    let client = crate::http_client::blocking_with_timeout(Duration::from_secs(
        commit_with_check_timeout_secs(),
    ))
    .context("failed to create HTTP client for remote commit message")?;

    let payload = json!({
        "diff": diff,
        "status": status,
        "truncated": truncated,
    });

    let response = client
        .post(&url)
        .bearer_auth(token)
        .json(&payload)
        .send()
        .context("failed to request remote commit message")?;

    if !response.status().is_success() {
        if response.status() == StatusCode::UNAUTHORIZED {
            bail!("remote commit message unauthorized. Run `f auth` to login.");
        }
        if response.status() == StatusCode::PAYMENT_REQUIRED {
            bail!(
                "remote commit message requires an active subscription. Visit myflow to subscribe."
            );
        }
        let status = response.status();
        let body = response.text().unwrap_or_default();
        bail!("remote commit message failed: HTTP {} {}", status, body);
    }

    let payload: RemoteCommitMessageResponse = response
        .json()
        .context("failed to parse remote commit message response")?;

    let message = payload.message.trim().to_string();
    if message.is_empty() {
        bail!("remote commit message was empty");
    }

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

/// Extract text content from kimi's stream-json output.
/// Format: {"role":"assistant","content":[{"type":"think","think":"..."},{"type":"text","text":"..."}]}
fn extract_kimi_text_content(output: &str) -> Option<String> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Try to parse as JSON
    let json: serde_json::Value = serde_json::from_str(trimmed).ok()?;

    // Extract content array
    let content = json.get("content")?.as_array()?;

    // Find the "text" type content and concatenate all text
    let mut text_parts = Vec::new();
    for item in content {
        if item.get("type").and_then(|t| t.as_str()) == Some("text") {
            if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                text_parts.push(text.to_string());
            }
        }
    }

    if text_parts.is_empty() {
        None
    } else {
        Some(text_parts.join("\n"))
    }
}

fn normalize_future_tasks(tasks: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut normalized = Vec::new();
    for task in tasks {
        let trimmed = task.trim().trim_start_matches('-').trim();
        if trimmed.is_empty() {
            continue;
        }
        let key = trimmed.to_lowercase();
        if seen.insert(key) {
            normalized.push(trimmed.to_string());
        }
    }
    normalized
}

fn openrouter_model_id(model: &str) -> &str {
    // Only strip "openrouter/" prefix when there's a nested provider path
    // e.g. "openrouter/meta-llama/llama-3.3-70b" → "meta-llama/llama-3.3-70b"
    // but keep "openrouter/pony-alpha" as-is (first-party OpenRouter model).
    if let Some(rest) = model
        .strip_prefix("openrouter/")
        .or_else(|| model.strip_prefix("openrouter:"))
    {
        if rest.contains('/') {
            return rest;
        }
    }
    model
}

fn openrouter_model_label(model: &str) -> String {
    format!("openrouter/{}", openrouter_model_id(model))
}

fn openrouter_api_key() -> Result<String> {
    if let Ok(value) = std::env::var("OPENROUTER_API_KEY") {
        if !value.trim().is_empty() {
            return Ok(value);
        }
    }

    if is_local_env_backend() {
        if let Ok(vars) = crate::env::fetch_personal_env_vars(&["OPENROUTER_API_KEY".to_string()]) {
            if let Some(value) = vars.get("OPENROUTER_API_KEY") {
                if !value.trim().is_empty() {
                    return Ok(value.clone());
                }
            }
        }
    }

    bail!("OPENROUTER_API_KEY not set. Get one at https://openrouter.ai/keys");
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

fn record_review_outputs_to_beads_rust(
    repo_root: &Path,
    review: &ReviewResult,
    reviewer: &str,
    model_label: &str,
    committed_sha: Option<&str>,
    review_run_id: &str,
) {
    if env_flag("FLOW_BEADS_RUST_DISABLE") {
        return;
    }
    let beads_dir = beads_rust_beads_dir(repo_root);
    if let Err(err) = fs::create_dir_all(&beads_dir) {
        println!(
            "⚠️ Failed to prepare repo-local beads dir {}: {}",
            beads_dir.display(),
            err
        );
        return;
    }

    let project_path = repo_root.display().to_string();
    let project_name = repo_root
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".to_string());
    let branch = git_capture_in(repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .unwrap_or_else(|_| "unknown".to_string())
        .trim()
        .to_string();

    let sha_short = committed_sha.map(short_sha).unwrap_or("unknown");
    let project_label = safe_label_value(&project_name);
    let branch_label = safe_label_value(&branch);
    let reviewer_label = safe_label_value(reviewer);

    let mut created = 0usize;

    // Snapshot bead: one per review run, always.
    match create_review_run_bead(
        &beads_dir,
        review,
        &project_path,
        &project_name,
        &branch,
        sha_short,
        reviewer,
        model_label,
        review_run_id,
    ) {
        Ok(true) => created += 1,
        Ok(false) => {}
        Err(err) => println!("⚠️ Failed to create review snapshot bead: {}", err),
    }

    // Issue beads: one per issue in this review run.
    for issue in &review.issues {
        match create_review_issue_bead(
            &beads_dir,
            issue,
            &project_path,
            &project_name,
            &branch,
            sha_short,
            reviewer,
            model_label,
            review.summary.as_deref(),
            review_run_id,
        ) {
            Ok(true) => created += 1,
            Ok(false) => {}
            Err(err) => println!("⚠️ Failed to create review issue bead: {}", err),
        }
    }

    // Future-task beads: one per suggestion in this review run.
    for task in &review.future_tasks {
        match create_review_future_task_bead(
            &beads_dir,
            task,
            &project_path,
            &project_label,
            &branch,
            &branch_label,
            sha_short,
            &reviewer_label,
            model_label,
            review.summary.as_deref(),
            review_run_id,
        ) {
            Ok(true) => created += 1,
            Ok(false) => {}
            Err(err) => println!("⚠️ Failed to create review task bead: {}", err),
        }
    }

    if created > 0 {
        println!(
            "Recorded {} review bead(s) to {}",
            created,
            beads_dir.display()
        );
    }
}

fn create_review_run_bead(
    beads_dir: &Path,
    review: &ReviewResult,
    project_path: &str,
    project_name: &str,
    branch: &str,
    sha_short: &str,
    reviewer: &str,
    model_label: &str,
    review_run_id: &str,
) -> Result<bool> {
    let title = format!("Review: {} {}", project_name, sha_short);
    let external_ref = format!(
        "flow-review-run:{}",
        flow_review_item_id(review_run_id, "run", "snapshot")
    );
    let labels = format!(
        "flow-review,review:run,task,project:{},commit:{},branch:{},reviewer:{}",
        safe_label_value(project_name),
        sha_short,
        safe_label_value(branch),
        safe_label_value(reviewer)
    );

    let mut desc = String::new();
    desc.push_str("Review snapshot\n\n");
    desc.push_str("Project: ");
    desc.push_str(project_path);
    desc.push_str("\nBranch: ");
    desc.push_str(branch);
    desc.push_str("\nCommit: ");
    desc.push_str(sha_short);
    desc.push_str("\nReviewer: ");
    desc.push_str(reviewer);
    desc.push_str("\nModel: ");
    desc.push_str(model_label);
    desc.push_str("\nRun ID: ");
    desc.push_str(review_run_id);
    if let Some(summary) = review
        .summary
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        desc.push_str("\n\nSummary:\n");
        desc.push_str(summary);
    }
    if !review.issues.is_empty() {
        desc.push_str("\n\nIssues:\n");
        for issue in &review.issues {
            desc.push_str("- ");
            desc.push_str(issue.trim());
            desc.push('\n');
        }
    }
    if !review.future_tasks.is_empty() {
        desc.push_str("\nFuture tasks:\n");
        for task in &review.future_tasks {
            desc.push_str("- ");
            desc.push_str(task.trim());
            desc.push('\n');
        }
    }
    if review.timed_out {
        desc.push_str("\nNote: Review timed out.\n");
    }

    br_create_ephemeral(
        beads_dir,
        &title,
        &desc,
        "task",
        "4",
        "open",
        &external_ref,
        &labels,
    )
    .context("run br create for review snapshot")
}

fn create_review_issue_bead(
    beads_dir: &Path,
    issue: &str,
    project_path: &str,
    project_name: &str,
    branch: &str,
    sha_short: &str,
    reviewer: &str,
    model_label: &str,
    summary: Option<&str>,
    review_run_id: &str,
) -> Result<bool> {
    let title = review_task_title(issue);
    let external_ref = format!(
        "flow-review-issue:{}",
        flow_review_item_id(review_run_id, "issue", issue)
    );
    let labels = format!(
        "flow-review,review:issue,bug,project:{},commit:{},branch:{},reviewer:{}",
        safe_label_value(project_name),
        sha_short,
        safe_label_value(branch),
        safe_label_value(reviewer)
    );
    let priority = infer_review_bead_priority(issue).to_string();

    let mut desc = String::new();
    desc.push_str(issue.trim());
    desc.push_str("\n\nProject: ");
    desc.push_str(project_path);
    desc.push_str("\nBranch: ");
    desc.push_str(branch);
    desc.push_str("\nCommit: ");
    desc.push_str(sha_short);
    desc.push_str("\nReviewer: ");
    desc.push_str(reviewer);
    desc.push_str("\nModel: ");
    desc.push_str(model_label);
    desc.push_str("\nRun ID: ");
    desc.push_str(review_run_id);
    if let Some(summary) = summary.map(|s| s.trim()).filter(|s| !s.is_empty()) {
        desc.push_str("\nReview summary: ");
        desc.push_str(summary);
    }

    br_create_ephemeral(
        beads_dir,
        &title,
        &desc,
        "bug",
        &priority,
        "open",
        &external_ref,
        &labels,
    )
    .context("run br create for review issue")
}

fn create_review_future_task_bead(
    beads_dir: &Path,
    task: &str,
    project_path: &str,
    project_label: &str,
    branch: &str,
    branch_label: &str,
    sha_short: &str,
    reviewer_label: &str,
    model_label: &str,
    summary: Option<&str>,
    review_run_id: &str,
) -> Result<bool> {
    let title = review_task_title(task);
    let description = review_task_description_with_commit(
        task,
        project_path,
        branch,
        sha_short,
        reviewer_label,
        summary,
        model_label,
        review_run_id,
    );
    let external_ref = format!(
        "flow-review-task:{}",
        flow_review_item_id(review_run_id, "task", task)
    );
    let labels = format!(
        "flow-review,review:task,task,project:{},commit:{},branch:{},reviewer:{}",
        project_label, sha_short, branch_label, reviewer_label
    );

    br_create_ephemeral(
        beads_dir,
        &title,
        &description,
        "task",
        "4",
        "open",
        &external_ref,
        &labels,
    )
    .context("run br create for review task")
}

fn br_create_ephemeral(
    beads_dir: &Path,
    title: &str,
    description: &str,
    issue_type: &str,
    priority: &str,
    status: &str,
    external_ref: &str,
    labels: &str,
) -> Result<bool> {
    let output = Command::new("br")
        .arg("create")
        .arg("--title")
        .arg(title)
        .arg("--description")
        .arg(description)
        .arg("--type")
        .arg(issue_type)
        .arg("--priority")
        .arg(priority)
        .arg("--status")
        .arg(status)
        .arg("--external-ref")
        .arg(external_ref)
        .arg("--labels")
        .arg(labels)
        .arg("--ephemeral")
        .arg("--silent")
        .arg("--no-auto-flush")
        .arg("--no-auto-import")
        .env("BEADS_DIR", beads_dir)
        .output()
        .context("run br create")?;

    if output.status.success() {
        return Ok(true);
    }
    if br_create_failed_due_to_duplicate_external_ref(&output) {
        return Ok(false);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let msg = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    bail!("beads create failed: {}", msg);
}

fn infer_review_bead_priority(issue: &str) -> u8 {
    let lower = issue.to_lowercase();
    if lower.contains("secret")
        || lower.contains("credential")
        || lower.contains("api key")
        || lower.contains("injection")
        || lower.contains("vulnerability")
    {
        return 0; // critical
    }
    if lower.contains("crash")
        || lower.contains("data loss")
        || lower.contains("race condition")
        || lower.contains("buffer overflow")
    {
        return 1; // high
    }
    if lower.contains("bug")
        || lower.contains("error handling")
        || lower.contains("panic")
        || lower.contains("unwrap")
        || lower.contains("missing validation")
    {
        return 2; // medium
    }
    if lower.contains("style")
        || lower.contains("refactor")
        || lower.contains("unused")
        || lower.contains("naming")
        || lower.contains("dead code")
    {
        return 3; // low
    }
    3 // default for issues
}

fn br_create_failed_due_to_duplicate_external_ref(output: &std::process::Output) -> bool {
    let mut combined = String::new();
    combined.push_str(&String::from_utf8_lossy(&output.stdout));
    combined.push('\n');
    combined.push_str(&String::from_utf8_lossy(&output.stderr));
    let lower = combined.to_lowercase();
    lower.contains("unique constraint failed")
        && (lower.contains("issues.external_ref") || lower.contains("external_ref"))
}

fn safe_label_value(value: &str) -> String {
    let mut out = String::new();
    for ch in value.trim().chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "unknown".to_string()
    } else {
        out
    }
}

fn flow_review_project_key(repo_root: &Path) -> String {
    if let Ok(url) = git_capture_in(repo_root, &["config", "--get", "remote.origin.url"]) {
        let url = url.trim();
        if !url.is_empty() {
            if let Some(key) = normalize_git_remote_to_owner_repo(url) {
                return key;
            }
            return url.to_string();
        }
    }
    repo_root.display().to_string()
}

fn normalize_git_remote_to_owner_repo(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches('/');
    // git@github.com:owner/repo.git
    if let Some(rest) = trimmed.strip_prefix("git@github.com:") {
        let rest = rest.trim_end_matches(".git");
        if rest.split('/').count() == 2 {
            return Some(rest.to_string());
        }
    }
    // https://github.com/owner/repo(.git)
    if let Some(rest) = trimmed.strip_prefix("https://github.com/") {
        let rest = rest.trim_end_matches(".git");
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() >= 2 {
            return Some(format!("{}/{}", parts[0], parts[1]));
        }
    }
    None
}

fn flow_review_run_id(repo_root: &Path, diff: &str, model_label: &str, reviewer: &str) -> String {
    let project_key = flow_review_project_key(repo_root);
    let mut hasher = Sha1::new();
    hasher.update(project_key.as_bytes());
    hasher.update(b":");
    hasher.update(reviewer.trim().as_bytes());
    hasher.update(b":");
    hasher.update(model_label.trim().as_bytes());
    hasher.update(b":");
    hasher.update(diff.as_bytes());
    let hex = hex::encode(hasher.finalize());
    hex.get(..12).unwrap_or(&hex).to_string()
}

fn flow_review_item_id(review_run_id: &str, kind: &str, text: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(kind.as_bytes());
    hasher.update(b":");
    hasher.update(review_run_id.as_bytes());
    hasher.update(b":");
    hasher.update(text.trim().as_bytes());
    let hex = hex::encode(hasher.finalize());
    hex.get(..12).unwrap_or(&hex).to_string()
}

fn review_task_title(task: &str) -> String {
    let trimmed = task.trim().trim_start_matches('-').trim();
    let max_len = 120;
    let mut title = String::new();
    let mut count = 0;
    for ch in trimmed.chars() {
        if count >= max_len {
            title.push_str("...");
            break;
        }
        title.push(ch);
        count += 1;
    }
    title
}

fn review_task_description_with_commit(
    task: &str,
    project_path: &str,
    branch: &str,
    sha_short: &str,
    reviewer_label: &str,
    summary: Option<&str>,
    model_label: &str,
    review_run_id: &str,
) -> String {
    let mut desc = String::new();
    desc.push_str(task.trim());
    desc.push_str("\n\nProject: ");
    desc.push_str(project_path);
    desc.push_str("\nBranch: ");
    desc.push_str(branch);
    desc.push_str("\nCommit: ");
    desc.push_str(sha_short);
    desc.push_str("\nReviewer: ");
    desc.push_str(reviewer_label);
    desc.push_str("\nModel: ");
    desc.push_str(model_label);
    desc.push_str("\nRun ID: ");
    desc.push_str(review_run_id);
    if let Some(summary) = summary {
        desc.push_str("\nReview summary: ");
        desc.push_str(summary);
    }
    desc
}

fn env_flag(name: &str) -> bool {
    env::var(name)
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

/// Send critical review issues to cloud for reactive display.
fn send_to_cloud(project_path: &std::path::Path, issues: &[String], summary: Option<&str>) {
    // Try production worker first, then local
    let endpoints = [
        "https://myflow.sh/api/v1/events",     // Production worker
        "http://localhost:8787/api/v1/events", // Local dev
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

    let client = match crate::http_client::blocking_with_timeout(Duration::from_secs(2)) {
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

/// Check if gitedit is globally enabled in ~/.config/flow/config.ts.
/// Returns true by default if not specified (opt-out).
fn gitedit_globally_enabled() -> bool {
    if let Some(ts_config) = config::load_ts_config() {
        if let Some(flow) = ts_config.flow {
            if let Some(enabled) = flow.gitedit {
                return enabled;
            }
        }
    }
    // Default to false (opt-in) - gitedit not working well currently
    false
}

/// Check if gitedit mirroring is enabled in flow.toml.
fn gitedit_mirror_enabled() -> bool {
    let repo_root = git_root_or_cwd();
    let local_config = repo_root.join("flow.toml");
    if local_config.exists() {
        if let Ok(cfg) = config::load(&local_config) {
            return cfg.options.gitedit_mirror.unwrap_or(false);
        }
    }

    false
}

/// Check if gitedit mirroring is enabled for commit in the repo root.
fn gitedit_mirror_enabled_for_commit(repo_root: &std::path::Path) -> bool {
    let local_config = repo_root.join("flow.toml");
    if local_config.exists() {
        if let Ok(cfg) = config::load(&local_config) {
            return cfg.options.gitedit_mirror.unwrap_or(false);
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

fn gitedit_token(repo_root: &std::path::Path) -> Option<String> {
    for key in [
        "GITEDIT_PUBLISH_TOKEN",
        "GITEDIT_TOKEN",
        "FLOW_GITEDIT_TOKEN",
    ] {
        if let Ok(value) = std::env::var(key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    let local_config = repo_root.join("flow.toml");
    if local_config.exists() {
        if let Ok(cfg) = config::load(&local_config) {
            if let Some(token) = cfg.options.gitedit_token {
                let trimmed = token.trim().to_string();
                if !trimmed.is_empty() {
                    return Some(trimmed);
                }
            }
        }
    }
    None
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
    let ref_name = branch.as_ref().map(|name| format!("refs/heads/{}", name));

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

    let client = match crate::http_client::blocking_with_timeout(Duration::from_secs(10)) {
        Ok(c) => c,
        Err(_) => return,
    };

    let mut request = client.post(&api_url).json(&payload);
    if let Some(token) = gitedit_token(repo_root) {
        request = request.bearer_auth(token);
    }
    match request.send() {
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

// ── myflow.sh sync ──────────────────────────────────────────────────

/// Check if myflow mirroring is enabled in flow.toml.
fn myflow_mirror_enabled(repo_root: &std::path::Path) -> bool {
    let local_config = repo_root.join("flow.toml");
    if local_config.exists() {
        if let Ok(cfg) = config::load(&local_config) {
            return cfg.options.myflow_mirror.unwrap_or(false);
        }
    }
    false
}

/// Get the myflow API URL from config or use default.
fn myflow_api_url(repo_root: &std::path::Path) -> String {
    let local_config = repo_root.join("flow.toml");
    if local_config.exists() {
        if let Ok(cfg) = config::load(&local_config) {
            if let Some(url) = cfg.options.myflow_url {
                return url;
            }
        }
    }
    "https://myflow.sh".to_string()
}

/// Get the myflow token from env, flow.toml, or ~/.config/flow/auth.toml.
fn myflow_token(repo_root: &std::path::Path) -> Option<String> {
    // 1. Check env var
    if let Ok(value) = std::env::var("MYFLOW_TOKEN") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    // 2. Check flow.toml
    let local_config = repo_root.join("flow.toml");
    if local_config.exists() {
        if let Ok(cfg) = config::load(&local_config) {
            if let Some(token) = cfg.options.myflow_token {
                let trimmed = token.trim().to_string();
                if !trimmed.is_empty() {
                    return Some(trimmed);
                }
            }
        }
    }

    // 3. Fall back to ~/.config/flow/auth.toml token
    let config_dir = dirs::config_dir()?.join("flow");
    let auth_path = config_dir.join("auth.toml");
    if auth_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&auth_path) {
            if let Ok(auth) = toml::from_str::<toml::Value>(&content) {
                if let Some(token) = auth.get("token").and_then(|v| v.as_str()) {
                    let trimmed = token.trim();
                    if !trimmed.is_empty() {
                        return Some(trimmed.to_string());
                    }
                }
            }
        }
    }

    None
}

fn post_myflow_sync_events(
    client: &Client,
    events_api_url: &str,
    token: Option<&str>,
    owner: &str,
    repo: &str,
    commit_sha: &str,
    events: Vec<serde_json::Value>,
) {
    if events.is_empty() {
        return;
    }

    let payload = json!({
        "owner": owner,
        "repo": repo,
        "commit_sha": commit_sha,
        "correlation_id": commit_sha,
        "events": events,
    });

    let mut request = client.post(events_api_url).json(&payload);
    if let Some(value) = token {
        request = request.bearer_auth(value);
    }

    match request.send() {
        Ok(resp) if resp.status().is_success() => {}
        Ok(resp) => {
            debug!("myflow sync-events failed: HTTP {}", resp.status());
        }
        Err(err) => {
            debug!("myflow sync-events error: {}", err);
        }
    }
}

/// Sync commit data to myflow.sh, mirroring the gitedit sync pattern.
/// Fire-and-forget: never fails the commit on sync error.
fn sync_to_myflow(
    repo_root: &std::path::Path,
    event: &str,
    ai_sessions: &[ai::GitEditSessionData],
    session_window: Option<&MyflowSessionWindow>,
    review_data: Option<&GitEditReviewData>,
    skill_gate: Option<&SkillGateReport>,
) {
    // Get remote origin URL to extract owner/repo
    let remote_url = match git_capture_in(repo_root, &["remote", "get-url", "origin"]) {
        Ok(url) => url.trim().to_string(),
        Err(_) => {
            debug!("No git remote found, skipping myflow sync");
            return;
        }
    };

    let (owner, repo) = match parse_github_remote(&remote_url) {
        Some((o, r)) => (o, r),
        None => {
            debug!(
                "Could not parse GitHub remote URL for myflow: {}",
                remote_url
            );
            return;
        }
    };

    // Get current commit SHA
    let commit_sha = match git_capture_in(repo_root, &["rev-parse", "HEAD"]) {
        Ok(sha) => sha.trim().to_string(),
        Err(_) => {
            debug!("Could not get commit SHA for myflow");
            return;
        }
    };

    // Get current branch
    let branch = git_capture_in(repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .ok()
        .map(|b| b.trim().to_string());

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

    let base_url = myflow_api_url(repo_root);
    let base_url = base_url.trim_end_matches('/').to_string();
    let api_url = format!("{}/api/sync", base_url);
    let events_api_url = format!("{}/api/sync/events", base_url);
    let started_at_ms = chrono::Utc::now().timestamp_millis();

    // Build review data if present
    let review_json = review_data.map(|r| {
        json!({
            "issues_found": r.issues_found,
            "issues": r.issues,
            "summary": r.summary,
            "reviewer": r.reviewer,
        })
    });

    // Build features data from .ai/features/ if present
    let features_json: Vec<serde_json::Value> = features::load_all_features(repo_root)
        .unwrap_or_default()
        .iter()
        .map(|f| {
            json!({
                "name": f.name,
                "title": f.content.lines().next().unwrap_or(&f.name).trim_start_matches('#').trim(),
                "status": f.status,
                "description": f.description,
                "files": f.files,
                "tests": f.tests,
                "coverage": f.coverage,
                "last_verified_sha": f.last_verified,
            })
        })
        .collect();

    let skill_gate_json = skill_gate.map(|gate| {
        json!({
            "pass": gate.pass,
            "mode": gate.mode,
            "override": gate.override_flag,
            "required_skills": gate.required_skills,
            "missing_skills": gate.missing_skills,
            "version_failures": gate.version_failures,
            "loaded_versions": gate.loaded_versions,
        })
    });

    let payload = json!({
        "owner": owner,
        "repo": repo,
        "commit_sha": commit_sha,
        "branch": branch,
        "event": event,
        "source": "flow-cli",
        "commit_message": commit_message,
        "author_name": author_name,
        "author_email": author_email,
        "ai_sessions": ai_sessions_json,
        "session_window": session_window.map(|window| json!({
            "mode": window.mode,
            "since_ts": window.since_ts,
            "until_ts": window.until_ts,
            "collected_at": window.collected_at,
        })),
        "review": review_json,
        "features": if features_json.is_empty() { None } else { Some(features_json) },
        "skill_gate": skill_gate_json,
        "sync_events": [
            {
                "correlation_id": commit_sha,
                "commit_sha": commit_sha,
                "event_type": "transport",
                "tier": "client",
                "direction": "outbound",
                "status": "pending",
                "at_ms": started_at_ms,
                "details": {
                    "phase": "request_start",
                    "target": "api/sync",
                    "source": "flow-cli",
                },
            }
        ],
    });

    let client = match crate::http_client::blocking_with_timeout(Duration::from_secs(10)) {
        Ok(c) => c,
        Err(_) => return,
    };

    let token = myflow_token(repo_root);
    let mut request = client.post(&api_url).json(&payload);
    if let Some(value) = token.as_deref() {
        request = request.bearer_auth(value);
    }
    match request.send() {
        Ok(resp) if resp.status().is_success() => {
            let finished_at_ms = chrono::Utc::now().timestamp_millis();
            let latency_ms = std::cmp::max(0, finished_at_ms - started_at_ms);
            post_myflow_sync_events(
                &client,
                &events_api_url,
                token.as_deref(),
                &owner,
                &repo,
                &commit_sha,
                vec![
                    json!({
                        "correlation_id": commit_sha,
                        "commit_sha": commit_sha,
                        "event_type": "transport",
                        "tier": "client",
                        "direction": "outbound",
                        "status": "ok",
                        "latency_ms": latency_ms,
                        "at_ms": finished_at_ms,
                        "details": {
                            "phase": "request_complete",
                            "target": "api/sync",
                        },
                    }),
                    json!({
                        "correlation_id": commit_sha,
                        "commit_sha": commit_sha,
                        "event_type": "persistence_ack",
                        "tier": "server",
                        "direction": "inbound",
                        "status": "ok",
                        "latency_ms": latency_ms,
                        "at_ms": finished_at_ms,
                        "details": {
                            "phase": "sync_ack",
                            "target": "api/sync",
                        },
                    }),
                    json!({
                        "correlation_id": commit_sha,
                        "commit_sha": commit_sha,
                        "event_type": "query_settled",
                        "tier": "client",
                        "direction": "inbound",
                        "status": "ok",
                        "latency_ms": latency_ms,
                        "at_ms": finished_at_ms,
                        "details": {
                            "phase": "ui_visible",
                            "source": "flow-cli",
                        },
                    }),
                ],
            );

            if session_count > 0 {
                println!(
                    "✓ Synced to myflow.sh ({} AI session{})",
                    session_count,
                    if session_count == 1 { "" } else { "s" }
                );
            } else {
                println!("✓ Synced to myflow.sh");
            }
        }
        Ok(resp) => {
            let finished_at_ms = chrono::Utc::now().timestamp_millis();
            let latency_ms = std::cmp::max(0, finished_at_ms - started_at_ms);
            let status_code = resp.status().as_u16();
            post_myflow_sync_events(
                &client,
                &events_api_url,
                token.as_deref(),
                &owner,
                &repo,
                &commit_sha,
                vec![
                    json!({
                        "correlation_id": commit_sha,
                        "commit_sha": commit_sha,
                        "event_type": "transport",
                        "tier": "client",
                        "direction": "outbound",
                        "status": "error",
                        "latency_ms": latency_ms,
                        "error_code": format!("HTTP_{}", status_code),
                        "at_ms": finished_at_ms,
                        "details": {
                            "phase": "request_failed",
                            "target": "api/sync",
                            "status_code": status_code,
                        },
                    }),
                    json!({
                        "correlation_id": commit_sha,
                        "commit_sha": commit_sha,
                        "event_type": "error",
                        "tier": "client",
                        "status": "error",
                        "error_code": format!("HTTP_{}", status_code),
                        "at_ms": finished_at_ms,
                        "details": {
                            "phase": "sync_error",
                            "target": "api/sync",
                            "status_code": status_code,
                        },
                    }),
                ],
            );
            debug!("myflow sync failed: HTTP {}", resp.status());
        }
        Err(e) => {
            let finished_at_ms = chrono::Utc::now().timestamp_millis();
            let latency_ms = std::cmp::max(0, finished_at_ms - started_at_ms);
            post_myflow_sync_events(
                &client,
                &events_api_url,
                token.as_deref(),
                &owner,
                &repo,
                &commit_sha,
                vec![
                    json!({
                        "correlation_id": commit_sha,
                        "commit_sha": commit_sha,
                        "event_type": "transport",
                        "tier": "client",
                        "direction": "outbound",
                        "status": "error",
                        "latency_ms": latency_ms,
                        "error_code": "NETWORK_ERROR",
                        "at_ms": finished_at_ms,
                        "details": {
                            "phase": "request_exception",
                            "target": "api/sync",
                            "error": e.to_string(),
                        },
                    }),
                    json!({
                        "correlation_id": commit_sha,
                        "commit_sha": commit_sha,
                        "event_type": "error",
                        "tier": "client",
                        "status": "error",
                        "error_code": "NETWORK_ERROR",
                        "at_ms": finished_at_ms,
                        "details": {
                            "phase": "sync_error",
                            "target": "api/sync",
                        },
                    }),
                ],
            );
            debug!("myflow sync error: {}", e);
        }
    }
}

fn entire_enabled() -> bool {
    if let Ok(value) = env::var("FLOW_ENTIRE_DISABLE") {
        let v = value.to_ascii_lowercase();
        if v == "1" || v == "true" || v == "yes" {
            return false;
        }
    }
    let repo_root = git_root_or_cwd();
    if !repo_root.join(".entire/settings.json").exists() {
        return false;
    }
    which::which("entire").is_ok()
}

fn unhash_capture_enabled() -> bool {
    if let Ok(value) = env::var("UNHASH_DISABLE") {
        let v = value.to_ascii_lowercase();
        if v == "1" || v == "true" || v == "yes" {
            return false;
        }
    }
    if let Ok(value) = env::var("FLOW_UNHASH") {
        let v = value.to_ascii_lowercase();
        if v == "0" || v == "false" || v == "no" {
            return false;
        }
    }
    true
}

fn capture_unhash_bundle(
    repo_root: &Path,
    diff: &str,
    status: Option<&str>,
    review: Option<&ReviewResult>,
    review_model: Option<&str>,
    review_reviewer: Option<&str>,
    review_instructions: Option<&str>,
    session_context: Option<&str>,
    sessions: Option<&[ai::GitEditSessionData]>,
    gitedit_session_hash: Option<&str>,
    commit_message: &str,
    author_message: Option<&str>,
    include_context: bool,
) -> Option<String> {
    if !unhash_capture_enabled() {
        return None;
    }

    match try_capture_unhash_bundle(
        repo_root,
        diff,
        status,
        review,
        review_model,
        review_reviewer,
        review_instructions,
        session_context,
        sessions,
        gitedit_session_hash,
        commit_message,
        author_message,
        include_context,
    ) {
        Ok(hash) => hash,
        Err(err) => {
            debug!("unhash capture failed: {}", err);
            None
        }
    }
}

const UNHASH_TRACE_DEFAULT_BYTES: u64 = 64 * 1024;

fn default_assistant_trace_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = dirs::home_dir() {
        roots.push(
            home.join("repos")
                .join("garden-co")
                .join("jazz2")
                .join("assistant-traces"),
        );
        roots.push(
            home.join("code")
                .join("org")
                .join("1f")
                .join("jazz2")
                .join("assistant-traces"),
        );
    }
    roots
}

fn assistant_traces_root() -> Option<std::path::PathBuf> {
    if let Ok(value) = env::var("UNHASH_TRACE_DIR") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(std::path::PathBuf::from(trimmed));
        }
    }
    default_assistant_trace_roots()
        .into_iter()
        .find(|candidate| candidate.exists())
        .or_else(|| default_assistant_trace_roots().into_iter().next())
}

fn unhash_trace_max_bytes() -> u64 {
    if let Ok(value) = env::var("UNHASH_TRACE_MAX_BYTES") {
        if let Ok(parsed) = value.trim().parse::<u64>() {
            if parsed > 0 {
                return parsed;
            }
        }
    }
    UNHASH_TRACE_DEFAULT_BYTES
}

fn read_tail_bytes(path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
    let mut file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let len = file.metadata()?.len();
    if len > max_bytes {
        let offset = max_bytes.min(len) as i64;
        file.seek(SeekFrom::End(-offset))?;
    }
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(buf)
}

fn write_agent_trace_file(bundle_path: &Path, rel_path: &str, data: &[u8]) -> Result<()> {
    let target = bundle_path.join(rel_path);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&target, data)?;
    Ok(())
}

fn write_agent_traces(bundle_path: &Path, repo_root: &Path) {
    let mut sources = Vec::new();
    let max_bytes = unhash_trace_max_bytes();
    if max_bytes == 0 {
        return;
    }

    let trace_root = assistant_traces_root();
    if let Some(root) = trace_root {
        let trace_files = [
            "ai.jsonl",
            "linsa.jsonl",
            "gen.new.jsonl",
            "last-failure.json",
        ];
        for name in trace_files {
            let path = root.join(name);
            if !path.exists() {
                continue;
            }
            match read_tail_bytes(&path, max_bytes) {
                Ok(data) => {
                    let rel = format!("agent/traces/{}", name);
                    if let Err(err) = write_agent_trace_file(bundle_path, &rel, &data) {
                        debug!("failed to write agent trace {}: {}", rel, err);
                        continue;
                    }
                    sources.push(json!({
                        "label": name,
                        "path": path.display().to_string(),
                        "bytes": data.len(),
                    }));
                }
                Err(err) => debug!("failed to read trace {}: {}", path.display(), err),
            }
        }
    }

    if let Some(home) = dirs::home_dir() {
        let cmdlog = home.join(".cmd").join("f").join("index.jsonl");
        if cmdlog.exists() {
            match read_tail_bytes(&cmdlog, max_bytes) {
                Ok(data) => {
                    let rel = "agent/cmdlog/f.index.jsonl";
                    if let Err(err) = write_agent_trace_file(bundle_path, rel, &data) {
                        debug!("failed to write {}: {}", rel, err);
                    } else {
                        sources.push(json!({
                            "label": "cmdlog.f.index",
                            "path": cmdlog.display().to_string(),
                            "bytes": data.len(),
                        }));
                    }
                }
                Err(err) => debug!("failed to read cmdlog: {}", err),
            }
        }

        let xdg = env::var("XDG_DATA_HOME")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| home.join(".local").join("share"));
        let fish_dir = xdg.join("fish").join("io-trace");
        let fish_files = [
            ("agent/fish/last.stdout", fish_dir.join("last.stdout")),
            ("agent/fish/last.stderr", fish_dir.join("last.stderr")),
            ("agent/fish/rise.meta", fish_dir.join("rise.meta")),
            (
                "agent/fish/rise.history.jsonl",
                fish_dir.join("rise.history.jsonl"),
            ),
        ];
        for (rel, path) in fish_files {
            if !path.exists() {
                continue;
            }
            match read_tail_bytes(&path, max_bytes) {
                Ok(data) => {
                    if let Err(err) = write_agent_trace_file(bundle_path, rel, &data) {
                        debug!("failed to write {}: {}", rel, err);
                    } else {
                        sources.push(json!({
                            "label": rel,
                            "path": path.display().to_string(),
                            "bytes": data.len(),
                        }));
                    }
                }
                Err(err) => debug!("failed to read {}: {}", path.display(), err),
            }
        }
    }

    if !sources.is_empty() {
        let index = json!({
            "captured_at": chrono::Utc::now().to_rfc3339(),
            "repo_root": repo_root.to_string_lossy().to_string(),
            "sources": sources,
        });
        if let Ok(encoded) = serde_json::to_vec_pretty(&index) {
            let _ = write_agent_trace_file(bundle_path, "agent/trace_index.json", &encoded);
        }
    }
}

fn write_agent_learning(
    bundle_path: &Path,
    repo_root: &Path,
    diff: &str,
    _status: &str,
    review: Option<&ReviewResult>,
    commit_message: &str,
    sessions_count: usize,
) {
    let changed_files = changed_files_from_diff(diff);
    let summary = review
        .and_then(|r| r.summary.clone())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| commit_message.to_string());
    let issues = review.map(|r| r.issues.clone()).unwrap_or_default();
    let future_tasks = review.map(|r| r.future_tasks.clone()).unwrap_or_default();

    let root_cause = if !summary.trim().is_empty() {
        summary.clone()
    } else if !issues.is_empty() {
        issues.join("; ")
    } else {
        "unknown (see diff)".to_string()
    };
    let prevention = if !future_tasks.is_empty() {
        future_tasks.join("; ")
    } else {
        "Add a regression test or guard for the affected behavior.".to_string()
    };

    let mut tag_texts = Vec::new();
    if !summary.is_empty() {
        tag_texts.push(summary.clone());
    }
    tag_texts.extend(issues.iter().cloned());
    let tags = classify_learning_tags(&tag_texts);

    let learn_json = json!({
        "commit": commit_message,
        "repo": repo_root.file_name().and_then(|n| n.to_str()).unwrap_or("repo"),
        "repo_root": repo_root.to_string_lossy().to_string(),
        "issue": issues.first().cloned().unwrap_or_else(|| "none".to_string()),
        "root_cause": root_cause,
        "fix": commit_message,
        "prevention": prevention,
        "affected_files": changed_files,
        "tests": [],
        "tags": tags,
        "ai_sessions": sessions_count,
        "review_issues": issues,
        "review_future_tasks": future_tasks,
        "created_at": chrono::Utc::now().to_rfc3339(),
    });

    let decision_md = render_learning_decision_md(&learn_json);
    let regression_md = render_learning_regression_md(&learn_json);
    let patch_summary_md = render_learning_patch_summary_md(&learn_json);

    if let Ok(encoded) = serde_json::to_vec_pretty(&learn_json) {
        let _ = write_agent_trace_file(bundle_path, "agent/learn.json", &encoded);
    }
    let _ = write_agent_trace_file(bundle_path, "agent/decision.md", decision_md.as_bytes());
    let _ = write_agent_trace_file(bundle_path, "agent/regression.md", regression_md.as_bytes());
    let _ = write_agent_trace_file(
        bundle_path,
        "agent/patch_summary.md",
        patch_summary_md.as_bytes(),
    );

    let _ = append_learning_store(
        repo_root,
        &learn_json,
        &decision_md,
        &regression_md,
        &patch_summary_md,
    );
}

fn classify_learning_tags(texts: &[String]) -> Vec<String> {
    let mut tags = HashSet::new();
    for text in texts {
        let lowered = text.to_lowercase();
        if lowered.contains("perf")
            || lowered.contains("performance")
            || lowered.contains("latency")
        {
            tags.insert("perf".to_string());
        }
        if lowered.contains("security") || lowered.contains("vulnerability") {
            tags.insert("security".to_string());
        }
        if lowered.contains("panic")
            || lowered.contains("crash")
            || lowered.contains("error")
            || lowered.contains("bug")
        {
            tags.insert("bug".to_string());
        }
        if lowered.contains("prompt") || lowered.contains("instruction") {
            tags.insert("prompt".to_string());
        }
        if lowered.contains("test") || lowered.contains("regression") {
            tags.insert("test".to_string());
        }
    }
    let mut out: Vec<String> = tags.into_iter().collect();
    out.sort();
    out
}

fn render_learning_decision_md(learn: &serde_json::Value) -> String {
    let summary = learn
        .get("root_cause")
        .and_then(|v| v.as_str())
        .unwrap_or("n/a");
    let fix = learn.get("fix").and_then(|v| v.as_str()).unwrap_or("n/a");
    let prevention = learn
        .get("prevention")
        .and_then(|v| v.as_str())
        .unwrap_or("n/a");
    format!(
        "# Decision\n\n## Summary\n{}\n\n## Fix\n{}\n\n## Prevention\n{}\n",
        summary, fix, prevention
    )
}

fn render_learning_regression_md(learn: &serde_json::Value) -> String {
    let issue = learn
        .get("issue")
        .and_then(|v| v.as_str())
        .unwrap_or("none");
    let prevention = learn
        .get("prevention")
        .and_then(|v| v.as_str())
        .unwrap_or("n/a");
    format!(
        "# Regression Guard\n\n- If you see: {}\n- Do: {}\n",
        issue, prevention
    )
}

fn render_learning_patch_summary_md(learn: &serde_json::Value) -> String {
    let commit = learn.get("fix").and_then(|v| v.as_str()).unwrap_or("n/a");
    let files = learn
        .get("affected_files")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out = String::from("# Patch Summary\n\n");
    out.push_str(&format!("- Commit: {}\n", commit));
    out.push_str("- Files:\n");
    if files.is_empty() {
        out.push_str("  - (none)\n");
    } else {
        for file in files {
            if let Some(name) = file.as_str() {
                out.push_str(&format!("  - {}\n", name));
            }
        }
    }
    out
}

fn append_learning_store(
    repo_root: &Path,
    learn_json: &serde_json::Value,
    decision_md: &str,
    regression_md: &str,
    patch_summary_md: &str,
) -> Result<()> {
    let learn_dir = learning_store_root(repo_root)?;
    fs::create_dir_all(&learn_dir)?;

    let learn_jsonl = learn_dir.join("learn.jsonl");
    let line = serde_json::to_string(learn_json)? + "\n";
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&learn_jsonl)?
        .write_all(line.as_bytes())?;

    let learn_md = learn_dir.join("learn.md");
    let mut md = String::new();
    md.push_str("\n---\n\n");
    md.push_str(decision_md);
    md.push('\n');
    md.push_str(regression_md);
    md.push('\n');
    md.push_str(patch_summary_md);
    md.push('\n');

    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&learn_md)?
        .write_all(md.as_bytes())?;

    let _ = append_jazz_learning(&line);

    Ok(())
}

fn learning_store_root(repo_root: &Path) -> Result<PathBuf> {
    if let Ok(value) = env::var("FLOW_LEARN_DIR") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Ok(PathBuf::from(trimmed));
        }
    }
    if let Ok(value) = env::var("FLOW_BASE_DIR") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Ok(PathBuf::from(trimmed)
                .join(".ai")
                .join("internal")
                .join("learn"));
        }
    }

    if let Some(home) = dirs::home_dir() {
        return Ok(home
            .join("code")
            .join("org")
            .join("linsa")
            .join("base")
            .join(".ai")
            .join("internal")
            .join("learn"));
    }

    Ok(repo_root.join(".ai").join("internal").join("learn"))
}

fn append_jazz_learning(line: &str) -> Result<()> {
    let Some(root) = jazz_assistant_traces_root() else {
        return Ok(());
    };
    fs::create_dir_all(&root)?;
    let path = root.join("base.learn.jsonl");
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?
        .write_all(line.as_bytes())?;
    Ok(())
}

fn jazz_assistant_traces_root() -> Option<PathBuf> {
    if let Ok(value) = env::var("FLOW_JAZZ_TRACE_DIR") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    default_assistant_trace_roots()
        .into_iter()
        .find(|candidate| candidate.exists())
        .or_else(|| default_assistant_trace_roots().into_iter().next())
}

fn try_capture_unhash_bundle(
    repo_root: &Path,
    diff: &str,
    status: Option<&str>,
    review: Option<&ReviewResult>,
    review_model: Option<&str>,
    review_reviewer: Option<&str>,
    review_instructions: Option<&str>,
    session_context: Option<&str>,
    sessions: Option<&[ai::GitEditSessionData]>,
    gitedit_session_hash: Option<&str>,
    commit_message: &str,
    author_message: Option<&str>,
    include_context: bool,
) -> Result<Option<String>> {
    let unhash_bin = match which::which("unhash") {
        Ok(path) => path,
        Err(_) => {
            debug!("unhash not found on PATH; skipping commit bundle");
            return Ok(None);
        }
    };

    let mut injected_key: Option<String> = None;
    if env::var("UNHASH_KEY").is_err() {
        if let Ok(Some(value)) = flow_env::get_personal_env_var("UNHASH_KEY") {
            injected_key = Some(value);
        } else {
            debug!("UNHASH_KEY not set; skipping commit bundle");
            return Ok(None);
        }
    }

    let unhash_dir = repo_root.join(".ai/internal/unhash");
    fs::create_dir_all(&unhash_dir)
        .with_context(|| format!("create unhash dir {}", unhash_dir.display()))?;

    let bundle_dir: TempDir = TempBuilder::new()
        .prefix("commit-")
        .tempdir_in(&unhash_dir)
        .context("create unhash temp dir")?;

    let bundle_path = bundle_dir.path();
    fs::write(bundle_path.join("diff.patch"), diff).context("write diff.patch")?;

    let status_value = status
        .map(|s| s.to_string())
        .unwrap_or_else(|| git_capture_in(repo_root, &["status", "--short"]).unwrap_or_default());
    fs::write(bundle_path.join("status.txt"), &status_value).context("write status.txt")?;

    if let Some(context) = session_context {
        fs::write(bundle_path.join("context.txt"), context).context("write context.txt")?;
    }

    let sessions_data: Vec<ai::GitEditSessionData> = match sessions {
        Some(items) => items.to_vec(),
        None => ai::get_sessions_for_gitedit(&repo_root.to_path_buf()).unwrap_or_default(),
    };
    if !sessions_data.is_empty() {
        let json =
            serde_json::to_string_pretty(&sessions_data).context("serialize sessions.json")?;
        fs::write(bundle_path.join("sessions.json"), json).context("write sessions.json")?;
    }

    write_agent_traces(bundle_path, repo_root);
    write_agent_learning(
        bundle_path,
        repo_root,
        diff,
        &status_value,
        review,
        commit_message,
        sessions_data.len(),
    );

    if let Some(review) = review {
        let review_payload = UnhashReviewPayload {
            issues_found: review.issues_found,
            issues: review.issues.clone(),
            summary: review.summary.clone(),
            future_tasks: review.future_tasks.clone(),
            timed_out: review.timed_out,
            model: review_model.map(|s| s.to_string()),
            reviewer: review_reviewer.map(|s| s.to_string()),
        };
        let json =
            serde_json::to_string_pretty(&review_payload).context("serialize review.json")?;
        fs::write(bundle_path.join("review.json"), json).context("write review.json")?;
    }

    let branch = git_capture_in(repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .unwrap_or_else(|_| "unknown".to_string());
    let repo_label = match get_gitedit_project(repo_root) {
        Some((owner, repo)) => format!("{}/{}", owner, repo),
        None => repo_root
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "local-repo".to_string()),
    };

    let metadata = UnhashCommitMetadata {
        repo: repo_label,
        repo_root: repo_root.to_string_lossy().to_string(),
        branch: branch.trim().to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        commit_message: commit_message.to_string(),
        author_message: author_message.map(|s| s.to_string()),
        include_context,
        context_chars: session_context.map(|c| c.len()),
        review_model: review_model.map(|s| s.to_string()),
        review_instructions: review_instructions.map(|s| s.to_string()),
        review_issues: review.map(|r| r.issues.clone()).unwrap_or_default(),
        review_summary: review.and_then(|r| r.summary.clone()),
        review_future_tasks: review.map(|r| r.future_tasks.clone()).unwrap_or_default(),
        review_timed_out: review.map(|r| r.timed_out).unwrap_or(false),
        gitedit_session_hash: gitedit_session_hash.map(|s| s.to_string()),
        session_count: sessions_data.len(),
    };
    let meta_json = serde_json::to_string_pretty(&metadata).context("serialize commit.json")?;
    fs::write(bundle_path.join("commit.json"), meta_json).context("write commit.json")?;

    let out_file = TempBuilder::new()
        .prefix("bundle-")
        .suffix(".uhx")
        .tempfile_in(&unhash_dir)
        .context("create temp bundle file")?;
    let out_path = out_file.path().to_path_buf();
    drop(out_file);

    let mut cmd = Command::new(unhash_bin);
    cmd.arg(bundle_path).arg("--out").arg(&out_path);
    cmd.current_dir(repo_root);
    if let Some(value) = injected_key {
        cmd.env("UNHASH_KEY", value);
    }

    let output = cmd.output().context("run unhash")?;
    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        debug!("unhash failed: {} {}{}", output.status, stdout, stderr);
        return Ok(None);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut hash = String::new();
    for line in stdout.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            hash = trimmed.to_string();
            break;
        }
    }
    if hash.is_empty() {
        debug!("unhash output missing hash");
        return Ok(None);
    }

    let final_path = unhash_dir.join(format!("{}.uhx", hash));
    if final_path != out_path {
        if let Err(err) = fs::rename(&out_path, &final_path) {
            debug!("failed to move unhash bundle: {}", err);
        }
    }

    Ok(Some(hash))
}

fn stage_changes_for_commit(workdir: &Path, stage_paths: &[String]) -> Result<()> {
    print!("Staging changes... ");
    io::stdout().flush()?;

    if stage_paths.is_empty() {
        git_run_in(workdir, &["add", "."])?;
        println!("done");
        return Ok(());
    }

    git_run_in(workdir, &["reset", "--quiet"])?;

    let mut cmd = Command::new("git");
    let status = cmd
        .current_dir(workdir)
        .arg("add")
        .arg("--")
        .args(stage_paths)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to run git add for selected paths")?;

    if !status.success() {
        bail!("git add -- <paths> failed with status {}", status);
    }

    println!(
        "done ({} path{})",
        stage_paths.len(),
        if stage_paths.len() == 1 { "" } else { "s" }
    );

    Ok(())
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

fn stage_paths_cli_flags(stage_paths: &[String]) -> String {
    let mut flags = String::new();
    for path in stage_paths {
        flags.push_str(&format!(" --path {:?}", path));
    }
    flags
}

fn delegate_to_hub(
    push: bool,
    queue: CommitQueueMode,
    include_unhash: bool,
    stage_paths: &[String],
) -> Result<()> {
    let repo_root = git_root_or_cwd();
    warn_if_commit_invoked_from_subdir(&repo_root);

    // Build the command to run using the current executable path
    let push_flag = if push { "" } else { " --no-push" };
    let queue_flag = queue_flag_for_command(queue);
    let review_flag = review_flag_for_command(queue);
    let hashed_flag = if include_unhash { " --hashed" } else { "" };
    let path_flags = stage_paths_cli_flags(stage_paths);
    let flow_bin = std::env::current_exe()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "flow".to_string());
    let command = format!(
        "{} commit --sync{}{}{}{}{}",
        flow_bin, push_flag, queue_flag, review_flag, hashed_flag, path_flags
    );

    let url = format!("http://{}:{}/tasks/run", HUB_HOST, HUB_PORT);
    let client = crate::http_client::blocking_with_timeout(Duration::from_secs(5))
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
        "cwd": repo_root.to_string_lossy(),
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
    queue: CommitQueueMode,
    include_unhash: bool,
    stage_paths: &[String],
    gate_overrides: CommitGateOverrides,
) -> Result<()> {
    let repo_root = resolve_commit_with_check_root()?;
    warn_if_commit_invoked_from_subdir(&repo_root);

    // Generate early gitedit hash from session IDs + owner/repo
    let early_gitedit_url = generate_early_gitedit_url(&repo_root);

    // Build the command to run using the current executable path
    let push_flag = if push { "" } else { " --no-push" };
    let queue_flag = queue_flag_for_command(queue);
    let review_flag = review_flag_for_command(queue);
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
    let hashed_flag = if include_unhash { " --hashed" } else { "" };
    let skip_quality_flag = if gate_overrides.skip_quality {
        " --skip-quality"
    } else {
        ""
    };
    let skip_docs_flag = if gate_overrides.skip_docs {
        " --skip-docs"
    } else {
        ""
    };
    let skip_tests_flag = if gate_overrides.skip_tests {
        " --skip-tests"
    } else {
        ""
    };
    let path_flags = stage_paths_cli_flags(stage_paths);
    let flow_bin = std::env::current_exe()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "flow".to_string());
    let command = format!(
        "{} {} --sync{}{}{}{}{}{}{}{}{}{}{}{} --tokens {}",
        flow_bin,
        command_name,
        push_flag,
        context_flag,
        codex_flag,
        review_model_flag,
        message_flag,
        queue_flag,
        review_flag,
        hashed_flag,
        skip_quality_flag,
        skip_docs_flag,
        skip_tests_flag,
        path_flags,
        max_tokens
    );

    let url = format!("http://{}:{}/tasks/run", HUB_HOST, HUB_PORT);
    let client = crate::http_client::blocking_with_timeout(Duration::from_secs(5))
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
        "cwd": repo_root.to_string_lossy(),
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
    // Check if gitedit is globally enabled
    if !gitedit_globally_enabled() {
        return None;
    }

    // Get owner/repo
    let (owner, repo) = get_gitedit_project(repo_root)?;

    // Get session IDs and checkpoint for hashing
    let (session_ids, checkpoint_ts) =
        ai::get_session_ids_for_hash(&repo_root.to_path_buf()).ok()?;

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
        "lowercase-filenames" => fix_lowercase_filenames(repo_root),
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

/// Rename staged files with uppercase basenames to lowercase.
fn fix_lowercase_filenames(repo_root: &Path) -> Result<bool> {
    // Get staged new/renamed files
    let output = Command::new("git")
        .args(["diff", "--cached", "--name-only", "--diff-filter=ACR"])
        .current_dir(repo_root)
        .output()?;

    let files: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect();

    let mut fixed_any = false;

    for file in &files {
        let path = Path::new(file);
        let basename = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };

        if !basename.chars().any(|c| c.is_ascii_uppercase()) {
            continue;
        }

        let lower = basename.to_ascii_lowercase();
        let new_path = match path.parent() {
            Some(p) if p != Path::new("") => p.join(&lower),
            _ => PathBuf::from(&lower),
        };

        let status = Command::new("git")
            .args(["mv", file, new_path.to_str().unwrap_or(&lower)])
            .current_dir(repo_root)
            .output()?;

        if status.status.success() {
            println!("  Renamed: {} → {}", file, new_path.display());
            fixed_any = true;
        }
    }

    if fixed_any {
        println!("✓ Fixed uppercase filenames");
    }

    Ok(fixed_any)
}

/// Simple binary file detection.
fn is_binary(path: &Path) -> bool {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    matches!(
        ext,
        "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "ico"
            | "webp"
            | "svg"
            | "woff"
            | "woff2"
            | "ttf"
            | "otf"
            | "eot"
            | "zip"
            | "tar"
            | "gz"
            | "rar"
            | "7z"
            | "pdf"
            | "doc"
            | "docx"
            | "xls"
            | "xlsx"
            | "exe"
            | "dll"
            | "so"
            | "dylib"
            | "mp3"
            | "mp4"
            | "wav"
            | "avi"
            | "mov"
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ai_scratch_tests_are_excluded_from_related_tests() {
        let repo_root = Path::new(".");
        let changed = vec![
            ".ai/test/generated/auth-flow.test.ts".to_string(),
            "mobile/src/pages/chats/home/ui/ChatsList.test.tsx".to_string(),
        ];

        let related = find_related_tests(repo_root, &changed, ".ai/test");
        assert_eq!(
            related,
            vec!["mobile/src/pages/chats/home/ui/ChatsList.test.tsx".to_string()]
        );
    }

    #[test]
    fn path_within_dir_handles_relative_prefixes() {
        assert!(path_is_within_dir("./.ai/test/foo.test.ts", ".ai/test"));
        assert!(path_is_within_dir(".ai/test", ".ai/test"));
        assert!(!path_is_within_dir("mobile/src/foo.test.ts", ".ai/test"));
    }

    #[test]
    fn commit_message_selection_parsing_supports_fallback_specs() {
        assert!(matches!(
            parse_commit_message_selection_spec("remote"),
            Some(CommitMessageSelection::Remote)
        ));
        assert!(matches!(
            parse_commit_message_selection_spec("openai"),
            Some(CommitMessageSelection::OpenAi)
        ));
        assert!(matches!(
            parse_commit_message_selection_spec("heuristic"),
            Some(CommitMessageSelection::Heuristic)
        ));

        match parse_commit_message_selection_spec("openrouter:moonshotai/kimi-k2") {
            Some(CommitMessageSelection::OpenRouter { model }) => {
                assert_eq!(model, "moonshotai/kimi-k2")
            }
            _ => panic!("expected openrouter message selection"),
        }

        match parse_commit_message_selection_with_model(
            "rise",
            Some("zai:glm-4.7-thinking".to_string()),
        ) {
            Some(CommitMessageSelection::Rise { model }) => {
                assert_eq!(model, "zai:glm-4.7-thinking")
            }
            _ => panic!("expected rise message selection"),
        }
    }

    #[test]
    fn deterministic_commit_message_includes_changed_files() {
        let diff = format!(
            "{} b/src/lib.rs\n+added\n{} b/src/main.rs\n+added",
            "+++", "+++"
        );
        let message = build_deterministic_commit_message(&diff);
        assert!(message.starts_with("Update 2 files"));
        assert!(message.contains("- src/lib.rs"));
        assert!(message.contains("- src/main.rs"));
    }

    #[test]
    fn glm5_alias_maps_to_rise_selection() {
        match parse_review_selection_spec("glm5") {
            Some(ReviewSelection::Rise { model }) => assert_eq!(model, DEFAULT_GLM5_RISE_MODEL),
            _ => panic!("expected glm5 to map to rise review selection"),
        }

        match parse_commit_message_selection_spec("glm5") {
            Some(CommitMessageSelection::Rise { model }) => {
                assert_eq!(model, DEFAULT_GLM5_RISE_MODEL)
            }
            _ => panic!("expected glm5 to map to rise commit message selection"),
        }
    }

    #[test]
    fn normalize_markdown_linebreaks_decodes_literal_newlines() {
        let input = "## Summary\\n- one\\n- two\\n\\n## Why\\n- because";
        let out = normalize_markdown_linebreaks(input);
        assert!(out.contains("## Summary\n- one\n- two"));
        assert!(out.contains("\n\n## Why\n- because"));
    }

    #[test]
    fn normalize_markdown_linebreaks_preserves_existing_multiline_text() {
        let input = "## Summary\n- already\n- multiline";
        let out = normalize_markdown_linebreaks(input);
        assert_eq!(out, input);
    }

    #[test]
    fn invariants_dep_check_flags_unapproved_dependencies() {
        let package_json = r#"{
          "dependencies": { "react": "^18.0.0", "@reatom/core": "^3.0.0" },
          "devDependencies": { "vitest": "^1.0.0" }
        }"#;
        let approved = vec!["@reatom/core".to_string(), "vitest".to_string()];
        let mut findings = Vec::new();

        check_unapproved_deps(package_json, &approved, "package.json", &mut findings);

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].category, "deps");
        assert!(findings[0].message.contains("react"));
        assert_eq!(findings[0].file.as_deref(), Some("package.json"));
    }

    #[test]
    fn invariant_prompt_context_includes_rules_and_findings() {
        let mut terminology = HashMap::new();
        terminology.insert("Flow".to_string(), "CLI tool".to_string());
        let inv = config::InvariantsConfig {
            architecture_style: Some("event-driven".to_string()),
            non_negotiable: vec!["no inline imports".to_string()],
            terminology,
            ..Default::default()
        };
        let report = InvariantGateReport {
            findings: vec![InvariantFinding {
                severity: "warning".to_string(),
                category: "forbidden".to_string(),
                message: "Forbidden pattern 'useState(' in added line".to_string(),
                file: Some("web/app.tsx".to_string()),
            }],
        };

        let ctx = report.to_prompt_context(&inv);
        assert!(ctx.contains("Project Invariants"));
        assert!(ctx.contains("Architecture: event-driven"));
        assert!(ctx.contains("no inline imports"));
        assert!(ctx.contains("Flow: CLI tool"));
        assert!(ctx.contains("web/app.tsx"));
        assert!(ctx.contains("Forbidden pattern"));
    }
}
