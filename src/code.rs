use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::cli::{CodeAction, CodeCommand, CodeMigrateOpts, CodeMoveSessionsOpts};
use crate::config;

const DEFAULT_CODE_ROOT: &str = "~/code";

pub fn run(cmd: CodeCommand) -> Result<()> {
    match cmd.action {
        Some(CodeAction::List) => list_code(&cmd.root),
        Some(CodeAction::Migrate(opts)) => migrate_project(opts, &cmd.root),
        Some(CodeAction::MoveSessions(opts)) => move_sessions(opts),
        None => fuzzy_select_code(&cmd.root),
    }
}

fn list_code(root: &str) -> Result<()> {
    let root = normalize_root(root)?;
    if !root.exists() {
        println!("No code directory found at {}", root.display());
        return Ok(());
    }

    let repos = discover_code_repos(&root)?;
    if repos.is_empty() {
        println!("No git repositories found in {}", root.display());
        return Ok(());
    }

    println!("Available repositories:");
    for repo in &repos {
        println!("  {}", repo.display);
    }
    Ok(())
}

fn fuzzy_select_code(root: &str) -> Result<()> {
    let root = normalize_root(root)?;
    if !root.exists() {
        println!("No code directory found at {}", root.display());
        return Ok(());
    }

    let repos = discover_code_repos(&root)?;
    if repos.is_empty() {
        println!("No git repositories found in {}", root.display());
        return Ok(());
    }

    if which::which("fzf").is_err() {
        println!("fzf not found on PATH – install it to use fuzzy selection.");
        println!("Available repositories:");
        for repo in &repos {
            println!("  {}", repo.display);
        }
        return Ok(());
    }

    if let Some(selected) = run_fzf(&repos)? {
        open_in_zed(&selected.path)?;
    }

    Ok(())
}

fn normalize_root(root: &str) -> Result<PathBuf> {
    let trimmed = root.trim();
    let expanded = if trimmed.is_empty() {
        config::expand_path(DEFAULT_CODE_ROOT)
    } else {
        config::expand_path(trimmed)
    };
    Ok(expanded)
}

struct CodeEntry {
    display: String,
    path: PathBuf,
}

fn discover_code_repos(root: &Path) -> Result<Vec<CodeEntry>> {
    let mut repos = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if !file_type.is_dir() {
                continue;
            }

            let name = entry.file_name().to_string_lossy().to_string();
            if should_skip_dir(&name) {
                continue;
            }

            let git_dir = path.join(".git");
            if git_dir.is_dir() || git_dir.is_file() {
                let display = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string();
                let key = path.to_string_lossy().to_string();
                if seen.insert(key) {
                    repos.push(CodeEntry { display, path });
                }
                continue;
            }

            stack.push(path);
        }
    }

    repos.sort_by(|a, b| a.display.cmp(&b.display));
    Ok(repos)
}

fn should_skip_dir(name: &str) -> bool {
    if name.starts_with('.') {
        return true;
    }
    matches!(
        name,
        "node_modules"
            | "target"
            | "dist"
            | "build"
            | ".git"
            | ".hg"
            | ".svn"
            | "__pycache__"
            | ".pytest_cache"
            | ".mypy_cache"
            | "venv"
            | ".venv"
            | "vendor"
            | "Pods"
            | ".cargo"
            | ".rustup"
            | ".next"
            | ".turbo"
            | ".cache"
    )
}

fn run_fzf(entries: &[CodeEntry]) -> Result<Option<&CodeEntry>> {
    let mut child = Command::new("fzf")
        .arg("--prompt")
        .arg("code> ")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("failed to spawn fzf")?;

    {
        let stdin = child.stdin.as_mut().context("failed to open fzf stdin")?;
        for entry in entries {
            writeln!(stdin, "{}", entry.display)?;
        }
    }

    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Ok(None);
    }

    let selection = String::from_utf8(output.stdout).context("fzf output was not valid UTF-8")?;
    let selection = selection.trim();
    if selection.is_empty() {
        return Ok(None);
    }

    Ok(entries.iter().find(|e| e.display == selection))
}

fn open_in_zed(path: &Path) -> Result<()> {
    Command::new("open")
        .args(["-a", "/Applications/Zed.app"])
        .arg(path)
        .status()
        .context("failed to open Zed")?;
    Ok(())
}

fn migrate_project(opts: CodeMigrateOpts, root: &str) -> Result<()> {
    let root = normalize_root(root)?;
    let from = normalize_path(&opts.from)?;
    let relative = normalize_relative_path(&opts.relative)?;
    let target = root.join(&relative);
    let target_display = target.display().to_string();

    if from == target {
        bail!("Source and destination are the same path.");
    }
    if !from.exists() {
        bail!("Source folder does not exist: {}", from.display());
    }
    if !from.is_dir() {
        bail!("Source path is not a directory: {}", from.display());
    }
    if target.exists() {
        bail!("Destination already exists: {}", target.display());
    }
    if target.starts_with(&from) {
        bail!("Destination cannot be inside the source folder.");
    }

    if !root.exists() {
        if opts.dry_run {
            println!("Would create {}", root.display());
        } else {
            fs::create_dir_all(&root)
                .with_context(|| format!("failed to create {}", root.display()))?;
        }
    }
    if let Some(parent) = target.parent() {
        if !parent.exists() {
            if opts.dry_run {
                println!("Would create {}", parent.display());
            } else {
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }
        }
    }

    if opts.dry_run {
        println!("Would move {} -> {}", from.display(), target_display);
    } else {
        move_dir(&from, &target)?;
        println!("Moved {} -> {}", from.display(), target_display);
    }

    let session_opts = CodeMoveSessionsOpts {
        from: from.to_string_lossy().to_string(),
        to: target.to_string_lossy().to_string(),
        dry_run: opts.dry_run,
        skip_claude: opts.skip_claude,
        skip_codex: opts.skip_codex,
    };
    move_sessions(session_opts)
        .with_context(|| format!("moved to {}, but session migration failed", target_display))?;

    Ok(())
}

fn move_sessions(opts: CodeMoveSessionsOpts) -> Result<()> {
    let from = normalize_path(&opts.from)?;
    let to = normalize_path(&opts.to)?;

    if from == to {
        bail!("Source and destination are the same path.");
    }

    let mut moved_claude = 0;
    let mut moved_codex = 0;
    let mut updated_codex_files = 0;

    if !opts.skip_claude {
        moved_claude = move_project_dir(&claude_projects_dir(), &from, &to, opts.dry_run)?;
    }
    if !opts.skip_codex {
        moved_codex = move_project_dir(&codex_projects_dir(), &from, &to, opts.dry_run)?;
        updated_codex_files = update_codex_sessions(&from, &to, opts.dry_run)?;
    }

    println!("Session migration summary:");
    println!("  Claude project dirs moved: {}", moved_claude);
    println!("  Codex legacy dirs moved: {}", moved_codex);
    println!("  Codex jsonl files updated: {}", updated_codex_files);
    if opts.dry_run {
        println!("Dry run only; no files were changed.");
    }

    Ok(())
}

fn normalize_path(path: &str) -> Result<PathBuf> {
    let expanded = config::expand_path(path);
    let canonical = expanded.canonicalize().unwrap_or(expanded);
    Ok(canonical)
}

fn normalize_relative_path(path: &str) -> Result<PathBuf> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        bail!("Relative path cannot be empty.");
    }
    let rel = PathBuf::from(trimmed);
    if rel.is_absolute() {
        bail!("Relative path must not be absolute.");
    }
    for component in rel.components() {
        if matches!(component, std::path::Component::ParentDir) {
            bail!("Relative path must not contain '..'.");
        }
    }
    Ok(rel)
}

fn move_dir(from: &Path, to: &Path) -> Result<()> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(err) => {
            if is_cross_device(&err) {
                copy_dir_all(from, to)?;
                fs::remove_dir_all(from)
                    .with_context(|| format!("failed to remove {}", from.display()))?;
                Ok(())
            } else {
                Err(err).with_context(|| {
                    format!("failed to move {} to {}", from.display(), to.display())
                })
            }
        }
    }
}

fn is_cross_device(err: &std::io::Error) -> bool {
    #[cfg(unix)]
    {
        err.raw_os_error() == Some(libc::EXDEV)
    }
    #[cfg(not(unix))]
    {
        let _ = err;
        false
    }
}

fn copy_dir_all(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to).with_context(|| format!("failed to create {}", to.display()))?;
    for entry in fs::read_dir(from).with_context(|| format!("failed to read {}", from.display()))? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        let target = to.join(entry.file_name());

        if target.exists() {
            bail!("Refusing to overwrite {}", target.display());
        }

        if file_type.is_dir() {
            copy_dir_all(&path, &target)?;
        } else if file_type.is_file() {
            fs::copy(&path, &target)
                .with_context(|| format!("failed to copy {}", path.display()))?;
        } else if file_type.is_symlink() {
            let link_target = fs::read_link(&path)
                .with_context(|| format!("failed to read link {}", path.display()))?;
            copy_symlink(&link_target, &target)?;
        }
    }
    Ok(())
}

fn copy_symlink(target: &Path, dest: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, dest)
            .with_context(|| format!("failed to create symlink {}", dest.display()))?;
        return Ok(());
    }
    #[cfg(not(unix))]
    {
        let metadata = fs::metadata(target)
            .with_context(|| format!("failed to read {}", target.display()))?;
        if metadata.is_dir() {
            copy_dir_all(target, dest)?;
        } else {
            fs::copy(target, dest)
                .with_context(|| format!("failed to copy {}", target.display()))?;
        }
        Ok(())
    }
}

fn claude_projects_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("projects")
}

fn codex_projects_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codex")
        .join("projects")
}

fn codex_sessions_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codex")
        .join("sessions")
}

fn move_project_dir(base: &Path, from: &Path, to: &Path, dry_run: bool) -> Result<usize> {
    if !base.exists() {
        return Ok(0);
    }

    let from_name = path_to_project_name(from);
    let to_name = path_to_project_name(to);
    let from_dir = base.join(&from_name);
    let to_dir = base.join(&to_name);

    if !from_dir.exists() {
        return Ok(0);
    }
    if to_dir.exists() {
        println!("Skip: {} already exists", to_dir.display());
        return Ok(0);
    }

    if dry_run {
        println!("Would move {} -> {}", from_dir.display(), to_dir.display());
    } else {
        if let Some(parent) = to_dir.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(&from_dir, &to_dir).with_context(|| {
            format!(
                "failed to move {} to {}",
                from_dir.display(),
                to_dir.display()
            )
        })?;
    }

    Ok(1)
}

fn path_to_project_name(path: &Path) -> String {
    path.to_string_lossy().replace('/', "-")
}

fn update_codex_sessions(from: &Path, to: &Path, dry_run: bool) -> Result<usize> {
    let root = codex_sessions_dir();
    if !root.exists() {
        return Ok(0);
    }

    let from_str = from.to_string_lossy().to_string();
    let to_str = to.to_string_lossy().to_string();
    let mut updated_files = 0;

    for file_path in collect_codex_session_files(&root) {
        if update_codex_session_file(&file_path, &from_str, &to_str, dry_run)? {
            updated_files += 1;
        }
    }

    Ok(updated_files)
}

fn collect_codex_session_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(v) => v,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().map(|e| e == "jsonl").unwrap_or(false) {
                out.push(path);
            }
        }
    }

    out
}

fn update_codex_session_file(
    path: &Path,
    from: &str,
    to: &str,
    dry_run: bool,
) -> Result<bool> {
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut changed = false;
    let mut lines = Vec::new();
    let ends_with_newline = content.ends_with('\n');

    for line in content.lines() {
        if line.trim().is_empty() {
            lines.push(String::new());
            continue;
        }

        match serde_json::from_str::<Value>(line) {
            Ok(mut value) => {
                let mut updated_line = false;
                if value.get("type").and_then(|v| v.as_str()) == Some("session_meta") {
                    if let Some(payload) = value.get_mut("payload") {
                        if let Some(obj) = payload.as_object_mut() {
                            if obj.get("cwd").and_then(|v| v.as_str()) == Some(from) {
                                obj.insert("cwd".to_string(), Value::String(to.to_string()));
                                updated_line = true;
                            }
                        }
                    }
                }
                if updated_line {
                    changed = true;
                    lines.push(serde_json::to_string(&value)?);
                } else {
                    lines.push(line.to_string());
                }
            }
            Err(_) => lines.push(line.to_string()),
        }
    }

    if !changed {
        return Ok(false);
    }

    if dry_run {
        println!("Would update {}", path.display());
        return Ok(true);
    }

    let mut output = lines.join("\n");
    if ends_with_newline {
        output.push('\n');
    }
    let tmp_path = path.with_extension("jsonl.tmp");
    fs::write(&tmp_path, output.as_bytes())
        .with_context(|| format!("failed to write {}", tmp_path.display()))?;
    fs::rename(&tmp_path, path)
        .with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(true)
}
