//! AI-powered git commit command using OpenAI.

use std::collections::{HashSet, hash_map::DefaultHasher};
use std::env;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{self, IsTerminal, Read, Seek, SeekFrom, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use sha1::{Digest, Sha1};
use reqwest::StatusCode;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tempfile::{Builder as TempBuilder, NamedTempFile, TempDir};
use tracing::{debug, info};
use regex::Regex;

use crate::ai;
use crate::cli::{CommitQueueAction, CommitQueueCommand, DaemonAction};
use crate::config;
use crate::daemon;
use crate::git_guard;
use crate::hub;
use crate::notify;
use crate::setup;
use crate::supervisor;
use crate::todo;
use crate::undo;
use crate::vcs;
use crate::env as flow_env;

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
    fn is_claude(&self) -> bool {
        matches!(self, ReviewSelection::Claude(_))
    }

    fn is_codex(&self) -> bool {
        matches!(self, ReviewSelection::Codex(_))
    }

    fn is_opencode(&self) -> bool {
        matches!(self, ReviewSelection::Opencode { .. })
    }

    fn is_rise(&self) -> bool {
        matches!(self, ReviewSelection::Rise { .. })
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

/// Common secret patterns to detect in diff content.
/// Each tuple is (pattern_name, regex_pattern).
const SECRET_PATTERNS: &[(&str, &str)] = &[
    // API Keys with known prefixes
    ("AWS Access Key", r"AKIA[0-9A-Z]{16}"),
    ("AWS Secret Key", r#"(?i)aws.{0,20}secret.{0,20}['"][0-9a-zA-Z/+]{40}['"]"#),
    ("GitHub Token", r"ghp_[0-9a-zA-Z]{36}"),
    ("GitHub OAuth", r"gho_[0-9a-zA-Z]{36}"),
    ("GitHub App Token", r"ghu_[0-9a-zA-Z]{36}"),
    ("GitHub Refresh Token", r"ghr_[0-9a-zA-Z]{36}"),
    ("GitLab Token", r"glpat-[0-9a-zA-Z\-_]{20,}"),
    ("Slack Token", r"xox[baprs]-[0-9a-zA-Z]{10,48}"),
    ("Slack Webhook", r"https://hooks\.slack\.com/services/T[0-9A-Z]{8,}/B[0-9A-Z]{8,}/[0-9a-zA-Z]{24}"),
    ("Discord Webhook", r"https://discord(?:app)?\.com/api/webhooks/[0-9]{17,}/[0-9a-zA-Z_-]{60,}"),
    ("Stripe Key", r"sk_live_[0-9a-zA-Z]{24,}"),
    ("Stripe Restricted", r"rk_live_[0-9a-zA-Z]{24,}"),
    // OpenAI keys - multiple formats (legacy, project, service account)
    ("OpenAI Key (Legacy)", r"sk-[a-zA-Z0-9]{32,}"),
    ("OpenAI Key (Project)", r"sk-proj-[a-zA-Z0-9\-_]{20,}"),
    ("OpenAI Key (Service)", r"sk-svcacct-[a-zA-Z0-9\-_]{20,}"),
    ("Anthropic Key", r"sk-ant-[0-9a-zA-Z\-_]{90,}"),
    ("Google API Key", r"AIza[0-9A-Za-z\-_]{35}"),
    ("Groq API Key", r"gsk_[0-9a-zA-Z]{50,}"),
    ("Mistral API Key", r#"(?i)mistral.{0,10}(api[_-]?key|key).{0,5}[=:].{0,5}["'][0-9a-zA-Z]{32,}["']"#),
    ("Cohere API Key", r#"(?i)cohere.{0,10}(api[_-]?key|key).{0,5}[=:].{0,5}["'][0-9a-zA-Z]{40,}["']"#),
    ("Heroku API Key", r"(?i)heroku.{0,20}[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}"),
    ("NPM Token", r"npm_[0-9a-zA-Z]{36}"),
    ("PyPI Token", r"pypi-[0-9a-zA-Z_-]{50,}"),
    ("Telegram Bot Token", r"[0-9]{8,10}:[0-9A-Za-z_-]{35}"),
    ("Twilio Key", r"SK[0-9a-fA-F]{32}"),
    ("SendGrid Key", r"SG\.[0-9a-zA-Z_-]{22}\.[0-9a-zA-Z_-]{43}"),
    ("Mailgun Key", r"key-[0-9a-zA-Z]{32}"),
    ("Private Key", r"-----BEGIN (RSA |EC |DSA |OPENSSH )?PRIVATE KEY-----"),
    ("Supabase Key", r"eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9\.[0-9a-zA-Z_-]{50,}"),
    ("Firebase Key", r#"(?i)firebase.{0,20}["'][A-Za-z0-9_-]{30,}["']"#),
    // Generic patterns (higher false positive risk, but catch common mistakes)
    ("Generic API Key Assignment", r#"(?i)(api[_-]?key|apikey)\s*[:=]\s*['"][0-9a-zA-Z\-_]{20,}['"]"#),
    ("Generic Secret Assignment", r#"(?i)(secret|password|passwd|pwd)\s*[:=]\s*['"][^'"]{8,}['"]"#),
    ("Bearer Token", r"(?i)bearer\s+[0-9a-zA-Z\-_.]{20,}"),
    ("Basic Auth", r"(?i)basic\s+[A-Za-z0-9+/=]{20,}"),
    // High-entropy strings that look like secrets (env var assignments)
    ("Env Var Secret", r#"(?i)(KEY|TOKEN|SECRET|PASSWORD|CREDENTIAL|AUTH)[_A-Z]*\s*=\s*['"]?[0-9a-zA-Z\-_/+=]{32,}['"]?"#),
];

const SECRET_SCAN_IGNORE_MARKERS: &[&str] = &[
    "flow:secret:ignore",
    "flow-secret-ignore",
    "flow:secret-scan:ignore",
    "gitleaks:allow",
];

fn should_ignore_secret_scan_line(content: &str) -> bool {
    let lower = content.to_lowercase();
    SECRET_SCAN_IGNORE_MARKERS
        .iter()
        .any(|m| lower.contains(&m.to_lowercase()))
}

fn extract_first_quoted_value(s: &str) -> Option<&str> {
    let (qpos, qch) = s
        .char_indices()
        .find(|(_, c)| *c == '"' || *c == '\'')?;
    let end = s.rfind(qch)?;
    if end <= qpos {
        return None;
    }
    Some(&s[qpos + 1..end])
}

fn looks_like_identifier_reference(value: &str) -> bool {
    let v = value.trim();
    // Common false positive: secret *names* (env var identifiers), not secret *values*.
    // Require underscore to avoid skipping high-entropy base32-ish strings.
    !v.is_empty()
        && v.len() >= 8
        && v.contains('_')
        && v.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_' || c == '.')
}

fn looks_like_secret_lookup(value: &str) -> bool {
    let v = value.trim();

    if v.starts_with("${") && v.ends_with('}') {
        // ${VAR} is dynamic (not hardcoded). Defaults like ${VAR:-literal} are not treated as safe.
        let inner = &v[2..v.len() - 1];
        return !inner.contains(":-")
            && !inner.contains("-")
            && inner
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    }

    if !(v.starts_with("$(") && v.ends_with(')')) {
        return false;
    }
    let inner = v[2..v.len() - 1].trim();
    // If the substitution contains quotes, assume it might embed a hardcoded secret.
    if inner.contains('"') || inner.contains('\'') || inner.contains('`') {
        return false;
    }
    let inner_lc = inner.to_lowercase();
    // Allowlist common secret lookups (dynamic, not hardcoded).
    inner_lc.starts_with("get_env ")
        || inner_lc.starts_with("getenv ")
        || inner_lc.starts_with("printenv ")
        || inner_lc.starts_with("op read ")
        || inner_lc.starts_with("pass show ")
        || inner_lc.starts_with("security find-generic-password")
        || inner_lc.starts_with("aws ssm get-parameter")
        || inner_lc.starts_with("vault kv get")
        || inner_lc.starts_with("bw get")
        || inner_lc.starts_with("gcloud secrets versions access")
}

fn generic_secret_assignment_is_false_positive(content: &str, matched: &str) -> bool {
    // Only apply these heuristics to the broad "Generic Secret Assignment" rule.
    // If the value is an identifier or a dynamic lookup, it's not a hardcoded secret.
    if let Some((_, rhs)) = matched.split_once('=') {
        let rhs = rhs.trim_start();
        // e.g. SECRET="$(printf "%s" "$output" | ...)" is dynamic; the literal match happens because
        // the regex stops at the first inner quote. This should not block commits.
        if rhs.starts_with("\"$(") || rhs.starts_with("'$(") || rhs.starts_with("`") {
            return true;
        }
        // Dynamic references like "$VAR" or "${VAR}" are not hardcoded secrets.
        if rhs.starts_with("\"$") || rhs.starts_with("'$") {
            return true;
        }
    } else if let Some((_, rhs)) = matched.split_once(':') {
        let rhs = rhs.trim_start();
        if rhs.starts_with("\"$(") || rhs.starts_with("'$(") || rhs.starts_with("`") {
            return true;
        }
        if rhs.starts_with("\"$") || rhs.starts_with("'$") {
            return true;
        }
    }

    if let Some(val) = extract_first_quoted_value(matched) {
        let v = val.trim();
        if looks_like_identifier_reference(v) {
            return true;
        }
        if looks_like_secret_lookup(v) {
            return true;
        }
    }

    // If the whole line is clearly a dynamic lookup, treat as non-hardcoded.
    // This catches cases where the regex match boundaries don't capture the full value cleanly.
        let lc = content.to_lowercase();
        lc.contains("$(get_env ")
}

/// Scan staged diff content for hardcoded secrets.
/// Returns list of (file, line_num, pattern_name, matched_text) for detected secrets.
fn scan_diff_for_secrets(repo_root: &Path) -> Vec<(String, usize, String, String)> {
    let output = Command::new("git")
        .args(["diff", "--cached", "-U0"])
        .current_dir(repo_root)
        .output();

    let Ok(output) = output else {
        return Vec::new();
    };

    if !output.status.success() {
        return Vec::new();
    }

    let diff = String::from_utf8_lossy(&output.stdout);
    let mut findings: Vec<(String, usize, String, String)> = Vec::new();
    let mut current_file = String::new();
    let mut current_line: usize = 0;
    let mut ignore_next_added_line = false;

    // Compile regexes
    let patterns: Vec<(&str, regex::Regex)> = SECRET_PATTERNS
        .iter()
        .filter_map(|(name, pattern)| {
            regex::Regex::new(pattern).ok().map(|re| (*name, re))
        })
        .collect();

    for line in diff.lines() {
        // Track current file
        if line.starts_with("+++ b/") {
            current_file = line.strip_prefix("+++ b/").unwrap_or("").to_string();
            ignore_next_added_line = false;
            continue;
        }

        // Track line numbers from hunk headers: @@ -old,count +new,count @@
        if line.starts_with("@@") {
            if let Some(plus_pos) = line.find('+') {
                let after_plus = &line[plus_pos + 1..];
                let num_str: String = after_plus.chars().take_while(|c| c.is_ascii_digit()).collect();
                current_line = num_str.parse().unwrap_or(0);
            }
            ignore_next_added_line = false;
            continue;
        }

        // Only scan added lines (start with +, but not +++)
        if line.starts_with('+') && !line.starts_with("+++") {
            let content = &line[1..]; // Remove leading +

            if ignore_next_added_line {
                ignore_next_added_line = false;
                current_line += 1;
                continue;
            }
            // If a comment line contains the ignore marker, treat it as applying to the next line.
            // This matches common tooling conventions and makes auto-fix more reliable.
            let trimmed = content.trim_start();
            if trimmed.starts_with('#') && should_ignore_secret_scan_line(trimmed) {
                ignore_next_added_line = true;
                current_line += 1;
                continue;
            }
            if should_ignore_secret_scan_line(content) {
                // One-line escape hatch. Prefer `# flow:secret:ignore` inline on the line being flagged.
                current_line += 1;
                continue;
            }
            if content.to_lowercase().contains("flow:secret:ignore-next") {
                ignore_next_added_line = true;
                current_line += 1;
                continue;
            }

            for (name, re) in &patterns {
                if let Some(m) = re.find(content) {
                    let matched = m.as_str();
                    let matched_lower = matched.to_lowercase();

                    // Skip only if the matched secret VALUE itself looks like a placeholder
                    // Don't skip based on surrounding context - real secrets can be on lines with comments
                    if matched_lower.contains("xxx")
                        || matched_lower.contains("your")
                        || matched_lower.contains("example")
                        || matched_lower.contains("placeholder")
                        || matched_lower.contains("replace")
                        || matched_lower.contains("insert")
                        || matched_lower.contains("todo")
                        || matched_lower.contains("fixme")
                        || matched == "sk-..."
                        || matched == "sk-xxxx"
                        || matched.chars().all(|c| c == 'x' || c == 'X' || c == '.' || c == '-' || c == '_')
                    {
                        continue;
                    }

                    if *name == "Generic Secret Assignment"
                        && generic_secret_assignment_is_false_positive(content, matched)
                    {
                        continue;
                    }

                    // Redact the middle of the matched secret for display
                    let redacted = if matched.len() > 12 {
                        format!("{}...{}", &matched[..6], &matched[matched.len()-4..])
                    } else {
                        matched.to_string()
                    };
                    findings.push((
                        current_file.clone(),
                        current_line,
                        name.to_string(),
                        redacted,
                    ));
                    break; // One finding per line is enough
                }
            }
            current_line += 1;
        } else if !line.starts_with('-') && !line.starts_with("\\") {
            // Context line (no prefix) - still increment line counter
            current_line += 1;
            ignore_next_added_line = false;
        }
    }

    findings
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
        println!("\n⚠️  Warning: Potential secrets detected but FLOW_ALLOW_SECRET_COMMIT=1, continuing...");
        return Ok(());
    }

    println!();
    print_secret_findings(
        "🔐 Potential secrets detected in staged changes:",
        findings,
    );
    println!();
    println!("If these are false positives (examples, placeholders, tests), you can:");
    println!("   - Set FLOW_ALLOW_SECRET_COMMIT=1 to override for this commit");
    println!("   - Mark the line with '# flow:secret:ignore' (or add it on the line above to ignore the next line)");
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

    let mut rescan_after_fix = |findings: &mut Vec<(String, usize, String, String)>| -> Result<()> {
        git_run_in(repo_root, &["add", "."])?;
        ensure_no_internal_staged(repo_root)?;
        ensure_no_unwanted_staged(repo_root)?;
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
    let hive_enabled =
        agent_name.trim().to_lowercase() != "off" && which::which("hive").is_ok();
    let ai_available = which::which("ai").is_ok();
    if !hive_enabled && !ai_available {
        return Ok(false);
    }

    git_run(&["add", "."])?;
    ensure_no_internal_staged(repo_root)?;
    ensure_no_unwanted_staged(repo_root)?;

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
        summary.push_str(&format!("- {}:{} — {} ({})\n", file, line, pattern, matched));
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

fn print_secret_findings(
    header: &str,
    findings: &[(String, usize, String, String)],
) {
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

    if std::env::var("CEREBRAS_API_KEY").map(|v| v.trim().is_empty()).unwrap_or(true) {
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
            let model = model.unwrap_or_else(|| "opencode/minimax-m2.1-free".to_string());
            Some(ReviewSelection::Opencode { model })
        }
        "openrouter" => {
            let model =
                model.unwrap_or_else(|| "arcee-ai/trinity-large-preview:free".to_string());
            Some(ReviewSelection::OpenRouter { model })
        }
        "rise" => {
            let model = model.unwrap_or_else(|| "zai:glm-4.7".to_string());
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

#[derive(Debug, Deserialize)]
struct ReviewJson {
    issues_found: bool,
    #[serde(default)]
    issues: Vec<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    future_tasks: Vec<String>,
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
pub fn run(push: bool, queue: CommitQueueMode, include_unhash: bool) -> Result<()> {
    // Check if hub is running - if so, delegate
    if hub::hub_healthy(HUB_HOST, HUB_PORT) {
        ensure_git_repo()?;
        let repo_root = git_root_or_cwd();
        ensure_commit_setup(&repo_root)?;
        git_guard::ensure_clean_for_commit(&repo_root)?;
        if should_run_sync_for_secret_fixes(&repo_root)? {
            return run_sync(push, queue, include_unhash);
        }
        return delegate_to_hub(push, queue, include_unhash);
    }

    run_sync(push, queue, include_unhash)
}

/// Run commit synchronously (called directly or by hub).
pub fn run_sync(push: bool, queue: CommitQueueMode, include_unhash: bool) -> Result<()> {
    let queue_enabled = queue.enabled;
    let push = push && !queue_enabled;
    info!(push = push, queue = queue_enabled, "starting commit workflow");

    // Ensure we're in a git repo
    ensure_git_repo()?;
    debug!("verified git repository");
    let repo_root = git_root_or_cwd();
    ensure_commit_setup(&repo_root)?;
    git_guard::ensure_clean_for_commit(&repo_root)?;

    let commit_message_override = resolve_commit_message_override(&repo_root);
    let commit_provider = if commit_message_override.is_none() {
        Some(resolve_commit_message_provider()?)
    } else {
        None
    };
    debug!(
        has_override = commit_message_override.is_some(),
        "resolved commit message override"
    );

    // Stage all changes
    print!("Staging changes... ");
    io::stdout().flush()?;
    git_run(&["add", "."])?;
    println!("done");
    debug!("staged all changes");
    ensure_no_internal_staged(&repo_root)?;
    ensure_no_unwanted_staged(&repo_root)?;

    // Check for sensitive files before proceeding
    let cwd = std::env::current_dir()?;
    let sensitive_files = check_sensitive_files(&cwd);
    warn_sensitive_files(&sensitive_files)?;

    // Scan diff content for hardcoded secrets
    let secret_findings = scan_diff_for_secrets(&cwd);
    warn_secrets_in_diff(&repo_root, &secret_findings)?;

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
    let mut message = match commit_message_override {
        Some(CommitMessageOverride::Kimi { model }) => generate_commit_message_kimi(
            &diff_for_prompt,
            &status,
            truncated,
            model.as_deref(),
        )?,
        None => {
            let commit_provider = commit_provider.as_ref().expect("commit provider missing");
            info!(model = MODEL, "calling OpenAI API");
            commit_message_from_provider(commit_provider, &diff_for_prompt, &status, truncated)?
        }
    };
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
                print_queue_instructions(&sha);
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
        print!("Pushing... ");
        io::stdout().flush()?;

        match git_push_try() {
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

                match git_try(&["pull", "--rebase"]) {
                    Ok(_) => {
                        println!("done");
                        print!("Pushing... ");
                        io::stdout().flush()?;
                        git_run(&["push"])?;
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

    // Sync to gitedit if enabled
    let cwd = std::env::current_dir().unwrap_or_default();
    if gitedit_globally_enabled() && gitedit_mirror_enabled_for_commit(&repo_root) {
        sync_to_gitedit(&cwd, "commit", &[], None, None);
    }

    Ok(())
}

/// Run a fast commit with the provided message (no AI review).
pub fn run_fast(message: &str, push: bool, queue: CommitQueueMode, include_unhash: bool) -> Result<()> {
    let queue_enabled = queue.enabled;
    let push = push && !queue_enabled;
    ensure_git_repo()?;
    let repo_root = git_root_or_cwd();
    ensure_commit_setup(&repo_root)?;
    git_guard::ensure_clean_for_commit(&repo_root)?;

    // Run pre-commit fixers if configured (fast lint/format)
    if let Ok(fixed) = run_fixers(&repo_root) {
        if fixed {
            println!();
        }
    }

    // Stage all changes
    print!("Staging changes... ");
    io::stdout().flush()?;
    git_run(&["add", "."])?;
    println!("done");
    ensure_no_internal_staged(&repo_root)?;
    ensure_no_unwanted_staged(&repo_root)?;

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

    // Commit
    git_run(&["commit", "-m", &full_message])?;
    println!("✓ Committed");

    log_commit_event_for_repo(&repo_root, &full_message, "commit", None, None);

    if queue_enabled {
        match queue_commit_for_review(&repo_root, &full_message, None, None, None, Vec::new()) {
            Ok(sha) => {
                print_queue_instructions(&sha);
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
        print!("Pushing... ");
        io::stdout().flush()?;

        match git_push_try() {
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

                match git_try(&["pull", "--rebase"]) {
                    Ok(_) => {
                        println!("done");
                        print!("Pushing... ");
                        io::stdout().flush()?;
                        git_run(&["push"])?;
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

    if gitedit_globally_enabled() && gitedit_mirror_enabled() {
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
    queue: CommitQueueMode,
    include_unhash: bool,
) -> Result<()> {
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
) -> Result<()> {
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
) -> Result<()> {
    let push_requested = push;
    let mut queue_enabled = queue.enabled;
    // Convert tokens to chars (roughly 4 chars per token)
    let max_context = max_tokens * 4;
    info!(
        push = push_requested && !queue_enabled,
        queue = queue_enabled,
        include_context = include_context,
        review_model = review_selection.model_label(),
        max_tokens = max_tokens,
        "starting commit with check workflow"
    );

    // Ensure we're in a git repo
    ensure_git_repo()?;

    let repo_root = resolve_commit_with_check_root()?;
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

    // Stage all changes
    print!("Staging changes... ");
    io::stdout().flush()?;
    git_run_in(&repo_root, &["add", "."])?;
    println!("done");
    ensure_no_internal_staged(&repo_root)?;

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
    } else if review_selection.is_openrouter() {
        println!("\nRunning OpenRouter review...");
    } else if review_selection.is_rise() {
        println!("\nRunning Rise AI review...");
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
        ReviewSelection::Claude(model) => run_claude_review(
            &diff,
            session_context.as_deref(),
            review_instructions.as_deref(),
            &repo_root,
            *model,
        ),
        ReviewSelection::Codex(model) => run_codex_review(
            &diff,
            session_context.as_deref(),
            review_instructions.as_deref(),
            &repo_root,
            *model,
        ),
        ReviewSelection::Opencode { model } => run_opencode_review(
            &diff,
            session_context.as_deref(),
            review_instructions.as_deref(),
            &repo_root,
            model,
        ),
        ReviewSelection::OpenRouter { model } => run_openrouter_review(
            &diff,
            session_context.as_deref(),
            review_instructions.as_deref(),
            &repo_root,
            model,
        ),
        ReviewSelection::Rise { model } => run_rise_review(
            &diff,
            session_context.as_deref(),
            review_instructions.as_deref(),
            &repo_root,
            model,
        ),
        ReviewSelection::Kimi { model } => run_kimi_review(
            &diff,
            session_context.as_deref(),
            review_instructions.as_deref(),
            &repo_root,
            model.as_deref(),
        ),
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

    let review_model_label = review_selection.model_label();
    let review_reviewer_label = if review_selection.is_claude() {
        "claude"
    } else if review_selection.is_codex() {
        "codex"
    } else if review_selection.is_opencode() {
        "opencode"
    } else if review_selection.is_openrouter() {
        "openrouter"
    } else if review_selection.is_rise() {
        "rise"
    } else {
        "kimi"
    };
    record_review_tasks(&repo_root, &review, &review_model_label);

    // Continue with normal commit flow
    let commit_message_override = resolve_commit_message_override(&repo_root);
    let commit_provider = if commit_message_override.is_none() {
        Some(resolve_commit_message_provider()?)
    } else {
        None
    };

    // Get status
    let status = git_capture_in(&repo_root, &["status", "--short"]).unwrap_or_default();

    // Truncate diff if needed
    let (diff_for_prompt, truncated) = truncate_diff(&diff);

    // Generate commit message based on the review tool
    print!("Generating commit message... ");
    io::stdout().flush()?;
    let message = if let Some(override_tool) = commit_message_override {
        match override_tool {
            CommitMessageOverride::Kimi { model } => generate_commit_message_kimi(
                &diff_for_prompt,
                &status,
                truncated,
                model.as_deref(),
            )?,
        }
    } else {
        let commit_provider = commit_provider.as_ref().expect("commit provider missing");
        match &review_selection {
            ReviewSelection::Opencode { model } => {
                match generate_commit_message_opencode(&diff_for_prompt, &status, truncated, model)
                {
                    Ok(message) => message,
                    Err(err) => match commit_provider {
                        CommitMessageProvider::Remote { .. } => {
                            println!(
                                "⚠ Opencode commit message failed: {}. Falling back to myflow.",
                                err
                            );
                            commit_message_from_provider(
                                commit_provider,
                                &diff_for_prompt,
                                &status,
                                truncated,
                            )?
                        }
                        _ => return Err(err),
                    },
                }
            }
            ReviewSelection::OpenRouter { model } => {
                match generate_commit_message_openrouter(&diff_for_prompt, &status, truncated, model)
                {
                    Ok(message) => message,
                    Err(err) => match commit_provider {
                        CommitMessageProvider::Remote { .. } => {
                            println!(
                                "⚠ OpenRouter commit message failed: {}. Falling back to myflow.",
                                err
                            );
                            commit_message_from_provider(
                                commit_provider,
                                &diff_for_prompt,
                                &status,
                                truncated,
                            )?
                        }
                        _ => return Err(err),
                    },
                }
            }
            ReviewSelection::Rise { model } => {
                match generate_commit_message_rise(&diff_for_prompt, &status, truncated, model) {
                    Ok(message) => message,
                    Err(err) => match commit_provider {
                        CommitMessageProvider::Remote { .. } => {
                            println!(
                                "⚠ Rise commit message failed: {}. Falling back to myflow.",
                                err
                            );
                            commit_message_from_provider(
                                commit_provider,
                                &diff_for_prompt,
                                &status,
                                truncated,
                            )?
                        }
                        _ => return Err(err),
                    },
                }
            }
            ReviewSelection::Kimi { model } => {
                match generate_commit_message_kimi(
                    &diff_for_prompt,
                    &status,
                    truncated,
                    model.as_deref(),
                ) {
                    Ok(message) => message,
                    Err(err) => match commit_provider {
                        CommitMessageProvider::Remote { .. } => {
                            println!(
                                "⚠ Kimi commit message failed: {}. Falling back to myflow.",
                                err
                            );
                            commit_message_from_provider(
                                commit_provider,
                                &diff_for_prompt,
                                &status,
                                truncated,
                            )?
                        }
                        _ => return Err(err),
                    },
                }
            }
            ReviewSelection::Claude(_) => {
                match generate_commit_message_claude(&diff_for_prompt, &status, truncated) {
                    Ok(message) => message,
                    Err(err) => match commit_provider {
                        CommitMessageProvider::Remote { .. } => {
                            println!(
                                "⚠ Claude commit message failed: {}. Falling back to myflow.",
                                err
                            );
                            commit_message_from_provider(
                                commit_provider,
                                &diff_for_prompt,
                                &status,
                                truncated,
                            )?
                        }
                        _ => return Err(err),
                    },
                }
            }
            _ => commit_message_from_provider(
                commit_provider,
                &diff_for_prompt,
                &status,
                truncated,
            )?,
        }
    };
    let message = sanitize_commit_message(&message);
    println!("done\n");

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

    if gitedit_enabled || unhash_enabled {
        match ai::get_sessions_for_gitedit(&repo_root) {
            Ok(sessions) => {
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
            Err(err) => {
                debug!("failed to collect AI sessions for gitedit/unhash: {}", err);
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

    let review_summary = ai::CommitReviewSummary {
        model: review_selection.model_label(),
        reviewer: if review_selection.is_claude() {
            "claude".to_string()
        } else {
            "codex".to_string()
        },
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
                print_queue_instructions(&sha);
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
        print!("Pushing... ");
        io::stdout().flush()?;

        match git_push_try() {
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

                match git_try(&["pull", "--rebase"]) {
                    Ok(_) => {
                        println!("done");
                        print!("Pushing... ");
                        io::stdout().flush()?;
                        git_run(&["push"])?;
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
            reviewer: Some(if review_selection.is_claude() {
                "claude".to_string()
            } else {
                "codex".to_string()
            }),
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
    let mut prompt = String::from(
        "Review diff for bugs, security, perf issues. Return JSON: {\"issues_found\":bool,\"issues\":[\"...\"],\"summary\":\"...\",\"future_tasks\":[\"...\"]}. future_tasks are optional follow-up improvements or optimizations (max 3), actionable, and not duplicates of issues; use [] if none.\n",
    );

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
            future_tasks: Vec::new(),
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
    let future_tasks = review_json
        .as_ref()
        .map(|parsed| normalize_future_tasks(&parsed.future_tasks))
        .unwrap_or_default();
    let summary = review_json.as_ref().and_then(|r| r.summary.clone());
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
        summary,
        future_tasks,
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
    let review_json = parse_review_json(&result);
    let future_tasks = review_json
        .as_ref()
        .map(|parsed| normalize_future_tasks(&parsed.future_tasks))
        .unwrap_or_default();
    let summary = review_json.as_ref().and_then(|r| r.summary.clone());
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
        let future_tasks = review_json
            .as_ref()
            .map(|parsed| normalize_future_tasks(&parsed.future_tasks))
            .unwrap_or_default();
        let summary = review_json.as_ref().and_then(|r| r.summary.clone());
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
    let review_json = parse_review_json(&output);
    let future_tasks = review_json
        .as_ref()
        .map(|json| normalize_future_tasks(&json.future_tasks))
        .unwrap_or_default();
    let summary = review_json.as_ref().and_then(|r| r.summary.clone());
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

    let stdout = child.stdout.take().context("failed to capture kimi stdout")?;
    let stderr = child.stderr.take().context("failed to capture kimi stderr")?;

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
    let review_json = parse_review_json(&result);
    let future_tasks = review_json
        .as_ref()
        .map(|json| normalize_future_tasks(&json.future_tasks))
        .unwrap_or_default();
    let mut summary = review_json.as_ref().and_then(|r| r.summary.clone());
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

    let review_json = parse_review_json(&output);
    let future_tasks = review_json
        .as_ref()
        .map(|json| normalize_future_tasks(&json.future_tasks))
        .unwrap_or_default();
    let mut summary = review_json.as_ref().and_then(|r| r.summary.clone());
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

    Err(last_err.unwrap_or_else(|| {
        anyhow::anyhow!("OpenRouter request failed after retries")
    }))
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

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
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
    let review_json = parse_review_json(&output);
    let future_tasks = review_json
        .as_ref()
        .map(|json| normalize_future_tasks(&json.future_tasks))
        .unwrap_or_default();
    let summary = review_json.as_ref().and_then(|r| r.summary.clone());
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

fn ensure_commit_setup(repo_root: &Path) -> Result<()> {
    let ai_internal = repo_root.join(".ai").join("internal");
    fs::create_dir_all(&ai_internal)
        .with_context(|| format!("failed to create {}", ai_internal.display()))?;
    setup::add_gitignore_entry(repo_root, ".ai/internal/")?;
    setup::add_gitignore_entry(repo_root, ".ai/todos/*.bike")?;
    setup::add_gitignore_entry(repo_root, ".beads/")?;
    setup::add_gitignore_entry(repo_root, ".rise/")?;
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
    for (path, reason) in &staged {
        println!("Refusing to commit generated file: {} ({})", path, reason);
        if path.starts_with(".beads/") || path == ".beads" {
            ignore_entries.insert(".beads/");
        }
        if path == ".rise" || path.starts_with(".rise/") || path.contains("/.rise/") {
            ignore_entries.insert(".rise/");
        }
        if path.ends_with(".pyc") || path.contains("/__pycache__/") || path.ends_with("/__pycache__")
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

    println!("Added ignore rules for generated files and unstaged them.");
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
    if path == ".beads" || path.starts_with(".beads/") || path.contains("/.beads/") {
        return Some("beads metadata");
    }
    if path == ".rise" || path.starts_with(".rise/") || path.contains("/.rise/") {
        return Some("rise metadata");
    }
    if path.ends_with(".pyc") {
        return Some("python bytecode");
    }
    if path.ends_with("/__pycache__") || path.contains("/__pycache__/") || path.starts_with("__pycache__/") {
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

    if let Err(e) = undo::record_action(
        repo_root,
        action_type,
        &before_sha,
        &after_sha,
        branch.trim(),
        pushed,
        Some("origin"),
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
    if sha.len() <= 7 {
        sha
    } else {
        &sha[..7]
    }
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
                    original_path: if original.is_empty() { None } else { Some(original) },
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
    let payload = serde_json::to_string_pretty(&session).context("serialize rise review session")?;
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
    let commit_sha = git_capture_in(repo_root, &["rev-parse", "HEAD"])?.trim().to_string();
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
    let (cmd, args): (String, Vec<String>) = if let Ok(rise_app_path) = which::which("rise-app")
    {
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
            .and_then(|bytes| bytes.get(0..128).map(|chunk| String::from_utf8_lossy(chunk).to_string()))
            .map(|head| !head.starts_with("#!") && (head.starts_with("/*") || head.starts_with("//")))
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

fn print_queue_instructions(commit_sha: &str) {
    println!("Queued commit {} for review.", short_sha(commit_sha));
    println!("  f commit-queue list");
    println!("  f commit-queue show {}", short_sha(commit_sha));
    println!("  f commit-queue approve {}", short_sha(commit_sha));
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
    let new_sha = output.split_whitespace().next().unwrap_or_default().trim().to_string();
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

    jj_run_in(&jj_root, &["bookmark", "create", &name, "-r", commit_sha])?;
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
    let status = Command::new("jj")
        .current_dir(repo_root)
        .args(args)
        .status()
        .with_context(|| format!("failed to run jj {}", args.join(" ")))?;
    if !status.success() {
        bail!("jj {} failed", args.join(" "));
    }
    Ok(())
}

fn jj_capture_in(repo_root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("jj")
        .current_dir(repo_root)
        .args(args)
        .output()
        .with_context(|| format!("failed to run jj {}", args.join(" ")))?;
    if !output.status.success() {
        bail!("jj {} failed", args.join(" "));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
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
        bail!("unable to determine GitHub repo (origin URL not GitHub, and `gh repo view` returned empty)");
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

fn ensure_pr_head_pushed(repo_root: &Path, head: &str, commit_sha: &str) -> Result<()> {
    // Prefer jj bookmarks when available.
    if which::which("jj").is_ok() {
        // Ensure bookmark points at the commit, then push it.
        jj_run_in(
            repo_root,
            &["bookmark", "set", head, "-r", commit_sha, "--allow-backwards"],
        )?;
        // We often push a brand new review/pr bookmark as the PR head.
        jj_run_in(repo_root, &["git", "push", "--bookmark", head, "--allow-new"])?;
        return Ok(());
    }

    // Fallback: create/update a git branch and push it.
    git_run_in(repo_root, &["branch", "-f", head, commit_sha])?;
    git_run_in(repo_root, &["push", "-u", "origin", head])?;
    Ok(())
}

fn extract_pr_url(text: &str) -> Option<String> {
    let re = Regex::new(r"https://github\\.com/[^/\\s]+/[^/\\s]+/pull/\\d+").ok()?;
    re.find(text).map(|m| m.as_str().to_string())
}

fn pr_number_from_url(url: &str) -> Option<u64> {
    let parts: Vec<&str> = url.trim_end_matches('/').split('/').collect();
    parts.last()?.parse().ok()
}

fn gh_find_open_pr_by_head(
    repo_root: &Path,
    repo: &str,
    head: &str,
) -> Result<Option<(u64, String)>> {
    let out = gh_capture_in(
        repo_root,
        &[
            "pr",
            "list",
            "--repo",
            repo,
            "--head",
            head,
            "--state",
            "open",
            "--json",
            "number,url",
            "-q",
            ".[0] | [.number, .url] | @tsv",
        ],
    )
    .unwrap_or_default();
    let line = out.trim();
    if line.is_empty() {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let number = parts.next().and_then(|v| v.parse::<u64>().ok());
    let url = parts.next().map(|v| v.to_string());
    match (number, url) {
        (Some(n), Some(u)) => Ok(Some((n, u))),
        _ => Ok(None),
    }
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
        body,
    ];
    if draft {
        args.push("--draft");
    }

    let output = Command::new("gh")
        .current_dir(repo_root)
        .args(&args)
        .output()
        .with_context(|| format!("failed to run gh {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "gh {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    // gh typically prints the PR URL, but some versions/configs can produce no stdout.
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
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
                let subject = entry
                    .message
                    .lines()
                    .next()
                    .unwrap_or("no message")
                    .trim();
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
                if let Some(body) = entry.review.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty())
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
        CommitQueueAction::Approve {
            hash,
            force,
            allow_issues,
            allow_unreviewed,
        } => {
            git_guard::ensure_clean_for_push(&repo_root)?;
            let mut entry = resolve_commit_queue_entry(&repo_root, &hash)?;
            let _ = refresh_queue_entry_commit(&repo_root, &mut entry);

            let issues_present = entry.review_issues_found
                || entry
                    .review
                    .as_deref()
                    .map(|s| !s.trim().is_empty())
                    .unwrap_or(false);
            let unreviewed = (entry.version >= 2 && !entry.review_completed) || entry.review_timed_out;

            if issues_present && !allow_issues && !force {
                bail!(
                    "Queued commit {} has review issues. Fix them, or re-run with --allow-issues.",
                    short_sha(&entry.commit_sha)
                );
            }
            if unreviewed && !allow_unreviewed && !force {
                bail!(
                    "Queued commit {} does not have a clean review (timed out/missing). Re-run review, or re-run with --allow-unreviewed.",
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

            let current_branch =
                git_capture_in(&repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])
                    .unwrap_or_else(|_| "unknown".to_string());
            if current_branch.trim() != entry.branch && !force {
                bail!(
                    "Queued commit was created on branch {} but current branch is {}. Checkout the branch or re-run with --force.",
                    entry.branch,
                    current_branch.trim()
                );
            }

            if git_try_in(&repo_root, &["fetch", "--quiet"]).is_ok() {
                if let Ok(counts) =
                    git_capture_in(&repo_root, &["rev-list", "--left-right", "--count", "@{u}...HEAD"])
                {
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

            print!("Pushing... ");
            io::stdout().flush()?;
            let mut pushed = false;
            match git_push_try_in(&repo_root) {
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
                    match git_try_in(&repo_root, &["pull", "--rebase"]) {
                        Ok(_) => {
                            println!("done");
                            print!("Pushing... ");
                            io::stdout().flush()?;
                            git_run_in(&repo_root, &["push"])?;
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
                        Some("origin"),
                        Some(&entry.message),
                    );
                }
                if let Some(bookmark) = entry.review_bookmark.as_ref() {
                    delete_review_bookmark(&repo_root, bookmark);
                }
                remove_commit_queue_entry_by_entry(&repo_root, &entry)?;
                println!("✓ Approved and pushed {}", short_sha(&entry.commit_sha));
            }
        }
        CommitQueueAction::ApproveAll {
            force,
            allow_issues,
            allow_unreviewed,
        } => {
            git_guard::ensure_clean_for_push(&repo_root)?;
            let mut entries = load_commit_queue_entries(&repo_root)?;
            if entries.is_empty() {
                println!("No queued commits.");
                return Ok(());
            }

            for entry in &mut entries {
                let _ = refresh_queue_entry_commit(&repo_root, entry);
            }

            let current_branch =
                git_capture_in(&repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])
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

            if git_try_in(&repo_root, &["fetch", "--quiet"]).is_ok() {
                if let Ok(counts) =
                    git_capture_in(&repo_root, &["rev-list", "--left-right", "--count", "@{u}...HEAD"])
                {
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

            print!("Pushing... ");
            io::stdout().flush()?;
            let mut pushed = false;
            match git_push_try_in(&repo_root) {
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
                    match git_try_in(&repo_root, &["pull", "--rebase"]) {
                        Ok(_) => {
                            println!("done");
                            print!("Pushing... ");
                            io::stdout().flush()?;
                            git_run_in(&repo_root, &["push"])?;
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
                    let branch = current_branch.as_str();
                    let before_sha = before_sha.trim();
                    let after_sha = after_sha.trim();
                    let _ = undo::record_action(
                        &repo_root,
                        undo::ActionType::Push,
                        before_sha,
                        after_sha,
                        branch,
                        true,
                        Some("origin"),
                        None,
                    );
                }

                let head_sha = git_capture_in(&repo_root, &["rev-parse", "HEAD"])
                    .unwrap_or_default();
                let head_sha = head_sha.trim();
                let mut approved = 0;
                let mut skipped = 0;

                for entry in &candidates {
                    if git_is_ancestor(&repo_root, &entry.commit_sha, head_sha) {
                        if let Some(bookmark) = entry.review_bookmark.as_ref() {
                            delete_review_bookmark(&repo_root, bookmark);
                        }
                        remove_commit_queue_entry_by_entry(&repo_root, entry)?;
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
        }
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
            ensure_pr_head_pushed(&repo_root, &head, &entry.commit_sha)?;

            let (number, url) = if let Some(found) = gh_find_open_pr_by_head(&repo_root, &repo, &head)? {
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
                gh_create_pr(&repo_root, &repo, &head, &base, &title, body.trim(), draft)?
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
                ensure_pr_head_pushed(&repo_root, &head, &entry.commit_sha)?;
                let (title, body_rest) = commit_message_title_body(&entry.message);
                let (number, url) =
                    gh_create_pr(&repo_root, &repo, &head, &base, &title, body_rest.trim(), true)?;
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

enum CommitMessageProvider {
    OpenAi { api_key: String },
    Remote { api_url: String, token: String },
}

#[derive(Debug, Clone)]
enum CommitMessageOverride {
    Kimi { model: Option<String> },
}

fn parse_commit_message_override(
    tool: &str,
    model: Option<String>,
) -> Option<CommitMessageOverride> {
    match tool.trim().to_ascii_lowercase().as_str() {
        "kimi" => Some(CommitMessageOverride::Kimi { model }),
        _ => None,
    }
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

fn resolve_commit_message_provider() -> Result<CommitMessageProvider> {
    if let Ok(api_key) = get_openai_key() {
        let trimmed = api_key.trim().to_string();
        if !trimmed.is_empty() {
            return Ok(CommitMessageProvider::OpenAi { api_key: trimmed });
        }
    }

    if let Ok(Some(token)) = crate::env::load_ai_auth_token() {
        let api_url = crate::env::load_ai_api_url()?;
        let trimmed_url = api_url.trim().trim_end_matches('/').to_string();
        if !trimmed_url.is_empty() {
            return Ok(CommitMessageProvider::Remote {
                api_url: trimmed_url,
                token,
            });
        }
    }

    bail!("OPENAI_API_KEY not set. Run `f auth` or set OPENAI_API_KEY.")
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

/// Try to push and detect if failure is due to missing remote repo.
fn git_push_try() -> PushResult {
    let output = Command::new("git").args(["push"]).output().ok();

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

fn git_push_try_in(workdir: &std::path::Path) -> PushResult {
    let output = Command::new("git")
        .current_dir(workdir)
        .args(["push"])
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

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
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

fn generate_commit_message_remote(
    api_url: &str,
    token: &str,
    diff: &str,
    status: &str,
    truncated: bool,
) -> Result<String> {
    let trimmed = api_url.trim().trim_end_matches('/');
    let url = format!("{}/api/ai/commit-message", trimmed);

    let client = Client::builder()
        .timeout(Duration::from_secs(commit_with_check_timeout_secs()))
        .build()
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
    model
        .strip_prefix("openrouter/")
        .or_else(|| model.strip_prefix("openrouter:"))
        .unwrap_or(model)
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
        if let Ok(vars) =
            crate::env::fetch_personal_env_vars(&["OPENROUTER_API_KEY".to_string()])
        {
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

fn record_review_tasks(repo_root: &Path, review: &ReviewResult, model_label: &str) {
    let tasks = &review.future_tasks;
    if tasks.is_empty() {
        return;
    }
    if env_flag("FLOW_REVIEW_TASKS_DISABLE") {
        return;
    }
    let Some(beads_dir) = review_tasks_beads_dir(repo_root) else {
        return;
    };
    match write_review_tasks(&beads_dir, repo_root, tasks, review.summary.as_deref(), model_label) {
        Ok(created) => {
            if created > 0 {
                println!(
                    "Added {} review follow-up task(s) to {}",
                    created,
                    beads_dir.display()
                );
            }
        }
        Err(err) => {
            println!("⚠️ Failed to record review tasks: {}", err);
        }
    }
}

fn review_tasks_beads_dir(repo_root: &Path) -> Option<std::path::PathBuf> {
    let allow_in_repo = env_flag("FLOW_REVIEW_TASKS_ALLOW_IN_REPO");
    if let Ok(dir) = env::var("FLOW_REVIEW_TASKS_DIR") {
        let trimmed = dir.trim();
        if !trimmed.is_empty() {
            let candidate = std::path::PathBuf::from(trimmed);
            return Some(resolve_review_tasks_dir(repo_root, candidate, allow_in_repo));
        }
    }
    if let Ok(root) = env::var("FLOW_REVIEW_TASKS_ROOT") {
        let trimmed = root.trim();
        if !trimmed.is_empty() {
            let candidate = std::path::PathBuf::from(trimmed).join(".beads");
            return Some(resolve_review_tasks_dir(repo_root, candidate, allow_in_repo));
        }
    }
    dirs::home_dir().map(|home| home.join(".beads"))
}

fn resolve_review_tasks_dir(
    repo_root: &Path,
    candidate: PathBuf,
    allow_in_repo: bool,
) -> PathBuf {
    let resolved = if candidate.is_relative() {
        repo_root.join(candidate)
    } else {
        candidate
    };
    if allow_in_repo {
        return resolved;
    }
    if resolved.starts_with(repo_root) {
        if let Some(home) = dirs::home_dir() {
            let fallback = home.join(".beads");
            println!(
                "⚠️ Review tasks dir is inside repo; writing tasks to {}",
                fallback.display()
            );
            return fallback;
        }
    }
    resolved
}

fn write_review_tasks(
    beads_dir: &Path,
    repo_root: &Path,
    tasks: &[String],
    summary: Option<&str>,
    model_label: &str,
) -> Result<usize> {
    if tasks.is_empty() {
        return Ok(0);
    }
    fs::create_dir_all(beads_dir)
        .with_context(|| format!("create beads dir {}", beads_dir.display()))?;

    let project_path = repo_root.display().to_string();
    let project_name = repo_root
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".to_string());
    let summary = summary.map(|text| text.trim()).filter(|text| !text.is_empty());

    let mut created = 0;
    for task in tasks {
        let title = review_task_title(task);
        let description = review_task_description(task, &project_path, summary, model_label);
        let external_ref = format!("flow-review:{}", review_task_id(&project_path, task));
        let labels = format!("review,flow,project:{}", project_name);

        let output = Command::new("br")
            .arg("create")
            .arg("--title")
            .arg(title)
            .arg("--description")
            .arg(description)
            .arg("--type")
            .arg("task")
            .arg("--priority")
            .arg("4")
            .arg("--status")
            .arg("open")
            .arg("--external-ref")
            .arg(external_ref)
            .arg("--labels")
            .arg(labels)
            .arg("--silent")
            .env("BEADS_DIR", beads_dir)
            .output()
            .context("run br create")?;

        if output.status.success() {
            created += 1;
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let msg = if stderr.trim().is_empty() {
                stdout.trim()
            } else {
                stderr.trim()
            };
            println!("⚠️ beads create failed: {}", msg);
        }
    }

    Ok(created)
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

fn review_task_description(
    task: &str,
    project_path: &str,
    summary: Option<&str>,
    model_label: &str,
) -> String {
    let mut desc = String::new();
    desc.push_str(task.trim());
    desc.push_str("\n\nProject: ");
    desc.push_str(project_path);
    desc.push_str("\nModel: ");
    desc.push_str(model_label);
    if let Some(summary) = summary {
        desc.push_str("\nReview summary: ");
        desc.push_str(summary);
    }
    desc
}

fn review_task_id(project_path: &str, task: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(project_path.as_bytes());
    hasher.update(b":");
    hasher.update(task.trim().as_bytes());
    let hex = hex::encode(hasher.finalize());
    let short = hex.get(..12).unwrap_or(&hex);
    format!("bd-{}", short)
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

    let client = match Client::builder().timeout(Duration::from_secs(10)).build() {
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

fn assistant_traces_root() -> Option<std::path::PathBuf> {
    if let Ok(value) = env::var("UNHASH_TRACE_DIR") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(std::path::PathBuf::from(trimmed));
        }
    }
    dirs::home_dir().map(|home| {
        home.join("code")
            .join("org")
            .join("1f")
            .join("jazz")
            .join("assistant-traces")
    })
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

fn write_agent_trace_file(
    bundle_path: &Path,
    rel_path: &str,
    data: &[u8],
) -> Result<()> {
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
            ("agent/fish/rise.history.jsonl", fish_dir.join("rise.history.jsonl")),
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
    let future_tasks = review
        .map(|r| r.future_tasks.clone())
        .unwrap_or_default();

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
    let _ = write_agent_trace_file(bundle_path, "agent/patch_summary.md", patch_summary_md.as_bytes());

    let _ = append_learning_store(repo_root, &learn_json, &decision_md, &regression_md, &patch_summary_md);
}

fn classify_learning_tags(texts: &[String]) -> Vec<String> {
    let mut tags = HashSet::new();
    for text in texts {
        let lowered = text.to_lowercase();
        if lowered.contains("perf") || lowered.contains("performance") || lowered.contains("latency") {
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
    let summary = learn.get("root_cause").and_then(|v| v.as_str()).unwrap_or("n/a");
    let fix = learn.get("fix").and_then(|v| v.as_str()).unwrap_or("n/a");
    let prevention = learn.get("prevention").and_then(|v| v.as_str()).unwrap_or("n/a");
    format!(
        "# Decision\n\n## Summary\n{}\n\n## Fix\n{}\n\n## Prevention\n{}\n",
        summary, fix, prevention
    )
}

fn render_learning_regression_md(learn: &serde_json::Value) -> String {
    let issue = learn.get("issue").and_then(|v| v.as_str()).unwrap_or("none");
    let prevention = learn.get("prevention").and_then(|v| v.as_str()).unwrap_or("n/a");
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
            return Ok(PathBuf::from(trimmed).join(".ai").join("internal").join("learn"));
        }
    }

    if let Some(home) = dirs::home_dir() {
        return Ok(
            home.join("code")
                .join("org")
                .join("linsa")
                .join("base")
                .join(".ai")
                .join("internal")
                .join("learn"),
        );
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
    dirs::home_dir().map(|home| {
        home.join("code")
            .join("org")
            .join("1f")
            .join("jazz")
            .join("assistant-traces")
    })
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
        let json = serde_json::to_string_pretty(&sessions_data)
            .context("serialize sessions.json")?;
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
    let meta_json =
        serde_json::to_string_pretty(&metadata).context("serialize commit.json")?;
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
        debug!(
            "unhash failed: {} {}{}",
            output.status, stdout, stderr
        );
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

fn delegate_to_hub(push: bool, queue: CommitQueueMode, include_unhash: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;

    // Build the command to run using the current executable path
    let push_flag = if push { "" } else { " --no-push" };
    let queue_flag = queue_flag_for_command(queue);
    let review_flag = review_flag_for_command(queue);
    let hashed_flag = if include_unhash { " --hashed" } else { "" };
    let flow_bin = std::env::current_exe()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "flow".to_string());
    let command = format!(
        "{} commit --sync{}{}{}{}",
        flow_bin, push_flag, queue_flag, review_flag, hashed_flag
    );

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
    queue: CommitQueueMode,
    include_unhash: bool,
) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    let repo_root = resolve_commit_with_check_root()?;

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
    let flow_bin = std::env::current_exe()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "flow".to_string());
    let command = format!(
        "{} {} --sync{}{}{}{}{}{}{}{} --tokens {}",
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
