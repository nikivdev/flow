use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use ignore::WalkBuilder;
use serde::Deserialize;
use toml::Value;
use toml::map::Map;

use crate::cli::{DepsAction, DepsCommand, DepsManager, ReposCloneOpts};
use crate::{config, repos, upstream};

pub fn run(cmd: DepsCommand) -> Result<()> {
    let action = cmd.action;
    let project_root = project_root()?;
    let manager = cmd.manager.unwrap_or_else(|| detect_manager(&project_root));

    match action {
        None | Some(DepsAction::Pick) => {
            pick_dependency(&project_root)?;
        }
        Some(DepsAction::Repo { repo, root }) => {
            link_repo_dependency(&project_root, &repo, &root)?;
        }
        Some(other) => {
            let (program, args) = build_command(manager, &project_root, &other)?;
            let status = Command::new(program)
                .args(&args)
                .current_dir(&project_root)
                .status()
                .with_context(|| format!("failed to run {}", program))?;

            if !status.success() {
                bail!("dependency command failed");
            }
        }
    }

    Ok(())
}

fn build_command(
    manager: DepsManager,
    project_root: &Path,
    action: &DepsAction,
) -> Result<(&'static str, Vec<String>)> {
    let workspace = is_workspace(project_root);
    let (base, mut args) = match (manager, workspace) {
        (DepsManager::Pnpm, true) => ("pnpm", vec!["-r".to_string()]),
        (DepsManager::Pnpm, false) => ("pnpm", Vec::new()),
        (DepsManager::Yarn, _) => ("yarn", Vec::new()),
        (DepsManager::Bun, _) => ("bun", Vec::new()),
        (DepsManager::Npm, _) => ("npm", Vec::new()),
    };

    match action {
        DepsAction::Install { args: extra } => {
            args.push("install".to_string());
            args.extend(extra.clone());
        }
        DepsAction::Update { args: extra } => {
            match manager {
                DepsManager::Pnpm => {
                    args.push("up".to_string());
                    args.push("--latest".to_string());
                }
                DepsManager::Yarn => {
                    args.push("up".to_string());
                }
                DepsManager::Bun => {
                    args.push("update".to_string());
                }
                DepsManager::Npm => {
                    args.push("update".to_string());
                }
            }
            args.extend(extra.clone());
        }
        DepsAction::Repo { .. } | DepsAction::Pick => {
            bail!("dependency action is not a package manager command");
        }
    }

    Ok((base, args))
}

fn detect_manager(project_root: &Path) -> DepsManager {
    if project_root.join("pnpm-lock.yaml").exists() || project_root.join("pnpm-workspace.yaml").exists() {
        return DepsManager::Pnpm;
    }
    if project_root.join("bun.lockb").exists() || project_root.join("bun.lock").exists() {
        return DepsManager::Bun;
    }
    if project_root.join("yarn.lock").exists() {
        return DepsManager::Yarn;
    }
    if project_root.join("package-lock.json").exists() {
        return DepsManager::Npm;
    }
    DepsManager::Npm
}

fn is_workspace(project_root: &Path) -> bool {
    project_root.join("pnpm-workspace.yaml").exists()
}

fn project_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    if let Some(flow_path) = find_flow_toml(&cwd) {
        return Ok(flow_path.parent().unwrap_or(&cwd).to_path_buf());
    }
    Ok(cwd)
}

fn find_flow_toml(start: &PathBuf) -> Option<PathBuf> {
    let mut current = start.clone();
    loop {
        let candidate = current.join("flow.toml");
        if candidate.exists() {
            return Some(candidate);
        }
        if !current.pop() {
            return None;
        }
    }
}

#[derive(Debug)]
enum DepPickAction {
    RepoLink { repo: String },
    RepoOpen { owner: String, repo: String },
    Project { path: PathBuf },
    Message { text: String },
}

#[derive(Debug)]
struct DepPickEntry {
    display: String,
    action: DepPickAction,
}

#[derive(Debug, Deserialize)]
struct RepoManifest {
    root: Option<String>,
    repos: Option<Vec<RepoManifestEntry>>,
}

#[derive(Debug, Deserialize)]
struct RepoManifestEntry {
    owner: String,
    repo: String,
    url: Option<String>,
}

fn pick_dependency(project_root: &Path) -> Result<()> {
    let manifest = load_repo_manifest(project_root)?;
    let default_root = manifest
        .as_ref()
        .and_then(|m| m.root.clone())
        .unwrap_or_else(|| "~/repos".to_string());

    let root_path = repos::normalize_root(&default_root)?;
    let entries = build_pick_entries(project_root, &root_path, manifest.as_ref())?;
    if entries.is_empty() {
        println!("No linked repos or dependency metadata found.");
        return Ok(());
    }

    if which::which("fzf").is_err() {
        println!("fzf not found on PATH – install it to use fuzzy selection.");
        for entry in &entries {
            println!("  {}", entry.display);
        }
        return Ok(());
    }

    let Some(entry) = run_deps_fzf(&entries)? else {
        return Ok(());
    };

    match &entry.action {
        DepPickAction::RepoLink { repo } => link_repo_dependency(project_root, repo, &default_root)?,
        DepPickAction::RepoOpen { owner, repo } => {
            let repo_ref = repos::RepoRef {
                owner: owner.clone(),
                repo: repo.clone(),
            };
            let repo_path = root_path.join(&repo_ref.owner).join(&repo_ref.repo);
            if !repo_path.exists() {
                let repo_id = format!("{}/{}", repo_ref.owner, repo_ref.repo);
                link_repo_dependency(project_root, &repo_id, &default_root)?;
            }
            open_in_zed(&repo_path)?;
        }
        DepPickAction::Project { path } => {
            println!("Project path: {}", path.display());
            println!("Hint: cd {}", path.display());
        }
        DepPickAction::Message { text } => {
            println!("{}", text);
        }
    }

    Ok(())
}

fn run_deps_fzf<'a>(entries: &'a [DepPickEntry]) -> Result<Option<&'a DepPickEntry>> {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = Command::new("fzf")
        .arg("--prompt")
        .arg("deps> ")
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

    Ok(entries.iter().find(|entry| entry.display == selection))
}

fn build_pick_entries(
    project_root: &Path,
    root_path: &Path,
    manifest: Option<&RepoManifest>,
) -> Result<Vec<DepPickEntry>> {
    let mut entries = Vec::new();

    if let Some(manifest) = manifest {
        if let Some(repos) = &manifest.repos {
            for repo in repos {
                let repo_id = format!("{}/{}", repo.owner, repo.repo);
                let is_local = root_path.join(&repo.owner).join(&repo.repo).exists();
                let _repo_url = repo.url.clone().unwrap_or_else(|| repo_id.clone());
                entries.push(DepPickEntry {
                    display: format!("[linked] {}{}", repo_id, if is_local { " (local)" } else { "" }),
                    action: DepPickAction::RepoOpen {
                        owner: repo.owner.clone(),
                        repo: repo.repo.clone(),
                    },
                });
            }
        }
    }

    let scan = scan_project_files(project_root)?;
    let mut js_deps = BTreeSet::new();
    let mut cargo_deps = BTreeSet::new();
    let mut project_entries = Vec::new();

    for path in scan {
        if path.file_name().and_then(|n| n.to_str()) == Some("package.json") {
            if let Ok(info) = parse_package_json(&path) {
                if let Some(name) = info.name {
                    let dir = path.parent().unwrap_or(&path);
                    if !is_project_root(project_root, dir) {
                        project_entries.push(DepPickEntry {
                            display: format!(
                                "[project] {} ({})",
                                name,
                                path_relative(project_root, dir)
                            ),
                            action: DepPickAction::Project {
                                path: dir.to_path_buf(),
                            },
                        });
                    }
                }
                for dep in info.deps {
                    js_deps.insert((dep, path.parent().unwrap_or(&path).to_path_buf()));
                }
            }
        } else if path.file_name().and_then(|n| n.to_str()) == Some("Cargo.toml") {
            if let Ok(info) = parse_cargo_toml(&path) {
                if let Some(name) = info.name {
                    let dir = path.parent().unwrap_or(&path);
                    if !is_project_root(project_root, dir) {
                        project_entries.push(DepPickEntry {
                            display: format!(
                                "[project] {} ({})",
                                name,
                                path_relative(project_root, dir)
                            ),
                            action: DepPickAction::Project {
                                path: dir.to_path_buf(),
                            },
                        });
                    }
                }
                for dep in info.deps {
                    cargo_deps.insert(dep);
                }
            }
        }
    }

    entries.extend(project_entries);

    let cargo_lock = load_cargo_lock(project_root).unwrap_or_default();
    for (dep, base_dir) in js_deps {
        let repo_url = resolve_js_repo(project_root, &base_dir, &dep);
        if let Some(repo_url) = repo_url {
            let is_local = local_repo_is_present(root_path, &repo_url);
            let label = if is_local { "[linked-js]" } else { "[js]" };
            let display = display_repo(&repo_url);
            let action = if is_local {
                match repos::parse_github_repo(&repo_url) {
                    Ok(repo_ref) => DepPickAction::RepoOpen {
                        owner: repo_ref.owner,
                        repo: repo_ref.repo,
                    },
                    Err(_) => DepPickAction::RepoLink { repo: repo_url.clone() },
                }
            } else {
                DepPickAction::RepoLink { repo: repo_url.clone() }
            };
            entries.push(DepPickEntry {
                display: format!("{} {} -> {}", label, dep, display),
                action,
            });
        } else {
            entries.push(DepPickEntry {
                display: format!("[js] {} (no repo found)", dep),
                action: DepPickAction::Message {
                    text: format!("No repository URL found for {}", dep),
                },
            });
        }
    }

    for dep in cargo_deps {
        let repo_url = resolve_cargo_repo(&cargo_lock, &dep);
        if let Some(repo_url) = repo_url {
            let is_local = local_repo_is_present(root_path, &repo_url);
            let label = if is_local { "[linked-crate]" } else { "[crate]" };
            let display = display_repo(&repo_url);
            let action = if is_local {
                match repos::parse_github_repo(&repo_url) {
                    Ok(repo_ref) => DepPickAction::RepoOpen {
                        owner: repo_ref.owner,
                        repo: repo_ref.repo,
                    },
                    Err(_) => DepPickAction::RepoLink { repo: repo_url.clone() },
                }
            } else {
                DepPickAction::RepoLink { repo: repo_url.clone() }
            };
            entries.push(DepPickEntry {
                display: format!("{} {} -> {}", label, dep, display),
                action,
            });
        } else {
            entries.push(DepPickEntry {
                display: format!("[crate] {} (no repo found)", dep),
                action: DepPickAction::Message {
                    text: format!("No repository URL found for {}", dep),
                },
            });
        }
    }

    Ok(entries)
}

fn load_repo_manifest(project_root: &Path) -> Result<Option<RepoManifest>> {
    let path = project_root.join(".ai").join("repos.toml");
    if !path.exists() {
        return Ok(None);
    }
    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let manifest = toml::from_str::<RepoManifest>(&contents)
        .context("failed to parse .ai/repos.toml")?;
    Ok(Some(manifest))
}

fn scan_project_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let mut builder = WalkBuilder::new(root);
    builder.hidden(false);
    builder.filter_entry(|entry| {
        let name = entry.file_name().to_string_lossy();
        match name.as_ref() {
            ".git" | ".ai" | "node_modules" | "target" | "dist" | "build" | ".next" | ".turbo" => {
                return false;
            }
            _ => {}
        }
        true
    });

    for entry in builder.build() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        if name == "package.json" || name == "Cargo.toml" {
            paths.push(entry.into_path());
        }
    }

    Ok(paths)
}

struct PackageJsonInfo {
    name: Option<String>,
    deps: Vec<String>,
}

fn parse_package_json(path: &Path) -> Result<PackageJsonInfo> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse {}", path.display()))?;

    let name = value.get("name").and_then(|v| v.as_str()).map(|s| s.to_string());
    let mut deps = BTreeSet::new();

    for key in ["dependencies", "devDependencies", "optionalDependencies", "peerDependencies"] {
        if let Some(obj) = value.get(key).and_then(|v| v.as_object()) {
            for dep in obj.keys() {
                deps.insert(dep.to_string());
            }
        }
    }

    Ok(PackageJsonInfo {
        name,
        deps: deps.into_iter().collect(),
    })
}

struct CargoTomlInfo {
    name: Option<String>,
    deps: Vec<String>,
}

fn parse_cargo_toml(path: &Path) -> Result<CargoTomlInfo> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let value: toml::Value = toml::from_str(&contents)
        .with_context(|| format!("failed to parse {}", path.display()))?;

    let name = value
        .get("package")
        .and_then(Value::as_table)
        .and_then(|pkg| pkg.get("name"))
        .and_then(Value::as_str)
        .map(|s| s.to_string());

    let mut deps = BTreeSet::new();
    for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(table) = value.get(key).and_then(Value::as_table) {
            for dep in table.keys() {
                deps.insert(dep.to_string());
            }
        }
    }

    Ok(CargoTomlInfo {
        name,
        deps: deps.into_iter().collect(),
    })
}

#[derive(Default)]
struct CargoLockIndex {
    versions: BTreeMap<String, String>,
    sources: BTreeMap<String, String>,
}

fn load_cargo_lock(project_root: &Path) -> Result<CargoLockIndex> {
    let lock_path = project_root.join("Cargo.lock");
    if !lock_path.exists() {
        return Ok(CargoLockIndex::default());
    }

    let contents = std::fs::read_to_string(&lock_path)
        .with_context(|| format!("failed to read {}", lock_path.display()))?;
    let value: toml::Value = toml::from_str(&contents)
        .with_context(|| format!("failed to parse {}", lock_path.display()))?;

    let mut index = CargoLockIndex::default();
    let packages = value
        .get("package")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    for pkg in packages {
        let table = match pkg.as_table() {
            Some(table) => table,
            None => continue,
        };
        let name = match table.get("name").and_then(Value::as_str) {
            Some(name) => name.to_string(),
            None => continue,
        };
        if let Some(version) = table.get("version").and_then(Value::as_str) {
            index.versions.entry(name.clone()).or_insert_with(|| version.to_string());
        }
        if let Some(source) = table.get("source").and_then(Value::as_str) {
            if source.starts_with("registry+") {
                continue;
            }
            if let Some(url) = normalize_github_url(source) {
                index.sources.entry(name).or_insert(url);
            }
        }
    }

    Ok(index)
}

fn resolve_js_repo(project_root: &Path, base_dir: &Path, dep: &str) -> Option<String> {
    let mut candidates = Vec::new();
    if base_dir.join("node_modules").exists() {
        candidates.push(base_dir.join("node_modules"));
    }
    if project_root.join("node_modules").exists() {
        candidates.push(project_root.join("node_modules"));
    }

    for base in candidates {
        let dep_path = join_node_modules(&base, dep).join("package.json");
        if dep_path.exists() {
            if let Ok(contents) = std::fs::read_to_string(&dep_path) {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&contents) {
                    if let Some(repo) = extract_repo_url(&value) {
                        if let Some(url) = normalize_github_url(&repo) {
                            return Some(url);
                        }
                    }
                }
            }
        }
    }

    None
}

fn resolve_cargo_repo(index: &CargoLockIndex, dep: &str) -> Option<String> {
    if let Some(url) = index.sources.get(dep) {
        return Some(url.clone());
    }

    let version = index.versions.get(dep)?;
    let cargo_home = cargo_home();
    let registry_src = cargo_home.join("registry").join("src");
    let entries = std::fs::read_dir(&registry_src).ok()?;

    for entry in entries.flatten() {
        let candidate = entry
            .path()
            .join(format!("{}-{}", dep, version))
            .join("Cargo.toml");
        if candidate.exists() {
            if let Ok(contents) = std::fs::read_to_string(&candidate) {
                if let Ok(value) = toml::from_str::<toml::Value>(&contents) {
                    if let Some(repo) = value
                        .get("package")
                        .and_then(Value::as_table)
                        .and_then(|pkg| pkg.get("repository"))
                        .and_then(Value::as_str)
                    {
                        if let Some(url) = normalize_github_url(repo) {
                            return Some(url);
                        }
                    }
                }
            }
        }
    }

    None
}

fn cargo_home() -> PathBuf {
    let raw = std::env::var("CARGO_HOME").unwrap_or_else(|_| "~/.cargo".to_string());
    config::expand_path(&raw)
}

fn join_node_modules(base: &Path, dep: &str) -> PathBuf {
    if let Some((scope, name)) = dep.split_once('/') {
        if scope.starts_with('@') {
            return base.join(scope).join(name);
        }
    }
    base.join(dep)
}

fn extract_repo_url(value: &serde_json::Value) -> Option<String> {
    match value.get("repository") {
        Some(serde_json::Value::String(url)) => Some(url.to_string()),
        Some(serde_json::Value::Object(map)) => map
            .get("url")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        _ => None,
    }
}

fn normalize_github_url(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_start_matches("git+");
    let cleaned = trimmed
        .trim_end_matches('/')
        .trim_end_matches(".git");
    if cleaned.contains("crates.io-index") {
        return None;
    }

    if let Ok(repo_ref) = repos::parse_github_repo(cleaned) {
        return Some(format!("https://github.com/{}/{}", repo_ref.owner, repo_ref.repo));
    }
    None
}

fn display_repo(url: &str) -> String {
    if let Ok(repo_ref) = repos::parse_github_repo(url) {
        return format!("{}/{}", repo_ref.owner, repo_ref.repo);
    }
    url.to_string()
}

fn local_repo_is_present(root_path: &Path, url: &str) -> bool {
    if let Ok(repo_ref) = repos::parse_github_repo(url) {
        if root_path.join(repo_ref.owner).join(repo_ref.repo).exists() {
            return true;
        }
    }
    false
}

fn open_in_zed(path: &Path) -> Result<()> {
    let status = Command::new("open")
        .args(["-a", "/Applications/Zed.app"])
        .arg(path)
        .status()
        .context("failed to launch Zed")?;

    if status.success() {
        Ok(())
    } else {
        bail!("failed to open {}", path.display());
    }
}

fn path_relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn is_project_root(root: &Path, candidate: &Path) -> bool {
    let root = root
        .canonicalize()
        .unwrap_or_else(|_| root.to_path_buf());
    let candidate = candidate
        .canonicalize()
        .unwrap_or_else(|_| candidate.to_path_buf());
    root == candidate
}

fn link_repo_dependency(project_root: &Path, repo: &str, root: &str) -> Result<()> {
    let ai_dir = project_root.join(".ai");
    let repos_dir = ai_dir.join("repos");
    std::fs::create_dir_all(&repos_dir)
        .with_context(|| format!("failed to create {}", repos_dir.display()))?;

    let root_path = repos::normalize_root(root)?;
    let repo_ref = if looks_like_repo_ref(repo) {
        repos::parse_github_repo(repo)?
    } else {
        resolve_repo_by_name(&root_path, repo)?
    };

    let target_dir = root_path.join(&repo_ref.owner).join(&repo_ref.repo);
    if !target_dir.exists() {
        let opts = ReposCloneOpts {
            url: repo.to_string(),
            root: root.to_string(),
            full: false,
            no_upstream: false,
            upstream_url: None,
        };
        repos::clone_repo(opts)?;
    } else {
        println!("✓ found repo at {}", target_dir.display());
    }

    let origin_url = format!("git@github.com:{}/{}.git", repo_ref.owner, repo_ref.repo);
    if let Err(err) = maybe_setup_private_origin(&target_dir, &repo_ref, &origin_url) {
        println!("⚠ private origin setup skipped: {}", err);
    }

    let owner_dir = repos_dir.join(&repo_ref.owner);
    std::fs::create_dir_all(&owner_dir)
        .with_context(|| format!("failed to create {}", owner_dir.display()))?;
    let link_path = owner_dir.join(&repo_ref.repo);
    if link_path.exists() {
        println!("✓ link already exists: {}", link_path.display());
    } else {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&target_dir, &link_path)
                .with_context(|| format!("failed to link {}", link_path.display()))?;
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_dir(&target_dir, &link_path)
                .with_context(|| format!("failed to link {}", link_path.display()))?;
        }
        println!("✓ linked {}", link_path.display());
    }

    let manifest_path = ai_dir.join("repos.toml");
    upsert_repo_manifest(&manifest_path, root, &repo_ref, repo)?;

    Ok(())
}

fn looks_like_repo_ref(input: &str) -> bool {
    let trimmed = input.trim();
    trimmed.contains("github.com/")
        || trimmed.starts_with("git@github.com:")
        || trimmed.contains('/')
        || trimmed.ends_with(".git")
}

fn resolve_repo_by_name(root: &Path, name: &str) -> Result<repos::RepoRef> {
    let mut matches = Vec::new();
    let root_entries = std::fs::read_dir(root)
        .with_context(|| format!("failed to read {}", root.display()))?;

    for owner_entry in root_entries.flatten() {
        if !owner_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let owner = owner_entry.file_name().to_string_lossy().to_string();
        let repo_path = owner_entry.path().join(name);
        if repo_path.is_dir() {
            matches.push(repos::RepoRef { owner, repo: name.to_string() });
        }
    }

    if matches.is_empty() {
        bail!(
            "repo '{}' not found under {}. Use owner/repo or run: f repos clone <url>",
            name,
            root.display()
        );
    }

    if matches.len() > 1 {
        let options = matches
            .iter()
            .map(|repo| format!("{}/{}", repo.owner, repo.repo))
            .collect::<Vec<_>>()
            .join(", ");
        bail!("multiple matches for '{}': {}. Use owner/repo.", name, options);
    }

    Ok(matches.remove(0))
}

fn upsert_repo_manifest(path: &Path, root: &str, repo: &repos::RepoRef, url: &str) -> Result<()> {
    let mut doc = if path.exists() {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        toml::from_str::<Value>(&contents).unwrap_or(Value::Table(Map::new()))
    } else {
        Value::Table(Map::new())
    };

    let table = doc.as_table_mut().ok_or_else(|| anyhow::anyhow!("invalid repos.toml"))?;
    table.entry("root".to_string()).or_insert_with(|| Value::String(root.to_string()));

    let repos_value = table.entry("repos".to_string()).or_insert_with(|| Value::Array(Vec::new()));
    let repos_array = repos_value
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("invalid repos list"))?;

    let exists = repos_array.iter().any(|entry| {
        entry.get("owner").and_then(Value::as_str) == Some(repo.owner.as_str())
            && entry.get("repo").and_then(Value::as_str) == Some(repo.repo.as_str())
    });

    if !exists {
        let mut entry = Map::new();
        entry.insert("owner".to_string(), Value::String(repo.owner.clone()));
        entry.insert("repo".to_string(), Value::String(repo.repo.clone()));
        entry.insert("url".to_string(), Value::String(url.to_string()));
        repos_array.push(Value::Table(entry));
    }

    let rendered = toml::to_string_pretty(&doc)?;
    std::fs::write(path, rendered).with_context(|| format!("failed to write {}", path.display()))?;
    println!("✓ updated {}", path.display());
    Ok(())
}

fn maybe_setup_private_origin(
    repo_dir: &Path,
    repo_ref: &repos::RepoRef,
    origin_url: &str,
) -> Result<()> {
    if !gh_available() {
        return Ok(());
    }

    if !gh_authenticated()? {
        println!("gh not authenticated; skipping private origin setup");
        println!("Authenticate with: gh auth login");
        return Ok(());
    }

    let gh_user = gh_username()?;
    if gh_user.is_empty() || repo_ref.owner == gh_user {
        return Ok(());
    }

    if !repo_dir.join(".git").exists() {
        return Ok(());
    }

    let origin_remote = git_remote_get(repo_dir, "origin")?;
    if let Some(origin_remote) = origin_remote {
        if origin_remote.contains(&format!("github.com:{}/", gh_user))
            || origin_remote.contains(&format!("github.com/{}/", gh_user))
        {
            return Ok(());
        }
    }

    let private_repo = format!("{}/{}", gh_user, repo_ref.repo);
    let private_url = format!("git@github.com:{}.git", private_repo);

    if !gh_repo_exists(&private_repo)? {
        println!("Creating private repo: {}", private_repo);
        let status = Command::new("gh")
            .args(["repo", "create", &private_repo, "--private"])
            .status()
            .context("failed to create private repo")?;
        if !status.success() {
            bail!("failed to create private repo {}", private_repo);
        }
    }

    set_origin_remote(repo_dir, &private_url)?;
    let upstream_remote = git_remote_get(repo_dir, "upstream")?;
    if upstream_remote.is_none() {
        configure_upstream(repo_dir, origin_url)?;
    } else if upstream_remote.as_deref() != Some(origin_url) {
        println!(
            "⚠ upstream already set to {} (expected {})",
            upstream_remote.unwrap_or_default(),
            origin_url
        );
    }
    println!("✓ origin -> {}", private_repo);

    Ok(())
}

fn gh_available() -> bool {
    Command::new("gh").arg("--version").output().is_ok()
}

fn gh_authenticated() -> Result<bool> {
    let status = Command::new("gh").args(["auth", "status"]).output()?;
    Ok(status.status.success())
}

fn gh_username() -> Result<String> {
    let output = Command::new("gh")
        .args(["api", "user", "-q", ".login"])
        .output()
        .context("failed to get GitHub username")?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn gh_repo_exists(full_name: &str) -> Result<bool> {
    let output = Command::new("gh")
        .args(["repo", "view", full_name])
        .output()
        .context("failed to check repo")?;
    Ok(output.status.success())
}

fn git_remote_get(repo_dir: &Path, name: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .args(["remote", "get-url", name])
        .current_dir(repo_dir)
        .output();

    let output = match output {
        Ok(output) if output.status.success() => output,
        _ => return Ok(None),
    };

    Ok(Some(String::from_utf8_lossy(&output.stdout).trim().to_string()))
}

fn set_origin_remote(repo_dir: &Path, url: &str) -> Result<()> {
    if git_remote_get(repo_dir, "origin")?.is_some() {
        Command::new("git")
            .args(["remote", "set-url", "origin", url])
            .current_dir(repo_dir)
            .status()
            .context("failed to set origin")?;
    } else {
        Command::new("git")
            .args(["remote", "add", "origin", url])
            .current_dir(repo_dir)
            .status()
            .context("failed to add origin")?;
    }
    Ok(())
}

fn configure_upstream(repo_dir: &Path, upstream_url: &str) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to capture current directory")?;
    std::env::set_current_dir(repo_dir)
        .with_context(|| format!("failed to enter {}", repo_dir.display()))?;

    let result = upstream::setup_upstream_with_depth(Some(upstream_url), None, None);

    if let Err(err) = std::env::set_current_dir(&cwd) {
        println!("warning: failed to restore working directory: {}", err);
    }

    result
}
