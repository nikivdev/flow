use std::{
    fs,
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    cli::{
        FailureAction, FailureCommand, FailureCopyFormat, FailureCopyOpts, FailureLastOpts,
        FailureListOpts,
    },
    secret_redact, setup,
};

const DEFAULT_OUTPUT_MAX_CHARS: usize = 20_000;
const PROMPT_MAX_LINES: usize = 80;
const PROMPT_MAX_CHARS: usize = 12_000;
const EXCERPT_MAX_LINES: usize = 40;
const EXCERPT_MAX_CHARS: usize = 4_000;
const GIT_STATUS_MAX_LINES: usize = 40;
const GIT_STATUS_MAX_CHARS: usize = 4_000;
const GIT_DIFF_STAT_MAX_LINES: usize = 40;
const GIT_DIFF_STAT_MAX_CHARS: usize = 4_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureRecord {
    pub task: String,
    pub command: String,
    pub workdir: String,
    pub config: String,
    pub project: Option<String>,
    pub status: i32,
    pub output: String,
    #[serde(default)]
    pub fishx: bool,
    pub ts: u64,
}

#[derive(Debug, Clone)]
struct FailureEntry {
    id: String,
    path: PathBuf,
    record: FailureRecord,
}

#[derive(Debug, Clone, Serialize)]
struct FailureSummary {
    id: String,
    path: String,
    task: String,
    project: Option<String>,
    status: i32,
    ts: u64,
    workdir: String,
    config: String,
}

impl FailureSummary {
    fn from_entry(entry: &FailureEntry) -> Self {
        Self {
            id: entry.id.clone(),
            path: entry.path.display().to_string(),
            task: entry.record.task.clone(),
            project: entry.record.project.clone(),
            status: entry.record.status,
            ts: entry.record.ts,
            workdir: entry.record.workdir.clone(),
            config: entry.record.config.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum FailurePromptTool {
    Codex,
    Claude,
}

impl FailurePromptTool {
    fn display_name(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude",
        }
    }

    fn completion_instruction(self) -> &'static str {
        match self {
            Self::Codex => {
                "Provide the smallest safe fix first, then summarize verification and any remaining blockers."
            }
            Self::Claude => {
                "Explain the likely root cause briefly, propose the safest fix, and note how to validate it."
            }
        }
    }
}

pub fn run_cli(cmd: FailureCommand) -> Result<()> {
    match cmd
        .action
        .unwrap_or(FailureAction::Last(FailureLastOpts { json: false }))
    {
        FailureAction::Last(opts) => run_last(opts),
        FailureAction::List(opts) => run_list(opts),
        FailureAction::Copy(opts) => run_copy(opts),
    }
}

pub fn latest_failure_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("FISHX_FAILURE_PATH") {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    if let Ok(path) = std::env::var("FLOW_FAILURE_BUNDLE_PATH") {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    dirs::cache_dir().map(|dir| dir.join("flow").join("last-task-failure.json"))
}

pub fn record_task_failure(
    task_name: &str,
    command: &str,
    workdir: &Path,
    config_path: &Path,
    project_name: Option<&str>,
    output: &str,
    status: Option<i32>,
    fishx: bool,
) {
    let Some(latest_path) = latest_failure_path() else {
        return;
    };

    let record = FailureRecord {
        task: task_name.to_string(),
        command: secret_redact::redact_text(command),
        workdir: workdir.display().to_string(),
        config: config_path.display().to_string(),
        project: project_name.map(ToOwned::to_owned),
        status: status.unwrap_or(-1),
        output: secret_redact::redact_text(&truncate_for_bundle(output, DEFAULT_OUTPUT_MAX_CHARS)),
        fishx,
        ts: now_ms(),
    };

    let rendered = match serde_json::to_string_pretty(&record) {
        Ok(rendered) => rendered,
        Err(err) => {
            tracing::warn!(?err, "failed to serialize task failure record");
            return;
        }
    };

    write_record(&latest_path, &rendered);

    let history_path = history_dir_for(&latest_path)
        .map(|dir| unique_history_path(&dir, &record))
        .and_then(|path| write_record(&path, &rendered).then_some(path));

    if std::io::stdin().is_terminal() {
        eprintln!("🧩 failure bundle: {}", latest_path.display());
        if let Some(path) = history_path {
            eprintln!("   History: {}", path.display());
        }
        eprintln!("   Tip: run `f failure copy` to copy the latest repair prompt.");
    }
}

fn run_last(opts: FailureLastOpts) -> Result<()> {
    let entry = latest_entry()?;
    if opts.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "id": entry.id,
                "path": entry.path.display().to_string(),
                "task": entry.record.task,
                "command": entry.record.command,
                "workdir": entry.record.workdir,
                "config": entry.record.config,
                "project": entry.record.project,
                "status": entry.record.status,
                "output": entry.record.output,
                "fishx": entry.record.fishx,
                "ts": entry.record.ts,
            }))
            .context("failed to encode failure JSON")?
        );
        return Ok(());
    }

    println!("{}", render_last(&entry));
    Ok(())
}

fn run_list(opts: FailureListOpts) -> Result<()> {
    let limit = opts.limit.max(1);
    let entries = recent_entries(limit)?;
    if opts.json {
        let summaries: Vec<FailureSummary> =
            entries.iter().map(FailureSummary::from_entry).collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&summaries)
                .context("failed to encode failure list JSON")?
        );
        return Ok(());
    }

    if entries.is_empty() {
        println!("No task failures recorded yet.");
        return Ok(());
    }

    for entry in entries {
        println!(
            "{}  {}  exit {}  {}",
            entry.id,
            task_label(&entry.record),
            entry.record.status,
            entry.record.workdir
        );
    }
    Ok(())
}

fn run_copy(opts: FailureCopyOpts) -> Result<()> {
    let entry = resolve_entry(opts.id.as_deref())?;
    let payload = match opts.format {
        FailureCopyFormat::Prompt => render_prompt(&entry),
        FailureCopyFormat::Excerpt => render_excerpt(&entry),
        FailureCopyFormat::Codex => render_ai_prompt(&entry, FailurePromptTool::Codex),
        FailureCopyFormat::Claude => render_ai_prompt(&entry, FailurePromptTool::Claude),
        FailureCopyFormat::Json => serde_json::to_string_pretty(&json!({
            "id": entry.id,
            "path": entry.path.display().to_string(),
            "task": entry.record.task,
            "command": entry.record.command,
            "workdir": entry.record.workdir,
            "config": entry.record.config,
            "project": entry.record.project,
            "status": entry.record.status,
            "output": entry.record.output,
            "fishx": entry.record.fishx,
            "ts": entry.record.ts,
        }))
        .context("failed to encode failure JSON")?,
    };

    let written_path = if opts.write_repo {
        Some(write_repo_payload(&entry, opts.format, &payload)?)
    } else {
        None
    };

    match copy_to_clipboard(&payload)? {
        true => match written_path.as_ref() {
            Some(path) => println!(
                "Copied {} failure {} to clipboard and wrote {}",
                format_name(opts.format),
                entry.id,
                path.display()
            ),
            None => println!(
                "Copied {} failure {} to clipboard",
                format_name(opts.format),
                entry.id
            ),
        },
        false => match written_path.as_ref() {
            Some(path) => println!(
                "Clipboard disabled by FLOW_NO_CLIPBOARD; wrote {} instead.",
                path.display()
            ),
            None => println!("Clipboard disabled by FLOW_NO_CLIPBOARD; skipped copy."),
        },
    }
    Ok(())
}

fn recent_entries(limit: usize) -> Result<Vec<FailureEntry>> {
    let Some(latest_path) = latest_failure_path() else {
        return Ok(Vec::new());
    };

    let mut entries = Vec::new();
    if let Some(history_dir) = history_dir_for(&latest_path)
        && history_dir.is_dir()
    {
        for entry in fs::read_dir(&history_dir)
            .with_context(|| format!("failed to read {}", history_dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            if let Ok(loaded) = load_entry_from_path(&path) {
                entries.push(loaded);
            }
        }
    }

    if entries.is_empty() && latest_path.is_file() {
        entries.push(load_entry_from_path(&latest_path)?);
    }

    entries.sort_by(|a, b| b.record.ts.cmp(&a.record.ts).then_with(|| b.id.cmp(&a.id)));
    if entries.len() > limit {
        entries.truncate(limit);
    }
    Ok(entries)
}

fn latest_entry() -> Result<FailureEntry> {
    recent_entries(1)?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("No task failures recorded yet."))
}

fn resolve_entry(id: Option<&str>) -> Result<FailureEntry> {
    let Some(id) = id else {
        return latest_entry();
    };

    let path_candidate = PathBuf::from(id);
    if (path_candidate.is_absolute() || id.contains(std::path::MAIN_SEPARATOR))
        && path_candidate.exists()
    {
        return load_entry_from_path(&path_candidate);
    }

    let limit = 200;
    recent_entries(limit)?
        .into_iter()
        .find(|entry| {
            entry.id == id
                || entry.path.file_name().and_then(|name| name.to_str()) == Some(id)
                || entry.path.display().to_string() == id
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Failure '{}' not found. Run `f failure list` to inspect recent ids.",
                id
            )
        })
}

fn load_entry_from_path(path: &Path) -> Result<FailureEntry> {
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let record: FailureRecord = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(FailureEntry {
        id: path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("failure")
            .to_string(),
        path: path.to_path_buf(),
        record,
    })
}

fn render_last(entry: &FailureEntry) -> String {
    let mut out = String::new();
    out.push_str(&format!("Task failure: {}\n", task_label(&entry.record)));
    out.push_str(&format!("Status: {}\n", entry.record.status));
    out.push_str(&format!("Workdir: {}\n", entry.record.workdir));
    out.push_str(&format!("Command: {}\n", entry.record.command));
    out.push_str(&format!("Bundle: {}\n", entry.path.display()));
    out.push_str("Flow-native prompt copy:\n");
    out.push_str("- Codex: f failure copy --format codex\n");
    out.push_str("- Claude: f failure copy --format claude\n");
    if let Some(path) = proxy_summary_path() {
        out.push_str(&format!("Proxy trace summary: {}\n", path.display()));
    }
    let excerpt = compact_output_tail(&entry.record.output, EXCERPT_MAX_LINES, EXCERPT_MAX_CHARS);
    if !excerpt.is_empty() {
        out.push_str("\nRecent output:\n");
        out.push_str(&excerpt);
        if !excerpt.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

fn render_prompt(entry: &FailureEntry) -> String {
    let mut out = String::new();
    out.push_str(&format!("Task failure in {}\n", task_label(&entry.record)));
    out.push_str(&format!("Workdir: {}\n", entry.record.workdir));
    out.push_str(&format!("Command: {}\n", entry.record.command));
    out.push_str(&format!("Exit status: {}\n", entry.record.status));
    out.push('\n');

    let recent = compact_output_tail(&entry.record.output, PROMPT_MAX_LINES, PROMPT_MAX_CHARS);
    if !recent.is_empty() {
        out.push_str("Recent output:\n");
        out.push_str(&recent);
        if !recent.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }

    out.push_str("Artifacts:\n");
    out.push_str(&format!("- Failure bundle: {}\n", entry.path.display()));
    out.push_str("- Codex prompt: f failure copy --format codex\n");
    out.push_str("- Claude prompt: f failure copy --format claude\n");
    if let Some(path) = proxy_summary_path() {
        out.push_str(&format!("- Proxy trace summary: {}\n", path.display()));
    }
    out
}

fn render_ai_prompt(entry: &FailureEntry, tool: FailurePromptTool) -> String {
    let mut out = String::new();
    out.push_str(&format!("# {} Failure Prompt\n\n", tool.display_name()));
    out.push_str("## Task\n");
    out.push_str(&format!(
        "Fix the `{}` task failure in `{}`.\n\n",
        task_label(&entry.record),
        entry.record.workdir
    ));

    out.push_str("## Failure Context\n");
    out.push_str(&format!("- Task: {}\n", task_label(&entry.record)));
    if let Some(project) = entry.record.project.as_deref()
        && !project.trim().is_empty()
    {
        out.push_str(&format!("- Project: {}\n", project));
    }
    out.push_str(&format!("- Workdir: {}\n", entry.record.workdir));
    out.push_str(&format!("- Config: {}\n", entry.record.config));
    out.push_str(&format!("- Command: {}\n", entry.record.command));
    out.push_str(&format!("- Exit status: {}\n", entry.record.status));
    out.push_str(&format!("- Failure bundle: {}\n", entry.path.display()));
    if let Some(path) = proxy_summary_path() {
        out.push_str(&format!("- Proxy trace summary: {}\n", path.display()));
    }

    let workdir = Path::new(&entry.record.workdir);
    if let Some(repo_root) = git_root_for(workdir) {
        out.push('\n');
        out.push_str("## Repo State\n");
        out.push_str(&format!("- Git root: {}\n", repo_root.display()));

        if let Some(status) = git_status_summary(&repo_root) {
            out.push_str("\n### Git Status\n```text\n");
            out.push_str(&status);
            if !status.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("```\n");
        }

        if let Some(diff_stat) = git_diff_stat_summary(&repo_root) {
            out.push_str("\n### Git Diff Stat\n```text\n");
            out.push_str(&diff_stat);
            if !diff_stat.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("```\n");
        }
    }

    let recent = compact_output_tail(&entry.record.output, PROMPT_MAX_LINES, PROMPT_MAX_CHARS);
    if !recent.is_empty() {
        out.push('\n');
        out.push_str("## Recent Output\n```text\n");
        out.push_str(&recent);
        if !recent.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("```\n");
    }

    out.push('\n');
    out.push_str("## Requested Output\n");
    out.push_str(tool.completion_instruction());
    out.push('\n');
    out
}

fn render_excerpt(entry: &FailureEntry) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Task failure: {} (exit {})\n",
        task_label(&entry.record),
        entry.record.status
    ));
    out.push_str(&format!("Workdir: {}\n", entry.record.workdir));
    out.push_str(&format!("Command: {}\n", entry.record.command));
    out.push_str(&format!("Bundle: {}\n", entry.path.display()));
    let excerpt = compact_output_tail(&entry.record.output, EXCERPT_MAX_LINES, EXCERPT_MAX_CHARS);
    if !excerpt.is_empty() {
        out.push('\n');
        out.push_str(&excerpt);
        if !excerpt.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

fn compact_output_tail(output: &str, max_lines: usize, max_chars: usize) -> String {
    let mut lines: Vec<&str> = output.lines().collect();
    if lines.len() > max_lines {
        lines = lines[lines.len().saturating_sub(max_lines)..].to_vec();
    }
    let mut joined = lines.join("\n");
    if joined.len() > max_chars {
        let start = joined.len().saturating_sub(max_chars);
        joined = format!("...{}", &joined[start..]);
    }
    joined
}

fn truncate_for_bundle(output: &str, max_chars: usize) -> String {
    if output.len() <= max_chars {
        return output.to_string();
    }
    let start = output.len().saturating_sub(max_chars);
    format!("...{}", &output[start..])
}

fn task_label(record: &FailureRecord) -> String {
    match record.project.as_deref() {
        Some(project) if !project.trim().is_empty() => format!("{project}/{}", record.task),
        _ => record.task.clone(),
    }
}

fn truncate_output_head(output: &str, max_lines: usize, max_chars: usize) -> String {
    let mut lines: Vec<&str> = output.lines().collect();
    if lines.len() > max_lines {
        lines.truncate(max_lines);
    }
    let mut joined = lines.join("\n");
    if joined.len() > max_chars {
        joined = format!("{}...", truncate_utf8_prefix(&joined, max_chars));
    }
    joined
}

fn truncate_utf8_prefix(value: &str, max_chars: usize) -> &str {
    if value.chars().count() <= max_chars {
        return value;
    }
    let end = value
        .char_indices()
        .nth(max_chars)
        .map(|(idx, _)| idx)
        .unwrap_or(value.len());
    &value[..end]
}

fn git_root_for(workdir: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workdir)
        .args(["rev-parse", "--show-toplevel"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if root.is_empty() {
        None
    } else {
        Some(PathBuf::from(root))
    }
}

fn repo_failure_dir(entry: &FailureEntry) -> PathBuf {
    let workdir = Path::new(&entry.record.workdir);
    let base = git_root_for(workdir).unwrap_or_else(|| workdir.to_path_buf());
    base.join(".ai").join("internal").join("failures")
}

fn repo_failure_filename(format: FailureCopyFormat) -> &'static str {
    match format {
        FailureCopyFormat::Prompt => "latest-prompt.md",
        FailureCopyFormat::Excerpt => "latest-excerpt.txt",
        FailureCopyFormat::Codex => "latest-codex.md",
        FailureCopyFormat::Claude => "latest-claude.md",
        FailureCopyFormat::Json => "latest-failure.json",
    }
}

fn write_repo_payload(
    entry: &FailureEntry,
    format: FailureCopyFormat,
    payload: &str,
) -> Result<PathBuf> {
    let dir = repo_failure_dir(entry);
    let repo_root = dir
        .ancestors()
        .nth(3)
        .context("failed to derive repo root for failure artifact")?;
    let _ = setup::add_gitignore_entry(repo_root, ".ai/internal/");
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let path = dir.join(repo_failure_filename(format));
    fs::write(&path, payload).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path)
}

fn git_capture(repo_root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

fn git_status_summary(repo_root: &Path) -> Option<String> {
    let status = git_capture(repo_root, &["status", "--short", "--branch"])?;
    Some(truncate_output_head(
        &status,
        GIT_STATUS_MAX_LINES,
        GIT_STATUS_MAX_CHARS,
    ))
}

fn git_diff_stat_summary(repo_root: &Path) -> Option<String> {
    let diff_stat = git_capture(repo_root, &["diff", "--stat", "--compact-summary", "HEAD"])?;
    Some(truncate_output_head(
        &diff_stat,
        GIT_DIFF_STAT_MAX_LINES,
        GIT_DIFF_STAT_MAX_CHARS,
    ))
}

fn proxy_summary_path() -> Option<PathBuf> {
    let path = dirs::config_dir()?
        .join("flow")
        .join("proxy")
        .join("trace-summary.json");
    path.is_file().then_some(path)
}

fn history_dir_for(latest_path: &Path) -> Option<PathBuf> {
    Some(latest_path.parent()?.join("task-failures"))
}

fn unique_history_path(dir: &Path, record: &FailureRecord) -> PathBuf {
    let project = slug_component(record.project.as_deref().unwrap_or("unknown"));
    let task = slug_component(&record.task);
    let base = format!("{}-{}-{}", record.ts, project, task);
    let mut path = dir.join(format!("{base}.json"));
    let mut idx = 1usize;
    while path.exists() {
        path = dir.join(format!("{base}-{idx}.json"));
        idx += 1;
    }
    path
}

fn slug_component(value: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in value.chars() {
        let mapped = if ch.is_ascii_alphanumeric() {
            last_dash = false;
            ch.to_ascii_lowercase()
        } else {
            if last_dash {
                continue;
            }
            last_dash = true;
            '-'
        };
        out.push(mapped);
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "unknown".to_string()
    } else {
        trimmed.to_string()
    }
}

fn write_record(path: &Path, rendered: &str) -> bool {
    if let Some(parent) = path.parent()
        && let Err(err) = fs::create_dir_all(parent)
    {
        tracing::warn!(?err, path = %path.display(), "failed to create task failure directory");
        return false;
    }
    if let Err(err) = fs::write(path, rendered.as_bytes()) {
        tracing::warn!(?err, path = %path.display(), "failed to write task failure record");
        return false;
    }
    true
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn format_name(format: FailureCopyFormat) -> &'static str {
    match format {
        FailureCopyFormat::Prompt => "prompt",
        FailureCopyFormat::Excerpt => "excerpt",
        FailureCopyFormat::Codex => "Codex prompt",
        FailureCopyFormat::Claude => "Claude prompt",
        FailureCopyFormat::Json => "JSON",
    }
}

fn copy_to_clipboard(text: &str) -> Result<bool> {
    if std::env::var("FLOW_NO_CLIPBOARD").is_ok() {
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

        let status = child.wait()?;
        if !status.success() {
            bail!("pbcopy exited with status {}", status);
        }
    }

    #[cfg(target_os = "linux")]
    {
        let result = Command::new("xclip")
            .arg("-selection")
            .arg("clipboard")
            .stdin(Stdio::piped())
            .spawn();

        let mut child = match result {
            Ok(child) => child,
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

        let status = child.wait()?;
        if !status.success() {
            bail!("clipboard command exited with status {}", status);
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        bail!("clipboard not supported on this platform");
    }

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::{
        FailureCopyFormat, FailureEntry, FailurePromptTool, FailureRecord, compact_output_tail,
        render_ai_prompt, render_excerpt, render_prompt, repo_failure_dir, repo_failure_filename,
        slug_component, unique_history_path,
    };
    use std::path::{Path, PathBuf};

    #[test]
    fn slug_component_normalizes_names() {
        assert_eq!(slug_component("Designer Hot"), "designer-hot");
        assert_eq!(slug_component("foo/bar_baz"), "foo-bar-baz");
        assert_eq!(slug_component("   "), "unknown");
    }

    #[test]
    fn compact_output_tail_limits_lines_and_chars() {
        let output = "one\ntwo\nthree\nfour\nfive";
        let trimmed = compact_output_tail(output, 3, 32);
        assert!(trimmed.contains("three"));
        assert!(trimmed.contains("five"));
        assert!(!trimmed.contains("one"));

        let char_trimmed = compact_output_tail(output, 5, 8);
        assert!(char_trimmed.starts_with("..."));
        assert!(char_trimmed.len() <= 11);
    }

    #[test]
    fn unique_history_path_uses_project_and_task() {
        let dir = tempfile::tempdir().expect("tempdir");
        let record = FailureRecord {
            task: "hot".to_string(),
            command: "npm run hot".to_string(),
            workdir: "/tmp/designer".to_string(),
            config: "/tmp/designer/flow.toml".to_string(),
            project: Some("designer".to_string()),
            status: 1,
            output: "boom".to_string(),
            fishx: false,
            ts: 123,
        };
        let path = unique_history_path(dir.path(), &record);
        assert_eq!(
            path.strip_prefix(dir.path()).expect("relative"),
            Path::new("123-designer-hot.json")
        );
    }

    #[test]
    fn renderers_include_bundle_path_and_status() {
        let entry = FailureEntry {
            id: "123-designer-hot".to_string(),
            path: PathBuf::from("/tmp/123-designer-hot.json"),
            record: FailureRecord {
                task: "hot".to_string(),
                command: "npm run hot".to_string(),
                workdir: "/tmp/designer".to_string(),
                config: "/tmp/designer/flow.toml".to_string(),
                project: Some("designer".to_string()),
                status: 1,
                output: "line1\nline2".to_string(),
                fishx: false,
                ts: 123,
            },
        };

        let prompt = render_prompt(&entry);
        assert!(prompt.contains("Task failure in designer/hot"));
        assert!(prompt.contains("Failure bundle: /tmp/123-designer-hot.json"));

        let excerpt = render_excerpt(&entry);
        assert!(excerpt.contains("exit 1"));
        assert!(excerpt.contains("/tmp/123-designer-hot.json"));
    }

    #[test]
    fn ai_prompt_renderer_includes_flow_native_context() {
        let entry = FailureEntry {
            id: "123-designer-hot".to_string(),
            path: PathBuf::from("/tmp/123-designer-hot.json"),
            record: FailureRecord {
                task: "hot".to_string(),
                command: "npm run hot".to_string(),
                workdir: "/tmp/designer".to_string(),
                config: "/tmp/designer/flow.toml".to_string(),
                project: Some("designer".to_string()),
                status: 1,
                output: "line1\nline2".to_string(),
                fishx: false,
                ts: 123,
            },
        };

        let prompt = render_ai_prompt(&entry, FailurePromptTool::Codex);
        assert!(prompt.contains("# Codex Failure Prompt"));
        assert!(prompt.contains("Fix the `designer/hot` task failure"));
        assert!(prompt.contains("Failure bundle: /tmp/123-designer-hot.json"));
        assert!(prompt.contains("## Recent Output"));
        assert!(prompt.contains("Provide the smallest safe fix first"));
    }

    #[test]
    fn repo_failure_artifacts_live_under_ai_internal_failures() {
        let entry = FailureEntry {
            id: "123-designer-hot".to_string(),
            path: PathBuf::from("/tmp/123-designer-hot.json"),
            record: FailureRecord {
                task: "hot".to_string(),
                command: "npm run hot".to_string(),
                workdir: "/tmp/designer".to_string(),
                config: "/tmp/designer/flow.toml".to_string(),
                project: Some("designer".to_string()),
                status: 1,
                output: "line1\nline2".to_string(),
                fishx: false,
                ts: 123,
            },
        };

        assert_eq!(
            repo_failure_dir(&entry),
            PathBuf::from("/tmp/designer/.ai/internal/failures")
        );
        assert_eq!(
            repo_failure_filename(FailureCopyFormat::Codex),
            "latest-codex.md"
        );
        assert_eq!(
            repo_failure_filename(FailureCopyFormat::Json),
            "latest-failure.json"
        );
    }
}
