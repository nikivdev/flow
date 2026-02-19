//! Explain commits via AI — generate markdown summaries for git commits.
//!
//! Used by `f explain-commits N` and as a post-sync hook to auto-explain
//! new commits in tracked repos.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cli::ExplainCommitsCommand;
use crate::config;
use crate::projects;

const DEFAULT_OUTPUT_DIR: &str = "docs/commits";
const DEFAULT_BATCH_SIZE: usize = 10;
const MAX_DIFF_CHARS: usize = 8000;
const AI_TASK_SCRIPT: &str = "~/code/org/gen/new/ai/scripts/ai-task.sh";
const DEFAULT_PROVIDER: &str = "nvidia";
const DEFAULT_MODEL: &str = "moonshotai/kimi-k2.5";

// -- Index tracking --

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CommitIndex {
    version: u32,
    commits: HashMap<String, CommitEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CommitEntry {
    digest: String,
    file: String,
    at: String,
    #[serde(default)]
    sha: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExplainedCommit {
    pub sha: String,
    pub short_sha: String,
    pub subject: String,
    pub author: String,
    pub date: String,
    pub summary: String,
    pub changes: String,
    pub files: Vec<String>,
    pub markdown_file: String,
    pub generated_at: String,
}

impl Default for CommitIndex {
    fn default() -> Self {
        Self {
            version: 1,
            commits: HashMap::new(),
        }
    }
}

fn index_path(output_dir: &Path) -> PathBuf {
    output_dir.join(".index.json")
}

fn load_index(output_dir: &Path) -> CommitIndex {
    let path = index_path(output_dir);
    if path.exists() {
        fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    } else {
        CommitIndex::default()
    }
}

fn save_index(output_dir: &Path, index: &CommitIndex) -> Result<()> {
    let path = index_path(output_dir);
    let json = serde_json::to_string_pretty(index)?;
    fs::write(&path, json).context("failed to write commit index")?;
    Ok(())
}

fn compute_digest(sha: &str, message: &str, diff: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(sha.as_bytes());
    hasher.update(b"\n");
    hasher.update(message.as_bytes());
    hasher.update(b"\n");
    hasher.update(diff.as_bytes());
    format!("{:x}", hasher.finalize())
}

// -- Git helpers --

fn git_capture_in(repo_root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(args)
        .output()
        .context("failed to run git")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

struct CommitInfo {
    sha: String,
    short_sha: String,
    message: String,
    subject: String,
    author: String,
    date: String,
    diff: String,
    files: Vec<String>,
}

fn get_commit_info(repo_root: &Path, sha: &str) -> Result<CommitInfo> {
    let short_sha = &sha[..7.min(sha.len())];
    let message = git_capture_in(repo_root, &["log", "-1", "--format=%B", sha])?
        .trim()
        .to_string();
    let subject = git_capture_in(repo_root, &["log", "-1", "--format=%s", sha])?
        .trim()
        .to_string();
    let author = git_capture_in(repo_root, &["log", "-1", "--format=%an", sha])?
        .trim()
        .to_string();
    let raw_date = git_capture_in(
        repo_root,
        &["log", "-1", "--date=format:%Y-%m-%d", "--format=%ad", sha],
    )?
    .trim()
    .to_string();
    let date = if raw_date.len() == 10
        && raw_date.as_bytes().get(4) == Some(&b'-')
        && raw_date.as_bytes().get(7) == Some(&b'-')
    {
        raw_date
    } else {
        Utc::now().format("%Y-%m-%d").to_string()
    };

    let diff_full =
        git_capture_in(repo_root, &["diff", &format!("{}~1", sha), sha]).unwrap_or_default();
    let diff = if diff_full.len() > MAX_DIFF_CHARS {
        format!(
            "{}\n\n... (truncated, {} total chars)",
            &diff_full[..MAX_DIFF_CHARS],
            diff_full.len()
        )
    } else {
        diff_full
    };

    let files_raw = git_capture_in(
        repo_root,
        &["diff", "--name-only", &format!("{}~1", sha), sha],
    )
    .unwrap_or_default();
    let files: Vec<String> = files_raw
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    Ok(CommitInfo {
        sha: sha.to_string(),
        short_sha: short_sha.to_string(),
        message,
        subject,
        author,
        date,
        diff,
        files,
    })
}

fn get_commits_in_range(repo_root: &Path, from: &str, to: &str) -> Result<Vec<String>> {
    let range = format!("{}..{}", from, to);
    let output = git_capture_in(repo_root, &["rev-list", "--reverse", &range])?;
    Ok(output
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

fn get_last_n_commits(repo_root: &Path, n: usize) -> Result<Vec<String>> {
    let n_str = format!("{}", n);
    let output = git_capture_in(repo_root, &["rev-list", "--reverse", "-n", &n_str, "HEAD"])?;
    Ok(output
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

// -- AI explanation --

fn call_ai_explain(info: &CommitInfo, provider: &str, model: &str) -> Result<String> {
    let script = shellexpand::tilde(AI_TASK_SCRIPT).to_string();

    let prompt = format!(
        "Explain this git commit concisely. Give a 1-2 sentence summary, then explain what changed and why.\n\n\
         Commit: {}\nMessage: {}\n\nDiff:\n{}",
        info.short_sha, info.message, info.diff
    );

    let output = Command::new(&script)
        .args([
            "--agent",
            "explain",
            "--provider",
            provider,
            "--model",
            model,
            "--prompt",
            &prompt,
            "--max-steps",
            "5",
        ])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if text.is_empty() {
                Ok("(AI returned empty response)".to_string())
            } else {
                Ok(text)
            }
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            bail!("ai-task.sh failed: {}", stderr.trim());
        }
        Err(e) => {
            bail!("failed to run ai-task.sh: {e}");
        }
    }
}

// -- Markdown output --

fn slugify(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c.to_ascii_lowercase()
            } else if c == ' ' || c == '_' || c == '/' {
                '-'
            } else {
                '\0'
            }
        })
        .filter(|c| *c != '\0')
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

fn write_commit_markdown(
    output_dir: &Path,
    info: &CommitInfo,
    ai_explanation: &str,
    generated_at: &str,
) -> Result<String> {
    let slug = slugify(&info.subject);
    let slug = if slug.len() > 60 {
        slug[..60].trim_end_matches('-').to_string()
    } else {
        slug
    };

    let filename = format!("{}-{}-{}.md", info.date, info.short_sha, slug);
    let filepath = output_dir.join(&filename);

    // Parse AI explanation into summary and details
    let (summary, details) = split_ai_response(ai_explanation);

    let files_section = info
        .files
        .iter()
        .map(|f| format!("- {}", f))
        .collect::<Vec<_>>()
        .join("\n");

    let content = format!(
        "# {subject}\n\n\
         **Commit**: `{sha}` | **Date**: {date} | **Author**: {author}\n\n\
         ## Summary\n{summary}\n\n\
         ## Changes\n{details}\n\n\
         ## Files\n{files}\n",
        subject = info.subject,
        sha = info.short_sha,
        date = info.date,
        author = info.author,
        summary = &summary,
        details = &details,
        files = files_section,
    );

    fs::write(&filepath, &content)
        .with_context(|| format!("failed to write {}", filepath.display()))?;

    let sidecar_file = filename.replacen(".md", ".json", 1);
    let sidecar_path = output_dir.join(&sidecar_file);
    let sidecar = ExplainedCommit {
        sha: info.sha.clone(),
        short_sha: info.short_sha.clone(),
        subject: info.subject.clone(),
        author: info.author.clone(),
        date: info.date.clone(),
        summary,
        changes: details,
        files: info.files.clone(),
        markdown_file: filename.clone(),
        generated_at: generated_at.to_string(),
    };
    let sidecar_json = serde_json::to_string_pretty(&sidecar)?;
    fs::write(&sidecar_path, sidecar_json)
        .with_context(|| format!("failed to write {}", sidecar_path.display()))?;

    Ok(filename)
}

fn split_ai_response(text: &str) -> (String, String) {
    // Try to split on first blank line — first paragraph is summary, rest is details
    let trimmed = text.trim();
    if let Some(pos) = trimmed.find("\n\n") {
        let summary = trimmed[..pos].trim().to_string();
        let details = trimmed[pos..].trim().to_string();
        (summary, details)
    } else {
        (trimmed.to_string(), trimmed.to_string())
    }
}

fn resolve_output_dir_name(repo_root: &Path) -> String {
    let cfg = load_explain_config(repo_root);
    cfg.as_ref()
        .and_then(|c| c.output_dir.clone())
        .unwrap_or_else(|| DEFAULT_OUTPUT_DIR.to_string())
}

fn resolve_output_dir(repo_root: &Path) -> PathBuf {
    repo_root.join(resolve_output_dir_name(repo_root))
}

fn resolve_explain_target(_repo_root: &Path) -> (String, String) {
    // Kimi is enforced for commit explanations to keep output quality predictable.
    (DEFAULT_PROVIDER.to_string(), DEFAULT_MODEL.to_string())
}

fn short_sha_from_sha(sha: &str) -> String {
    sha[..7.min(sha.len())].to_string()
}

fn short_sha_from_filename(file: &str) -> String {
    // Filename convention: YYYY-MM-DD-<short_sha>-<slug>.md
    if file.len() > 11 {
        let rest = &file[11..];
        if let Some(short_sha) = rest.split('-').next() {
            return short_sha.trim().to_string();
        }
    }
    String::new()
}

fn extract_markdown_section(content: &str, heading: &str) -> String {
    let marker = format!("## {heading}");
    let Some(start) = content.find(&marker) else {
        return String::new();
    };
    let section_start = start + marker.len();
    let mut tail = &content[section_start..];
    if let Some(stripped) = tail.strip_prefix('\n') {
        tail = stripped;
    }
    if let Some(stripped) = tail.strip_prefix('\r') {
        tail = stripped;
    }
    if let Some(next) = tail.find("\n## ") {
        tail[..next].trim().to_string()
    } else {
        tail.trim().to_string()
    }
}

fn parse_markdown_metadata(content: &str) -> (String, String, String, String) {
    let subject = content
        .lines()
        .find_map(|line| line.strip_prefix("# ").map(str::trim))
        .unwrap_or_default()
        .to_string();
    let mut sha = String::new();
    let mut date = String::new();
    let mut author = String::new();
    if let Some(meta) = content.lines().find(|line| line.starts_with("**Commit**:")) {
        for part in meta.split('|').map(str::trim) {
            if part.starts_with("**Commit**:") {
                sha = part
                    .split('`')
                    .nth(1)
                    .unwrap_or_default()
                    .trim()
                    .to_string();
            } else if part.starts_with("**Date**:") {
                date = part.trim_start_matches("**Date**:").trim().to_string();
            } else if part.starts_with("**Author**:") {
                author = part.trim_start_matches("**Author**:").trim().to_string();
            }
        }
    }
    (subject, sha, date, author)
}

fn read_explained_commit(
    output_dir: &Path,
    short_sha_key: &str,
    entry: &CommitEntry,
) -> Result<Option<ExplainedCommit>> {
    let sidecar_file = entry.file.replacen(".md", ".json", 1);
    let sidecar_path = output_dir.join(&sidecar_file);
    if sidecar_path.exists() {
        let json = fs::read_to_string(&sidecar_path)
            .with_context(|| format!("failed to read {}", sidecar_path.display()))?;
        let mut commit: ExplainedCommit = serde_json::from_str(&json)
            .with_context(|| format!("failed to parse {}", sidecar_path.display()))?;
        if commit.markdown_file.is_empty() {
            commit.markdown_file = entry.file.clone();
        }
        if commit.generated_at.is_empty() {
            commit.generated_at = entry.at.clone();
        }
        if commit.short_sha.is_empty() {
            commit.short_sha = short_sha_key.to_string();
        }
        return Ok(Some(commit));
    }

    let markdown_path = output_dir.join(&entry.file);
    if !markdown_path.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(&markdown_path)
        .with_context(|| format!("failed to read {}", markdown_path.display()))?;
    let (subject, parsed_sha, parsed_date, parsed_author) = parse_markdown_metadata(&content);
    let summary = extract_markdown_section(&content, "Summary");
    let changes = extract_markdown_section(&content, "Changes");
    let files = extract_markdown_section(&content, "Files")
        .lines()
        .map(str::trim)
        .filter_map(|line| line.strip_prefix("- ").map(str::to_string))
        .collect::<Vec<_>>();

    let short_sha = if !short_sha_key.is_empty() {
        short_sha_key.to_string()
    } else if !entry.sha.is_empty() {
        short_sha_from_sha(&entry.sha)
    } else {
        short_sha_from_filename(&entry.file)
    };
    let fallback_sha = if !entry.sha.is_empty() {
        entry.sha.clone()
    } else if !parsed_sha.is_empty() {
        parsed_sha.clone()
    } else {
        short_sha.clone()
    };

    Ok(Some(ExplainedCommit {
        sha: fallback_sha,
        short_sha,
        subject,
        author: parsed_author,
        date: parsed_date,
        summary,
        changes,
        files,
        markdown_file: entry.file.clone(),
        generated_at: entry.at.clone(),
    }))
}

// -- Core functions --

fn load_explain_config(repo_root: &Path) -> Option<config::ExplainCommitsConfig> {
    let flow_toml = repo_root.join("flow.toml");
    if !flow_toml.exists() {
        return None;
    }
    let cfg = config::load_or_default(&flow_toml);
    cfg.explain_commits
}

fn maybe_register_project(repo_root: &Path) {
    let flow_toml = repo_root.join("flow.toml");
    if !flow_toml.exists() {
        return;
    }
    let cfg = config::load_or_default(&flow_toml);
    if let Some(name) = cfg.project_name.as_deref() {
        let _ = projects::register_project(name, &flow_toml);
    }
}

/// Read explained commits for a project, newest first.
pub fn list_explained_commits(
    repo_root: &Path,
    limit: Option<usize>,
) -> Result<Vec<ExplainedCommit>> {
    let output_dir = resolve_output_dir(repo_root);
    if !output_dir.exists() {
        return Ok(Vec::new());
    }

    let index = load_index(&output_dir);
    let mut indexed_entries = index.commits.into_iter().collect::<Vec<_>>();
    indexed_entries.sort_by(|(_, left), (_, right)| right.at.cmp(&left.at));

    let mut commits = Vec::new();
    for (short_sha_key, entry) in indexed_entries {
        if let Some(commit) = read_explained_commit(&output_dir, &short_sha_key, &entry)? {
            commits.push(commit);
            if let Some(max_items) = limit
                && commits.len() >= max_items
            {
                break;
            }
        }
    }

    Ok(commits)
}

/// Read one explained commit by SHA (full or prefix).
pub fn get_explained_commit(repo_root: &Path, sha: &str) -> Result<Option<ExplainedCommit>> {
    let trimmed = sha.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let commits = list_explained_commits(repo_root, None)?;
    let mut prefix_match: Option<ExplainedCommit> = None;
    for commit in commits {
        if commit.sha.eq_ignore_ascii_case(trimmed)
            || commit.short_sha.eq_ignore_ascii_case(trimmed)
        {
            return Ok(Some(commit));
        }
        if commit.sha.starts_with(trimmed) || commit.short_sha.starts_with(trimmed) {
            if prefix_match.is_none() {
                prefix_match = Some(commit);
            } else {
                // Ambiguous prefix.
                return Ok(None);
            }
        }
    }
    Ok(prefix_match)
}

/// Explain new commits since `head_before` (used by post-sync hook).
pub fn explain_new_commits_since(repo_root: &Path, head_before: &str) -> Result<()> {
    let head_after = git_capture_in(repo_root, &["rev-parse", "HEAD"])?
        .trim()
        .to_string();

    if head_before == head_after {
        return Ok(());
    }

    let cfg = load_explain_config(repo_root);
    let output_dir_name = resolve_output_dir_name(repo_root);
    let batch_size = cfg
        .as_ref()
        .and_then(|c| c.batch_size)
        .unwrap_or(DEFAULT_BATCH_SIZE);

    let output_dir = resolve_output_dir(repo_root);
    fs::create_dir_all(&output_dir)?;
    let (provider, model) = resolve_explain_target(repo_root);

    let commits = get_commits_in_range(repo_root, head_before, &head_after)?;
    if commits.is_empty() {
        return Ok(());
    }

    let to_process = if commits.len() > batch_size {
        println!(
            "  {} new commits, processing last {} (batch limit)",
            commits.len(),
            batch_size
        );
        &commits[commits.len() - batch_size..]
    } else {
        &commits
    };

    let mut index = load_index(&output_dir);
    let mut explained = 0;

    for sha in to_process {
        let info = match get_commit_info(repo_root, sha) {
            Ok(info) => info,
            Err(e) => {
                eprintln!("  warn: skipping {}: {e}", &sha[..7.min(sha.len())]);
                continue;
            }
        };

        let digest = compute_digest(&info.sha, &info.message, &info.diff);

        // Skip if already processed with same digest
        if let Some(entry) = index.commits.get(&info.short_sha)
            && entry.digest == digest
        {
            continue;
        }

        println!("  explaining {} {}", info.short_sha, info.subject);

        let explanation = match call_ai_explain(&info, &provider, &model) {
            Ok(text) => text,
            Err(e) => {
                eprintln!("  warn: AI failed for {}: {e}", info.short_sha);
                continue;
            }
        };

        let generated_at = Utc::now().to_rfc3339();
        let filename = write_commit_markdown(&output_dir, &info, &explanation, &generated_at)?;

        index.commits.insert(
            info.short_sha.clone(),
            CommitEntry {
                digest,
                file: filename,
                at: generated_at,
                sha: info.sha,
            },
        );
        explained += 1;
    }

    if explained > 0 {
        save_index(&output_dir, &index)?;
        println!("  explained {explained} commit(s) → {output_dir_name}/");
    }

    Ok(())
}

/// Explain last N commits (CLI entry point).
pub fn explain_last_n_commits(repo_root: &Path, n: usize, force: bool) -> Result<()> {
    let output_dir_name = resolve_output_dir_name(repo_root);
    let output_dir = resolve_output_dir(repo_root);
    fs::create_dir_all(&output_dir)?;
    let (provider, model) = resolve_explain_target(repo_root);
    println!("using provider={provider} model={model}");

    let commits = get_last_n_commits(repo_root, n)?;
    if commits.is_empty() {
        println!("No commits found.");
        return Ok(());
    }

    let mut index = load_index(&output_dir);
    let mut explained = 0;
    let mut skipped = 0;

    for sha in &commits {
        let info = match get_commit_info(repo_root, sha) {
            Ok(info) => info,
            Err(e) => {
                eprintln!("warn: skipping {}: {e}", &sha[..7.min(sha.len())]);
                continue;
            }
        };

        let digest = compute_digest(&info.sha, &info.message, &info.diff);

        // Skip if already processed with same digest (unless --force)
        if !force {
            if let Some(entry) = index.commits.get(&info.short_sha)
                && entry.digest == digest
            {
                skipped += 1;
                continue;
            }
        }

        println!("explaining {} {}", info.short_sha, info.subject);

        let explanation = match call_ai_explain(&info, &provider, &model) {
            Ok(text) => text,
            Err(e) => {
                eprintln!("warn: AI failed for {}: {e}", info.short_sha);
                continue;
            }
        };

        let generated_at = Utc::now().to_rfc3339();
        let filename = write_commit_markdown(&output_dir, &info, &explanation, &generated_at)?;

        index.commits.insert(
            info.short_sha.clone(),
            CommitEntry {
                digest,
                file: filename,
                at: generated_at,
                sha: info.sha,
            },
        );
        explained += 1;
    }

    save_index(&output_dir, &index)?;

    if explained > 0 {
        println!("explained {explained} commit(s) → {output_dir_name}/");
    }
    if skipped > 0 {
        println!("skipped {skipped} already-processed commit(s)");
    }
    if explained == 0 && skipped == 0 {
        println!("no commits to explain");
    }

    Ok(())
}

/// Called after sync — checks config and explains new commits. Non-fatal.
pub fn maybe_run_after_sync(repo_root: &Path, head_before: &str) -> Result<()> {
    maybe_register_project(repo_root);
    let cfg = load_explain_config(repo_root);
    let enabled = cfg.as_ref().and_then(|c| c.enabled).unwrap_or(false);
    if !enabled {
        return Ok(());
    }
    explain_new_commits_since(repo_root, head_before)
}

/// CLI entry point for `f explain-commits`.
pub fn run_cli(cmd: ExplainCommitsCommand) -> Result<()> {
    let repo_root = std::env::current_dir()?;

    // Verify we're in a git repo
    git_capture_in(&repo_root, &["rev-parse", "--git-dir"])?;
    maybe_register_project(&repo_root);

    let n = cmd.count.unwrap_or(1);
    explain_last_n_commits(&repo_root, n, cmd.force)
}
