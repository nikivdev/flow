use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::cli::{GitignoreAction, GitignoreCommand, GitignorePolicyInitOpts, GitignoreScanOpts};
use crate::config;

const POLICY_OVERRIDE_ENV: &str = "FLOW_ALLOW_GITIGNORE_POLICY";
const POLICY_FILE_NAME: &str = "gitignore-policy.toml";
const DEFAULT_REPOS_ROOT: &str = "~/repos";
const DEFAULT_BLOCKED_PATTERNS: &[&str] = &[".ai/todos/*.bike", ".beads/", ".rise/"];
const DEFAULT_ALLOWED_OWNERS: &[&str] = &["nikivdev"];

#[derive(Debug, Clone)]
pub struct GitignorePolicy {
    pub blocked_patterns: Vec<String>,
    pub allowed_owners: Vec<String>,
    pub repos_roots: Vec<PathBuf>,
}

#[derive(Debug, Deserialize, Default)]
struct GitignorePolicyFile {
    blocked_patterns: Option<Vec<String>>,
    allowed_owners: Option<Vec<String>>,
    repos_roots: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
struct Violation {
    file: PathBuf,
    line: usize,
    entry: String,
    blocked_pattern: String,
}

impl Default for GitignorePolicy {
    fn default() -> Self {
        Self {
            blocked_patterns: DEFAULT_BLOCKED_PATTERNS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            allowed_owners: DEFAULT_ALLOWED_OWNERS
                .iter()
                .map(|s| s.to_ascii_lowercase())
                .collect(),
            repos_roots: vec![expand_home(DEFAULT_REPOS_ROOT)],
        }
    }
}

pub fn run(cmd: GitignoreCommand) -> Result<()> {
    match cmd
        .action
        .unwrap_or(GitignoreAction::Audit(GitignoreScanOpts {
            root: None,
            all: false,
        })) {
        GitignoreAction::Audit(opts) => run_scan(opts, false),
        GitignoreAction::Fix(opts) => run_scan(opts, true),
        GitignoreAction::PolicyInit(opts) => init_policy_file(opts),
        GitignoreAction::SetupGlobal { print_only } => setup_global_gitignore(print_only),
        GitignoreAction::PolicyPath => {
            println!("{}", policy_path().display());
            Ok(())
        }
    }
}

pub fn enforce_staged_policy(repo_root: &Path) -> Result<()> {
    if policy_override_enabled() {
        return Ok(());
    }

    let policy = load_policy();
    if !is_external_repo(repo_root, &policy) {
        return Ok(());
    }

    let violations = staged_gitignore_violations(repo_root, &policy)?;
    if violations.is_empty() {
        return Ok(());
    }

    eprintln!("Refusing to commit personal tooling ignore entries in an external repo:");
    for v in &violations {
        eprintln!(
            "  - {}:{} adds '{}' (blocked by policy '{}')",
            v.file.display(),
            v.line,
            v.entry,
            v.blocked_pattern
        );
    }
    eprintln!();
    eprintln!("Use global gitignore for personal tooling, then retry.");
    eprintln!("To clean existing repos: f gitignore fix");
    eprintln!("Override once with {}=1", POLICY_OVERRIDE_ENV);

    bail!("blocked personal tooling .gitignore entries")
}

pub fn load_policy() -> GitignorePolicy {
    let mut policy = GitignorePolicy::default();
    let path = policy_path();

    if !path.exists() {
        return policy;
    }

    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) => {
            eprintln!(
                "warn: failed to read {}: {} (using defaults)",
                path.display(),
                err
            );
            return policy;
        }
    };

    let parsed: GitignorePolicyFile = match toml::from_str(&content) {
        Ok(parsed) => parsed,
        Err(err) => {
            eprintln!(
                "warn: failed to parse {}: {} (using defaults)",
                path.display(),
                err
            );
            return policy;
        }
    };

    if let Some(patterns) = parsed.blocked_patterns {
        let cleaned = clean_patterns(patterns.into_iter());
        if !cleaned.is_empty() {
            policy.blocked_patterns = cleaned;
        }
    }

    if let Some(owners) = parsed.allowed_owners {
        let cleaned: Vec<String> = owners
            .into_iter()
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        if !cleaned.is_empty() {
            policy.allowed_owners = cleaned;
        }
    }

    if let Some(roots) = parsed.repos_roots {
        let cleaned: Vec<PathBuf> = roots
            .into_iter()
            .map(|s| expand_home(s.trim()))
            .filter(|p| !p.as_os_str().is_empty())
            .collect();
        if !cleaned.is_empty() {
            policy.repos_roots = cleaned;
        }
    }

    policy
}

pub fn policy_path() -> PathBuf {
    config::global_config_dir().join(POLICY_FILE_NAME)
}

pub fn is_external_repo(repo_root: &Path, policy: &GitignorePolicy) -> bool {
    let Some(owner) = repo_origin_owner(repo_root) else {
        return true;
    };

    !policy
        .allowed_owners
        .iter()
        .any(|o| o.eq_ignore_ascii_case(owner.as_str()))
}

fn run_scan(opts: GitignoreScanOpts, apply_fix: bool) -> Result<()> {
    let policy = load_policy();
    let roots = scan_roots(&opts, &policy);

    let mut repo_roots: Vec<PathBuf> = Vec::new();
    for root in roots {
        repo_roots.extend(discover_repo_roots(&root));
    }
    repo_roots.sort();
    repo_roots.dedup();

    if repo_roots.is_empty() {
        println!("No repositories found.");
        return Ok(());
    }

    let mut findings_by_repo: BTreeMap<PathBuf, Vec<Violation>> = BTreeMap::new();
    let mut touched_files: BTreeSet<PathBuf> = BTreeSet::new();

    for repo_root in repo_roots {
        if !opts.all && !is_external_repo(&repo_root, &policy) {
            continue;
        }

        if apply_fix {
            let changed = fix_repo_gitignores(&repo_root, &policy)?;
            touched_files.extend(changed);
        }

        let repo_findings = inspect_repo_gitignores(&repo_root, &policy)?;
        if !repo_findings.is_empty() {
            findings_by_repo.insert(repo_root, repo_findings);
        }
    }

    if apply_fix {
        if touched_files.is_empty() {
            println!("No policy entries needed removal.");
        } else {
            println!(
                "Removed policy entries from {} .gitignore file(s).",
                touched_files.len()
            );
            for path in touched_files {
                println!("  - {}", path.display());
            }
        }
    }

    if findings_by_repo.is_empty() {
        println!("No blocked personal-tooling patterns found.");
        return Ok(());
    }

    println!("Blocked personal-tooling patterns found:");
    for (repo, findings) in &findings_by_repo {
        println!("\n{}", repo.display());
        for v in findings {
            println!(
                "  - {}:{} '{}' (blocked by '{}')",
                v.file.display(),
                v.line,
                v.entry,
                v.blocked_pattern
            );
        }
    }

    if apply_fix {
        bail!("Some blocked entries remain; review output above")
    } else {
        bail!("Found blocked personal-tooling entries")
    }
}

fn init_policy_file(opts: GitignorePolicyInitOpts) -> Result<()> {
    let path = policy_path();
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("invalid policy path {}", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;

    if path.exists() && !opts.force {
        bail!(
            "{} already exists (use --force to overwrite)",
            path.display()
        );
    }

    fs::write(&path, default_policy_template())
        .with_context(|| format!("failed to write {}", path.display()))?;
    println!("Wrote {}", path.display());
    Ok(())
}

fn setup_global_gitignore(print_only: bool) -> Result<()> {
    let policy = load_policy();
    let target = resolve_global_excludes_path()?;

    if print_only {
        println!("Global excludes file: {}", target.display());
        println!("Patterns:");
        for pattern in &policy.blocked_patterns {
            println!("  - {}", pattern);
        }
        return Ok(());
    }

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let existing = fs::read_to_string(&target).unwrap_or_default();
    let mut lines: Vec<String> = existing.lines().map(|s| s.to_string()).collect();
    let mut appended = 0usize;

    for pattern in &policy.blocked_patterns {
        let wanted = pattern.trim();
        let Some(target_norm) = normalize_entry(wanted) else {
            continue;
        };
        let present = lines.iter().any(|line| {
            normalize_entry(line)
                .map(|norm| norm == target_norm)
                .unwrap_or(false)
        });
        if present {
            continue;
        }
        lines.push(wanted.to_string());
        appended += 1;
    }

    let mut rendered = lines.join("\n");
    if !rendered.is_empty() {
        rendered.push('\n');
    }
    fs::write(&target, rendered)
        .with_context(|| format!("failed to write {}", target.display()))?;

    ensure_global_excludes_config(&target)?;

    if appended == 0 {
        println!("Global excludes already up to date: {}", target.display());
    } else {
        println!(
            "Added {} pattern(s) to global excludes: {}",
            appended,
            target.display()
        );
    }

    Ok(())
}

fn resolve_global_excludes_path() -> Result<PathBuf> {
    if let Some(configured) = git_capture_global_config("core.excludesFile")? {
        return Ok(expand_home(configured.trim()));
    }

    Ok(home_dir_or_default().join(".config/git/ignore"))
}

fn ensure_global_excludes_config(path: &Path) -> Result<()> {
    let current = git_capture_global_config("core.excludesFile")?;
    if let Some(current) = current {
        let current_path = expand_home(current.trim());
        if current_path == path {
            return Ok(());
        }
    }

    let value = path.to_string_lossy().to_string();
    let status = Command::new("git")
        .args(["config", "--global", "core.excludesFile", &value])
        .status()
        .context("failed to run git config --global core.excludesFile")?;
    if !status.success() {
        bail!("git config --global core.excludesFile failed")
    }
    Ok(())
}

fn git_capture_global_config(key: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .args(["config", "--global", "--get", key])
        .output()
        .with_context(|| format!("failed to read global git config key {}", key))?;

    if !output.status.success() {
        return Ok(None);
    }

    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

fn staged_gitignore_violations(
    repo_root: &Path,
    policy: &GitignorePolicy,
) -> Result<Vec<Violation>> {
    let staged_files = staged_gitignore_files(repo_root)?;
    if staged_files.is_empty() {
        return Ok(Vec::new());
    }

    let blocked = blocked_lookup(policy);
    let mut violations = Vec::new();

    for file in staged_files {
        let output = Command::new("git")
            .current_dir(repo_root)
            .args(["diff", "--cached", "-U0", "--", &file])
            .output()
            .with_context(|| format!("failed to inspect staged diff for {}", file))?;

        if !output.status.success() {
            continue;
        }

        let diff = String::from_utf8_lossy(&output.stdout);
        let mut line_no: usize = 0;

        for line in diff.lines() {
            if line.starts_with("@@") {
                line_no = parse_hunk_new_line(line).unwrap_or(0);
                continue;
            }

            if let Some(rest) = line.strip_prefix('+') {
                if line.starts_with("+++") {
                    continue;
                }
                if let Some(normalized) = normalize_entry(rest) {
                    if let Some((_, blocked_pattern)) =
                        blocked.iter().find(|(norm, _)| norm == &normalized)
                    {
                        violations.push(Violation {
                            file: PathBuf::from(&file),
                            line: if line_no == 0 { 1 } else { line_no },
                            entry: rest.trim().to_string(),
                            blocked_pattern: blocked_pattern.clone(),
                        });
                    }
                }
                line_no = line_no.saturating_add(1);
                continue;
            }

            if line.starts_with(' ') {
                line_no = line_no.saturating_add(1);
            }
        }
    }

    Ok(violations)
}

fn staged_gitignore_files(repo_root: &Path) -> Result<Vec<String>> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(["diff", "--cached", "--name-only", "--diff-filter=ACMR"])
        .output()
        .context("failed to list staged files")?;

    if !output.status.success() {
        bail!("git diff --cached --name-only failed")
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|s| s.ends_with(".gitignore"))
        .map(|s| s.to_string())
        .collect())
}

fn inspect_repo_gitignores(repo_root: &Path, policy: &GitignorePolicy) -> Result<Vec<Violation>> {
    let blocked = blocked_lookup(policy);
    let files = list_gitignore_files(repo_root);
    let mut out = Vec::new();

    for file in files {
        let content = fs::read_to_string(&file)
            .with_context(|| format!("failed to read {}", file.display()))?;
        for (idx, line) in content.lines().enumerate() {
            let Some(normalized) = normalize_entry(line) else {
                continue;
            };
            if let Some((_, blocked_pattern)) = blocked.iter().find(|(norm, _)| norm == &normalized)
            {
                out.push(Violation {
                    file: file.clone(),
                    line: idx + 1,
                    entry: line.trim().to_string(),
                    blocked_pattern: blocked_pattern.clone(),
                });
            }
        }
    }

    Ok(out)
}

fn fix_repo_gitignores(repo_root: &Path, policy: &GitignorePolicy) -> Result<Vec<PathBuf>> {
    let blocked: HashSet<String> = blocked_lookup(policy).into_iter().map(|(n, _)| n).collect();
    let files = list_gitignore_files(repo_root);
    let mut changed = Vec::new();

    for file in files {
        let content = fs::read_to_string(&file)
            .with_context(|| format!("failed to read {}", file.display()))?;
        let had_trailing_newline = content.ends_with('\n');

        let mut kept = Vec::new();
        let mut removed_any = false;
        for line in content.lines() {
            let remove = normalize_entry(line)
                .map(|normalized| blocked.contains(&normalized))
                .unwrap_or(false);
            if remove {
                removed_any = true;
                continue;
            }
            kept.push(line.to_string());
        }

        if !removed_any {
            continue;
        }

        let mut new_content = kept.join("\n");
        if had_trailing_newline && !new_content.is_empty() {
            new_content.push('\n');
        }

        fs::write(&file, new_content)
            .with_context(|| format!("failed to write {}", file.display()))?;
        changed.push(file);
    }

    Ok(changed)
}

fn list_gitignore_files(repo_root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_gitignore_files(repo_root, 0, 64, &mut files);
    files.sort();
    files
}

fn discover_repo_roots(root: &Path) -> Vec<PathBuf> {
    let mut repos: BTreeSet<PathBuf> = BTreeSet::new();
    collect_repo_roots(root, 0, 4, &mut repos);
    repos.into_iter().collect()
}

fn collect_gitignore_files(dir: &Path, depth: usize, max_depth: usize, out: &mut Vec<PathBuf>) {
    if depth > max_depth {
        return;
    }

    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else {
            continue;
        };

        if ft.is_file() {
            if path.file_name() == Some(OsStr::new(".gitignore")) {
                out.push(path);
            }
            continue;
        }

        if !ft.is_dir() {
            continue;
        }

        let name = path.file_name().and_then(OsStr::to_str).unwrap_or_default();
        if name == ".git" || name == "node_modules" || name == "target" {
            continue;
        }

        collect_gitignore_files(&path, depth + 1, max_depth, out);
    }
}

fn collect_repo_roots(dir: &Path, depth: usize, max_depth: usize, out: &mut BTreeSet<PathBuf>) {
    if depth > max_depth {
        return;
    }

    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else {
            continue;
        };

        if !ft.is_dir() {
            continue;
        }

        let name = path.file_name().and_then(OsStr::to_str).unwrap_or_default();
        if name == ".git" {
            if let Some(parent) = path.parent() {
                out.insert(parent.to_path_buf());
            }
            continue;
        }

        if name == "node_modules" || name == "target" {
            continue;
        }

        collect_repo_roots(&path, depth + 1, max_depth, out);
    }
}

fn scan_roots(opts: &GitignoreScanOpts, policy: &GitignorePolicy) -> Vec<PathBuf> {
    if let Some(root) = opts.root.as_deref() {
        return vec![expand_home(root)];
    }

    if !policy.repos_roots.is_empty() {
        return policy.repos_roots.clone();
    }

    vec![expand_home(DEFAULT_REPOS_ROOT)]
}

fn blocked_lookup(policy: &GitignorePolicy) -> Vec<(String, String)> {
    policy
        .blocked_patterns
        .iter()
        .filter_map(|p| normalize_entry(p).map(|norm| (norm, p.trim().to_string())))
        .collect()
}

fn clean_patterns<I>(patterns: I) -> Vec<String>
where
    I: Iterator<Item = String>,
{
    patterns
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn normalize_entry(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }

    let content = if let Some((head, _)) = trimmed.split_once(" #") {
        head.trim()
    } else {
        trimmed
    };

    let normalized = content.trim_start_matches('/').trim();
    if normalized.is_empty() {
        return None;
    }

    Some(normalized.to_string())
}

fn parse_hunk_new_line(hunk: &str) -> Option<usize> {
    let plus = hunk.find('+')?;
    let after_plus = &hunk[plus + 1..];
    let digits: String = after_plus
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse::<usize>().ok()
}

fn repo_origin_owner(repo_root: &Path) -> Option<String> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let url = String::from_utf8_lossy(&output.stdout);
    parse_github_owner(url.trim())
}

fn parse_github_owner(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches('/');

    if let Some(rest) = trimmed.strip_prefix("git@github.com:") {
        let repo = rest.trim_end_matches(".git");
        return repo.split('/').next().map(|s| s.to_string());
    }

    if let Some(rest) = trimmed.strip_prefix("https://github.com/") {
        let repo = rest.trim_end_matches(".git");
        return repo.split('/').next().map(|s| s.to_string());
    }

    None
}

fn policy_override_enabled() -> bool {
    env::var(POLICY_OVERRIDE_ENV)
        .ok()
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            v == "1" || v == "true" || v == "yes"
        })
        .unwrap_or(false)
}

fn expand_home(input: &str) -> PathBuf {
    let trimmed = input.trim();
    if trimmed == "~" {
        return home_dir_or_default();
    }
    if let Some(rest) = trimmed.strip_prefix("~/") {
        return home_dir_or_default().join(rest);
    }
    PathBuf::from(trimmed)
}

fn home_dir_or_default() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

fn default_policy_template() -> String {
    format!(
        "# Flow gitignore policy\n#\n# These patterns are local developer tooling noise and should stay out of\n# external/public repositories.\n\nblocked_patterns = [\n  \"{}\",\n  \"{}\",\n  \"{}\",\n]\n\n# Repositories owned by these GitHub users are treated as internal and exempt.\nallowed_owners = [\n  \"{}\",\n]\n\n# Roots scanned by `f gitignore audit` and `f gitignore fix` when --root is omitted.\nrepos_roots = [\n  \"{}\",\n]\n",
        DEFAULT_BLOCKED_PATTERNS[0],
        DEFAULT_BLOCKED_PATTERNS[1],
        DEFAULT_BLOCKED_PATTERNS[2],
        DEFAULT_ALLOWED_OWNERS[0],
        DEFAULT_REPOS_ROOT,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_entry_ignores_comments_and_slashes() {
        assert_eq!(normalize_entry("/.beads/"), Some(".beads/".to_string()));
        assert_eq!(
            normalize_entry(".rise/ # local"),
            Some(".rise/".to_string())
        );
        assert_eq!(normalize_entry("# note"), None);
    }

    #[test]
    fn parse_github_owner_from_remote_url() {
        assert_eq!(
            parse_github_owner("https://github.com/pqrs-org/Karabiner-Elements.git"),
            Some("pqrs-org".to_string())
        );
        assert_eq!(
            parse_github_owner("git@github.com:nikivdev/Karabiner-Elements.git"),
            Some("nikivdev".to_string())
        );
    }
}
