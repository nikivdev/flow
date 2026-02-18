use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use ignore::WalkBuilder;
use sha2::{Digest, Sha256};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[derive(Debug, Clone)]
pub struct DiscoveredAiTask {
    pub id: String,
    pub selector: String,
    pub name: String,
    pub title: String,
    pub description: String,
    pub path: PathBuf,
    pub relative_path: String,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct Metadata {
    title: Option<String>,
    description: Option<String>,
    tags: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CachedTaskArtifact {
    pub cache_key: String,
    pub binary_path: PathBuf,
    pub rebuilt: bool,
}

pub fn discover_tasks(root: &Path) -> Result<Vec<DiscoveredAiTask>> {
    let root = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir()?.join(root)
    };
    let root = root.canonicalize().unwrap_or(root);
    let task_root = root.join(".ai").join("tasks");

    if !task_root.exists() {
        return Ok(Vec::new());
    }

    let walker = WalkBuilder::new(&task_root)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .max_depth(Some(12))
        .build();

    let mut out = Vec::new();
    for entry in walker.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let relative = match path.strip_prefix(&task_root) {
            Ok(relative) => relative,
            Err(_) => continue,
        };
        let has_generated_component = relative.components().any(|component| {
            let s = component.as_os_str().to_string_lossy();
            s == ".mooncakes" || s == "_build"
        });
        if has_generated_component {
            continue;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if ext != "mbt" {
            continue;
        }
        let task = parse_task(&task_root, path)?;
        out.push(task);
    }

    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

pub fn resolve_task_fast(root: &Path, selector: &str) -> Result<Option<DiscoveredAiTask>> {
    let root = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir()?.join(root)
    };
    let root = root.canonicalize().unwrap_or(root);
    let task_root = root.join(".ai").join("tasks");
    if !task_root.exists() {
        return Ok(None);
    }

    let mut needle = selector.trim().to_string();
    if needle.is_empty() {
        return Ok(None);
    }
    if let Some(stripped) = needle.strip_prefix("ai:") {
        needle = stripped.trim().to_string();
    } else if let Some((scope, scoped)) = parse_scoped_selector(&needle)
        && scope.eq_ignore_ascii_case("ai")
    {
        needle = scoped;
    }
    if needle.is_empty() {
        return Ok(None);
    }

    let mut candidates = Vec::new();
    let base = task_root.join(&needle);
    if base.extension().and_then(|e| e.to_str()) == Some("mbt") {
        candidates.push(base);
    } else {
        candidates.push(base.with_extension("mbt"));
        candidates.push(base.join("main.mbt"));
    }
    if needle.contains(':') {
        let normalized = needle.replace(':', "/");
        let norm = task_root.join(normalized);
        candidates.push(norm.with_extension("mbt"));
        candidates.push(norm.join("main.mbt"));
    }

    for candidate in candidates {
        if candidate.is_file() {
            return Ok(Some(parse_task(&task_root, &candidate)?));
        }
    }
    Ok(None)
}

pub fn select_task<'a>(
    tasks: &'a [DiscoveredAiTask],
    selector: &str,
) -> Result<Option<&'a DiscoveredAiTask>> {
    let needle = selector.trim();
    if needle.is_empty() {
        return Ok(None);
    }

    let normalized = normalize_selector(needle);
    let mut matches: Vec<&DiscoveredAiTask> = tasks
        .iter()
        .filter(|t| {
            t.id.eq_ignore_ascii_case(needle)
                || t.selector.eq_ignore_ascii_case(needle)
                || t.name.eq_ignore_ascii_case(needle)
                || normalize_selector(&t.selector) == normalized
                || normalize_selector(&t.name) == normalized
        })
        .collect();

    if let Some((scope, scoped)) = parse_scoped_selector(needle)
        && scope.eq_ignore_ascii_case("ai")
    {
        matches = tasks
            .iter()
            .filter(|t| {
                t.selector.eq_ignore_ascii_case(&scoped)
                    || t.name.eq_ignore_ascii_case(&scoped)
                    || normalize_selector(&t.selector) == normalize_selector(&scoped)
            })
            .collect();
    }

    if matches.is_empty() {
        return Ok(None);
    }
    if matches.len() == 1 {
        return Ok(Some(matches[0]));
    }

    let mut msg = String::new();
    msg.push_str(&format!("AI task '{}' is ambiguous.\n", selector));
    msg.push_str("Matches:\n");
    for m in &matches {
        msg.push_str(&format!("  - {}\n", m.id));
    }
    msg.push_str("Try one of the full selectors above.");
    bail!(msg);
}

pub fn run_task(task: &DiscoveredAiTask, project_root: &Path, args: &[String]) -> Result<()> {
    if !task_has_workspace(task, project_root) {
        return run_task_via_moon(task, project_root, args);
    }

    let runtime = std::env::var("FLOW_AI_TASK_RUNTIME")
        .ok()
        .unwrap_or_else(|| "cached".to_string())
        .to_ascii_lowercase();

    if runtime == "moon-run" || runtime == "moon" {
        return run_task_via_moon(task, project_root, args);
    }

    match run_task_cached(task, project_root, args) {
        Ok(()) => Ok(()),
        Err(cached_error) => {
            eprintln!(
                "warning: ai task cache execution failed for {} ({}), falling back to moon run",
                task.id, cached_error
            );
            run_task_via_moon(task, project_root, args)
        }
    }
}

pub fn run_task_via_moon(
    task: &DiscoveredAiTask,
    project_root: &Path,
    args: &[String],
) -> Result<()> {
    let mut cmd = moon_run_command(task, project_root, args);
    let status = cmd.status().with_context(|| {
        format!(
            "failed to run AI task {} via moon ({})",
            task.id,
            task.path.display()
        )
    })?;

    if !status.success() {
        bail!("AI task '{}' failed with status {}", task.id, status);
    }
    Ok(())
}

pub fn run_task_via_moon_output(
    task: &DiscoveredAiTask,
    project_root: &Path,
    args: &[String],
) -> Result<Output> {
    let mut cmd = moon_run_command(task, project_root, args);
    let output = cmd.output().with_context(|| {
        format!(
            "failed to run AI task {} via moon ({})",
            task.id,
            task.path.display()
        )
    })?;
    Ok(output)
}

pub fn build_task_cached(
    task: &DiscoveredAiTask,
    project_root: &Path,
    force_rebuild: bool,
) -> Result<CachedTaskArtifact> {
    let (workspace_dir, run_path) = resolve_moon_workspace_and_entry(task, project_root);
    if !workspace_dir.join("moon.mod.json").exists() && !workspace_dir.join("moon.mod").exists() {
        bail!(
            "AI task '{}' has no moon workspace root; cannot build cached binary",
            task.id
        );
    }

    let cache_key = compute_cache_key(task, &workspace_dir, &run_path)?;
    let cache_dir = ai_task_cache_root()?.join(&cache_key);
    fs::create_dir_all(&cache_dir)
        .with_context(|| format!("failed to create ai task cache dir {}", cache_dir.display()))?;
    let binary_path = cache_dir.join("task-bin");
    if binary_path.exists() && !force_rebuild {
        return Ok(CachedTaskArtifact {
            cache_key,
            binary_path,
            rebuilt: false,
        });
    }

    let mut cmd = Command::new("moon");
    cmd.arg("build")
        .arg("--target")
        .arg("native")
        .arg("--release");
    if std::env::var("FLOW_AI_TASK_NO_FROZEN").is_err() {
        cmd.arg("--frozen");
    }
    cmd.arg(&run_path).current_dir(&workspace_dir);
    let status = cmd.status().with_context(|| {
        format!(
            "failed to build AI task '{}' with moon build (workspace: {})",
            task.id,
            workspace_dir.display()
        )
    })?;
    if !status.success() {
        bail!(
            "moon build failed for AI task '{}' with status {}",
            task.id,
            status
        );
    }

    let built_binary = find_built_binary(&workspace_dir)?;
    fs::copy(&built_binary, &binary_path).with_context(|| {
        format!(
            "failed to copy built binary {} -> {}",
            built_binary.display(),
            binary_path.display()
        )
    })?;
    #[cfg(unix)]
    {
        let mut perms = fs::metadata(&binary_path)
            .with_context(|| format!("failed to stat {}", binary_path.display()))?
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&binary_path, perms)
            .with_context(|| format!("failed to chmod {}", binary_path.display()))?;
    }

    Ok(CachedTaskArtifact {
        cache_key,
        binary_path,
        rebuilt: true,
    })
}

pub fn run_task_cached(
    task: &DiscoveredAiTask,
    project_root: &Path,
    args: &[String],
) -> Result<()> {
    let artifact = build_task_cached(task, project_root, false)?;
    let status = Command::new(&artifact.binary_path)
        .args(args)
        .current_dir(project_root)
        .env(
            "FLOW_AI_TASK_PROJECT_ROOT",
            project_root.to_string_lossy().to_string(),
        )
        .status()
        .with_context(|| {
            format!(
                "failed to run cached AI task '{}' binary {}",
                task.id,
                artifact.binary_path.display()
            )
        })?;
    if !status.success() {
        bail!("AI task '{}' failed with status {}", task.id, status);
    }
    Ok(())
}

pub fn run_task_cached_output(
    task: &DiscoveredAiTask,
    project_root: &Path,
    args: &[String],
) -> Result<Output> {
    let artifact = build_task_cached(task, project_root, false)?;
    let output = Command::new(&artifact.binary_path)
        .args(args)
        .current_dir(project_root)
        .env(
            "FLOW_AI_TASK_PROJECT_ROOT",
            project_root.to_string_lossy().to_string(),
        )
        .output()
        .with_context(|| {
            format!(
                "failed to run cached AI task '{}' binary {}",
                task.id,
                artifact.binary_path.display()
            )
        })?;
    Ok(output)
}

pub fn default_cache_root() -> Result<PathBuf> {
    ai_task_cache_root()
}

fn moon_run_command(task: &DiscoveredAiTask, project_root: &Path, args: &[String]) -> Command {
    let mode = std::env::var("FLOW_AI_TASK_MODE")
        .ok()
        .unwrap_or_else(|| "dev".to_string())
        .to_ascii_lowercase();

    let mut cmd = Command::new("moon");
    cmd.arg("run");

    // Keep "dev" mode fast to iterate; allow release mode for lower runtime overhead.
    match mode.as_str() {
        "release" | "hot" | "prod" => {
            cmd.arg("--target").arg("native").arg("--release");
        }
        "js" => {
            cmd.arg("--target").arg("js");
        }
        _ => {
            cmd.arg("--target").arg("native");
        }
    }

    if std::env::var("FLOW_AI_TASK_NO_FROZEN").is_err() {
        cmd.arg("--frozen");
    }

    let (workspace_dir, run_path) = resolve_moon_workspace_and_entry(task, project_root);
    cmd.arg(&run_path);
    for arg in args {
        cmd.arg(arg);
    }
    cmd.current_dir(&workspace_dir);
    cmd.env(
        "FLOW_AI_TASK_PROJECT_ROOT",
        project_root.to_string_lossy().to_string(),
    );
    cmd
}

pub fn task_reference(task: &DiscoveredAiTask) -> String {
    task.id.clone()
}

fn resolve_moon_workspace_and_entry(
    task: &DiscoveredAiTask,
    project_root: &Path,
) -> (PathBuf, PathBuf) {
    let entry_path = task.path.clone();
    let start_dir = entry_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| project_root.to_path_buf());

    if let Some(workspace) = find_moon_workspace_root(&start_dir) {
        if let Ok(relative) = entry_path.strip_prefix(&workspace) {
            return (workspace, relative.to_path_buf());
        }
    }

    // Fallback to prior behavior if no moon workspace is found.
    (project_root.to_path_buf(), entry_path)
}

fn task_has_workspace(task: &DiscoveredAiTask, project_root: &Path) -> bool {
    let entry_path = task.path.clone();
    let start_dir = entry_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| project_root.to_path_buf());
    find_moon_workspace_root(&start_dir).is_some()
}

fn ai_task_cache_root() -> Result<PathBuf> {
    let root = dirs::cache_dir()
        .or_else(|| dirs::home_dir().map(|home| home.join(".cache")))
        .context("failed to resolve cache root for AI tasks")?
        .join("flow")
        .join("ai-tasks");
    Ok(root)
}

fn compute_cache_key(
    task: &DiscoveredAiTask,
    workspace_dir: &Path,
    run_path: &Path,
) -> Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(b"flow-ai-task-v2");
    hasher.update(task.id.as_bytes());
    hasher.update(task.selector.as_bytes());
    hasher.update(task.path.to_string_lossy().as_bytes());
    hasher.update(run_path.to_string_lossy().as_bytes());
    hash_file_signature_if_exists(&mut hasher, &task.path)?;
    hash_file_signature_if_exists(&mut hasher, &workspace_dir.join("moon.mod.json"))?;
    hash_file_signature_if_exists(&mut hasher, &workspace_dir.join("moon.mod"))?;
    hash_file_signature_if_exists(&mut hasher, &workspace_dir.join("moon.pkg.json"))?;
    hash_file_signature_if_exists(&mut hasher, &workspace_dir.join("moon.pkg"))?;
    if let Some(version) = moon_version_for_cache_key() {
        hasher.update(version);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn hash_file_signature_if_exists(hasher: &mut Sha256, path: &Path) -> Result<()> {
    if !path.exists() || !path.is_file() {
        return Ok(());
    }
    let meta = fs::metadata(path).with_context(|| format!("failed to stat {}", path.display()))?;
    hasher.update(path.to_string_lossy().as_bytes());
    hasher.update(meta.len().to_le_bytes());
    if let Ok(modified) = meta.modified() {
        let duration = modified
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| Duration::from_secs(0));
        hasher.update(duration.as_secs().to_le_bytes());
        hasher.update(duration.subsec_nanos().to_le_bytes());
    }
    Ok(())
}

fn moon_version_for_cache_key() -> Option<Vec<u8>> {
    if let Ok(raw) = std::env::var("FLOW_AI_TASK_MOON_VERSION") {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.as_bytes().to_vec());
        }
    }

    let ttl_secs = std::env::var("FLOW_AI_TASK_MOON_VERSION_TTL_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(12 * 60 * 60);
    let ttl = Duration::from_secs(ttl_secs);

    let cache_file = ai_task_cache_root().ok()?.join("moon-version.txt");
    if let Ok(meta) = fs::metadata(&cache_file)
        && let Ok(modified) = meta.modified()
        && modified.elapsed().ok().is_some_and(|age| age <= ttl)
        && let Ok(raw) = fs::read_to_string(&cache_file)
    {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.as_bytes().to_vec());
        }
    }

    let out = Command::new("moon").arg("--version").output().ok()?;
    let mut version = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if version.is_empty() {
        version = String::from_utf8_lossy(&out.stderr).trim().to_string();
    }
    if version.is_empty() {
        return None;
    }
    if let Some(parent) = cache_file.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(&cache_file, format!("{version}\n"));
    Some(version.into_bytes())
}

fn find_built_binary(workspace_dir: &Path) -> Result<PathBuf> {
    let build_dir = workspace_dir
        .join("_build")
        .join("native")
        .join("release")
        .join("build");
    if !build_dir.exists() {
        bail!(
            "moon build output directory missing: {}",
            build_dir.display()
        );
    }

    if let Some(name) = moon_mod_package_name(workspace_dir)? {
        let candidates = [
            build_dir.join(format!("{name}.exe")),
            build_dir.join(name.clone()),
        ];
        for candidate in candidates {
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }

    let mut fallback = None;
    for entry in fs::read_dir(&build_dir)
        .with_context(|| format!("failed to read {}", build_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if file_name.ends_with(".exe") || is_executable(&path) {
            fallback = Some(path);
            break;
        }
    }
    fallback.context(format!(
        "failed to locate built AI task binary in {}",
        build_dir.display()
    ))
}

fn moon_mod_package_name(workspace_dir: &Path) -> Result<Option<String>> {
    let path = workspace_dir.join("moon.mod.json");
    if !path.exists() {
        return Ok(None);
    }
    let value: serde_json::Value = serde_json::from_slice(
        &fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?,
    )
    .with_context(|| format!("failed to parse {}", path.display()))?;
    let raw = value
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim();
    if raw.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        raw.rsplit('/').next().unwrap_or(raw).replace('.', "-"),
    ))
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        fs::metadata(path)
            .map(|m| (m.permissions().mode() & 0o111) != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

fn find_moon_workspace_root(start: &Path) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(dir) = current {
        if dir.join("moon.mod.json").exists() || dir.join("moon.mod").exists() {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

fn parse_task(task_root: &Path, path: &Path) -> Result<DiscoveredAiTask> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read AI task {}", path.display()))?;
    let metadata = parse_metadata(&content);

    let relative = path.strip_prefix(task_root).unwrap_or(path);
    let mut selector = relative
        .with_extension("")
        .to_string_lossy()
        .replace('\\', "/");
    if let Some(trimmed) = selector.strip_suffix("/main") {
        selector = trimmed.to_string();
    }

    let mut name = relative
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("task")
        .to_string();
    if name == "main" {
        if let Some(parent_name) = relative
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
        {
            name = parent_name.to_string();
        }
    }
    if selector.is_empty() {
        selector = name.clone();
    }
    let title = metadata.title.unwrap_or_else(|| name.replace('-', " "));
    let description = metadata.description.unwrap_or_default();
    let id = format!("ai:{}", selector);

    Ok(DiscoveredAiTask {
        id,
        selector,
        name,
        title,
        description,
        path: path.to_path_buf(),
        relative_path: relative.to_string_lossy().replace('\\', "/"),
        tags: metadata.tags,
    })
}

fn parse_metadata(content: &str) -> Metadata {
    let mut md = Metadata::default();
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let Some(comment) = line.strip_prefix("//") else {
            break;
        };
        let comment = comment.trim();
        let Some((key, value)) = comment.split_once(':') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        if key == "title" {
            md.title = Some(strip_quotes(value));
        } else if key == "description" {
            md.description = Some(strip_quotes(value));
        } else if key == "tags" {
            md.tags = parse_tags(value);
        }
    }
    md
}

fn parse_tags(value: &str) -> Vec<String> {
    let v = strip_quotes(value);
    let trimmed = v.trim();
    let inner = if trimmed.starts_with('[') && trimmed.ends_with(']') && trimmed.len() >= 2 {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    };
    inner
        .split(',')
        .map(strip_quotes)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn strip_quotes(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.len() >= 2 {
        let bytes = trimmed.as_bytes();
        if (bytes[0] == b'"' && bytes[trimmed.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[trimmed.len() - 1] == b'\'')
        {
            return trimmed[1..trimmed.len() - 1].to_string();
        }
    }
    trimmed.to_string()
}

fn parse_scoped_selector(selector: &str) -> Option<(String, String)> {
    let trimmed = selector.trim();
    if let Some((scope, task)) = trimmed.split_once(':') {
        let scope = scope.trim();
        let task = task.trim();
        if !scope.is_empty() && !task.is_empty() {
            return Some((scope.to_string(), task.to_string()));
        }
    }
    if let Some((scope, task)) = trimmed.split_once('/') {
        let scope = scope.trim();
        let task = task.trim();
        if !scope.is_empty() && !task.is_empty() {
            return Some((scope.to_string(), task.to_string()));
        }
    }
    None
}

fn normalize_selector(raw: &str) -> String {
    raw.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_metadata_comments() {
        let text = "// title: Fast Open\n\
// description: Open app quickly\n\
// tags: [moonbit, fast]\n\
\n\
fn main {}\n";
        let md = parse_metadata(text);
        assert_eq!(md.title.as_deref(), Some("Fast Open"));
        assert_eq!(md.description.as_deref(), Some("Open app quickly"));
        assert_eq!(md.tags, vec!["moonbit".to_string(), "fast".to_string()]);
    }
}
