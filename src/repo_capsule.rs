use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::cli::{RepoAliasAction, RepoAliasCommand, RepoCapsuleOpts};
use crate::{config, project_snapshot};

const DEFAULT_STORE_DIR: &str = "~/repos/garden-co/jazz2/.jazz2/flow-repo-capsules";
const STORE_DIR_ENV: &str = "FLOW_REPO_CAPSULE_STORE";
const CAPSULE_VERSION: u32 = 1;
const REGISTRY_FILE: &str = "repo-aliases.json";
const DEFAULT_SHELF_CONFIG: &str = "~/.agents/shelf/config.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepoCapsule {
    pub version: u32,
    pub repo_root: String,
    pub repo_name: String,
    pub repo_id: String,
    pub origin_url: Option<String>,
    pub summary: String,
    pub languages: Vec<String>,
    pub manifests: Vec<String>,
    pub commands: Vec<String>,
    pub important_paths: Vec<String>,
    pub docs_hints: Vec<String>,
    pub updated_at_unix: u64,
    watched: Vec<PathStamp>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PathStamp {
    path: String,
    exists: bool,
    len: u64,
    modified_sec: u64,
    modified_nsec: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepoCapsuleReference {
    pub matched: String,
    pub repo_root: String,
    pub output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepoAliasEntry {
    pub alias: String,
    pub path: String,
    pub source: String,
    pub updated_at_unix: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct RepoAliasRegistry {
    version: u32,
    aliases: Vec<RepoAliasEntry>,
}

#[derive(Debug, Clone, Serialize)]
struct RepoAliasImportSummary {
    imported: usize,
    skipped: usize,
    aliases: Vec<RepoAliasEntry>,
}

#[derive(Debug, Deserialize)]
struct ShelfConfigFile {
    #[serde(default)]
    repos: Vec<ShelfRepoEntry>,
}

#[derive(Debug, Deserialize)]
struct ShelfRepoEntry {
    alias: String,
}

pub fn run_capsule(opts: RepoCapsuleOpts) -> Result<()> {
    let target = resolve_target_path(opts.path.as_deref())?;
    let capsule = if opts.refresh {
        refresh_capsule_for_path(&target)?
    } else {
        load_or_refresh_capsule_for_path(&target)?
    };

    if opts.json {
        println!("{}", serde_json::to_string_pretty(&capsule)?);
    } else {
        print!("{}", render_capsule_report(&capsule));
    }
    Ok(())
}

pub fn run_alias(cmd: RepoAliasCommand) -> Result<()> {
    match cmd.action.unwrap_or(RepoAliasAction::List { json: false }) {
        RepoAliasAction::List { json } => {
            let aliases = list_aliases()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&aliases)?);
            } else if aliases.is_empty() {
                println!("No repo aliases registered.");
            } else {
                for entry in aliases {
                    println!("{} -> {} ({})", entry.alias, entry.path, entry.source);
                }
            }
        }
        RepoAliasAction::Set { alias, path, json } => {
            let entry = set_alias(&alias, &path, "manual")?;
            if json {
                println!("{}", serde_json::to_string_pretty(&entry)?);
            } else {
                println!("{} -> {}", entry.alias, entry.path);
            }
        }
        RepoAliasAction::Remove { alias } => {
            remove_alias(&alias)?;
            println!("Removed alias {}", normalize_alias(&alias));
        }
        RepoAliasAction::ImportShelf { config, json } => {
            let summary = import_shelf_aliases(config.as_deref())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&summary)?);
            } else {
                println!(
                    "Imported {} alias(es), skipped {}.",
                    summary.imported, summary.skipped
                );
                for entry in summary.aliases {
                    println!("{} -> {} ({})", entry.alias, entry.path, entry.source);
                }
            }
        }
    }
    Ok(())
}

pub fn load_or_refresh_capsule_for_path(path: &Path) -> Result<RepoCapsule> {
    let root = resolve_reference_root(path)?;
    load_or_refresh_capsule_for_root(&storage_dir(), &root)
}

pub fn refresh_capsule_for_path(path: &Path) -> Result<RepoCapsule> {
    let root = resolve_reference_root(path)?;
    refresh_capsule_for_root(&storage_dir(), &root)
}

pub fn list_aliases() -> Result<Vec<RepoAliasEntry>> {
    let mut aliases = load_alias_registry(&storage_dir())?.aliases;
    aliases.sort_by(|a, b| a.alias.cmp(&b.alias));
    Ok(aliases)
}

pub fn set_alias(alias: &str, path: &str, source: &str) -> Result<RepoAliasEntry> {
    set_alias_in_store(&storage_dir(), alias, path, source)
}

fn set_alias_in_store(
    store_dir: &Path,
    alias: &str,
    path: &str,
    source: &str,
) -> Result<RepoAliasEntry> {
    let target = resolve_target_path(Some(path))?;
    let root = resolve_reference_root(&target)?;
    let _ = load_or_refresh_capsule_for_root(store_dir, &root)?;
    let mut registry = load_alias_registry(store_dir)?;
    let entry = RepoAliasEntry {
        alias: normalize_alias(alias),
        path: root.display().to_string(),
        source: source.to_string(),
        updated_at_unix: now_unix(),
    };
    registry.aliases.retain(|value| value.alias != entry.alias);
    registry.aliases.push(entry.clone());
    save_alias_registry(store_dir, &registry)?;
    Ok(entry)
}

pub fn remove_alias(alias: &str) -> Result<()> {
    let store_dir = storage_dir();
    let mut registry = load_alias_registry(&store_dir)?;
    registry
        .aliases
        .retain(|value| value.alias != normalize_alias(alias));
    save_alias_registry(&store_dir, &registry)
}

fn import_shelf_aliases(config_path: Option<&str>) -> Result<RepoAliasImportSummary> {
    let config_path = config::expand_path(config_path.unwrap_or(DEFAULT_SHELF_CONFIG));
    import_shelf_aliases_into_store(&storage_dir(), &config_path)
}

fn import_shelf_aliases_into_store(
    store_dir: &Path,
    config_path: &Path,
) -> Result<RepoAliasImportSummary> {
    let payload = fs::read_to_string(&config_path)
        .with_context(|| format!("read {}", config_path.display()))?;
    let parsed =
        serde_json::from_str::<ShelfConfigFile>(&payload).context("parse Shelf config JSON")?;
    let shelf_repos_dir = config_path
        .parent()
        .map(|value| value.join("repos"))
        .unwrap_or_else(|| config::expand_path("~/.agents/shelf/repos"));

    let mut imported = Vec::new();
    let mut skipped = 0usize;
    for repo in parsed.repos {
        let alias = normalize_alias(&repo.alias);
        let path = shelf_repos_dir.join(&alias);
        if !path.exists() {
            skipped += 1;
            continue;
        }
        match set_alias_in_store(store_dir, &alias, &path.display().to_string(), "shelf") {
            Ok(entry) => imported.push(entry),
            Err(_) => skipped += 1,
        }
    }

    Ok(RepoAliasImportSummary {
        imported: imported.len(),
        skipped,
        aliases: imported,
    })
}

pub fn resolve_reference_candidates(
    target_path: &Path,
    query_text: &str,
    candidates: &[String],
    limit: usize,
) -> Result<Vec<RepoCapsuleReference>> {
    let store_dir = storage_dir();
    let registry = load_alias_registry(&store_dir)?;
    let mut seen_roots = BTreeSet::new();
    let mut matches = Vec::new();

    for candidate in candidates {
        if matches.len() >= limit {
            break;
        }
        let Some(root) =
            resolve_reference_candidate_root(target_path, query_text, candidate, &registry)
        else {
            continue;
        };
        let root_key = root.display().to_string();
        if !seen_roots.insert(root_key) {
            continue;
        }
        let capsule = load_or_refresh_capsule_for_root(&store_dir, &root)?;
        matches.push(RepoCapsuleReference {
            matched: candidate.clone(),
            repo_root: capsule.repo_root.clone(),
            output: render_reference_output(&capsule, candidate),
        });
    }

    Ok(matches)
}

fn load_or_refresh_capsule_for_root(store_dir: &Path, root: &Path) -> Result<RepoCapsule> {
    if let Some(existing) = load_capsule(store_dir, root)? {
        if capsule_is_fresh(&existing) {
            return Ok(existing);
        }
    }
    refresh_capsule_for_root(store_dir, root)
}

fn refresh_capsule_for_root(store_dir: &Path, root: &Path) -> Result<RepoCapsule> {
    let capsule = build_capsule(root)?;
    save_capsule(store_dir, &capsule)?;
    Ok(capsule)
}

fn resolve_target_path(path: Option<&str>) -> Result<PathBuf> {
    let base = match path.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => config::expand_path(value),
        None => std::env::current_dir().context("read current dir")?,
    };
    Ok(base.canonicalize().unwrap_or(base))
}

fn resolve_reference_root(path: &Path) -> Result<PathBuf> {
    let Some(root) = detect_reference_root(path) else {
        bail!("no repo or flow project found for {}", path.display());
    };
    Ok(root)
}

fn resolve_candidate_root(target_path: &Path, candidate: &str) -> Option<PathBuf> {
    let trimmed = candidate.trim();
    if trimmed.is_empty() {
        return None;
    }

    let expanded = if trimmed.starts_with("~/") {
        config::expand_path(trimmed)
    } else if Path::new(trimmed).is_absolute() {
        PathBuf::from(trimmed)
    } else if trimmed.starts_with("./") || trimmed.starts_with("../") {
        target_path.join(trimmed)
    } else {
        return None;
    };

    if !expanded.exists() {
        return None;
    }
    detect_reference_root(&expanded)
}

fn resolve_reference_candidate_root(
    target_path: &Path,
    query_text: &str,
    candidate: &str,
    registry: &RepoAliasRegistry,
) -> Option<PathBuf> {
    if looks_like_local_path(candidate) {
        return resolve_candidate_root(target_path, candidate);
    }

    let alias = normalize_alias(candidate);
    let entry = registry.aliases.iter().find(|value| value.alias == alias)?;
    if !alias_reference_allowed(query_text, &entry.alias) {
        return None;
    }

    let path = PathBuf::from(&entry.path);
    if !path.exists() {
        return None;
    }
    detect_reference_root(&path)
}

fn alias_reference_allowed(query_text: &str, alias: &str) -> bool {
    let normalized_query = query_text.to_ascii_lowercase();
    let alias = normalize_alias(alias);
    if normalized_query.trim() == alias {
        return true;
    }

    let cue_prefixes = [
        "see ", "in ", "from ", "using ", "compare ", "inspect ", "study ", "read ", "use ",
        "open ",
    ];
    cue_prefixes.iter().any(|prefix| {
        normalized_query.contains(&format!("{prefix}{alias}"))
            || normalized_query.contains(&format!("{prefix}{alias} "))
            || normalized_query.contains(&format!("{prefix}{alias},"))
    })
}

fn normalize_alias(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn detect_reference_root(path: &Path) -> Option<PathBuf> {
    let base = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()?.to_path_buf()
    };

    if let Some(root) = find_git_root(&base) {
        return Some(root.canonicalize().unwrap_or(root));
    }
    if let Some(flow_toml) = project_snapshot::find_flow_toml_upwards(&base) {
        let root = flow_toml.parent().unwrap_or(Path::new(".")).to_path_buf();
        return Some(root.canonicalize().unwrap_or(root));
    }
    if base.exists() {
        return Some(base.canonicalize().unwrap_or(base));
    }
    None
}

fn looks_like_local_path(candidate: &str) -> bool {
    let trimmed = candidate.trim();
    trimmed.starts_with("~/")
        || trimmed.starts_with('/')
        || trimmed.starts_with("./")
        || trimmed.starts_with("../")
}

fn find_git_root(start: &Path) -> Option<PathBuf> {
    let mut current = if start.is_dir() {
        start.to_path_buf()
    } else {
        start.parent()?.to_path_buf()
    };
    loop {
        let dot_git = current.join(".git");
        if dot_git.is_dir() || dot_git.is_file() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn build_capsule(root: &Path) -> Result<RepoCapsule> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let repo_root = root.display().to_string();
    let repo_name = root
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("repo")
        .to_string();
    let origin_url = read_origin_url(&root);
    let repo_id = infer_repo_id(&root, origin_url.as_deref());
    let manifests = detect_manifests(&root);
    let languages = detect_languages(&root, &manifests);
    let commands = detect_commands(&root, &manifests);
    let important_paths = detect_important_paths(&root);
    let docs_hints = detect_docs_hints(&root);
    let summary = build_summary(&repo_id, &languages, &manifests, &commands, &docs_hints);
    let watched = collect_watched_stamps(&root);
    let updated_at_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0);

    Ok(RepoCapsule {
        version: CAPSULE_VERSION,
        repo_root,
        repo_name,
        repo_id,
        origin_url,
        summary,
        languages,
        manifests,
        commands,
        important_paths,
        docs_hints,
        updated_at_unix,
        watched,
    })
}

fn build_summary(
    repo_id: &str,
    languages: &[String],
    manifests: &[String],
    commands: &[String],
    docs_hints: &[String],
) -> String {
    let mut parts = vec![repo_id.to_string()];
    if !languages.is_empty() {
        parts.push(format!("languages: {}", languages.join(", ")));
    }
    if !manifests.is_empty() {
        parts.push(format!(
            "manifests: {}",
            manifests
                .iter()
                .take(4)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !commands.is_empty() {
        parts.push(format!(
            "commands: {}",
            commands
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(hint) = docs_hints.first() {
        parts.push(format!("note: {}", trim_chars(hint, 120)));
    }
    trim_chars(&parts.join(" | "), 360)
}

fn detect_manifests(root: &Path) -> Vec<String> {
    let candidates = [
        "flow.toml",
        "package.json",
        "Cargo.toml",
        "pyproject.toml",
        "go.mod",
        "justfile",
        "Justfile",
        "Makefile",
        "flake.nix",
        "wrangler.toml",
        "wrangler.json",
        "wrangler.jsonc",
        "uv.lock",
        "pnpm-lock.yaml",
        "bun.lockb",
        "bun.lock",
    ];
    candidates
        .into_iter()
        .filter(|candidate| root.join(candidate).exists())
        .map(|value| value.to_string())
        .collect()
}

fn detect_languages(root: &Path, manifests: &[String]) -> Vec<String> {
    let mut langs = BTreeSet::new();
    let manifests_set: BTreeSet<_> = manifests.iter().map(String::as_str).collect();

    if manifests_set.contains("Cargo.toml") {
        langs.insert("Rust".to_string());
    }
    if manifests_set.contains("package.json")
        || manifests_set.contains("bun.lockb")
        || manifests_set.contains("bun.lock")
        || root.join("tsconfig.json").exists()
    {
        langs.insert("TypeScript/JavaScript".to_string());
    }
    if manifests_set.contains("pyproject.toml") || manifests_set.contains("uv.lock") {
        langs.insert("Python".to_string());
    }
    if manifests_set.contains("go.mod") {
        langs.insert("Go".to_string());
    }
    if manifests_set.contains("flake.nix") {
        langs.insert("Nix".to_string());
    }
    if root.join("moon.mod.json").exists() {
        langs.insert("MoonBit".to_string());
    }
    langs.into_iter().collect()
}

fn detect_commands(root: &Path, manifests: &[String]) -> Vec<String> {
    let mut commands = Vec::new();
    let manifests_set: BTreeSet<_> = manifests.iter().map(String::as_str).collect();

    for task in read_flow_task_names(&root.join("flow.toml"))
        .into_iter()
        .take(4)
    {
        commands.push(format!("f {}", task));
    }

    for script in read_package_scripts(&root.join("package.json"))
        .into_iter()
        .take(4)
    {
        commands.push(format!("npm run {}", script));
    }

    if manifests_set.contains("Cargo.toml") {
        commands.push("cargo test".to_string());
        commands.push("cargo build".to_string());
    }
    if manifests_set.contains("pyproject.toml") || manifests_set.contains("uv.lock") {
        commands.push("uv run pytest".to_string());
    }
    if manifests_set.contains("go.mod") {
        commands.push("go test ./...".to_string());
    }
    if manifests_set.contains("flake.nix") {
        commands.push("nix develop".to_string());
    }

    dedupe_preserving_order(commands)
        .into_iter()
        .take(6)
        .collect()
}

fn read_flow_task_names(path: &Path) -> Vec<String> {
    let Ok(content) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(value) = toml::from_str::<toml::Value>(&content) else {
        return Vec::new();
    };
    value
        .get("tasks")
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .filter_map(|task| {
            task.get("name")
                .and_then(|value| value.as_str())
                .map(|value| value.to_string())
        })
        .collect()
}

fn read_package_scripts(path: &Path) -> Vec<String> {
    let Ok(content) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
        return Vec::new();
    };
    let Some(scripts) = value.get("scripts").and_then(|value| value.as_object()) else {
        return Vec::new();
    };

    let preferred = ["dev", "start", "test", "build", "lint", "typecheck"];
    let mut names = Vec::new();
    for name in preferred {
        if scripts.contains_key(name) {
            names.push(name.to_string());
        }
    }
    for name in scripts.keys() {
        if !names.iter().any(|existing| existing == name) {
            names.push(name.to_string());
        }
    }
    names
}

fn detect_important_paths(root: &Path) -> Vec<String> {
    let candidates = [
        "flow.toml",
        "README.md",
        "README.mdx",
        "AGENTS.md",
        "agents.md",
        "package.json",
        "Cargo.toml",
        "pyproject.toml",
        "docs",
        "src",
        "apps",
        "crates",
        "packages",
        "workers",
    ];

    candidates
        .into_iter()
        .filter(|candidate| root.join(candidate).exists())
        .map(|value| value.to_string())
        .take(8)
        .collect()
}

fn detect_docs_hints(root: &Path) -> Vec<String> {
    let mut hints = Vec::new();

    for (label, path) in [
        ("AGENTS", root.join("AGENTS.md")),
        ("AGENTS", root.join("agents.md")),
        ("README", root.join("README.md")),
        ("README", root.join("README.mdx")),
        ("README", root.join("readme.md")),
        ("README", root.join("readme.mdx")),
    ] {
        if let Some(hint) = read_text_hint(&path, label) {
            hints.push(hint);
            break;
        }
    }

    if let Some(hint) = read_docs_index_hint(&root.join("docs")) {
        hints.push(hint);
    }

    hints.into_iter().take(3).collect()
}

fn read_text_hint(path: &Path, label: &str) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    let mut lines = Vec::new();
    let mut in_code_block = false;
    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.starts_with("```") {
            in_code_block = !in_code_block;
            continue;
        }
        if in_code_block || line.is_empty() {
            continue;
        }
        if matches!(line, "---" | "+++") || line.starts_with("title:") {
            continue;
        }
        let normalized = line.trim_start_matches('#').trim_start_matches('-').trim();
        if normalized.is_empty()
            || normalized.starts_with('<')
            || normalized.starts_with('[')
            || normalized.eq_ignore_ascii_case("instructions")
        {
            continue;
        }
        lines.push(normalized.to_string());
        if lines.len() >= 3 {
            break;
        }
    }

    if lines.is_empty() {
        None
    } else {
        Some(format!("{label}: {}", trim_chars(&lines.join(" "), 220)))
    }
}

fn read_docs_index_hint(docs_dir: &Path) -> Option<String> {
    if !docs_dir.is_dir() {
        return None;
    }

    let mut names = fs::read_dir(docs_dir)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let ext = path.extension()?.to_str()?;
            if !matches!(ext, "md" | "mdx") {
                return None;
            }
            path.file_name()
                .and_then(|value| value.to_str())
                .map(|value| value.to_string())
        })
        .collect::<Vec<_>>();
    names.sort();
    names.truncate(5);
    if names.is_empty() {
        None
    } else {
        Some(format!("Docs: {}", names.join(", ")))
    }
}

fn collect_watched_stamps(root: &Path) -> Vec<PathStamp> {
    watched_paths(root)
        .into_iter()
        .map(|path| stamp_path(&path))
        .collect()
}

fn watched_paths(root: &Path) -> Vec<PathBuf> {
    let mut paths = vec![root.to_path_buf()];
    for candidate in [
        "flow.toml",
        "README.md",
        "README.mdx",
        "readme.md",
        "readme.mdx",
        "AGENTS.md",
        "agents.md",
        "package.json",
        "Cargo.toml",
        "pyproject.toml",
        "go.mod",
        "justfile",
        "Justfile",
        "Makefile",
        "flake.nix",
        "docs/README.md",
        "docs/index.md",
        "docs/index.mdx",
    ] {
        paths.push(root.join(candidate));
    }
    paths
}

fn stamp_path(path: &Path) -> PathStamp {
    let metadata = fs::metadata(path).ok();
    let exists = metadata.is_some();
    let (len, modified_sec, modified_nsec) = metadata
        .and_then(|meta| {
            let modified = meta.modified().ok()?;
            let duration = modified.duration_since(UNIX_EPOCH).ok()?;
            Some((meta.len(), duration.as_secs(), duration.subsec_nanos()))
        })
        .unwrap_or((0, 0, 0));

    PathStamp {
        path: path.display().to_string(),
        exists,
        len,
        modified_sec,
        modified_nsec,
    }
}

fn capsule_is_fresh(capsule: &RepoCapsule) -> bool {
    capsule
        .watched
        .iter()
        .all(|stamp| stamp_path(Path::new(&stamp.path)) == *stamp)
}

fn render_reference_output(capsule: &RepoCapsule, matched: &str) -> String {
    let mut lines = vec![format!("Repo reference: {}", matched)];
    lines.push(format!("- Repo: {}", capsule.repo_id));
    lines.push(format!("- Root: {}", capsule.repo_root));
    if let Some(origin) = capsule.origin_url.as_deref() {
        lines.push(format!("- Remote: {}", origin));
    }
    if !capsule.languages.is_empty() {
        lines.push(format!("- Languages: {}", capsule.languages.join(", ")));
    }
    if !capsule.commands.is_empty() {
        lines.push(format!(
            "- Common commands: {}",
            capsule
                .commands
                .iter()
                .take(4)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !capsule.important_paths.is_empty() {
        lines.push(format!(
            "- Important paths: {}",
            capsule
                .important_paths
                .iter()
                .take(5)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    for hint in capsule.docs_hints.iter().take(2) {
        lines.push(format!("- {}", hint));
    }
    lines.join("\n")
}

fn render_capsule_report(capsule: &RepoCapsule) -> String {
    let mut out = String::new();
    out.push_str(&format!("Repo capsule: {}\n", capsule.repo_id));
    out.push_str(&format!("root: {}\n", capsule.repo_root));
    if let Some(origin) = capsule.origin_url.as_deref() {
        out.push_str(&format!("origin: {}\n", origin));
    }
    out.push_str(&format!("summary: {}\n", capsule.summary));
    if !capsule.languages.is_empty() {
        out.push_str(&format!("languages: {}\n", capsule.languages.join(", ")));
    }
    if !capsule.manifests.is_empty() {
        out.push_str(&format!("manifests: {}\n", capsule.manifests.join(", ")));
    }
    if !capsule.commands.is_empty() {
        out.push_str(&format!("commands: {}\n", capsule.commands.join(", ")));
    }
    if !capsule.important_paths.is_empty() {
        out.push_str("important_paths:\n");
        for path in &capsule.important_paths {
            out.push_str(&format!("- {}\n", path));
        }
    }
    if !capsule.docs_hints.is_empty() {
        out.push_str("notes:\n");
        for hint in &capsule.docs_hints {
            out.push_str(&format!("- {}\n", hint));
        }
    }
    out
}

fn infer_repo_id(root: &Path, origin_url: Option<&str>) -> String {
    if let Some(origin) = origin_url
        && let Some(id) = parse_repo_id_from_remote(origin)
    {
        return id;
    }

    let name = root
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("repo");
    if let Some(parent) = root
        .parent()
        .and_then(|value| value.file_name())
        .and_then(|value| value.to_str())
    {
        return format!("{}/{}", parent, name);
    }
    name.to_string()
}

fn parse_repo_id_from_remote(remote: &str) -> Option<String> {
    let trimmed = remote.trim().trim_end_matches(".git");
    if let Some(rest) = trimmed.strip_prefix("git@github.com:") {
        return Some(rest.to_string());
    }
    if let Some(rest) = trimmed.strip_prefix("https://github.com/") {
        return Some(rest.to_string());
    }
    if let Some(rest) = trimmed.strip_prefix("ssh://git@github.com/") {
        return Some(rest.to_string());
    }
    None
}

fn read_origin_url(root: &Path) -> Option<String> {
    let git_dir = resolve_git_dir(root)?;
    let common_dir = resolve_common_git_dir(&git_dir);
    let config_path = common_dir.join("config");
    parse_git_remote_url(&config_path, "origin")
}

fn resolve_git_dir(root: &Path) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    let content = fs::read_to_string(&dot_git).ok()?;
    let gitdir = content.strip_prefix("gitdir:")?.trim();
    let path = PathBuf::from(gitdir);
    let resolved = if path.is_absolute() {
        path
    } else {
        dot_git.parent()?.join(path)
    };
    Some(resolved.canonicalize().unwrap_or(resolved))
}

fn resolve_common_git_dir(git_dir: &Path) -> PathBuf {
    let commondir = git_dir.join("commondir");
    let Ok(content) = fs::read_to_string(&commondir) else {
        return git_dir.to_path_buf();
    };
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return git_dir.to_path_buf();
    }
    let path = PathBuf::from(trimmed);
    let resolved = if path.is_absolute() {
        path
    } else {
        git_dir.join(path)
    };
    resolved.canonicalize().unwrap_or(resolved)
}

fn parse_git_remote_url(config_path: &Path, remote_name: &str) -> Option<String> {
    let content = fs::read_to_string(config_path).ok()?;
    let mut in_remote = false;
    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_remote = parse_remote_section(line)
                .is_some_and(|value| value.eq_ignore_ascii_case(remote_name));
            continue;
        }
        if !in_remote {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("url") {
            return Some(value.trim().to_string());
        }
    }
    None
}

fn parse_remote_section(section: &str) -> Option<String> {
    let inner = section.strip_prefix('[')?.strip_suffix(']')?.trim();
    let rest = inner.strip_prefix("remote")?.trim();
    let name = rest.strip_prefix('"')?.strip_suffix('"')?.trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

fn dedupe_preserving_order(values: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut deduped = Vec::new();
    for value in values {
        if seen.insert(value.clone()) {
            deduped.push(value);
        }
    }
    deduped
}

fn trim_chars(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_string();
    }
    let keep = limit.saturating_sub(3);
    value.chars().take(keep).collect::<String>() + "..."
}

fn storage_dir() -> PathBuf {
    if let Ok(path) = std::env::var(STORE_DIR_ENV) {
        return config::expand_path(&path);
    }
    config::expand_path(DEFAULT_STORE_DIR)
}

fn registry_path(store_dir: &Path) -> PathBuf {
    store_dir.join(REGISTRY_FILE)
}

fn load_alias_registry(store_dir: &Path) -> Result<RepoAliasRegistry> {
    let path = registry_path(store_dir);
    if !path.exists() {
        return Ok(RepoAliasRegistry {
            version: 1,
            aliases: Vec::new(),
        });
    }

    let payload = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str::<RepoAliasRegistry>(&payload)
        .with_context(|| format!("parse {}", path.display()))
}

fn save_alias_registry(store_dir: &Path, registry: &RepoAliasRegistry) -> Result<()> {
    fs::create_dir_all(store_dir)
        .with_context(|| format!("create store dir {}", store_dir.display()))?;
    let path = registry_path(store_dir);
    let payload = serde_json::to_string_pretty(registry)?;
    fs::write(&path, payload).with_context(|| format!("write {}", path.display()))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0)
}

fn save_capsule(store_dir: &Path, capsule: &RepoCapsule) -> Result<()> {
    fs::create_dir_all(store_dir)
        .with_context(|| format!("create store dir {}", store_dir.display()))?;
    let path = capsule_path(store_dir, &capsule.repo_root);
    let payload = serde_json::to_string_pretty(capsule)?;
    fs::write(&path, payload).with_context(|| format!("write {}", path.display()))
}

fn load_capsule(store_dir: &Path, root: &Path) -> Result<Option<RepoCapsule>> {
    if !store_dir.exists() {
        return Ok(None);
    }

    let path = capsule_path(store_dir, &root.display().to_string());
    if !path.exists() {
        return Ok(None);
    }

    let payload = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let capsule = serde_json::from_str::<RepoCapsule>(&payload)
        .with_context(|| format!("parse {}", path.display()))?;
    Ok(Some(capsule))
}

fn capsule_path(store_dir: &Path, repo_root: &str) -> PathBuf {
    let hash = blake3::hash(repo_root.as_bytes()).to_hex().to_string();
    store_dir.join(format!("{hash}.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn build_capsule_captures_repo_shape() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path().join("owner").join("repo");
        fs::create_dir_all(root.join(".git")).expect("create .git");
        fs::write(
            root.join("README.md"),
            "# Repo\n\nFast TypeScript service\n",
        )
        .expect("write readme");
        fs::write(
            root.join("AGENTS.md"),
            "Use flow tasks first.\nKeep changes small.\n",
        )
        .expect("write agents");
        fs::write(
            root.join("flow.toml"),
            "[[tasks]]\nname = \"dev\"\ncommand = \"bun run dev\"\n\n[[tasks]]\nname = \"test\"\ncommand = \"bun test\"\n",
        )
        .expect("write flow");
        fs::write(
            root.join("package.json"),
            r#"{"name":"repo","scripts":{"dev":"vite","test":"vitest","build":"tsc -b"}}"#,
        )
        .expect("write package");

        let capsule = build_capsule(&root).expect("build capsule");
        assert_eq!(capsule.repo_id, "owner/repo");
        assert!(
            capsule
                .languages
                .iter()
                .any(|value| value == "TypeScript/JavaScript")
        );
        assert!(capsule.commands.iter().any(|value| value == "f dev"));
        assert!(capsule.commands.iter().any(|value| value == "npm run test"));
        assert!(
            capsule
                .docs_hints
                .iter()
                .any(|value| value.starts_with("AGENTS:"))
        );
    }

    #[test]
    fn load_or_refresh_capsule_reuses_fresh_store() {
        let dir = tempdir().expect("tempdir");
        let store = dir.path().join("store");
        let root = dir.path().join("repo");
        fs::create_dir_all(root.join(".git")).expect("create .git");
        fs::write(root.join("README.md"), "# Repo\n\nhello\n").expect("write readme");

        let first = load_or_refresh_capsule_for_root(&store, &root).expect("first load");
        let second = load_or_refresh_capsule_for_root(&store, &root).expect("second load");

        assert_eq!(first.repo_root, second.repo_root);
        assert_eq!(first.updated_at_unix, second.updated_at_unix);
    }

    #[test]
    fn resolve_reference_candidates_finds_repo_paths() {
        let dir = tempdir().expect("tempdir");
        let store = dir.path().join("store");
        let target = dir.path().join("target");
        let repo = dir.path().join("external");
        fs::create_dir_all(&target).expect("create target");
        fs::create_dir_all(repo.join(".git")).expect("create .git");
        fs::write(repo.join("README.md"), "# External\n\ncompare this repo\n")
            .expect("write readme");

        let candidates = vec![repo.display().to_string()];
        let matches = resolve_reference_candidates_with_store(
            &store,
            &target,
            "see external and compare",
            &candidates,
            2,
        )
        .expect("resolve refs");
        assert_eq!(matches.len(), 1);
        assert!(matches[0].output.contains("Repo reference:"));
    }

    #[test]
    fn resolve_reference_candidates_finds_registered_aliases() {
        let dir = tempdir().expect("tempdir");
        let store = dir.path().join("store");
        let target = dir.path().join("target");
        let repo = dir.path().join("Effect-TS").join("effect-smol");
        fs::create_dir_all(&target).expect("create target");
        fs::create_dir_all(repo.join(".git")).expect("create .git");
        fs::write(repo.join("README.md"), "# effect-smol\n\nsmall repo\n").expect("write readme");

        let entry = RepoAliasEntry {
            alias: "effect-smol".to_string(),
            path: repo.display().to_string(),
            source: "manual".to_string(),
            updated_at_unix: 1,
        };
        save_alias_registry(
            &store,
            &RepoAliasRegistry {
                version: 1,
                aliases: vec![entry],
            },
        )
        .expect("save registry");

        let candidates = vec!["effect-smol".to_string()];
        let matches = resolve_reference_candidates_with_store(
            &store,
            &target,
            "see effect-smol and compare architecture",
            &candidates,
            2,
        )
        .expect("resolve alias refs");
        assert_eq!(matches.len(), 1);
        assert!(matches[0].output.contains("effect-smol"));
    }

    #[test]
    fn import_shelf_aliases_loads_sibling_repos_dir() {
        let dir = tempdir().expect("tempdir");
        let store = dir.path().join("store");
        let shelf = dir.path().join("shelf");
        let repos_dir = shelf.join("repos");
        let repo = repos_dir.join("effect-smol");
        fs::create_dir_all(repo.join(".git")).expect("create .git");
        fs::write(repo.join("README.md"), "# effect-smol\n\nsmall repo\n").expect("write readme");
        fs::create_dir_all(&shelf).expect("create shelf");
        fs::write(
            shelf.join("config.json"),
            r#"{"version":1,"syncIntervalMinutes":60,"repos":[{"alias":"effect-smol"}]}"#,
        )
        .expect("write shelf config");

        let summary =
            import_shelf_aliases_into_store(&store, &shelf.join("config.json")).expect("import");

        assert_eq!(summary.imported, 1);
        assert_eq!(summary.skipped, 0);
        let aliases = load_alias_registry(&store).expect("load registry").aliases;
        assert!(aliases.iter().any(|entry| entry.alias == "effect-smol"));
    }

    fn resolve_reference_candidates_with_store(
        store: &Path,
        target_path: &Path,
        query_text: &str,
        candidates: &[String],
        limit: usize,
    ) -> Result<Vec<RepoCapsuleReference>> {
        let registry = load_alias_registry(store)?;
        let mut seen_roots = BTreeSet::new();
        let mut matches = Vec::new();
        for candidate in candidates {
            if matches.len() >= limit {
                break;
            }
            let Some(root) =
                resolve_reference_candidate_root(target_path, query_text, candidate, &registry)
            else {
                continue;
            };
            if !seen_roots.insert(root.display().to_string()) {
                continue;
            }
            let capsule = load_or_refresh_capsule_for_root(store, &root)?;
            matches.push(RepoCapsuleReference {
                matched: candidate.clone(),
                repo_root: capsule.repo_root.clone(),
                output: render_reference_output(&capsule, candidate),
            });
        }
        Ok(matches)
    }
}
