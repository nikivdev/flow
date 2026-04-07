use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use ignore::{DirEntry, WalkBuilder};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tempfile::NamedTempFile;

use crate::ai_project_manifest;
use crate::cli::{SyncPlanAction, SyncPlanCommand};
use crate::commit::configured_codex_bin_for_workdir;
use crate::jd;
use crate::{config, projects, push};

const INDEX_VERSION: u32 = 1;
const QUEUE_VERSION: u32 = 1;
const MAX_HISTORY: usize = 40;
const MAX_RECENT_PROJECTS: usize = 50;
const MAX_SYNCED_COMMITS_PROMPT: usize = 80;
const MAX_DEPENDENTS_PROMPT: usize = 12;
const MAX_REPRESENTATIVE_COMMITS: usize = 4;
const MAX_REPRESENTATIVE_CHARS: usize = 2200;
const MAX_DIFFSTAT_CHARS: usize = 4000;
const MAX_QUEUE_DRAIN_BATCH: usize = 2;
const MAX_QUEUE_ATTEMPTS: u32 = 4;
const MAX_DEPENDENCY_SCAN_DEPTH: usize = 8;
const LOCAL_DEPENDENCY_SCAN_ROOTS: &[&str] = &["~/code", "~/org", "~/repos"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncPlanRemoteUpdate {
    pub remote: String,
    pub branch: String,
    pub before_tip: Option<String>,
    pub after_tip: String,
    pub commit_count: usize,
    pub commits: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncPlanRequest {
    pub generated_at: String,
    pub repo_root: String,
    pub repo_name: String,
    pub branch_before: String,
    pub branch_after: String,
    pub head_before: String,
    pub head_after: String,
    pub upstream_before: Option<String>,
    pub upstream_after: Option<String>,
    pub origin_url: Option<String>,
    pub upstream_url: Option<String>,
    pub synced_commits: Vec<String>,
    pub remote_updates: Vec<SyncPlanRemoteUpdate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncPlanRunRecord {
    pub generated_at: String,
    pub ref_name: String,
    pub from_commit: Option<String>,
    pub to_commit: String,
    pub synced_commit_count: usize,
    pub dependents_count: usize,
    pub markdown_path: Option<String>,
    pub json_path: Option<String>,
    pub source_repo_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SyncPlanIndex {
    version: u32,
    repo_root: String,
    repo_slug: String,
    scopes: BTreeMap<String, SyncPlanScopeIndex>,
}

impl Default for SyncPlanIndex {
    fn default() -> Self {
        Self {
            version: INDEX_VERSION,
            repo_root: String::new(),
            repo_slug: String::new(),
            scopes: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct SyncPlanScopeIndex {
    ref_name: String,
    runs: Vec<SyncPlanRunRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredSyncPlanArtifact {
    version: u32,
    request: SyncPlanRequest,
    evidence: SyncPlanEvidence,
    summary: SyncPlanLlmOutput,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct QueuedSyncPlanRequest {
    version: u32,
    queued_at: String,
    attempts: u32,
    request: SyncPlanRequest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SyncPlanEvidence {
    generated_at: String,
    repo_root: String,
    repo_slug: String,
    repo_id: Option<String>,
    branch_before: String,
    branch_after: String,
    head_before: String,
    head_after: String,
    synced_commits: Vec<SyncCommitRecord>,
    remote_updates: Vec<SyncPlanRemoteUpdate>,
    dependent_projects: Vec<DependentProjectEvidence>,
    representative_commits: Vec<RepresentativeCommit>,
    diffstat: Option<String>,
    gaps: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SyncCommitRecord {
    sha: String,
    short_sha: String,
    subject: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RepresentativeCommit {
    heading: String,
    excerpt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DependentProjectEvidence {
    repo_root: String,
    repo_slug: String,
    dependency_type: String,
    dependency_locator: String,
    confidence: String,
    why: String,
    ecosystem_hints: Vec<String>,
    has_context: bool,
    has_docs: bool,
    has_tasks: bool,
    open_todos_count: usize,
    latest_context_doc: Option<String>,
    latest_task_paths: Vec<String>,
    latest_skill_names: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct SyncPlanLlmOutput {
    overview: String,
    #[serde(default)]
    themes: Vec<SyncPlanTheme>,
    #[serde(default)]
    watch_items: Vec<String>,
    #[serde(default)]
    notable_commits: Vec<NotableCommit>,
    #[serde(default)]
    dependent_projects: Vec<DependentProjectPlan>,
    #[serde(default)]
    gaps: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct SyncPlanTheme {
    title: String,
    summary: String,
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    commit_refs: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct NotableCommit {
    commit: String,
    why_it_matters: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct DependentProjectPlan {
    repo_root: String,
    impact_level: String,
    why: String,
    #[serde(default)]
    checks_to_run: Vec<String>,
    #[serde(default)]
    changes_to_consider: Vec<String>,
    #[serde(default)]
    follow_up_tasks: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RepoManifest {
    repos: Option<Vec<RepoManifestEntry>>,
}

#[derive(Debug, Deserialize)]
struct RepoManifestEntry {
    owner: String,
    repo: String,
}

#[derive(Debug, Clone)]
struct DependencyMatch {
    dependency_type: String,
    dependency_locator: String,
    confidence: String,
    why: String,
}

pub fn queue_after_sync(request: SyncPlanRequest) -> Result<()> {
    if request.synced_commits.is_empty() {
        return Ok(());
    }
    enqueue_sync_plan_request(request)
}

pub fn run_cli(cmd: SyncPlanCommand) -> Result<()> {
    let repo_root = resolve_repo_root_from_cwd()?;
    match cmd.action.unwrap_or(SyncPlanAction::Last) {
        SyncPlanAction::Last => {
            let runs = jd::recent_sync_plans(&repo_root, 1)?;
            let Some(run) = runs.into_iter().next() else {
                println!(
                    "No stored sync improvement plan for {}.",
                    repo_root.display()
                );
                return Ok(());
            };
            let Some(path) = run.markdown_path.as_deref() else {
                bail!(
                    "latest sync plan for {} has no markdown path",
                    repo_root.display()
                );
            };
            let markdown =
                fs::read_to_string(path).with_context(|| format!("failed to read {}", path))?;
            print!("{markdown}");
            Ok(())
        }
        SyncPlanAction::List { limit } => {
            let runs = jd::recent_sync_plans(&repo_root, limit)?;
            if runs.is_empty() {
                println!(
                    "No stored sync improvement plans for {}.",
                    repo_root.display()
                );
                return Ok(());
            }
            for run in runs {
                let path = run.markdown_path.as_deref().unwrap_or("-");
                println!(
                    "{}  {}  commits:{}  dependents:{}  {}",
                    run.generated_at,
                    run.ref_name,
                    run.synced_commit_count,
                    run.dependents_count,
                    path
                );
            }
            Ok(())
        }
    }
}

pub(crate) fn drain_queued_requests(limit: usize) -> Result<usize> {
    let limit = limit.clamp(1, MAX_QUEUE_DRAIN_BATCH);
    let queue_dir = sync_plan_queue_dir()?;
    let mut paths = fs::read_dir(&queue_dir)
        .with_context(|| format!("failed to read {}", queue_dir.display()))?
        .filter_map(|entry| entry.ok().map(|item| item.path()))
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
        .collect::<Vec<_>>();
    paths.sort();

    let mut processed = 0usize;
    for queued_path in paths.into_iter().take(limit) {
        let processing_path = queued_path.with_extension("processing");
        if fs::rename(&queued_path, &processing_path).is_err() {
            continue;
        }

        let queued = match read_queue_request(&processing_path) {
            Ok(queued) => queued,
            Err(err) => {
                move_queue_file_to_failed(&processing_path, Some(format!("{err:#}")))?;
                continue;
            }
        };

        match generate_and_store(queued.request.clone()) {
            Ok(_) => {
                let _ = fs::remove_file(&processing_path);
                processed += 1;
            }
            Err(err) => {
                requeue_failed_request(&processing_path, queued, &err)?;
                break;
            }
        }
    }

    Ok(processed)
}

pub(crate) fn generate_and_store(request: SyncPlanRequest) -> Result<SyncPlanRunRecord> {
    let generated_at = parse_generated_at(&request.generated_at);
    let evidence = build_evidence(&request, generated_at)?;
    let summary = run_codex_planner(&evidence)?;
    let markdown = render_markdown(&evidence, &summary);
    let (markdown_path, json_path) = write_artifacts(&request, &evidence, &summary, &markdown)?;
    let run = SyncPlanRunRecord {
        generated_at: evidence.generated_at.clone(),
        ref_name: plan_ref_name(&request),
        from_commit: if request.head_before.trim().is_empty() {
            None
        } else {
            Some(request.head_before.clone())
        },
        to_commit: request.head_after.clone(),
        synced_commit_count: evidence.synced_commits.len(),
        dependents_count: evidence.dependent_projects.len(),
        markdown_path: Some(markdown_path.display().to_string()),
        json_path: Some(json_path.display().to_string()),
        source_repo_only: evidence.dependent_projects.is_empty(),
    };
    record_run(&request, &evidence.repo_slug, &run)?;
    Ok(run)
}

pub(crate) fn recent_runs(repo_root: &Path, limit: usize) -> Result<Vec<SyncPlanRunRecord>> {
    let index_path = sync_plan_index_path(repo_root)?;
    let index = load_index(&index_path)?;
    let mut runs = index
        .scopes
        .into_values()
        .flat_map(|scope| scope.runs.into_iter())
        .collect::<Vec<_>>();
    runs.sort_by(|left, right| right.generated_at.cmp(&left.generated_at));
    runs.truncate(limit.clamp(1, MAX_HISTORY));
    Ok(runs)
}

fn resolve_repo_root_from_cwd() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let output = Command::new("git")
        .current_dir(&cwd)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("failed to run git rev-parse --show-toplevel")?;
    if !output.status.success() {
        bail!("Not a git repository");
    }
    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if root.is_empty() {
        bail!("failed to resolve repository root");
    }
    Ok(PathBuf::from(root))
}

fn parse_generated_at(raw: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(raw)
        .map(|value| value.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

fn build_evidence(
    request: &SyncPlanRequest,
    generated_at: DateTime<Utc>,
) -> Result<SyncPlanEvidence> {
    let repo_root = PathBuf::from(&request.repo_root);
    let repo_id = resolve_source_repo_id(request);
    let repo_slug = repo_id
        .clone()
        .unwrap_or_else(|| request.repo_name.trim().to_string());
    let synced_commits = parse_synced_commits(&request.synced_commits);
    let representative_commits = collect_representative_commits(&repo_root, &synced_commits)?;
    let diffstat = collect_diffstat(&repo_root, &request.head_before, &request.head_after)?;
    let mut gaps = Vec::new();
    if repo_id.is_none() {
        gaps.push(
            "Could not resolve a canonical owner/repo from Git remotes; dependency matching relied on local repo links only."
                .to_string(),
        );
    }
    let dependent_projects =
        discover_dependent_projects(&repo_root, repo_id.as_deref(), &mut gaps)?;

    Ok(SyncPlanEvidence {
        generated_at: generated_at.to_rfc3339(),
        repo_root: repo_root.display().to_string(),
        repo_slug,
        repo_id,
        branch_before: request.branch_before.clone(),
        branch_after: request.branch_after.clone(),
        head_before: request.head_before.clone(),
        head_after: request.head_after.clone(),
        synced_commits,
        remote_updates: request.remote_updates.clone(),
        dependent_projects,
        representative_commits,
        diffstat,
        gaps,
    })
}

fn resolve_source_repo_id(request: &SyncPlanRequest) -> Option<String> {
    request
        .upstream_url
        .as_deref()
        .and_then(push::parse_github_owner_repo)
        .or_else(|| {
            request
                .origin_url
                .as_deref()
                .and_then(push::parse_github_owner_repo)
        })
        .map(|(owner, repo)| format!("{owner}/{repo}"))
}

fn parse_synced_commits(lines: &[String]) -> Vec<SyncCommitRecord> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (sha, subject) = if let Some((sha, rest)) = trimmed.split_once(char::is_whitespace) {
            (sha.trim(), rest.trim())
        } else {
            (trimmed, "(no description)")
        };
        if sha.is_empty() {
            continue;
        }
        let key = sha.to_string();
        if !seen.insert(key.clone()) {
            continue;
        }
        out.push(SyncCommitRecord {
            short_sha: short_sha(sha),
            sha: key,
            subject: if subject.is_empty() {
                "(no description)".to_string()
            } else {
                subject.to_string()
            },
        });
    }
    out
}

fn collect_representative_commits(
    repo_root: &Path,
    commits: &[SyncCommitRecord],
) -> Result<Vec<RepresentativeCommit>> {
    let mut rendered = Vec::new();
    for commit in commits.iter().take(MAX_REPRESENTATIVE_COMMITS) {
        let excerpt = collect_commit_excerpt(repo_root, &commit.sha)?;
        rendered.push(RepresentativeCommit {
            heading: format!("{} {}", commit.short_sha, commit.subject),
            excerpt,
        });
    }
    Ok(rendered)
}

fn collect_commit_excerpt(repo_root: &Path, sha: &str) -> Result<String> {
    let output = git_capture_in(
        repo_root,
        &[
            "show",
            "--no-ext-diff",
            "--date=short",
            "--stat=140,100",
            "--summary",
            "--format=commit %H%nshort: %h%nauthor: %an%ndate: %ad%nsubject: %s",
            "--unified=1",
            sha,
        ],
    )?;
    Ok(truncate_chars(output.trim(), MAX_REPRESENTATIVE_CHARS))
}

fn collect_diffstat(repo_root: &Path, before: &str, after: &str) -> Result<Option<String>> {
    let before = before.trim();
    let after = after.trim();
    if before.is_empty() || after.is_empty() || before == after {
        return Ok(None);
    }
    let output = git_capture_in(
        repo_root,
        &[
            "diff",
            "--stat=140,100",
            "--summary",
            &format!("{before}..{after}"),
        ],
    )?;
    let trimmed = output.trim();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(truncate_chars(trimmed, MAX_DIFFSTAT_CHARS)))
    }
}

fn discover_dependent_projects(
    source_repo_root: &Path,
    source_repo_id: Option<&str>,
    gaps: &mut Vec<String>,
) -> Result<Vec<DependentProjectEvidence>> {
    let source_canonical = source_repo_root
        .canonicalize()
        .unwrap_or_else(|_| source_repo_root.to_path_buf());
    let mut dependents = Vec::new();
    for candidate_root in candidate_dependent_project_roots(gaps) {
        let candidate_canonical = candidate_root
            .canonicalize()
            .unwrap_or_else(|_| candidate_root.clone());
        if candidate_canonical == source_canonical {
            continue;
        }

        if let Some(dependent) = inspect_dependent_project(
            &candidate_root,
            &candidate_canonical,
            &source_canonical,
            source_repo_id,
            gaps,
        )? {
            dependents.push(dependent);
        }
    }

    dependents.sort_by(|left, right| {
        right
            .confidence
            .cmp(&left.confidence)
            .then_with(|| left.repo_slug.cmp(&right.repo_slug))
            .then_with(|| left.repo_root.cmp(&right.repo_root))
    });
    Ok(dependents)
}

fn inspect_dependent_project(
    candidate_root: &Path,
    candidate_canonical: &Path,
    source_canonical: &Path,
    source_repo_id: Option<&str>,
    gaps: &mut Vec<String>,
) -> Result<Option<DependentProjectEvidence>> {
    let Some(matched_dependency) =
        match_dependent_project(candidate_root, source_canonical, source_repo_id)?
    else {
        return Ok(None);
    };
    let manifest = match ai_project_manifest::load_for_target_without_usage(candidate_root, false) {
        Ok(manifest) => Some(manifest),
        Err(err) => {
            gaps.push(format!(
                "Could not load AI project manifest for {} while building sync plan evidence: {err}",
                candidate_root.display()
            ));
            None
        }
    };
    let repo_slug = resolve_repo_slug(candidate_canonical).unwrap_or_else(|| {
        candidate_root
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_else(|| candidate_root.display().to_string())
    });

    Ok(Some(DependentProjectEvidence {
        repo_root: candidate_root.display().to_string(),
        repo_slug,
        dependency_type: matched_dependency.dependency_type,
        dependency_locator: matched_dependency.dependency_locator,
        confidence: matched_dependency.confidence,
        why: matched_dependency.why,
        ecosystem_hints: ecosystem_hints(candidate_root),
        has_context: manifest
            .as_ref()
            .map(|item| item.has_context)
            .unwrap_or(false),
        has_docs: manifest.as_ref().map(|item| item.has_docs).unwrap_or(false),
        has_tasks: manifest
            .as_ref()
            .map(|item| item.has_tasks)
            .unwrap_or(false),
        open_todos_count: manifest
            .as_ref()
            .map(|item| item.open_todos_count)
            .unwrap_or(0),
        latest_context_doc: manifest
            .as_ref()
            .and_then(|item| item.latest_context_doc.clone()),
        latest_task_paths: manifest
            .as_ref()
            .map(|item| item.latest_task_paths.clone())
            .unwrap_or_default(),
        latest_skill_names: manifest
            .as_ref()
            .map(|item| item.latest_skill_names.clone())
            .unwrap_or_default(),
    }))
}

fn candidate_dependent_project_roots(gaps: &mut Vec<String>) -> Vec<PathBuf> {
    let mut roots = BTreeSet::new();

    match ai_project_manifest::recent(MAX_RECENT_PROJECTS) {
        Ok(manifests) => {
            for manifest in manifests {
                add_candidate_root(&mut roots, Path::new(&manifest.repo_root));
            }
        }
        Err(err) => gaps.push(format!(
            "Could not read recent Flow AI project manifests for dependency discovery: {err}"
        )),
    }

    match projects::list_projects() {
        Ok(entries) => {
            for entry in entries {
                add_candidate_root(&mut roots, &entry.project_root);
            }
        }
        Err(err) => gaps.push(format!(
            "Could not read registered Flow projects for dependency discovery: {err}"
        )),
    }

    for scan_root in local_dependency_scan_roots() {
        if !scan_root.exists() {
            continue;
        }
        match scan_local_dependency_marker_roots(&scan_root) {
            Ok(paths) => {
                for path in paths {
                    add_candidate_root(&mut roots, &path);
                }
            }
            Err(err) => gaps.push(format!(
                "Could not scan {} for local dependency markers: {err}",
                scan_root.display()
            )),
        }
    }

    roots.into_iter().collect()
}

fn add_candidate_root(roots: &mut BTreeSet<PathBuf>, path: &Path) {
    if !path.exists() {
        return;
    }
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    roots.insert(canonical);
}

fn local_dependency_scan_roots() -> Vec<PathBuf> {
    LOCAL_DEPENDENCY_SCAN_ROOTS
        .iter()
        .map(|root| config::expand_path(root))
        .collect()
}

fn scan_local_dependency_marker_roots(scan_root: &Path) -> Result<Vec<PathBuf>> {
    let mut roots = BTreeSet::new();
    let walker = WalkBuilder::new(scan_root)
        .hidden(false)
        .ignore(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .max_depth(Some(MAX_DEPENDENCY_SCAN_DEPTH))
        .filter_entry(should_descend_dependency_scan)
        .build();

    for entry in walker.flatten() {
        if !entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            continue;
        }
        if entry.file_name().to_str() != Some(".ai") {
            continue;
        }
        let ai_dir = entry.path();
        if !ai_dir.join("repos.toml").exists() && !ai_dir.join("repos").exists() {
            continue;
        }
        if let Some(project_root) = ai_dir.parent() {
            add_candidate_root(&mut roots, project_root);
        }
    }

    Ok(roots.into_iter().collect())
}

fn should_descend_dependency_scan(entry: &DirEntry) -> bool {
    if !entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
        return true;
    }
    let name = entry.file_name().to_string_lossy();
    if entry.depth() == 0 {
        return true;
    }
    if entry.depth() > MAX_DEPENDENCY_SCAN_DEPTH {
        return false;
    }
    if matches!(
        name.as_ref(),
        ".git"
            | ".jj"
            | "node_modules"
            | "target"
            | "dist"
            | "build"
            | "vendor"
            | "Pods"
            | "__pycache__"
            | ".venv"
            | "venv"
            | "tmp"
            | "cache"
    ) {
        return false;
    }
    if name.starts_with('.') && name.as_ref() != ".ai" {
        return false;
    }
    true
}

fn match_dependent_project(
    candidate_root: &Path,
    source_canonical: &Path,
    source_repo_id: Option<&str>,
) -> Result<Option<DependencyMatch>> {
    let manifest_path = candidate_root.join(".ai").join("repos.toml");
    let manifest_match = if manifest_path.exists() {
        load_repo_manifest_match(&manifest_path, source_repo_id)?
    } else {
        None
    };

    let linked_repo_locator =
        linked_repo_match_locator(candidate_root, source_canonical, source_repo_id);
    let symlink_match = linked_repo_locator.is_some();

    if manifest_match.is_none() && !symlink_match {
        return Ok(None);
    }

    let dependency_type = match (manifest_match.is_some(), symlink_match) {
        (true, true) => "ai_repos_toml+linked_repo",
        (true, false) => "ai_repos_toml",
        (false, true) => "linked_repo",
        (false, false) => return Ok(None),
    };
    let dependency_locator = manifest_match
        .clone()
        .or(linked_repo_locator.clone())
        .unwrap_or_else(|| source_repo_id.unwrap_or("local-link").to_string());
    let confidence = if symlink_match { "high" } else { "medium" };
    let why = if symlink_match {
        format!(
            "{} references {} and the local linked repo resolves to the synced source repository.",
            candidate_root.display(),
            dependency_locator
        )
    } else {
        format!(
            "{} references {} in .ai/repos.toml.",
            candidate_root.display(),
            dependency_locator
        )
    };

    Ok(Some(DependencyMatch {
        dependency_type: dependency_type.to_string(),
        dependency_locator,
        confidence: confidence.to_string(),
        why,
    }))
}

fn linked_repo_match_locator(
    candidate_root: &Path,
    source_canonical: &Path,
    source_repo_id: Option<&str>,
) -> Option<String> {
    let repos_dir = candidate_root.join(".ai").join("repos");
    if !repos_dir.is_dir() {
        return None;
    }

    if let Some(repo_id) = source_repo_id
        && let Some((owner, repo)) = repo_id.split_once('/')
    {
        let linked = repos_dir.join(owner).join(repo);
        if linked.exists()
            && linked
                .canonicalize()
                .ok()
                .filter(|target| target == source_canonical)
                .is_some()
        {
            return Some(repo_id.to_string());
        }
    }

    let owners = fs::read_dir(&repos_dir).ok()?;
    for owner_entry in owners.flatten() {
        let owner_path = owner_entry.path();
        if !owner_path.is_dir() {
            continue;
        }
        let owner = owner_path.file_name()?.to_string_lossy().to_string();
        let repos = fs::read_dir(&owner_path).ok()?;
        for repo_entry in repos.flatten() {
            let repo_path = repo_entry.path();
            let repo_name = repo_path.file_name()?.to_string_lossy().to_string();
            if repo_path
                .canonicalize()
                .ok()
                .filter(|target| target == source_canonical)
                .is_some()
            {
                return Some(format!("{owner}/{repo_name}"));
            }
        }
    }

    None
}

fn load_repo_manifest_match(path: &Path, source_repo_id: Option<&str>) -> Result<Option<String>> {
    let Some(source_repo_id) = source_repo_id else {
        return Ok(None);
    };
    let contents =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let parsed: RepoManifest =
        toml::from_str(&contents).with_context(|| format!("failed to parse {}", path.display()))?;
    let matched =
        parsed.repos.unwrap_or_default().into_iter().find(|entry| {
            format!("{}/{}", entry.owner.trim(), entry.repo.trim()) == source_repo_id
        });
    Ok(matched.map(|entry| format!("{}/{}", entry.owner, entry.repo)))
}

fn ecosystem_hints(repo_root: &Path) -> Vec<String> {
    let mut hints = Vec::new();
    if repo_root.join("package.json").exists() {
        hints.push("javascript".to_string());
    }
    if repo_root.join("Cargo.toml").exists() {
        hints.push("rust".to_string());
    }
    if repo_root.join("go.mod").exists() {
        hints.push("go".to_string());
    }
    if repo_root.join("flow.toml").exists() {
        hints.push("flow".to_string());
    }
    hints
}

fn resolve_repo_slug(repo_root: &Path) -> Option<String> {
    ["upstream", "origin"]
        .into_iter()
        .find_map(|remote| {
            git_capture_in(repo_root, &["remote", "get-url", remote])
                .ok()
                .and_then(|value| {
                    push::parse_github_owner_repo(value.trim())
                        .map(|(owner, repo)| format!("{owner}/{repo}"))
                })
        })
        .or_else(|| {
            repo_root
                .file_name()
                .map(|value| value.to_string_lossy().to_string())
        })
}

fn run_codex_planner(evidence: &SyncPlanEvidence) -> Result<SyncPlanLlmOutput> {
    let prompt = build_codex_prompt(evidence);
    let repo_root = PathBuf::from(&evidence.repo_root);
    let codex_bin = configured_codex_bin_for_workdir(&repo_root);

    let mut schema_file = NamedTempFile::new().context("failed to create temporary schema file")?;
    let mut output_file = NamedTempFile::new().context("failed to create temporary output file")?;
    let schema = planner_output_schema();
    schema_file
        .write_all(
            serde_json::to_string_pretty(&schema)
                .context("failed to encode sync plan output schema")?
                .as_bytes(),
        )
        .context("failed to write sync plan output schema")?;
    output_file
        .write_all(b"")
        .context("failed to initialize sync plan output file")?;

    let mut command = Command::new(&codex_bin);
    command
        .current_dir(&repo_root)
        .arg("exec")
        .arg("--ephemeral")
        .arg("--sandbox")
        .arg("read-only")
        .arg("--output-schema")
        .arg(schema_file.path())
        .arg("--output-last-message")
        .arg(output_file.path())
        .arg("-C")
        .arg(&repo_root)
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn().with_context(|| {
        format!(
            "failed to spawn Codex binary '{}' for sync improvement planning",
            codex_bin
        )
    })?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(prompt.as_bytes())
            .context("failed to write sync plan prompt to Codex stdin")?;
    }

    let output = child
        .wait_with_output()
        .context("failed while waiting for Codex sync planner")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        bail!(
            "codex exec failed for sync plan (status {:?})\nstderr: {}\nstdout: {}",
            output.status.code(),
            stderr,
            stdout
        );
    }

    let last_message = fs::read_to_string(output_file.path())
        .context("failed to read sync plan Codex output file")?;
    parse_summary_output(&last_message)
}

fn build_codex_prompt(evidence: &SyncPlanEvidence) -> String {
    let mut prompt = String::new();
    prompt.push_str(
        "You are generating a durable engineering sync-improvement plan for a repository.\n\
         Use only the evidence below. Do not invent details and do not run tools.\n\
         Keep the overview high-signal and concrete.\n\
         Focus on dependency impact, upgrade opportunities, workflow changes, risks, and concrete next actions for known local dependent projects.\n\
         If dependent-project evidence is weak or missing, say so in gaps instead of pretending certainty.\n\
         Keep watch items sparse and actionable.\n\n",
    );

    writeln!(&mut prompt, "Source repo: {}", evidence.repo_slug).ok();
    writeln!(&mut prompt, "Source root: {}", evidence.repo_root).ok();
    if let Some(repo_id) = evidence.repo_id.as_deref() {
        writeln!(&mut prompt, "Source repo id: {}", repo_id).ok();
    }
    writeln!(&mut prompt, "Generated at: {}", evidence.generated_at).ok();
    writeln!(&mut prompt, "Branch before: {}", evidence.branch_before).ok();
    writeln!(&mut prompt, "Branch after: {}", evidence.branch_after).ok();
    writeln!(&mut prompt, "Head before: {}", evidence.head_before).ok();
    writeln!(&mut prompt, "Head after: {}", evidence.head_after).ok();
    writeln!(
        &mut prompt,
        "Synced commit count: {}",
        evidence.synced_commits.len()
    )
    .ok();
    writeln!(
        &mut prompt,
        "Dependent projects found: {}",
        evidence.dependent_projects.len()
    )
    .ok();

    prompt.push_str("\nSynced commits\n");
    for commit in evidence
        .synced_commits
        .iter()
        .take(MAX_SYNCED_COMMITS_PROMPT)
    {
        writeln!(&mut prompt, "- {} {}", commit.short_sha, commit.subject).ok();
    }
    if evidence.synced_commits.len() > MAX_SYNCED_COMMITS_PROMPT {
        writeln!(
            &mut prompt,
            "- ... {} additional synced commits omitted ...",
            evidence
                .synced_commits
                .len()
                .saturating_sub(MAX_SYNCED_COMMITS_PROMPT)
        )
        .ok();
    }

    if let Some(diffstat) = evidence.diffstat.as_deref() {
        prompt.push_str("\nCombined diffstat\n```text\n");
        prompt.push_str(diffstat);
        prompt.push_str("\n```\n");
    }

    if !evidence.representative_commits.is_empty() {
        prompt.push_str("\nRepresentative commits\n");
        for commit in &evidence.representative_commits {
            prompt.push_str("```text\n");
            prompt.push_str(&commit.heading);
            prompt.push('\n');
            prompt.push_str(&commit.excerpt);
            prompt.push_str("\n```\n");
        }
    }

    prompt.push_str("\nRemote updates\n");
    for update in &evidence.remote_updates {
        writeln!(
            &mut prompt,
            "- {}/{} commits:{} before:{} after:{}",
            update.remote,
            update.branch,
            update.commit_count,
            update.before_tip.as_deref().unwrap_or("<none>"),
            update.after_tip
        )
        .ok();
    }

    prompt.push_str("\nKnown dependent projects\n");
    if evidence.dependent_projects.is_empty() {
        prompt.push_str("- none\n");
    } else {
        for project in evidence
            .dependent_projects
            .iter()
            .take(MAX_DEPENDENTS_PROMPT)
        {
            writeln!(&mut prompt, "- repoRoot: {}", project.repo_root).ok();
            writeln!(&mut prompt, "  repoSlug: {}", project.repo_slug).ok();
            writeln!(&mut prompt, "  dependencyType: {}", project.dependency_type).ok();
            writeln!(
                &mut prompt,
                "  dependencyLocator: {}",
                project.dependency_locator
            )
            .ok();
            writeln!(&mut prompt, "  confidence: {}", project.confidence).ok();
            writeln!(&mut prompt, "  why: {}", project.why).ok();
            writeln!(
                &mut prompt,
                "  ecosystemHints: {}",
                if project.ecosystem_hints.is_empty() {
                    "none".to_string()
                } else {
                    project.ecosystem_hints.join(", ")
                }
            )
            .ok();
            writeln!(
                &mut prompt,
                "  aiSurface: context={} docs={} tasks={} openTodos={}",
                project.has_context, project.has_docs, project.has_tasks, project.open_todos_count
            )
            .ok();
            if let Some(doc) = project.latest_context_doc.as_deref() {
                writeln!(&mut prompt, "  latestContextDoc: {}", doc).ok();
            }
            if !project.latest_task_paths.is_empty() {
                writeln!(
                    &mut prompt,
                    "  latestTasks: {}",
                    project.latest_task_paths.join(", ")
                )
                .ok();
            }
            if !project.latest_skill_names.is_empty() {
                writeln!(
                    &mut prompt,
                    "  latestSkills: {}",
                    project.latest_skill_names.join(", ")
                )
                .ok();
            }
        }
    }

    if !evidence.gaps.is_empty() {
        prompt.push_str("\nKnown evidence gaps\n");
        for gap in &evidence.gaps {
            writeln!(&mut prompt, "- {}", gap).ok();
        }
    }

    prompt.push_str(
        "\nOutput requirements\n\
         - overview: 2-4 sentences.\n\
         - themes: 2-5 themes when evidence supports them.\n\
         - watchItems: concrete risks or follow-ups only.\n\
         - notableCommits: only commits that materially changed APIs, behavior, docs, workflow, or rollout shape.\n\
         - dependentProjects: include only known local projects from the evidence; do not invent projects.\n\
         - For each dependent project, give checksToRun, changesToConsider, and followUpTasks only when supported by the evidence.\n\
         - gaps: list missing evidence or low-confidence areas.\n",
    );

    prompt
}

fn planner_output_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "overview": { "type": "string" },
            "themes": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "title": { "type": "string" },
                        "summary": { "type": "string" },
                        "paths": { "type": "array", "items": { "type": "string" } },
                        "commitRefs": { "type": "array", "items": { "type": "string" } }
                    },
                    "required": ["title", "summary", "paths", "commitRefs"],
                    "additionalProperties": false
                }
            },
            "watchItems": { "type": "array", "items": { "type": "string" } },
            "notableCommits": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "commit": { "type": "string" },
                        "whyItMatters": { "type": "string" }
                    },
                    "required": ["commit", "whyItMatters"],
                    "additionalProperties": false
                }
            },
            "dependentProjects": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "repoRoot": { "type": "string" },
                        "impactLevel": { "type": "string" },
                        "why": { "type": "string" },
                        "checksToRun": { "type": "array", "items": { "type": "string" } },
                        "changesToConsider": { "type": "array", "items": { "type": "string" } },
                        "followUpTasks": { "type": "array", "items": { "type": "string" } }
                    },
                    "required": ["repoRoot", "impactLevel", "why", "checksToRun", "changesToConsider", "followUpTasks"],
                    "additionalProperties": false
                }
            },
            "gaps": { "type": "array", "items": { "type": "string" } }
        },
        "required": ["overview", "themes", "watchItems", "notableCommits", "dependentProjects", "gaps"],
        "additionalProperties": false
    })
}

fn parse_summary_output(raw: &str) -> Result<SyncPlanLlmOutput> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("Codex returned an empty sync improvement payload");
    }
    if let Ok(parsed) = serde_json::from_str::<SyncPlanLlmOutput>(trimmed) {
        return Ok(parsed);
    }
    if let Some(inner) = strip_json_code_fence(trimmed)
        && let Ok(parsed) = serde_json::from_str::<SyncPlanLlmOutput>(inner)
    {
        return Ok(parsed);
    }
    bail!(
        "failed to parse sync plan output as JSON: {}",
        truncate_chars(trimmed, 600)
    )
}

fn strip_json_code_fence(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    if !trimmed.starts_with("```") {
        return None;
    }
    let body = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))?;
    let body = body.trim_start_matches('\n');
    body.strip_suffix("```").map(str::trim)
}

fn render_markdown(evidence: &SyncPlanEvidence, summary: &SyncPlanLlmOutput) -> String {
    let mut body = String::new();
    body.push_str(&render_frontmatter(evidence));
    writeln!(&mut body, "# Sync Plan: {}", evidence.repo_slug).ok();
    writeln!(
        &mut body,
        "\nGenerated on {} for `{}` at `{}`.",
        evidence.generated_at,
        evidence.branch_after,
        short_sha(&evidence.head_after)
    )
    .ok();

    body.push_str("\n## Overview\n");
    body.push_str(summary.overview.trim());
    body.push('\n');

    if !summary.themes.is_empty() {
        body.push_str("\n## Themes\n");
        for theme in &summary.themes {
            if theme.title.trim().is_empty() || theme.summary.trim().is_empty() {
                continue;
            }
            writeln!(&mut body, "\n### {}", theme.title.trim()).ok();
            body.push_str(theme.summary.trim());
            body.push('\n');
            if !theme.paths.is_empty() {
                body.push_str("\nPaths:\n");
                for path in &theme.paths {
                    if path.trim().is_empty() {
                        continue;
                    }
                    writeln!(&mut body, "- `{}`", path.trim()).ok();
                }
            }
            if !theme.commit_refs.is_empty() {
                body.push_str("\nEvidence:\n");
                for commit in &theme.commit_refs {
                    if commit.trim().is_empty() {
                        continue;
                    }
                    writeln!(&mut body, "- `{}`", commit.trim()).ok();
                }
            }
        }
    }

    if !summary.watch_items.is_empty() {
        body.push_str("\n## Watch\n");
        for item in &summary.watch_items {
            if item.trim().is_empty() {
                continue;
            }
            writeln!(&mut body, "- {}", item.trim()).ok();
        }
    }

    if !summary.notable_commits.is_empty() {
        body.push_str("\n## Notable Commits\n");
        for commit in &summary.notable_commits {
            if commit.commit.trim().is_empty() || commit.why_it_matters.trim().is_empty() {
                continue;
            }
            writeln!(
                &mut body,
                "- `{}`: {}",
                commit.commit.trim(),
                commit.why_it_matters.trim()
            )
            .ok();
        }
    }

    body.push_str("\n## Dependent Projects\n");
    if summary.dependent_projects.is_empty() {
        body.push_str(
            "No known local dependent projects were identified from the current Flow AI manifest cache.\n",
        );
    } else {
        for project in &summary.dependent_projects {
            if project.repo_root.trim().is_empty() {
                continue;
            }
            writeln!(&mut body, "\n### `{}`", project.repo_root.trim()).ok();
            writeln!(
                &mut body,
                "- Impact: {}",
                normalize_empty(&project.impact_level, "unknown")
            )
            .ok();
            writeln!(
                &mut body,
                "- Why: {}",
                normalize_empty(&project.why, "No rationale provided.")
            )
            .ok();
            if !project.checks_to_run.is_empty() {
                body.push_str("- Checks to run:\n");
                for item in &project.checks_to_run {
                    if item.trim().is_empty() {
                        continue;
                    }
                    writeln!(&mut body, "  - {}", item.trim()).ok();
                }
            }
            if !project.changes_to_consider.is_empty() {
                body.push_str("- Changes to consider:\n");
                for item in &project.changes_to_consider {
                    if item.trim().is_empty() {
                        continue;
                    }
                    writeln!(&mut body, "  - {}", item.trim()).ok();
                }
            }
            if !project.follow_up_tasks.is_empty() {
                body.push_str("- Follow-up tasks:\n");
                for item in &project.follow_up_tasks {
                    if item.trim().is_empty() {
                        continue;
                    }
                    writeln!(&mut body, "  - {}", item.trim()).ok();
                }
            }
        }
    }

    if !summary.gaps.is_empty() || !evidence.gaps.is_empty() {
        body.push_str("\n## Gaps\n");
        for gap in summary.gaps.iter().chain(evidence.gaps.iter()) {
            if gap.trim().is_empty() {
                continue;
            }
            writeln!(&mut body, "- {}", gap.trim()).ok();
        }
    }

    body.push_str("\n## Evidence Snapshot\n");
    writeln!(
        &mut body,
        "- Synced commits: {}",
        evidence.synced_commits.len()
    )
    .ok();
    writeln!(
        &mut body,
        "- Known dependent projects: {}",
        evidence.dependent_projects.len()
    )
    .ok();
    if let Some(repo_id) = evidence.repo_id.as_deref() {
        writeln!(&mut body, "- Source repo id: `{}`", repo_id).ok();
    }
    writeln!(
        &mut body,
        "- Branch transition: `{}` -> `{}`",
        evidence.branch_before, evidence.branch_after
    )
    .ok();
    if let Some(diffstat) = evidence.diffstat.as_deref() {
        body.push_str("\nDiffstat\n```text\n");
        body.push_str(diffstat);
        body.push_str("\n```\n");
    }
    if !evidence.representative_commits.is_empty() {
        body.push_str("\nRepresentative commits\n");
        for commit in &evidence.representative_commits {
            body.push_str("```text\n");
            body.push_str(&commit.heading);
            body.push('\n');
            body.push_str(&commit.excerpt);
            body.push_str("\n```\n");
        }
    }

    body
}

fn normalize_empty<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        fallback
    } else {
        trimmed
    }
}

fn render_frontmatter(evidence: &SyncPlanEvidence) -> String {
    let mut out = String::new();
    out.push_str("---\n");
    yaml_line(&mut out, "repo", &evidence.repo_slug);
    yaml_line(&mut out, "repo_root", &evidence.repo_root);
    yaml_line(&mut out, "generated_at", &evidence.generated_at);
    yaml_line(&mut out, "branch_before", &evidence.branch_before);
    yaml_line(&mut out, "branch_after", &evidence.branch_after);
    yaml_line(&mut out, "head_before", &evidence.head_before);
    yaml_line(&mut out, "head_after", &evidence.head_after);
    if let Some(repo_id) = evidence.repo_id.as_deref() {
        yaml_line(&mut out, "repo_id", repo_id);
    }
    out.push_str(&format!(
        "synced_commit_count: {}\n",
        evidence.synced_commits.len()
    ));
    out.push_str(&format!(
        "dependent_project_count: {}\n",
        evidence.dependent_projects.len()
    ));
    out.push_str("---\n\n");
    out
}

fn yaml_line(out: &mut String, key: &str, value: &str) {
    out.push_str(key);
    out.push_str(": ");
    out.push_str(&yaml_quote(value));
    out.push('\n');
}

fn yaml_quote(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn write_artifacts(
    request: &SyncPlanRequest,
    evidence: &SyncPlanEvidence,
    summary: &SyncPlanLlmOutput,
    markdown: &str,
) -> Result<(PathBuf, PathBuf)> {
    let root = sync_plan_repo_dir(Path::new(&request.repo_root))?;
    let artifact_dir = root.join("artifacts");
    fs::create_dir_all(&artifact_dir)
        .with_context(|| format!("failed to create {}", artifact_dir.display()))?;
    let timestamp = filename_timestamp(&evidence.generated_at);
    let markdown_path = artifact_dir.join(format!("{timestamp}.md"));
    let json_path = artifact_dir.join(format!("{timestamp}.json"));

    fs::write(&markdown_path, markdown)
        .with_context(|| format!("failed to write {}", markdown_path.display()))?;
    let artifact = StoredSyncPlanArtifact {
        version: INDEX_VERSION,
        request: request.clone(),
        evidence: evidence.clone(),
        summary: summary.clone(),
    };
    let encoded =
        serde_json::to_string_pretty(&artifact).context("failed to encode sync plan artifact")?;
    fs::write(&json_path, encoded)
        .with_context(|| format!("failed to write {}", json_path.display()))?;
    Ok((markdown_path, json_path))
}

fn filename_timestamp(generated_at: &str) -> String {
    parse_generated_at(generated_at)
        .format("%Y-%m-%dT%H-%M-%SZ")
        .to_string()
}

fn record_run(request: &SyncPlanRequest, repo_slug: &str, run: &SyncPlanRunRecord) -> Result<()> {
    let index_path = sync_plan_index_path(Path::new(&request.repo_root))?;
    let mut index = load_index(&index_path)?;
    let scope_key = scope_key(&request.branch_after);
    let scope = index
        .scopes
        .entry(scope_key)
        .or_insert_with(|| SyncPlanScopeIndex {
            ref_name: plan_ref_name(request),
            runs: Vec::new(),
        });
    scope.ref_name = plan_ref_name(request);
    scope.runs.push(run.clone());
    if scope.runs.len() > MAX_HISTORY {
        let drain = scope.runs.len().saturating_sub(MAX_HISTORY);
        scope.runs.drain(0..drain);
    }
    index.version = INDEX_VERSION;
    index.repo_root = request.repo_root.clone();
    index.repo_slug = repo_slug.to_string();
    save_index(&index_path, &index)
}

fn scope_key(branch_after: &str) -> String {
    let trimmed = branch_after.trim();
    if trimmed.is_empty() {
        "detached".to_string()
    } else {
        trimmed.to_string()
    }
}

fn plan_ref_name(request: &SyncPlanRequest) -> String {
    let trimmed = request.branch_after.trim();
    if trimmed.is_empty() || trimmed == "HEAD" {
        request.branch_before.trim().to_string()
    } else {
        trimmed.to_string()
    }
}

fn enqueue_sync_plan_request(request: SyncPlanRequest) -> Result<()> {
    let queued = QueuedSyncPlanRequest {
        version: QUEUE_VERSION,
        queued_at: Utc::now().to_rfc3339(),
        attempts: 0,
        request,
        last_error: None,
    };
    let path = sync_plan_queue_path(&queued.request)?;
    write_queue_request(&path, &queued)?;
    wake_jd_for_queued_sync_plans();
    Ok(())
}

fn sync_plan_state_root() -> Result<PathBuf> {
    let root = config::ensure_global_state_dir()?.join("sync-plans");
    fs::create_dir_all(&root).with_context(|| format!("failed to create {}", root.display()))?;
    Ok(root)
}

fn sync_plan_queue_dir() -> Result<PathBuf> {
    let dir = sync_plan_state_root()?.join("queue");
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    Ok(dir)
}

fn sync_plan_failed_queue_dir() -> Result<PathBuf> {
    let dir = sync_plan_state_root()?.join("failed");
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    Ok(dir)
}

fn sync_plan_queue_path(request: &SyncPlanRequest) -> Result<PathBuf> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0);
    let slug = sanitize_filename_fragment(&request.repo_name);
    Ok(sync_plan_queue_dir()?.join(format!("{}-{}-{}.json", slug, std::process::id(), nonce)))
}

fn wake_jd_for_queued_sync_plans() {
    if jd::is_running() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let _ = Command::new(exe)
        .arg("codex")
        .arg("daemon")
        .arg("start")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

fn sync_plan_repo_dir(repo_root: &Path) -> Result<PathBuf> {
    let canonical = repo_root
        .canonicalize()
        .unwrap_or_else(|_| repo_root.to_path_buf());
    let digest = blake3::hash(canonical.to_string_lossy().as_bytes())
        .to_hex()
        .to_string();
    let dir = sync_plan_state_root()?.join(digest);
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    Ok(dir)
}

fn sync_plan_index_path(repo_root: &Path) -> Result<PathBuf> {
    Ok(sync_plan_repo_dir(repo_root)?.join("index.json"))
}

fn read_queue_request(path: &Path) -> Result<QueuedSyncPlanRequest> {
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let queued = serde_json::from_str::<QueuedSyncPlanRequest>(&text)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    if queued.version != QUEUE_VERSION {
        bail!(
            "unsupported sync plan queue version {} in {}",
            queued.version,
            path.display()
        );
    }
    Ok(queued)
}

fn write_queue_request(path: &Path, queued: &QueuedSyncPlanRequest) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let data = serde_json::to_string_pretty(queued)
        .context("failed to encode queued sync improvement request")?;
    let tmp_path = path.with_extension(format!("json.tmp.{}", std::process::id()));
    fs::write(&tmp_path, data)
        .with_context(|| format!("failed to write {}", tmp_path.display()))?;
    fs::rename(&tmp_path, path)
        .or_else(|_| {
            let _ = fs::remove_file(path);
            fs::rename(&tmp_path, path)
        })
        .with_context(|| format!("failed to finalize {}", path.display()))?;
    Ok(())
}

fn requeue_failed_request(
    processing_path: &Path,
    mut queued: QueuedSyncPlanRequest,
    err: &anyhow::Error,
) -> Result<()> {
    queued.attempts = queued.attempts.saturating_add(1);
    queued.last_error = Some(truncate_chars(&format!("{err:#}"), 1200));
    if queued.attempts >= MAX_QUEUE_ATTEMPTS {
        move_queue_file_to_failed_with_payload(processing_path, &queued)?;
        return Ok(());
    }

    let queued_path = sync_plan_queue_path(&queued.request)?;
    write_queue_request(&queued_path, &queued)?;
    let _ = fs::remove_file(processing_path);
    Ok(())
}

fn move_queue_file_to_failed(processing_path: &Path, error: Option<String>) -> Result<()> {
    let mut target = sync_plan_failed_queue_dir()?;
    target.push(processing_path.file_name().unwrap_or_default());

    fs::rename(processing_path, &target)
        .or_else(|_| {
            let _ = fs::remove_file(&target);
            fs::rename(processing_path, &target)
        })
        .with_context(|| {
            format!(
                "failed to move {} to {}",
                processing_path.display(),
                target.display()
            )
        })?;

    if let Some(message) = error {
        let error_path = target.with_extension("error.txt");
        fs::write(&error_path, message)
            .with_context(|| format!("failed to write {}", error_path.display()))?;
    }

    Ok(())
}

fn move_queue_file_to_failed_with_payload(
    processing_path: &Path,
    queued: &QueuedSyncPlanRequest,
) -> Result<()> {
    let mut target = sync_plan_failed_queue_dir()?;
    target.push(processing_path.file_name().unwrap_or_default());
    write_queue_request(&target, queued)?;
    let _ = fs::remove_file(processing_path);
    Ok(())
}

fn sanitize_filename_fragment(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();
    let trimmed = sanitized.trim_matches('-');
    if trimmed.is_empty() {
        "repo".to_string()
    } else {
        trimmed.to_string()
    }
}

fn load_index(path: &Path) -> Result<SyncPlanIndex> {
    if !path.exists() {
        return Ok(SyncPlanIndex::default());
    }
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let parsed = serde_json::from_str::<SyncPlanIndex>(&text)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(parsed)
}

fn save_index(path: &Path, index: &SyncPlanIndex) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let data =
        serde_json::to_string_pretty(index).context("failed to encode sync improvement index")?;
    let tmp_path = path.with_extension(format!("json.tmp.{}", std::process::id()));
    fs::write(&tmp_path, data)
        .with_context(|| format!("failed to write {}", tmp_path.display()))?;
    fs::rename(&tmp_path, path)
        .or_else(|_| {
            let _ = fs::remove_file(path);
            fs::rename(&tmp_path, path)
        })
        .with_context(|| format!("failed to finalize {}", path.display()))?;
    Ok(())
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(8).collect()
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut iter = value.chars();
    let collected: String = iter.by_ref().take(max_chars).collect();
    if iter.next().is_some() {
        format!("{collected}\n... (truncated)")
    } else {
        collected
    }
}

fn git_capture_in(repo_root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(args)
        .output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn parse_summary_output_accepts_fenced_json() {
        let parsed = parse_summary_output(
            "```json\n{\"overview\":\"ok\",\"themes\":[],\"watchItems\":[],\"notableCommits\":[],\"dependentProjects\":[],\"gaps\":[]}\n```",
        )
        .expect("json should parse");
        assert_eq!(parsed.overview, "ok");
    }

    #[test]
    fn parse_synced_commits_dedupes_exact_hashes() {
        let commits = parse_synced_commits(&[
            "abc12345 first".to_string(),
            "abc12345 first".to_string(),
            "def67890 second".to_string(),
        ]);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].short_sha, "abc12345");
    }

    #[test]
    fn scan_local_dependency_marker_roots_finds_projects_with_ai_repo_links() {
        let root = tempdir().expect("tempdir");
        let project = root.path().join("code").join("demo");
        fs::create_dir_all(project.join(".ai").join("repos")).expect("mkdir .ai/repos");
        fs::write(
            project.join(".ai").join("repos.toml"),
            "[[repos]]\nowner = \"acme\"\nrepo = \"kit\"\n",
        )
        .expect("write repos.toml");

        let scanned = scan_local_dependency_marker_roots(root.path()).expect("scan");
        let expected = project.canonicalize().unwrap_or(project.clone());
        assert!(scanned.iter().any(|path| path == &expected));
    }

    #[test]
    fn match_dependent_project_detects_repos_toml_reference() {
        let root = tempdir().expect("tempdir");
        let source = root.path().join("source");
        let candidate = root.path().join("candidate");
        fs::create_dir_all(&source).expect("mkdir source");
        fs::create_dir_all(candidate.join(".ai")).expect("mkdir candidate .ai");
        fs::write(
            candidate.join(".ai").join("repos.toml"),
            "[[repos]]\nowner = \"acme\"\nrepo = \"kit\"\n",
        )
        .expect("write repos.toml");

        let matched = match_dependent_project(
            &candidate,
            &source.canonicalize().unwrap_or(source.clone()),
            Some("acme/kit"),
        )
        .expect("match");

        let matched = matched.expect("should match manifest entry");
        assert_eq!(matched.dependency_type, "ai_repos_toml");
        assert_eq!(matched.dependency_locator, "acme/kit");
        assert_eq!(matched.confidence, "medium");
    }

    #[cfg(unix)]
    #[test]
    fn linked_repo_match_locator_finds_local_link_without_repo_id() {
        let root = tempdir().expect("tempdir");
        let source = root.path().join("source");
        let candidate = root.path().join("candidate");
        fs::create_dir_all(&source).expect("mkdir source");
        fs::create_dir_all(candidate.join(".ai").join("repos").join("acme"))
            .expect("mkdir linked owner");
        std::os::unix::fs::symlink(
            &source,
            candidate.join(".ai").join("repos").join("acme").join("kit"),
        )
        .expect("symlink repo");

        let locator = linked_repo_match_locator(
            &candidate,
            &source.canonicalize().unwrap_or(source.clone()),
            None,
        );

        assert_eq!(locator.as_deref(), Some("acme/kit"));
    }

    #[test]
    fn sanitize_filename_fragment_keeps_readable_slug() {
        assert_eq!(
            sanitize_filename_fragment("gitkraken/vscode-gitlens"),
            "gitkraken-vscode-gitlens"
        );
        assert_eq!(sanitize_filename_fragment(""), "repo");
    }
}
