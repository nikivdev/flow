use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{SecondsFormat, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use crate::projects::{self, ProjectEntry};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowOverview {
    pub generated_at: String,
    pub repos: Vec<WorkflowRepoSnapshot>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRepoSnapshot {
    pub id: String,
    pub name: String,
    pub vcs: String,
    pub repo_key: String,
    pub repo_root: String,
    pub repo_slug: Option<String>,
    pub default_branch: Option<String>,
    pub project_count: usize,
    pub workspace_count: usize,
    pub active_branch_count: usize,
    pub open_pr_count: usize,
    pub hidden_branch_count: usize,
    pub pr_error: Option<String>,
    pub projects: Vec<WorkflowProjectRef>,
    pub workspaces: Vec<WorkflowWorkspaceSnapshot>,
    pub branches: Vec<WorkflowBranchSnapshot>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowProjectRef {
    pub name: String,
    pub project_root: String,
    pub repo_relative_path: String,
    pub workspace_name: Option<String>,
    pub workspace_root: Option<String>,
    pub current_branches: Vec<String>,
    pub dirty: bool,
    pub conflict: bool,
    pub updated_ms: u128,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowWorkspaceSnapshot {
    pub name: String,
    pub root_path: Option<String>,
    pub current_branches: Vec<String>,
    pub dirty: bool,
    pub conflict: bool,
    pub target_commit: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowBranchSnapshot {
    pub name: String,
    pub head_sha: String,
    pub short_sha: String,
    pub subject: String,
    pub updated_at: Option<String>,
    pub is_current: bool,
    pub is_active: bool,
    pub hidden: bool,
    pub workspace_names: Vec<String>,
    pub dirty: bool,
    pub conflict: bool,
    pub tracking_remote: Option<String>,
    pub tracked: bool,
    pub synced: bool,
    pub ahead_count: Option<u32>,
    pub behind_count: Option<u32>,
    pub upstream_sha: Option<String>,
    pub compare_base_branch: Option<String>,
    pub pull_request: Option<WorkflowPullRequestSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowPullRequestSummary {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub state: String,
    pub is_draft: bool,
    pub base_ref_name: String,
    pub head_ref_name: String,
    pub updated_at: String,
    pub review_decision: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepoVcs {
    Jj,
    Git,
}

impl RepoVcs {
    fn as_str(self) -> &'static str {
        match self {
            RepoVcs::Jj => "jj",
            RepoVcs::Git => "git",
        }
    }
}

#[derive(Debug, Clone)]
struct ProjectBinding {
    project: ProjectEntry,
    vcs: RepoVcs,
    logical_key: String,
    repo_root: PathBuf,
    workspace_root: Option<PathBuf>,
    workspace_name: Option<String>,
    current_branches: Vec<String>,
    dirty: bool,
    conflict: bool,
}

#[derive(Debug, Clone)]
struct RepoSeed {
    vcs: RepoVcs,
    logical_key: String,
    repo_root: PathBuf,
    bindings: Vec<ProjectBinding>,
}

#[derive(Debug, Clone)]
struct WorkspaceState {
    name: String,
    target_commit: String,
    current_branches: Vec<String>,
    dirty: bool,
    conflict: bool,
    description: String,
}

#[derive(Debug, Clone)]
struct CommitMeta {
    head_sha: String,
    short_sha: String,
    subject: String,
    updated_at: Option<String>,
}

#[derive(Debug, Clone)]
struct JjBookmarkRow {
    name: String,
    remote: Option<String>,
    tracked: bool,
    conflict: bool,
    present: bool,
    synced: bool,
    ahead_count: Option<u32>,
    behind_count: Option<u32>,
    target_sha: Option<String>,
}

#[derive(Debug, Clone)]
struct GitWorktreeState {
    name: String,
    path: PathBuf,
    branch: Option<String>,
    dirty: bool,
}

#[derive(Debug, Clone)]
struct GitBranchRow {
    name: String,
    head_sha: String,
    short_sha: String,
    updated_at: Option<String>,
    subject: String,
    upstream: Option<String>,
    ahead_count: Option<u32>,
    behind_count: Option<u32>,
}

#[derive(Clone)]
struct CachedWorkflowOverview {
    captured_at: Instant,
    overview: WorkflowOverview,
}

static WORKFLOW_OVERVIEW_CACHE: OnceLock<Mutex<Option<CachedWorkflowOverview>>> = OnceLock::new();
const WORKFLOW_OVERVIEW_TTL: Duration = Duration::from_secs(30);

pub fn load_workflow_overview() -> Result<WorkflowOverview> {
    if let Some(cached) = workflow_cache()
        .lock()
        .expect("workflow cache mutex poisoned")
        .clone()
        .filter(|cached| cached.captured_at.elapsed() < WORKFLOW_OVERVIEW_TTL)
    {
        return Ok(cached.overview);
    }

    let projects = projects::list_projects()?;
    let mut repo_map: HashMap<String, RepoSeed> = HashMap::new();
    let mut errors = Vec::new();

    for project in projects {
        match detect_project_binding(&project) {
            Ok(Some(binding)) => {
                let key = binding.logical_key.clone();
                repo_map
                    .entry(key.clone())
                    .and_modify(|seed| seed.bindings.push(binding.clone()))
                    .or_insert_with(|| RepoSeed {
                        vcs: binding.vcs,
                        logical_key: key,
                        repo_root: binding.repo_root.clone(),
                        bindings: vec![binding],
                    });
            }
            Ok(None) => errors.push(format!(
                "{}: no jj or git repo found at {}",
                project.name,
                project.project_root.display()
            )),
            Err(err) => errors.push(format!(
                "{}: failed to inspect {}: {err:#}",
                project.name,
                project.project_root.display()
            )),
        }
    }

    let mut repos = Vec::new();
    for seed in repo_map.into_values() {
        let snapshot = match seed.vcs {
            RepoVcs::Jj => inspect_jj_repo(&seed),
            RepoVcs::Git => inspect_git_repo(&seed),
        };
        match snapshot {
            Ok(repo) => repos.push(repo),
            Err(err) => {
                errors.push(format!(
                    "{}: failed to build workflow snapshot: {err:#}",
                    seed.repo_root.display()
                ));
                repos.push(repo_error_snapshot(&seed, err.to_string()));
            }
        }
    }

    repos.sort_by(|a, b| {
        b.active_branch_count
            .cmp(&a.active_branch_count)
            .then_with(|| b.open_pr_count.cmp(&a.open_pr_count))
            .then_with(|| a.name.cmp(&b.name))
    });

    let overview = WorkflowOverview {
        generated_at: now_iso(),
        repos,
        errors,
    };

    *workflow_cache().lock().expect("workflow cache mutex poisoned") = Some(CachedWorkflowOverview {
        captured_at: Instant::now(),
        overview: overview.clone(),
    });

    Ok(overview)
}

fn workflow_cache() -> &'static Mutex<Option<CachedWorkflowOverview>> {
    WORKFLOW_OVERVIEW_CACHE.get_or_init(|| Mutex::new(None))
}

fn detect_project_binding(project: &ProjectEntry) -> Result<Option<ProjectBinding>> {
    if let Ok(workspace_root) = capture_trimmed_in(&project.project_root, "jj", &["root"]) {
        let workspace_root = canonical_or_same(PathBuf::from(workspace_root));
        if let Ok(repo_root) = resolve_jj_repo_store(&workspace_root) {
            let workspace_name = capture_trimmed_in(
                &project.project_root,
                "jj",
                &[
                    "log",
                    "-r",
                    "@",
                    "-n",
                    "1",
                    "--no-graph",
                    "-T",
                    "working_copies.map(|w| w.name()).join(\",\") ++ \"\\n\"",
                ],
            )
            .ok()
            .and_then(|value| first_non_empty_csv(&value));
            let current_branches = capture_trimmed_in(
                &project.project_root,
                "jj",
                &[
                    "log",
                    "-r",
                    "@-",
                    "-n",
                    "1",
                    "--no-graph",
                    "-T",
                    "local_bookmarks.map(|b| b.name()).join(\",\") ++ \"\\n\"",
                ],
            )
            .map(|value| split_csv(&value))
            .unwrap_or_default();
            let workspace_state = capture_trimmed_in(
                &project.project_root,
                "jj",
                &[
                    "log",
                    "-r",
                    "@",
                    "-n",
                    "1",
                    "--no-graph",
                    "-T",
                    "empty ++ \"\\t\" ++ conflict ++ \"\\n\"",
                ],
            )
            .ok();
            let (dirty, conflict) = parse_dirty_conflict_state(workspace_state.as_deref());

            return Ok(Some(ProjectBinding {
                project: project.clone(),
                vcs: RepoVcs::Jj,
                logical_key: format!("jj:{}", repo_root.display()),
                repo_root,
                workspace_root: Some(workspace_root),
                workspace_name,
                current_branches,
                dirty,
                conflict,
            }));
        }
    }

    if let Ok(repo_root) = capture_trimmed_in(
        &project.project_root,
        "git",
        &["rev-parse", "--show-toplevel"],
    ) {
        let repo_root = canonical_or_same(PathBuf::from(repo_root));
        let common_dir = capture_trimmed_in(
            &project.project_root,
            "git",
            &["rev-parse", "--git-common-dir"],
        )?;
        let common_dir = resolve_path(&project.project_root, common_dir.trim());
        let common_dir = canonical_or_same(common_dir);
        let current_branch = capture_trimmed_in(
            &project.project_root,
            "git",
            &["branch", "--show-current"],
        )
        .ok()
        .into_iter()
        .flat_map(|value| split_csv(&value))
        .collect::<Vec<_>>();
        let status = capture_trimmed_in(&project.project_root, "git", &["status", "--porcelain"])
            .unwrap_or_default();
        let dirty = !status.trim().is_empty();
        let conflict = status.lines().any(git_status_line_has_conflict);

        return Ok(Some(ProjectBinding {
            project: project.clone(),
            vcs: RepoVcs::Git,
            logical_key: format!("git:{}", common_dir.display()),
            repo_root,
            workspace_root: None,
            workspace_name: None,
            current_branches: current_branch,
            dirty,
            conflict,
        }));
    }

    Ok(None)
}

fn inspect_jj_repo(seed: &RepoSeed) -> Result<WorkflowRepoSnapshot> {
    let root = preferred_repo_root(seed);
    let repo_slug = jj_repo_slug(&root);
    let (pr_map, pr_error) = fetch_open_prs(repo_slug.as_deref());
    let workspaces = jj_workspace_states(&root)?;
    let bookmark_rows = jj_bookmark_rows(&root)?;
    let commit_meta = jj_commit_meta_by_bookmark(&root)?;
    let default_branch = infer_default_branch_jj(&bookmark_rows, pr_map.values());
    let workspace_roots = seed
        .bindings
        .iter()
        .filter_map(|binding| {
            binding
                .workspace_name
                .as_ref()
                .zip(binding.workspace_root.as_ref())
                .map(|(name, root)| (name.clone(), root.clone()))
        })
        .collect::<HashMap<_, _>>();

    let mut grouped = BTreeMap::<String, Vec<JjBookmarkRow>>::new();
    for row in bookmark_rows {
        grouped.entry(row.name.clone()).or_default().push(row);
    }

    let mut branches = Vec::new();
    for (name, rows) in grouped {
        let Some(local) = rows.iter().find(|row| row.remote.is_none()).cloned() else {
            continue;
        };

        let remotes = rows
            .iter()
            .filter(|row| row.remote.is_some())
            .cloned()
            .collect::<Vec<_>>();
        let tracking = remotes
            .iter()
            .find(|row| row.remote.as_deref() == Some("origin"))
            .or_else(|| remotes.first());
        let workspace_names = workspaces
            .iter()
            .filter(|workspace| workspace.current_branches.iter().any(|branch| branch == &name))
            .map(|workspace| workspace.name.clone())
            .collect::<Vec<_>>();
        let dirty = workspaces
            .iter()
            .any(|workspace| workspace.dirty && workspace.current_branches.iter().any(|branch| branch == &name));
        let workspace_conflict = workspaces
            .iter()
            .any(|workspace| workspace.conflict && workspace.current_branches.iter().any(|branch| branch == &name));
        let meta = commit_meta.get(&name).cloned().unwrap_or_else(|| CommitMeta {
            head_sha: local.target_sha.clone().unwrap_or_default(),
            short_sha: truncate_sha(local.target_sha.as_deref().unwrap_or("")),
            subject: String::new(),
            updated_at: None,
        });
        let pull_request = pr_map.get(&name).cloned();
        let compare_base_branch = pull_request
            .as_ref()
            .map(|pr| pr.base_ref_name.clone())
            .or_else(|| default_branch.clone());
        let is_current = !workspace_names.is_empty();
        let hidden = is_hidden_branch(&name);
        let is_active = is_current
            || dirty
            || local.conflict
            || workspace_conflict
            || pull_request.is_some();

        branches.push(WorkflowBranchSnapshot {
            name: name.clone(),
            head_sha: meta.head_sha,
            short_sha: meta.short_sha,
            subject: meta.subject,
            updated_at: meta.updated_at,
            is_current,
            is_active,
            hidden,
            workspace_names,
            dirty,
            conflict: local.conflict || workspace_conflict,
            tracking_remote: tracking.and_then(|row| row.remote.clone()),
            tracked: tracking.map(|row| row.tracked).unwrap_or(false),
            synced: local.synced,
            ahead_count: tracking.and_then(|row| row.ahead_count),
            behind_count: tracking.and_then(|row| row.behind_count),
            upstream_sha: tracking.and_then(|row| row.target_sha.clone()),
            compare_base_branch,
            pull_request,
        });
    }

    sort_branches(&mut branches);

    let projects = seed
        .bindings
        .iter()
        .map(|binding| WorkflowProjectRef {
            name: binding.project.name.clone(),
            project_root: binding.project.project_root.display().to_string(),
            repo_relative_path: relative_display_path(
                binding.workspace_root.as_deref().unwrap_or(&binding.repo_root),
                &binding.project.project_root,
            ),
            workspace_name: binding.workspace_name.clone(),
            workspace_root: binding
                .workspace_root
                .as_ref()
                .map(|value| value.display().to_string()),
            current_branches: binding.current_branches.clone(),
            dirty: binding.dirty,
            conflict: binding.conflict,
            updated_ms: binding.project.updated_ms,
        })
        .collect::<Vec<_>>();

    let workspaces = workspaces
        .into_iter()
        .map(|workspace| WorkflowWorkspaceSnapshot {
            name: workspace.name.clone(),
            root_path: workspace_roots
                .get(&workspace.name)
                .map(|value| value.display().to_string()),
            current_branches: workspace.current_branches,
            dirty: workspace.dirty,
            conflict: workspace.conflict,
            target_commit: workspace.target_commit,
            description: workspace.description,
        })
        .collect::<Vec<_>>();

    let hidden_branch_count = branches.iter().filter(|branch| branch.hidden).count();
    let active_branch_count = branches
        .iter()
        .filter(|branch| branch.is_active && !branch.hidden)
        .count();
    let open_pr_count = branches
        .iter()
        .filter(|branch| {
            branch
                .pull_request
                .as_ref()
                .map(|pr| pr.state == "OPEN")
                .unwrap_or(false)
        })
        .count();

    Ok(WorkflowRepoSnapshot {
        id: seed.logical_key.clone(),
        name: display_repo_name(repo_slug.as_deref(), &root),
        vcs: seed.vcs.as_str().to_string(),
        repo_key: seed.logical_key.clone(),
        repo_root: root.display().to_string(),
        repo_slug,
        default_branch,
        project_count: projects.len(),
        workspace_count: workspaces.len(),
        active_branch_count,
        open_pr_count,
        hidden_branch_count,
        pr_error,
        projects,
        workspaces,
        branches,
        error: None,
    })
}

fn inspect_git_repo(seed: &RepoSeed) -> Result<WorkflowRepoSnapshot> {
    let root = seed.repo_root.clone();
    let repo_slug = git_repo_slug(&root);
    let (pr_map, pr_error) = fetch_open_prs(repo_slug.as_deref());
    let default_branch = git_default_branch(&root);
    let worktrees = git_worktree_states(&root)?;
    let mut branches = git_branch_rows(&root)?
        .into_iter()
        .map(|row| {
            let workspace_names = worktrees
                .iter()
                .filter(|workspace| workspace.branch.as_deref() == Some(row.name.as_str()))
                .map(|workspace| workspace.name.clone())
                .collect::<Vec<_>>();
            let dirty = worktrees
                .iter()
                .any(|workspace| workspace.dirty && workspace.branch.as_deref() == Some(row.name.as_str()));
            let pull_request = pr_map.get(&row.name).cloned();
            let compare_base_branch = pull_request
                .as_ref()
                .map(|pr| pr.base_ref_name.clone())
                .or_else(|| default_branch.clone());
            let hidden = is_hidden_branch(&row.name);
            let is_current = !workspace_names.is_empty();
            let is_active = is_current || dirty || pull_request.is_some();

            WorkflowBranchSnapshot {
                name: row.name.clone(),
                head_sha: row.head_sha.clone(),
                short_sha: row.short_sha.clone(),
                subject: row.subject.clone(),
                updated_at: row.updated_at.clone(),
                is_current,
                is_active,
                hidden,
                workspace_names,
                dirty,
                conflict: false,
                tracking_remote: row.upstream.clone(),
                tracked: row.upstream.is_some(),
                synced: row.ahead_count.unwrap_or(0) == 0 && row.behind_count.unwrap_or(0) == 0,
                ahead_count: row.ahead_count,
                behind_count: row.behind_count,
                upstream_sha: None,
                compare_base_branch,
                pull_request,
            }
        })
        .collect::<Vec<_>>();

    sort_branches(&mut branches);

    let projects = seed
        .bindings
        .iter()
        .map(|binding| WorkflowProjectRef {
            name: binding.project.name.clone(),
            project_root: binding.project.project_root.display().to_string(),
            repo_relative_path: relative_display_path(&binding.repo_root, &binding.project.project_root),
            workspace_name: None,
            workspace_root: None,
            current_branches: binding.current_branches.clone(),
            dirty: binding.dirty,
            conflict: binding.conflict,
            updated_ms: binding.project.updated_ms,
        })
        .collect::<Vec<_>>();
    let workspaces = worktrees
        .iter()
        .map(|workspace| WorkflowWorkspaceSnapshot {
            name: workspace.name.clone(),
            root_path: Some(workspace.path.display().to_string()),
            current_branches: workspace.branch.clone().into_iter().collect(),
            dirty: workspace.dirty,
            conflict: false,
            target_commit: String::new(),
            description: String::new(),
        })
        .collect::<Vec<_>>();
    let hidden_branch_count = branches.iter().filter(|branch| branch.hidden).count();
    let active_branch_count = branches
        .iter()
        .filter(|branch| branch.is_active && !branch.hidden)
        .count();
    let open_pr_count = branches
        .iter()
        .filter(|branch| {
            branch
                .pull_request
                .as_ref()
                .map(|pr| pr.state == "OPEN")
                .unwrap_or(false)
        })
        .count();

    Ok(WorkflowRepoSnapshot {
        id: seed.logical_key.clone(),
        name: display_repo_name(repo_slug.as_deref(), &root),
        vcs: seed.vcs.as_str().to_string(),
        repo_key: seed.logical_key.clone(),
        repo_root: root.display().to_string(),
        repo_slug,
        default_branch,
        project_count: projects.len(),
        workspace_count: workspaces.len(),
        active_branch_count,
        open_pr_count,
        hidden_branch_count,
        pr_error,
        projects,
        workspaces,
        branches,
        error: None,
    })
}

fn repo_error_snapshot(seed: &RepoSeed, error: String) -> WorkflowRepoSnapshot {
    WorkflowRepoSnapshot {
        id: seed.logical_key.clone(),
        name: display_repo_name(None, &seed.repo_root),
        vcs: seed.vcs.as_str().to_string(),
        repo_key: seed.logical_key.clone(),
        repo_root: seed.repo_root.display().to_string(),
        repo_slug: None,
        default_branch: None,
        project_count: seed.bindings.len(),
        workspace_count: 0,
        active_branch_count: 0,
        open_pr_count: 0,
        hidden_branch_count: 0,
        pr_error: None,
        projects: seed
            .bindings
            .iter()
            .map(|binding| WorkflowProjectRef {
                name: binding.project.name.clone(),
                project_root: binding.project.project_root.display().to_string(),
                repo_relative_path: ".".to_string(),
                workspace_name: binding.workspace_name.clone(),
                workspace_root: binding
                    .workspace_root
                    .as_ref()
                    .map(|value| value.display().to_string()),
                current_branches: binding.current_branches.clone(),
                dirty: binding.dirty,
                conflict: binding.conflict,
                updated_ms: binding.project.updated_ms,
            })
            .collect(),
        workspaces: Vec::new(),
        branches: Vec::new(),
        error: Some(error),
    }
}

fn preferred_repo_root(seed: &RepoSeed) -> PathBuf {
    let mut roots = seed
        .bindings
        .iter()
        .filter_map(|binding| binding.workspace_root.clone())
        .collect::<Vec<_>>();
    if roots.is_empty() {
        return seed.repo_root.clone();
    }
    roots.sort_by(|a, b| {
        root_preference_score(a)
            .cmp(&root_preference_score(b))
            .then_with(|| a.to_string_lossy().len().cmp(&b.to_string_lossy().len()))
    });
    roots[0].clone()
}

fn root_preference_score(path: &Path) -> u8 {
    let text = path.to_string_lossy();
    if text.contains("/.jj/workspaces/") {
        2
    } else if text.contains("/private/tmp/") {
        1
    } else {
        0
    }
}

fn resolve_jj_repo_store(workspace_root: &Path) -> Result<PathBuf> {
    let repo_marker = workspace_root.join(".jj").join("repo");
    if repo_marker.is_dir() {
        return Ok(canonical_or_same(repo_marker));
    }
    if repo_marker.is_file() {
        let target = fs::read_to_string(&repo_marker)
            .with_context(|| format!("failed to read {}", repo_marker.display()))?;
        let resolved = resolve_path(
            repo_marker
                .parent()
                .ok_or_else(|| anyhow!("missing .jj parent for {}", repo_marker.display()))?,
            target.trim(),
        );
        return Ok(canonical_or_same(resolved));
    }
    bail!("expected {} to exist", repo_marker.display())
}

fn jj_workspace_states(repo_root: &Path) -> Result<Vec<WorkspaceState>> {
    let output = capture_trimmed_in(
        repo_root,
        "jj",
        &[
            "workspace",
            "list",
            "-T",
            "name ++ \"\\t\" ++ target.commit_id().short() ++ \"\\t\" ++ target.parents().map(|p| p.local_bookmarks().map(|b| b.name()).join(\",\")).join(\",\") ++ \"\\t\" ++ target.empty() ++ \"\\t\" ++ target.conflict() ++ \"\\t\" ++ target.description().first_line() ++ \"\\n\"",
        ],
    )?;
    let mut workspaces = Vec::new();
    for line in output.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut parts = line.splitn(6, '\t');
        let name = parts.next().unwrap_or("").trim();
        if name.is_empty() {
            continue;
        }
        let target_commit = parts.next().unwrap_or("").trim().to_string();
        let current_branches = split_csv(parts.next().unwrap_or(""));
        let dirty = !parse_bool(parts.next().unwrap_or("true"));
        let conflict = parse_bool(parts.next().unwrap_or("false"));
        let description = parts.next().unwrap_or("").trim().to_string();
        workspaces.push(WorkspaceState {
            name: name.to_string(),
            target_commit,
            current_branches,
            dirty,
            conflict,
            description,
        });
    }
    Ok(workspaces)
}

fn jj_bookmark_rows(repo_root: &Path) -> Result<Vec<JjBookmarkRow>> {
    let output = capture_trimmed_in(
        repo_root,
        "jj",
        &[
            "bookmark",
            "list",
            "--all-remotes",
            "-T",
            "name ++ \"\\t\" ++ remote ++ \"\\t\" ++ tracked ++ \"\\t\" ++ conflict ++ \"\\t\" ++ present ++ \"\\t\" ++ synced ++ \"\\t\" ++ if(tracked, tracking_ahead_count.exact(), \"\") ++ \"\\t\" ++ if(tracked, tracking_behind_count.exact(), \"\") ++ \"\\t\" ++ if(normal_target, normal_target.commit_id().short(), \"\") ++ \"\\n\"",
        ],
    )?;
    let mut rows = Vec::new();
    for line in output.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut parts = line.splitn(9, '\t');
        let name = parts.next().unwrap_or("").trim();
        if name.is_empty() {
            continue;
        }
        rows.push(JjBookmarkRow {
            name: name.to_string(),
            remote: non_empty(parts.next().unwrap_or("")),
            tracked: parse_bool(parts.next().unwrap_or("false")),
            conflict: parse_bool(parts.next().unwrap_or("false")),
            present: parse_bool(parts.next().unwrap_or("false")),
            synced: parse_bool(parts.next().unwrap_or("false")),
            ahead_count: parse_u32(parts.next().unwrap_or("")),
            behind_count: parse_u32(parts.next().unwrap_or("")),
            target_sha: non_empty(parts.next().unwrap_or("")),
        });
    }
    Ok(rows)
}

fn jj_commit_meta_by_bookmark(repo_root: &Path) -> Result<HashMap<String, CommitMeta>> {
    let output = capture_trimmed_in(
        repo_root,
        "jj",
        &[
            "log",
            "-r",
            "bookmarks()",
            "--no-graph",
            "-T",
            "local_bookmarks.map(|b| b.name()).join(\",\") ++ \"\\t\" ++ commit_id.short() ++ \"\\t\" ++ commit_id.short() ++ \"\\t\" ++ description.first_line() ++ \"\\t\" ++ author.timestamp().utc().format(\"%Y-%m-%dT%H:%M:%SZ\") ++ \"\\n\"",
        ],
    )?;
    let mut commits = HashMap::new();
    for line in output.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut parts = line.splitn(5, '\t');
        let names = parts.next().unwrap_or("");
        let head_sha = parts.next().unwrap_or("").trim().to_string();
        let short_sha = parts.next().unwrap_or("").trim().to_string();
        let subject = parts.next().unwrap_or("").trim().to_string();
        let updated_at = non_empty(parts.next().unwrap_or(""));
        for name in split_csv(names) {
            commits.entry(name).or_insert_with(|| CommitMeta {
                head_sha: head_sha.clone(),
                short_sha: short_sha.clone(),
                subject: subject.clone(),
                updated_at: updated_at.clone(),
            });
        }
    }
    Ok(commits)
}

fn jj_repo_slug(repo_root: &Path) -> Option<String> {
    let output = capture_trimmed_in(repo_root, "jj", &["git", "remote", "list"]).ok()?;
    for line in output.lines() {
        let mut parts = line.split_whitespace();
        let remote = parts.next().unwrap_or("");
        let url = parts.next().unwrap_or("");
        if remote == "origin" {
            if let Some(slug) = parse_github_repo_slug(url) {
                return Some(slug);
            }
        }
    }
    output
        .lines()
        .find_map(|line| line.split_whitespace().nth(1))
        .and_then(parse_github_repo_slug)
}

fn infer_default_branch_jj<'a>(
    rows: &[JjBookmarkRow],
    prs: impl Iterator<Item = &'a WorkflowPullRequestSummary>,
) -> Option<String> {
    let local_names = rows
        .iter()
        .filter(|row| row.remote.is_none() && row.present)
        .map(|row| row.name.as_str())
        .collect::<HashSet<_>>();
    if local_names.contains("main") {
        return Some("main".to_string());
    }
    if local_names.contains("master") {
        return Some("master".to_string());
    }

    let mut counts = HashMap::<String, usize>::new();
    for pr in prs {
        *counts.entry(pr.base_ref_name.clone()).or_default() += 1;
    }
    counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(branch, _)| branch)
}

fn git_worktree_states(repo_root: &Path) -> Result<Vec<GitWorktreeState>> {
    let output = capture_trimmed_in(repo_root, "git", &["worktree", "list", "--porcelain"])?;
    let mut worktrees = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut current_branch: Option<String> = None;

    for line in output.lines().chain(std::iter::once("")) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if let Some(path) = current_path.take() {
                let dirty = path.exists()
                    && capture_trimmed_in(&path, "git", &["status", "--porcelain"])
                        .map(|status| !status.trim().is_empty())
                        .unwrap_or(false);
                worktrees.push(GitWorktreeState {
                    name: path
                        .file_name()
                        .and_then(|value| value.to_str())
                        .unwrap_or("worktree")
                        .to_string(),
                    path,
                    branch: current_branch.take(),
                    dirty,
                });
            }
            current_branch = None;
            continue;
        }

        if let Some(path) = trimmed.strip_prefix("worktree ") {
            current_path = Some(PathBuf::from(path.trim()));
            continue;
        }
        if let Some(branch) = trimmed.strip_prefix("branch refs/heads/") {
            current_branch = Some(branch.trim().to_string());
        }
    }

    Ok(worktrees)
}

fn git_branch_rows(repo_root: &Path) -> Result<Vec<GitBranchRow>> {
    let output = capture_trimmed_in(
        repo_root,
        "git",
        &[
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname:short)%09%(objectname)%09%(committerdate:iso-strict)%09%(subject)%09%(upstream:short)%09%(upstream:track)",
            "refs/heads",
        ],
    )?;
    let mut branches = Vec::new();
    for line in output.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut parts = line.splitn(6, '\t');
        let name = parts.next().unwrap_or("").trim();
        if name.is_empty() {
            continue;
        }
        let head_sha = parts.next().unwrap_or("").trim().to_string();
        let updated_at = non_empty(parts.next().unwrap_or(""));
        let subject = parts.next().unwrap_or("").trim().to_string();
        let upstream = non_empty(parts.next().unwrap_or(""));
        let (ahead_count, behind_count) = parse_git_track(parts.next().unwrap_or(""));
        branches.push(GitBranchRow {
            name: name.to_string(),
            short_sha: truncate_sha(&head_sha),
            head_sha,
            updated_at,
            subject,
            upstream,
            ahead_count,
            behind_count,
        });
    }
    Ok(branches)
}

fn git_repo_slug(repo_root: &Path) -> Option<String> {
    capture_trimmed_in(repo_root, "git", &["remote", "get-url", "origin"])
        .ok()
        .as_deref()
        .and_then(parse_github_repo_slug)
}

fn git_default_branch(repo_root: &Path) -> Option<String> {
    capture_trimmed_in(repo_root, "git", &["symbolic-ref", "refs/remotes/origin/HEAD"])
        .ok()
        .and_then(|value| value.trim().rsplit('/').next().map(|branch| branch.to_string()))
        .or_else(|| {
            let branches = capture_trimmed_in(
                repo_root,
                "git",
                &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
            )
            .ok()?;
            let names = branches.lines().map(str::trim).collect::<HashSet<_>>();
            if names.contains("main") {
                Some("main".to_string())
            } else if names.contains("master") {
                Some("master".to_string())
            } else {
                None
            }
        })
}

fn fetch_open_prs(
    repo_slug: Option<&str>,
) -> (HashMap<String, WorkflowPullRequestSummary>, Option<String>) {
    let Some(repo_slug) = repo_slug else {
        return (HashMap::new(), None);
    };

    let output = capture_trimmed(
        "gh",
        &[
            "pr",
            "list",
            "--repo",
            repo_slug,
            "--state",
            "open",
            "--limit",
            "200",
            "--json",
            "number,title,url,state,isDraft,baseRefName,headRefName,updatedAt,reviewDecision",
        ],
    );
    let output = match output {
        Ok(output) => output,
        Err(err) => return (HashMap::new(), Some(err.to_string())),
    };

    let prs: Vec<WorkflowPullRequestSummary> = match serde_json::from_str(&output) {
        Ok(prs) => prs,
        Err(err) => {
            return (
                HashMap::new(),
                Some(format!("failed to parse gh pr list for {repo_slug}: {err}")),
            );
        }
    };

    let by_head = prs
        .into_iter()
        .map(|pr| (pr.head_ref_name.clone(), pr))
        .collect::<HashMap<_, _>>();
    (by_head, None)
}

fn sort_branches(branches: &mut [WorkflowBranchSnapshot]) {
    branches.sort_by(|a, b| {
        branch_rank(b)
            .cmp(&branch_rank(a))
            .then_with(|| b.updated_at.cmp(&a.updated_at))
            .then_with(|| a.name.cmp(&b.name))
    });
}

fn branch_rank(branch: &WorkflowBranchSnapshot) -> u8 {
    if branch.conflict {
        5
    } else if branch.pull_request.is_some() {
        4
    } else if branch.is_current {
        3
    } else if branch.dirty {
        2
    } else if branch.is_active {
        1
    } else {
        0
    }
}

fn is_hidden_branch(name: &str) -> bool {
    name.starts_with("backup/") || name.starts_with("jj/keep/")
}

fn display_repo_name(repo_slug: Option<&str>, repo_root: &Path) -> String {
    repo_slug
        .and_then(|slug| slug.rsplit('/').next())
        .map(str::to_string)
        .or_else(|| {
            repo_root
                .file_name()
                .and_then(|value| value.to_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "repo".to_string())
}

fn relative_display_path(root: &Path, path: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(relative) if relative.as_os_str().is_empty() => ".".to_string(),
        Ok(relative) => relative.display().to_string(),
        Err(_) => path.display().to_string(),
    }
}

fn parse_git_track(value: &str) -> (Option<u32>, Option<u32>) {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return (None, None);
    }
    let ahead = extract_number_after(trimmed, "ahead ");
    let behind = extract_number_after(trimmed, "behind ");
    (ahead, behind)
}

fn extract_number_after(text: &str, needle: &str) -> Option<u32> {
    let start = text.find(needle)? + needle.len();
    let digits = text[start..]
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>();
    digits.parse().ok()
}

fn parse_dirty_conflict_state(value: Option<&str>) -> (bool, bool) {
    let Some(value) = value else {
        return (false, false);
    };
    let mut parts = value.splitn(2, '\t');
    let empty = parse_bool(parts.next().unwrap_or("true"));
    let conflict = parse_bool(parts.next().unwrap_or("false"));
    (!empty, conflict)
}

fn git_status_line_has_conflict(line: &str) -> bool {
    let bytes = line.as_bytes();
    if bytes.len() < 2 {
        return false;
    }
    matches!(
        (bytes[0] as char, bytes[1] as char),
        ('U', _) | (_, 'U') | ('A', 'A') | ('D', 'D')
    )
}

fn parse_github_repo_slug(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches(".git").trim_end_matches('/');
    if let Some(rest) = trimmed.strip_prefix("git@github.com:") {
        return normalize_repo_slug(rest);
    }
    let marker = "github.com/";
    let start = trimmed.find(marker)?;
    normalize_repo_slug(&trimmed[start + marker.len()..])
}

fn normalize_repo_slug(rest: &str) -> Option<String> {
    let mut parts = rest.split('/').filter(|part| !part.is_empty());
    let owner = parts.next()?;
    let repo = parts.next()?;
    Some(format!("{owner}/{repo}"))
}

fn capture_trimmed(command: &str, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new(command);
    cmd.args(args);
    capture_trimmed_inner(&mut cmd)
}

fn capture_trimmed_in(cwd: &Path, command: &str, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new(command);
    cmd.args(args).current_dir(cwd);
    capture_trimmed_inner(&mut cmd)
}

fn capture_trimmed_inner(cmd: &mut Command) -> Result<String> {
    let rendered = format!("{cmd:?}");
    let output = cmd
        .output()
        .with_context(|| format!("failed to run {rendered}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("command failed: {rendered}: {}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn resolve_path(base: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

fn canonical_or_same(path: PathBuf) -> PathBuf {
    path.canonicalize().unwrap_or(path)
}

fn parse_bool(value: &str) -> bool {
    value.trim() == "true"
}

fn parse_u32(value: &str) -> Option<u32> {
    value.trim().parse().ok()
}

fn split_csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

fn first_non_empty_csv(value: &str) -> Option<String> {
    split_csv(value).into_iter().next()
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn truncate_sha(value: &str) -> String {
    value.chars().take(12).collect()
}

fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

#[allow(dead_code)]
fn ms_to_iso(ms: u128) -> Option<String> {
    let ms = i64::try_from(ms).ok()?;
    Utc.timestamp_millis_opt(ms)
        .single()
        .map(|ts| ts.to_rfc3339_opts(SecondsFormat::Secs, true))
}
