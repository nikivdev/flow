use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::cli::{CodeAction, CodeCommand, CodeMigrateOpts, CodeMoveSessionsOpts, CodeNewOpts};
use crate::config;

const DEFAULT_CODE_ROOT: &str = "~/code";
const DEFAULT_TEMPLATE_ROOT: &str = "~/new";

pub fn run(cmd: CodeCommand) -> Result<()> {
    match cmd.action {
        Some(CodeAction::List) => list_code(&cmd.root),
        Some(CodeAction::New(opts)) => new_project(opts, &cmd.root),
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

fn new_project(opts: CodeNewOpts, root: &str) -> Result<()> {
    let root = normalize_root(root)?;
    let template_root = config::expand_path(DEFAULT_TEMPLATE_ROOT);
    let template_dir = template_root.join(opts.template.trim());
    if !template_dir.exists() {
        bail!("Template not found: {}", template_dir.display());
    }
    if !template_dir.is_dir() {
        bail!(
            "Template path is not a directory: {}",
            template_dir.display()
        );
    }

    let relative = normalize_relative_path(&opts.name)?;
    let target = root.join(&relative);
    let target_display = target.display().to_string();
    let mut planned_dirs = Vec::new();

    if target.exists() {
        bail!("Destination already exists: {}", target.display());
    }

    ensure_dir(&root, opts.dry_run, &mut planned_dirs)?;
    if let Some(parent) = target.parent() {
        if parent != root {
            ensure_dir(parent, opts.dry_run, &mut planned_dirs)?;
        }
    }

    if opts.dry_run {
        println!(
            "Would copy template {} -> {}",
            template_dir.display(),
            target_display
        );
        if opts.ignored {
            if let Some((repo_root, entry)) = gitignore_entry_for_target(&target)? {
                println!(
                    "Would add {} to {}",
                    entry,
                    repo_root.join(".gitignore").display()
                );
            } else {
                bail!("--ignored requires the target to be inside a git repository");
            }
        }
        return Ok(());
    }

    copy_dir_all(&template_dir, &target)?;
    println!("Created {}", target_display);
    if opts.ignored {
        if let Some((repo_root, entry)) = gitignore_entry_for_target(&target)? {
            ensure_gitignore_entry(&repo_root, &entry)?;
        } else {
            bail!("--ignored requires the target to be inside a git repository");
        }
    }
    Ok(())
}

fn migrate_project(opts: CodeMigrateOpts, root: &str) -> Result<()> {
    let root = normalize_root(root)?;
    let from = normalize_path(&opts.from)?;
    let relative = normalize_relative_path(&opts.relative)?;
    let target = root.join(&relative);
    let target_display = target.display().to_string();
    let root_display = root.to_string_lossy().to_string();
    let mut planned_dirs = Vec::new();

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

    ensure_dir(&root, opts.dry_run, &mut planned_dirs)?;
    if let Some(parent) = target.parent() {
        if parent.to_string_lossy() != root_display {
            ensure_dir(parent, opts.dry_run, &mut planned_dirs)?;
        }
    }

    if opts.dry_run {
        println!("Would move {} -> {}", from.display(), target_display);
    } else {
        move_dir(&from, &target)?;
        println!("Moved {} -> {}", from.display(), target_display);
    }

    let relinked = relink_bin_symlinks(&from, &target, opts.dry_run)?;
    if relinked > 0 {
        println!("Updated {} symlink(s) in ~/bin", relinked);
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
    let mut remaining_codex_files = Vec::new();

    if !opts.skip_claude {
        let base = claude_projects_dir();
        let from_dir = base.join(path_to_project_name(&from));
        let to_dir = base.join(path_to_project_name(&to));
        let from_exists = from_dir.exists();
        let to_exists = to_dir.exists();
        moved_claude = move_project_dir(&base, &from, &to, opts.dry_run)?;
        if from_exists && !opts.dry_run {
            if from_dir.exists() {
                println!(
                    "WARN Claude session dir still present: {}",
                    from_dir.display()
                );
            }
            if !to_dir.exists() && !to_exists {
                println!(
                    "WARN Claude session dir missing after migration: {}",
                    to_dir.display()
                );
            }
        }
    }
    if !opts.skip_codex {
        let base = codex_projects_dir();
        let from_dir = base.join(path_to_project_name(&from));
        let to_dir = base.join(path_to_project_name(&to));
        let from_exists = from_dir.exists();
        let to_exists = to_dir.exists();
        moved_codex = move_project_dir(&base, &from, &to, opts.dry_run)?;
        let codex_update = update_codex_sessions(&from, &to, opts.dry_run)?;
        updated_codex_files = codex_update.updated_files;
        remaining_codex_files = codex_update.remaining_files;
        if from_exists && !opts.dry_run {
            if from_dir.exists() {
                println!(
                    "WARN Codex session dir still present: {}",
                    from_dir.display()
                );
            }
            if !to_dir.exists() && !to_exists {
                println!(
                    "WARN Codex session dir missing after migration: {}",
                    to_dir.display()
                );
            }
        }
    }

    println!("Session migration summary:");
    println!("  Claude project dirs moved: {}", moved_claude);
    println!("  Codex legacy dirs moved: {}", moved_codex);
    println!("  Codex jsonl files updated: {}", updated_codex_files);
    if !remaining_codex_files.is_empty() {
        println!("WARN Codex sessions still reference the old path:");
        for path in &remaining_codex_files {
            println!("  {}", path.display());
        }
    }
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
        let metadata =
            fs::metadata(target).with_context(|| format!("failed to read {}", target.display()))?;
        if metadata.is_dir() {
            copy_dir_all(target, dest)?;
        } else {
            fs::copy(target, dest)
                .with_context(|| format!("failed to copy {}", target.display()))?;
        }
        Ok(())
    }
}

fn relink_bin_symlinks(from: &Path, to: &Path, dry_run: bool) -> Result<usize> {
    let bin_dir = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("bin");
    if !bin_dir.exists() {
        return Ok(0);
    }

    let mut updated = 0;
    for entry in fs::read_dir(&bin_dir)
        .with_context(|| format!("failed to read bin directory {}", bin_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)?;
        if !meta.file_type().is_symlink() {
            continue;
        }

        let link_target = fs::read_link(&path)?;
        let resolved = if link_target.is_absolute() {
            link_target.clone()
        } else {
            path.parent().unwrap_or(&bin_dir).join(&link_target)
        };

        if !resolved.starts_with(from) {
            continue;
        }

        let suffix = match resolved.strip_prefix(from) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let new_target = to.join(suffix);
        if dry_run {
            println!(
                "Would relink {} -> {}",
                path.display(),
                new_target.display()
            );
        } else {
            relink_symlink(&path, &new_target)?;
        }
        updated += 1;
    }

    Ok(updated)
}

fn relink_symlink(path: &Path, target: &Path) -> Result<()> {
    fs::remove_file(path).with_context(|| format!("failed to remove {}", path.display()))?;
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        return Ok(());
    }
    #[cfg(windows)]
    {
        if target.is_dir() {
            std::os::windows::fs::symlink_dir(target, path)
                .with_context(|| format!("failed to create {}", path.display()))?;
        } else {
            std::os::windows::fs::symlink_file(target, path)
                .with_context(|| format!("failed to create {}", path.display()))?;
        }
        return Ok(());
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (path, target);
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

struct CodexUpdateSummary {
    updated_files: usize,
    remaining_files: Vec<PathBuf>,
}

fn update_codex_sessions(from: &Path, to: &Path, dry_run: bool) -> Result<CodexUpdateSummary> {
    let root = codex_sessions_dir();
    if !root.exists() {
        return Ok(CodexUpdateSummary {
            updated_files: 0,
            remaining_files: Vec::new(),
        });
    }

    let from_str = from.to_string_lossy().to_string();
    let to_str = to.to_string_lossy().to_string();
    let mut updated_files = 0;
    let mut remaining_files = Vec::new();

    for file_path in collect_codex_session_files(&root) {
        let result = update_codex_session_file(&file_path, &from_str, &to_str, dry_run)?;
        if result.updated {
            updated_files += 1;
        }
        if result.remaining {
            remaining_files.push(file_path);
        }
    }

    Ok(CodexUpdateSummary {
        updated_files,
        remaining_files,
    })
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

struct CodexFileUpdate {
    updated: bool,
    remaining: bool,
}

fn update_codex_session_file(
    path: &Path,
    from: &str,
    to: &str,
    dry_run: bool,
) -> Result<CodexFileUpdate> {
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut changed = false;
    let mut matched = false;
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
                                matched = true;
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
        let remaining = if matched && !dry_run {
            file_has_session_meta_cwd(path, from)?
        } else {
            false
        };
        return Ok(CodexFileUpdate {
            updated: false,
            remaining,
        });
    }

    if dry_run {
        println!("Would update {}", path.display());
        return Ok(CodexFileUpdate {
            updated: true,
            remaining: true,
        });
    }

    let mut output = lines.join("\n");
    if ends_with_newline {
        output.push('\n');
    }
    let tmp_path = path.with_extension("jsonl.tmp");
    fs::write(&tmp_path, output.as_bytes())
        .with_context(|| format!("failed to write {}", tmp_path.display()))?;
    fs::rename(&tmp_path, path).with_context(|| format!("failed to replace {}", path.display()))?;
    let remaining = file_has_session_meta_cwd(path, from)?;
    Ok(CodexFileUpdate {
        updated: true,
        remaining,
    })
}

fn file_has_session_meta_cwd(path: &Path, from: &str) -> Result<bool> {
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(content
        .lines()
        .any(|line| session_meta_cwd_matches(line, from)))
}

fn session_meta_cwd_matches(line: &str, from: &str) -> bool {
    if line.trim().is_empty() {
        return false;
    }
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return false;
    };
    if value.get("type").and_then(|v| v.as_str()) != Some("session_meta") {
        return false;
    }
    let Some(payload) = value.get("payload") else {
        return false;
    };
    let Some(obj) = payload.as_object() else {
        return false;
    };
    obj.get("cwd").and_then(|v| v.as_str()) == Some(from)
}

fn ensure_dir(path: &Path, dry_run: bool, planned: &mut Vec<PathBuf>) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    if planned.iter().any(|p| p == path) {
        return Ok(());
    }
    if dry_run {
        println!("Would create {}", path.display());
        planned.push(path.to_path_buf());
        return Ok(());
    }
    fs::create_dir_all(path).with_context(|| format!("failed to create {}", path.display()))?;
    planned.push(path.to_path_buf());
    Ok(())
}

fn gitignore_entry_for_target(target: &Path) -> Result<Option<(PathBuf, String)>> {
    let root = find_git_root(target)?;
    let Some(repo_root) = root else {
        return Ok(None);
    };
    let relative = target
        .strip_prefix(&repo_root)
        .unwrap_or(target)
        .to_string_lossy()
        .replace('\\', "/");
    let mut entry = relative.trim().trim_start_matches("./").to_string();
    if entry.is_empty() {
        return Ok(None);
    }
    if !entry.ends_with('/') {
        entry.push('/');
    }
    Ok(Some((repo_root, entry)))
}

fn find_git_root(start: &Path) -> Result<Option<PathBuf>> {
    let mut current = start.to_path_buf();
    if !current.exists() {
        if let Some(parent) = current.parent() {
            current = parent.to_path_buf();
        }
    }
    loop {
        let git_dir = current.join(".git");
        if git_dir.is_dir() || git_dir.is_file() {
            return Ok(Some(current));
        }
        if !current.pop() {
            return Ok(None);
        }
    }
}

fn ensure_gitignore_entry(repo_root: &Path, entry: &str) -> Result<()> {
    let gitignore = repo_root.join(".gitignore");
    let entry_trimmed = entry.trim().trim_end_matches('/');
    let entry_with_slash = format!("{}/", entry_trimmed);
    let mut existing = String::new();
    if gitignore.exists() {
        existing = fs::read_to_string(&gitignore)
            .with_context(|| format!("failed to read {}", gitignore.display()))?;
        if existing.lines().any(|line| {
            let trimmed = line.trim();
            trimmed == entry_trimmed || trimmed == entry_with_slash
        }) {
            return Ok(());
        }
    }
    let mut output = existing;
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
    output.push_str(&entry_with_slash);
    output.push('\n');
    fs::write(&gitignore, output.as_bytes())
        .with_context(|| format!("failed to write {}", gitignore.display()))?;
    Ok(())
}
