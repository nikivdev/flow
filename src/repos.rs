//! Repository management commands.
//!
//! Supports cloning repos into a structured local directory.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::cli::{
    CloneOpts, ReposAction, ReposBootstrapHomeBranchOpts, ReposCloneOpts, ReposCommand,
    ReposHomeBranchStatusOpts, ReposMigrateHomeBranchOpts,
};
use crate::{config, publish, repo_capsule, ssh, ssh_keys, upstream, vcs};

const DEFAULT_HOME_BRANCH: &str = "nikiv";
const DEFAULT_FORK_REMOTE: &str = "fork";
const DEFAULT_REPOS_ROOT: &str = "~/repos";
const DEFAULT_CODE_ROOT: &str = "~/code";
const REPOS_ROOT_OVERRIDE_ENV: &str = "FLOW_REPOS_ALLOW_ROOT_OVERRIDE";

/// Run the repos subcommand.
pub fn run(cmd: ReposCommand) -> Result<()> {
    match cmd.action {
        Some(ReposAction::Clone(opts)) => {
            let no_home_branch_bootstrap = opts.no_home_branch_bootstrap;
            let path = clone_repo(opts)?;
            if !no_home_branch_bootstrap {
                bootstrap_home_branch(
                    &path,
                    &HomeBranchBootstrapOptions {
                        dry_run: false,
                        allow_switch: true,
                        fail_on_dirty: true,
                        home_branch: DEFAULT_HOME_BRANCH.to_string(),
                        quiet: false,
                    },
                )?;
            }
            open_in_zed(&path)?;
            Ok(())
        }
        Some(ReposAction::HomeBranchStatus(opts)) => run_home_branch_status(opts),
        Some(ReposAction::BootstrapHomeBranch(opts)) => run_bootstrap_home_branch(opts),
        Some(ReposAction::MigrateHomeBranch(opts)) => run_migrate_home_branch(opts),
        Some(ReposAction::Create(opts)) => publish::run_github(opts),
        Some(ReposAction::Capsule(opts)) => repo_capsule::run_capsule(opts),
        Some(ReposAction::Alias(cmd)) => repo_capsule::run_alias(cmd),
        None => fuzzy_select_repo(),
    }
}

/// Clone into the current working directory (git clone style destination behavior).
pub fn clone_git_like(opts: CloneOpts) -> Result<()> {
    ssh::ensure_ssh_env();
    let mode = ssh::ssh_mode();
    if matches!(mode, ssh::SshMode::Force) && !ssh::has_identities() {
        match ssh_keys::ensure_default_identity(24) {
            Ok(()) => {}
            Err(err) => {
                bail!(
                    "SSH mode is forced but no key is available. Run `f ssh setup` or `f ssh unlock` (error: {})",
                    err
                );
            }
        }
    }

    let clone_url = resolve_git_like_clone_url(&opts.url)?;
    let mut cmd = Command::new("git");
    cmd.arg("clone").arg(&clone_url);
    if let Some(dir) = opts.directory {
        cmd.arg(dir);
    }

    let status = cmd
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to run git clone")?;

    if !status.success() {
        bail!("git clone failed");
    }

    Ok(())
}

fn open_in_zed(path: &std::path::Path) -> Result<()> {
    let try_open = |app: &str| -> Result<()> {
        let status = std::process::Command::new("open")
            .args(["-a", app])
            .arg(path)
            .status()
            .with_context(|| format!("failed to open {app}"))?;
        if !status.success() {
            bail!("{app} exited with status {status}");
        }
        Ok(())
    };

    try_open("/Applications/Zed.app").or_else(|_| try_open("/Applications/Zed Preview.app"))
}

/// Fuzzy search through repos in ~/repos and print the selected path.
fn fuzzy_select_repo() -> Result<()> {
    let root = config::expand_path(DEFAULT_REPOS_ROOT);
    if !root.exists() {
        println!("No repos directory found at {}", root.display());
        println!("Clone a repo with: f repos clone <url>");
        return Ok(());
    }

    let repos = discover_repos(&root)?;
    if repos.is_empty() {
        println!("No repositories found in {}", root.display());
        println!("Clone a repo with: f repos clone <url>");
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

struct RepoEntry {
    display: String,
    path: PathBuf,
}

#[derive(Debug, Clone)]
struct HomeBranchBootstrapOptions {
    dry_run: bool,
    allow_switch: bool,
    fail_on_dirty: bool,
    home_branch: String,
    quiet: bool,
}

#[derive(Debug, Clone, Serialize)]
struct HomeBranchBootstrapResult {
    repo_root: String,
    changed: bool,
    switched_to_home_branch: bool,
    status: HomeBranchRepoStatus,
    steps: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct HomeBranchMigrationResult {
    repo_root: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bootstrap: Option<HomeBranchBootstrapResult>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum HomeBranchWorkflowMode {
    PrivateMirror,
    DirectPush,
}

#[derive(Debug, Clone, Serialize)]
struct HomeBranchRepoStatus {
    repo_root: String,
    repo_display: String,
    category: HomeBranchRepoCategory,
    workflow_mode: HomeBranchWorkflowMode,
    eligible: bool,
    current_branch: String,
    home_branch: String,
    configured_home_branch: Option<String>,
    configured_public_remote: Option<String>,
    working_tree_dirty: bool,
    public_remote: Option<String>,
    public_default_branch: Option<String>,
    home_branch_exists: bool,
    current_is_home_branch: bool,
    fork_remote_exists: bool,
    fork_remote_url: Option<String>,
    expected_fork_remote_url: Option<String>,
    fork_remote_matches_expected: Option<bool>,
    remote_push_default: Option<String>,
    home_branch_push_remote: Option<String>,
    default_branch_push_remote: Option<String>,
    tracking_remote: Option<String>,
    tracking_merge_ref: Option<String>,
    github_repo_exists: Option<bool>,
    github_default_branch: Option<String>,
    github_private: Option<bool>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum HomeBranchRepoCategory {
    StandardGithub,
    PersonalGithub,
    PartiallyMigratedGithub,
    NonGithub,
}

#[derive(Debug, Clone)]
struct GithubRemoteRef {
    owner: String,
    repo: String,
}

#[derive(Debug, Clone, Deserialize)]
struct PrivateMirrorStatusResponse {
    #[serde(default)]
    github_default_branch: Option<String>,
    #[serde(default)]
    github_private: Option<bool>,
    github_repo_exists: bool,
    #[serde(default)]
    remote_push_default: Option<String>,
    #[serde(default)]
    remote_url: Option<String>,
}

/// Discover repos in either a flat root (`~/code/*`) or owner/repo root (`~/repos/*/*`).
fn discover_repos(root: &Path) -> Result<Vec<RepoEntry>> {
    let mut repos = Vec::new();
    let mut nested_repos = Vec::new();

    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return Ok(repos),
    };

    for entry in entries.flatten() {
        let entry_path = entry.path();
        if !entry_path.is_dir() {
            continue;
        }

        let entry_name = match entry_path.file_name() {
            Some(name) => name.to_string_lossy().to_string(),
            None => continue,
        };

        if entry_name.starts_with('.') {
            continue;
        }

        if entry_path.join(".git").exists() {
            repos.push(RepoEntry {
                display: entry_name,
                path: entry_path,
            });
            continue;
        }

        let repo_entries = match fs::read_dir(&entry_path) {
            Ok(entries) => entries,
            Err(_) => continue,
        };

        for repo_entry in repo_entries.flatten() {
            let repo_path = repo_entry.path();
            if !repo_path.is_dir() {
                continue;
            }

            let repo_name = match repo_path.file_name() {
                Some(name) => name.to_string_lossy().to_string(),
                None => continue,
            };

            // Skip hidden directories
            if repo_name.starts_with('.') {
                continue;
            }

            // Check if it's a git repo
            if repo_path.join(".git").exists() {
                nested_repos.push(RepoEntry {
                    display: format!("{}/{}", entry_name, repo_name),
                    path: repo_path,
                });
            }
        }
    }

    if repos.is_empty() {
        repos = nested_repos;
    } else {
        repos.extend(nested_repos);
    }

    repos.sort_by(|a, b| a.display.cmp(&b.display));
    Ok(repos)
}

fn run_fzf(entries: &[RepoEntry]) -> Result<Option<&RepoEntry>> {
    let mut child = Command::new("fzf")
        .arg("--prompt")
        .arg("repo> ")
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

#[derive(Debug, Clone)]
pub(crate) struct RepoRef {
    pub(crate) owner: String,
    pub(crate) repo: String,
}

#[derive(Debug, Deserialize)]
struct RepoInfo {
    fork: bool,
    parent: Option<RepoParent>,
    source: Option<RepoParent>,
}

#[derive(Debug, Deserialize)]
struct RepoParent {
    #[serde(rename = "ssh_url")]
    ssh_url: String,
    #[serde(default)]
    clone_url: Option<String>,
}

#[derive(Debug)]
enum RepoTarget {
    GitHub(RepoRef),
    Generic(GenericRepoRef),
}

#[derive(Debug)]
struct GenericRepoRef {
    path: Vec<String>,
    clone_url: String,
}

pub(crate) fn clone_repo(opts: ReposCloneOpts) -> Result<PathBuf> {
    ssh::ensure_ssh_env();
    let mode = ssh::ssh_mode();
    if matches!(mode, ssh::SshMode::Force) && !ssh::has_identities() {
        match ssh_keys::ensure_default_identity(24) {
            Ok(()) => {}
            Err(err) => {
                bail!(
                    "SSH mode is forced but no key is available. Run `f ssh setup` or `f ssh unlock` (error: {})",
                    err
                );
            }
        }
    }
    // Always prefer SSH for GitHub clone/upstream URLs.
    let prefer_ssh = true;
    let repo_target = parse_repo_target(&opts.url)?;
    let root = normalize_root(&opts.root)?;
    let mut github_ref: Option<RepoRef> = None;
    let (target_dir, clone_url, is_github) = match repo_target {
        RepoTarget::GitHub(repo_ref) => {
            github_ref = Some(RepoRef {
                owner: repo_ref.owner.clone(),
                repo: repo_ref.repo.clone(),
            });
            let owner_dir = root.join(&repo_ref.owner);
            let target_dir = owner_dir.join(&repo_ref.repo);
            let clone_url = if prefer_ssh {
                format!("git@github.com:{}/{}.git", repo_ref.owner, repo_ref.repo)
            } else {
                format!(
                    "https://github.com/{}/{}.git",
                    repo_ref.owner, repo_ref.repo
                )
            };
            (target_dir, clone_url, true)
        }
        RepoTarget::Generic(repo_ref) => {
            let mut target_dir = root.to_path_buf();
            let path_len = repo_ref.path.len();
            let parts = if path_len >= 2 {
                &repo_ref.path[path_len - 2..]
            } else {
                repo_ref.path.as_slice()
            };
            for part in parts {
                target_dir = target_dir.join(part);
            }
            (target_dir, repo_ref.clone_url, false)
        }
    };

    if preflight_clone_target(&target_dir)? {
        println!("Already cloned: {}", target_dir.display());
        return Ok(target_dir);
    }

    if let Some(parent) = target_dir.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let shallow = !opts.full;
    let complete_history_before_bootstrap = shallow
        && should_complete_history_before_clone_bootstrap(
            &opts,
            is_github,
            home_branch_workflow_mode(&target_dir),
        );
    let fetch_depth = if shallow { Some(1) } else { None };
    run_git_clone(&clone_url, &target_dir, shallow)?;

    println!("✓ cloned to {}", target_dir.display());

    if opts.no_upstream {
        if shallow {
            if complete_history_before_bootstrap {
                fetch_complete_history_for_clone(&target_dir, false)?;
            } else {
                spawn_background_history_fetch(&target_dir, false)?;
            }
        }
        init_jj_repo(&target_dir)?;
        return Ok(target_dir);
    }

    if !is_github {
        let upstream_url = opts
            .upstream_url
            .clone()
            .unwrap_or_else(|| clone_url.clone());
        let upstream_is_origin = upstream_url.trim() == clone_url.as_str();
        if upstream_is_origin {
            println!("No upstream provided; using origin as upstream.");
        }
        configure_upstream(&target_dir, &upstream_url, fetch_depth)?;
        if shallow {
            if complete_history_before_bootstrap {
                fetch_complete_history_for_clone(&target_dir, !upstream_is_origin)?;
            } else {
                spawn_background_history_fetch(&target_dir, !upstream_is_origin)?;
            }
        }
        init_jj_repo(&target_dir)?;
        return Ok(target_dir);
    }

    let upstream_url = if let Some(url) = opts.upstream_url {
        Some(url)
    } else {
        let repo_ref = github_ref
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("missing GitHub repo reference"))?;
        resolve_upstream_url(repo_ref, prefer_ssh)?
    };

    let (upstream_url, upstream_is_origin) = match upstream_url {
        Some(url) => {
            let is_origin = url.trim() == clone_url.as_str();
            (url, is_origin)
        }
        None => {
            println!("No fork detected; using origin as upstream.");
            (clone_url.clone(), true)
        }
    };

    configure_upstream(&target_dir, &upstream_url, fetch_depth)?;
    if shallow {
        if complete_history_before_bootstrap {
            fetch_complete_history_for_clone(&target_dir, !upstream_is_origin)?;
        } else {
            spawn_background_history_fetch(&target_dir, !upstream_is_origin)?;
        }
    }

    init_jj_repo(&target_dir)?;
    Ok(target_dir)
}

fn should_complete_history_before_clone_bootstrap(
    opts: &ReposCloneOpts,
    is_github: bool,
    workflow_mode: HomeBranchWorkflowMode,
) -> bool {
    is_github
        && !opts.full
        && !opts.no_home_branch_bootstrap
        && workflow_mode == HomeBranchWorkflowMode::PrivateMirror
}

fn preflight_clone_target(target_dir: &Path) -> Result<bool> {
    match clone_target_state(target_dir)? {
        CloneTargetState::Missing | CloneTargetState::EmptyDir => Ok(false),
        CloneTargetState::GitCheckout => Ok(true),
        CloneTargetState::OccupiedNonRepo => bail!(
            "target path exists but is not a git checkout: {}",
            target_dir.display()
        ),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloneTargetState {
    Missing,
    EmptyDir,
    GitCheckout,
    OccupiedNonRepo,
}

fn clone_target_state(path: &Path) -> Result<CloneTargetState> {
    if !path.exists() {
        return Ok(CloneTargetState::Missing);
    }

    if path.join(".git").exists() {
        return Ok(CloneTargetState::GitCheckout);
    }

    if !path.is_dir() {
        return Ok(CloneTargetState::OccupiedNonRepo);
    }

    let mut entries =
        fs::read_dir(path).with_context(|| format!("failed to inspect {}", path.display()))?;
    if entries.next().is_none() {
        return Ok(CloneTargetState::EmptyDir);
    }

    Ok(CloneTargetState::OccupiedNonRepo)
}

fn init_jj_repo(repo_dir: &Path) -> Result<()> {
    if repo_dir.join(".jj").exists() {
        return Ok(());
    }
    if vcs::ensure_jj_installed().is_err() {
        println!("⚠ jj not found; skipping jj init");
        return Ok(());
    }

    let has_git = repo_dir.join(".git").exists();
    let mut cmd = Command::new("jj");
    cmd.current_dir(repo_dir).arg("git").arg("init");
    if has_git {
        cmd.arg("--colocate");
    }
    let status = cmd.status().context("failed to run jj git init")?;
    if !status.success() {
        println!("⚠ jj git init failed; continuing");
        return Ok(());
    }

    let _ = Command::new("jj")
        .current_dir(repo_dir)
        .args(["git", "fetch"])
        .status();

    if jj_auto_track(repo_dir) {
        let branch = jj_default_branch(repo_dir);
        let remote = jj_default_remote(repo_dir);
        let track_ref = format!("{}@{}", branch, remote);
        let _ = Command::new("jj")
            .current_dir(repo_dir)
            .args(["bookmark", "track", &track_ref])
            .status();
    }

    println!("✓ jj initialized for {}", repo_dir.display());
    Ok(())
}

fn jj_default_remote(repo_dir: &Path) -> String {
    if let Some(cfg) = load_jj_config(repo_dir) {
        if let Some(remote) = cfg.remote {
            return remote;
        }
    }
    "origin".to_string()
}

fn jj_auto_track(repo_dir: &Path) -> bool {
    load_jj_config(repo_dir)
        .and_then(|cfg| cfg.auto_track)
        .unwrap_or(true)
}

fn jj_default_branch(repo_dir: &Path) -> String {
    if let Some(cfg) = load_jj_config(repo_dir) {
        if let Some(branch) = cfg.default_branch {
            return branch;
        }
    }
    if git_ref_exists(repo_dir, "refs/remotes/origin/main")
        || git_ref_exists(repo_dir, "refs/heads/main")
    {
        return "main".to_string();
    }
    if git_ref_exists(repo_dir, "refs/remotes/origin/master")
        || git_ref_exists(repo_dir, "refs/heads/master")
    {
        return "master".to_string();
    }
    "main".to_string()
}

fn git_ref_exists(repo_dir: &Path, reference: &str) -> bool {
    Command::new("git")
        .current_dir(repo_dir)
        .args(["rev-parse", "--verify", reference])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn load_jj_config(repo_dir: &Path) -> Option<config::JjConfig> {
    let local = repo_dir.join("flow.toml");
    if local.exists() {
        if let Ok(cfg) = config::load(&local) {
            if cfg.jj.is_some() {
                return cfg.jj;
            }
        }
    }

    let global = config::default_config_path();
    if global.exists() {
        if let Ok(cfg) = config::load(&global) {
            if cfg.jj.is_some() {
                return cfg.jj;
            }
        }
    }

    None
}

fn run_home_branch_status(opts: ReposHomeBranchStatusOpts) -> Result<()> {
    if let Some(root) = opts.root.as_deref() {
        let root = normalize_root(root)?;
        let statuses = discover_repos(&root)?
            .into_iter()
            .map(|entry| repo_home_branch_status(&entry.path, DEFAULT_HOME_BRANCH))
            .collect::<Result<Vec<_>>>()?;
        if opts.json {
            println!("{}", serde_json::to_string_pretty(&statuses)?);
        } else {
            for status in statuses {
                print_home_branch_status(&status);
            }
        }
        return Ok(());
    }

    let repo_root = resolve_repo_root(opts.path.as_deref())?;
    let status = repo_home_branch_status(&repo_root, DEFAULT_HOME_BRANCH)?;
    if opts.json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        print_home_branch_status(&status);
    }
    Ok(())
}

fn run_bootstrap_home_branch(opts: ReposBootstrapHomeBranchOpts) -> Result<()> {
    let repo_root = resolve_repo_root(opts.path.as_deref())?;
    let result = bootstrap_home_branch(
        &repo_root,
        &HomeBranchBootstrapOptions {
            dry_run: opts.dry_run,
            allow_switch: !opts.no_switch,
            fail_on_dirty: false,
            home_branch: DEFAULT_HOME_BRANCH.to_string(),
            quiet: opts.json,
        },
    )?;
    if opts.json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        print_home_branch_bootstrap_result(&result);
    }
    Ok(())
}

fn run_migrate_home_branch(opts: ReposMigrateHomeBranchOpts) -> Result<()> {
    let root = normalize_root(&opts.root)?;
    let only = opts.only.as_deref().map(str::to_string);
    let skip = opts.skip.as_deref().map(str::to_string);
    let mut results = Vec::new();

    for entry in discover_repos(&root)? {
        if let Some(pattern) = only.as_deref()
            && !entry.display.contains(pattern)
        {
            continue;
        }
        if let Some(pattern) = skip.as_deref()
            && entry.display.contains(pattern)
        {
            continue;
        }

        let result = match bootstrap_home_branch(
            &entry.path,
            &HomeBranchBootstrapOptions {
                dry_run: opts.dry_run,
                allow_switch: true,
                fail_on_dirty: false,
                home_branch: DEFAULT_HOME_BRANCH.to_string(),
                quiet: opts.json,
            },
        ) {
            Ok(bootstrap) => HomeBranchMigrationResult {
                repo_root: entry.path.display().to_string(),
                ok: true,
                error: None,
                bootstrap: Some(bootstrap),
            },
            Err(err) => HomeBranchMigrationResult {
                repo_root: entry.path.display().to_string(),
                ok: false,
                error: Some(err.to_string()),
                bootstrap: None,
            },
        };

        if !result.ok && !opts.continue_on_error {
            if opts.json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            }
            if let Some(error) = result.error {
                bail!("{}", error);
            }
        }

        results.push(result);
    }

    if opts.json {
        println!("{}", serde_json::to_string_pretty(&results)?);
    } else {
        for result in &results {
            if result.ok {
                if let Some(bootstrap) = result.bootstrap.as_ref() {
                    print_home_branch_bootstrap_result(bootstrap);
                }
            } else if let Some(error) = result.error.as_deref() {
                eprintln!("{}: {}", result.repo_root, error);
            }
        }
    }

    Ok(())
}

fn bootstrap_home_branch(
    repo_root: &Path,
    options: &HomeBranchBootstrapOptions,
) -> Result<HomeBranchBootstrapResult> {
    let mut steps = Vec::new();
    let mut changed = false;
    let mut switched_to_home_branch = false;
    let mut status = repo_home_branch_status(repo_root, &options.home_branch)?;

    if !status.eligible {
        let result = HomeBranchBootstrapResult {
            repo_root: repo_root.display().to_string(),
            changed: false,
            switched_to_home_branch: false,
            status,
            steps: vec!["skipped: repo is not a supported GitHub checkout".to_string()],
        };
        return Ok(result);
    }

    if options.fail_on_dirty && status.working_tree_dirty {
        bail!(
            "refusing to bootstrap dirty repo {} during clone bootstrap",
            repo_root.display()
        );
    }

    let public_remote = status
        .public_remote
        .clone()
        .ok_or_else(|| anyhow::anyhow!("missing public remote for {}", repo_root.display()))?;
    let public_default_branch = status.public_default_branch.clone().ok_or_else(|| {
        anyhow::anyhow!("missing public default branch for {}", repo_root.display())
    })?;
    let home_branch = status.home_branch.clone();

    if status.configured_home_branch.as_deref() != Some(home_branch.as_str()) {
        steps.push(format!("set git config flow.homeBranch={home_branch}"));
        if !options.dry_run {
            git_run_in(
                repo_root,
                &["config", "--local", "flow.homeBranch", &home_branch],
                options.quiet,
            )?;
        }
        changed = true;
    }

    if status.configured_public_remote.as_deref() != Some(public_remote.as_str()) {
        steps.push(format!("set git config flow.publicRemote={public_remote}"));
        if !options.dry_run {
            git_run_in(
                repo_root,
                &["config", "--local", "flow.publicRemote", &public_remote],
                options.quiet,
            )?;
        }
        changed = true;
    }

    if !status.home_branch_exists {
        steps.push(format!(
            "create branch {home_branch} from {public_remote}/{public_default_branch}"
        ));
        if !options.dry_run {
            git_run_in(
                repo_root,
                &[
                    "branch",
                    &home_branch,
                    &format!("{public_remote}/{public_default_branch}"),
                ],
                options.quiet,
            )?;
        }
        changed = true;
    }

    if !git_ref_exists_in(repo_root, &format!("refs/heads/{public_default_branch}")) {
        steps.push(format!(
            "create branch {public_default_branch} from {public_remote}/{public_default_branch}"
        ));
        if !options.dry_run {
            git_run_in(
                repo_root,
                &[
                    "branch",
                    &public_default_branch,
                    &format!("{public_remote}/{public_default_branch}"),
                ],
                options.quiet,
            )?;
        }
        changed = true;
    }

    let default_branch_remote_key = format!("branch.{public_default_branch}.remote");
    let default_branch_merge_key = format!("branch.{public_default_branch}.merge");
    let default_branch_merge_ref = format!("refs/heads/{public_default_branch}");
    if git_config_get(repo_root, &default_branch_remote_key).as_deref()
        != Some(public_remote.as_str())
        || git_config_get(repo_root, &default_branch_merge_key).as_deref()
            != Some(default_branch_merge_ref.as_str())
    {
        steps.push(format!(
            "set {public_default_branch} tracking to {public_remote}/{public_default_branch}"
        ));
        if !options.dry_run {
            git_run_in(
                repo_root,
                &[
                    "config",
                    "--local",
                    &default_branch_remote_key,
                    &public_remote,
                ],
                options.quiet,
            )?;
            git_run_in(
                repo_root,
                &[
                    "config",
                    "--local",
                    &default_branch_merge_key,
                    &default_branch_merge_ref,
                ],
                options.quiet,
            )?;
        }
        changed = true;
    }

    if status.tracking_remote.as_deref() != Some(public_remote.as_str())
        || status.tracking_merge_ref.as_deref() != Some(public_default_branch.as_str())
    {
        steps.push(format!(
            "set {home_branch} tracking to {public_remote}/{public_default_branch}"
        ));
        if !options.dry_run {
            git_run_in(
                repo_root,
                &[
                    "config",
                    "--local",
                    &format!("branch.{home_branch}.remote"),
                    &public_remote,
                ],
                options.quiet,
            )?;
            git_run_in(
                repo_root,
                &[
                    "config",
                    "--local",
                    &format!("branch.{home_branch}.merge"),
                    &format!("refs/heads/{public_default_branch}"),
                ],
                options.quiet,
            )?;
        }
        changed = true;
    }

    if should_switch_to_home_branch(&status, options.allow_switch) {
        steps.push(format!("switch working tree to {home_branch}"));
        if !options.dry_run {
            git_run_in(repo_root, &["switch", &home_branch], options.quiet)?;
        }
        changed = true;
        switched_to_home_branch = true;
    }

    match status.workflow_mode {
        HomeBranchWorkflowMode::PrivateMirror => {
            let mirror_status_before =
                private_mirror_status(repo_root, &public_default_branch).ok();
            let needs_mirror_ensure = mirror_status_before
                .as_ref()
                .map(|mirror_status| {
                    mirror_status.remote_push_default.as_deref() != Some(DEFAULT_FORK_REMOTE)
                        || !mirror_status.github_repo_exists
                        || mirror_status.github_default_branch.as_deref()
                            != Some(public_default_branch.as_str())
                        || mirror_status.remote_url.as_deref()
                            != status.expected_fork_remote_url.as_deref()
                })
                .unwrap_or(true)
                || status.remote_push_default.as_deref() != Some(DEFAULT_FORK_REMOTE)
                || status.home_branch_push_remote.as_deref() != Some(DEFAULT_FORK_REMOTE)
                || status.default_branch_push_remote.as_deref() != Some(DEFAULT_FORK_REMOTE);

            if needs_mirror_ensure {
                steps.push(format!(
                    "ensure private mirror trunk {public_default_branch} and push {home_branch} to {}",
                    DEFAULT_FORK_REMOTE
                ));
                if !options.dry_run {
                    ensure_private_mirror(
                        repo_root,
                        &home_branch,
                        &public_default_branch,
                        options.quiet,
                    )?;
                }
                changed = true;
            }
        }
        HomeBranchWorkflowMode::DirectPush => {
            let push_remote = resolve_push_remote_name(repo_root, status.public_remote.as_deref())
                .ok_or_else(|| {
                    anyhow::anyhow!("missing push remote for {}", repo_root.display())
                })?;
            let needs_push_config = status.remote_push_default.as_deref()
                != Some(push_remote.as_str())
                || status.home_branch_push_remote.as_deref() != Some(push_remote.as_str())
                || status.default_branch_push_remote.as_deref() != Some(push_remote.as_str());

            if needs_push_config {
                steps.push(format!(
                    "set {home_branch} and {public_default_branch} push remote to {push_remote}"
                ));
                if !options.dry_run {
                    ensure_direct_push_remote(
                        repo_root,
                        &home_branch,
                        &public_default_branch,
                        &push_remote,
                        options.quiet,
                    )?;
                }
                changed = true;
            }
        }
    }

    status = repo_home_branch_status(repo_root, &options.home_branch)?;
    Ok(HomeBranchBootstrapResult {
        repo_root: repo_root.display().to_string(),
        changed,
        switched_to_home_branch,
        status,
        steps,
    })
}

fn repo_home_branch_status(
    repo_root: &Path,
    requested_home_branch: &str,
) -> Result<HomeBranchRepoStatus> {
    let repo_root = repo_root
        .canonicalize()
        .unwrap_or_else(|_| repo_root.to_path_buf());
    if !repo_root.join(".git").exists() {
        bail!("not a git repository: {}", repo_root.display());
    }

    let current_branch = git_capture_in(&repo_root, &["branch", "--show-current"])
        .unwrap_or_else(|_| "HEAD".to_string());
    let configured_home_branch = config::configured_home_branch_for_repo(&repo_root);
    let home_branch = configured_home_branch
        .clone()
        .unwrap_or_else(|| requested_home_branch.to_string());
    let configured_public_remote = config::configured_public_remote_for_repo(&repo_root);
    let origin_url = git_config_get(&repo_root, "remote.origin.url");
    let upstream_url = git_config_get(&repo_root, "remote.upstream.url");
    let fork_remote_url = git_config_get(&repo_root, "remote.fork.url");
    let public_remote = resolve_public_remote_name(
        configured_public_remote.as_deref(),
        origin_url.as_deref(),
        upstream_url.as_deref(),
    );
    let public_default_branch = public_remote
        .as_deref()
        .and_then(|remote| resolve_remote_default_branch_in(&repo_root, remote));
    let home_branch_exists = git_ref_exists_in(&repo_root, &format!("refs/heads/{home_branch}"));
    let tracking_remote = git_config_get(&repo_root, &format!("branch.{home_branch}.remote"));
    let tracking_merge_ref = git_config_get(&repo_root, &format!("branch.{home_branch}.merge"))
        .map(|value| value.trim_start_matches("refs/heads/").to_string());
    let home_branch_push_remote =
        git_config_get(&repo_root, &format!("branch.{home_branch}.pushRemote"));
    let default_branch_push_remote = public_default_branch
        .as_deref()
        .and_then(|branch| git_config_get(&repo_root, &format!("branch.{branch}.pushRemote")));
    let gh_login = github_login().ok();
    let public_github_ref = public_remote
        .as_deref()
        .and_then(|remote| git_config_get(&repo_root, &format!("remote.{remote}.url")))
        .as_deref()
        .and_then(parse_github_remote_url);
    let workflow_mode = home_branch_workflow_mode(&repo_root);
    let mirror_repo_name = public_github_ref
        .as_ref()
        .map(|github_ref| format!("{}-i", github_ref.repo));
    let expected_fork_remote_url = match (gh_login.as_deref(), mirror_repo_name.as_deref()) {
        (Some(owner), Some(repo)) => Some(format!("git@github.com:{owner}/{repo}.git")),
        _ => None,
    };
    let private_mirror_status =
        if workflow_mode == HomeBranchWorkflowMode::PrivateMirror && public_github_ref.is_some() {
            private_mirror_status(
                &repo_root,
                public_default_branch
                    .as_deref()
                    .unwrap_or(home_branch.as_str()),
            )
            .ok()
        } else {
            None
        };
    let category = classify_repo_category(
        public_github_ref.as_ref(),
        gh_login.as_deref(),
        origin_url.as_deref(),
        upstream_url.as_deref(),
        fork_remote_url.is_some(),
    );
    Ok(HomeBranchRepoStatus {
        repo_root: repo_root.display().to_string(),
        repo_display: repo_display(&repo_root),
        category,
        workflow_mode,
        eligible: public_github_ref.is_some(),
        current_branch: current_branch.trim().to_string(),
        home_branch: home_branch.clone(),
        configured_home_branch,
        configured_public_remote,
        working_tree_dirty: working_tree_dirty(&repo_root)?,
        public_remote,
        public_default_branch,
        home_branch_exists,
        current_is_home_branch: current_branch.trim() == home_branch,
        fork_remote_exists: fork_remote_url.is_some(),
        fork_remote_url,
        expected_fork_remote_url: expected_fork_remote_url.clone(),
        fork_remote_matches_expected: match (
            git_config_get(&repo_root, "remote.fork.url"),
            expected_fork_remote_url,
        ) {
            (Some(actual), Some(expected)) => {
                Some(normalize_git_url(&actual) == normalize_git_url(&expected))
            }
            (Some(_), None) => None,
            (None, Some(_)) => Some(false),
            (None, None) => None,
        },
        remote_push_default: private_mirror_status
            .as_ref()
            .and_then(|status| status.remote_push_default.clone())
            .or_else(|| git_config_get(&repo_root, "remote.pushDefault")),
        home_branch_push_remote,
        default_branch_push_remote,
        tracking_remote,
        tracking_merge_ref,
        github_repo_exists: private_mirror_status
            .as_ref()
            .map(|status| status.github_repo_exists),
        github_default_branch: private_mirror_status
            .as_ref()
            .and_then(|status| status.github_default_branch.clone()),
        github_private: private_mirror_status
            .as_ref()
            .and_then(|status| status.github_private),
    })
}

fn resolve_repo_root(path: Option<&str>) -> Result<PathBuf> {
    if let Some(path) = path {
        let expanded = config::expand_path(path);
        return expanded
            .canonicalize()
            .with_context(|| format!("failed to resolve {}", expanded.display()));
    }

    let cwd = std::env::current_dir().context("failed to resolve current directory")?;
    cwd.canonicalize()
        .with_context(|| format!("failed to resolve {}", cwd.display()))
}

fn should_switch_to_home_branch(status: &HomeBranchRepoStatus, allow_switch: bool) -> bool {
    if !allow_switch || status.current_is_home_branch || status.working_tree_dirty {
        return false;
    }

    if status.current_branch == "HEAD" {
        return false;
    }

    match status.public_default_branch.as_deref() {
        Some(default_branch) => status.current_branch == default_branch,
        None => false,
    }
}

fn classify_repo_category(
    public_github_ref: Option<&GithubRemoteRef>,
    gh_login: Option<&str>,
    origin_url: Option<&str>,
    upstream_url: Option<&str>,
    has_fork_remote: bool,
) -> HomeBranchRepoCategory {
    let Some(public_github_ref) = public_github_ref else {
        return HomeBranchRepoCategory::NonGithub;
    };

    if has_fork_remote {
        return HomeBranchRepoCategory::PartiallyMigratedGithub;
    }

    if Some(public_github_ref.owner.as_str()) == gh_login
        && upstream_url.is_none()
        && origin_url.is_some()
    {
        return HomeBranchRepoCategory::PersonalGithub;
    }

    HomeBranchRepoCategory::StandardGithub
}

fn resolve_public_remote_name(
    configured_public_remote: Option<&str>,
    origin_url: Option<&str>,
    upstream_url: Option<&str>,
) -> Option<String> {
    if let Some(remote) = configured_public_remote {
        return Some(remote.to_string());
    }

    match (origin_url, upstream_url) {
        (Some(origin_url), Some(upstream_url)) => {
            if normalize_git_url(origin_url) == normalize_git_url(upstream_url) {
                Some("origin".to_string())
            } else {
                Some("upstream".to_string())
            }
        }
        (Some(_), None) => Some("origin".to_string()),
        (None, Some(_)) => Some("upstream".to_string()),
        (None, None) => None,
    }
}

fn resolve_push_remote_name(repo_root: &Path, public_remote: Option<&str>) -> Option<String> {
    let preferred = config::preferred_git_remote_for_repo(repo_root);
    if git_config_get(repo_root, &format!("remote.{preferred}.url")).is_some() {
        return Some(preferred);
    }

    public_remote.map(ToString::to_string)
}

fn home_branch_workflow_mode(repo_root: &Path) -> HomeBranchWorkflowMode {
    let repos_root = config::expand_path(DEFAULT_REPOS_ROOT);
    if repo_root.starts_with(&repos_root) {
        HomeBranchWorkflowMode::PrivateMirror
    } else {
        HomeBranchWorkflowMode::DirectPush
    }
}

fn repo_display(repo_root: &Path) -> String {
    let repos_root = config::expand_path(DEFAULT_REPOS_ROOT);
    if let Ok(relative) = repo_root.strip_prefix(&repos_root) {
        return relative.display().to_string();
    }
    let code_root = config::expand_path(DEFAULT_CODE_ROOT);
    if let Ok(relative) = repo_root.strip_prefix(&code_root) {
        return relative.display().to_string();
    }
    repo_root.display().to_string()
}

fn working_tree_dirty(repo_root: &Path) -> Result<bool> {
    Ok(!git_capture_in(repo_root, &["status", "--porcelain"])?
        .trim()
        .is_empty())
}

fn ensure_private_mirror(
    repo_root: &Path,
    home_branch: &str,
    default_branch: &str,
    quiet: bool,
) -> Result<()> {
    ensure_complete_history_before_private_mirror(repo_root, quiet)?;
    run_private_mirror_script(
        repo_root,
        &[
            "ensure",
            "--repo-root",
            &repo_root.display().to_string(),
            "--branch",
            default_branch,
            "--default-branch",
            default_branch,
        ],
    )?;
    git_run_in(
        repo_root,
        &[
            "config",
            "--local",
            "remote.pushDefault",
            DEFAULT_FORK_REMOTE,
        ],
        quiet,
    )?;
    git_run_in(
        repo_root,
        &[
            "config",
            "--local",
            &format!("branch.{default_branch}.pushRemote"),
            DEFAULT_FORK_REMOTE,
        ],
        quiet,
    )?;
    git_run_in(
        repo_root,
        &[
            "config",
            "--local",
            &format!("branch.{home_branch}.pushRemote"),
            DEFAULT_FORK_REMOTE,
        ],
        quiet,
    )?;
    if home_branch != default_branch {
        git_run_in(
            repo_root,
            &[
                "push",
                DEFAULT_FORK_REMOTE,
                &format!("{home_branch}:{home_branch}"),
            ],
            quiet,
        )?;
    }
    Ok(())
}

fn ensure_complete_history_before_private_mirror(repo_root: &Path, quiet: bool) -> Result<()> {
    if !git_repo_is_shallow(repo_root)? {
        return Ok(());
    }

    let has_distinct_upstream = repo_has_distinct_upstream_remote(repo_root);
    if !quiet {
        println!("Completing git history before private mirror bootstrap...");
    }
    fetch_complete_history(repo_root, has_distinct_upstream, quiet)
}

fn ensure_direct_push_remote(
    repo_root: &Path,
    home_branch: &str,
    default_branch: &str,
    push_remote: &str,
    quiet: bool,
) -> Result<()> {
    git_run_in(
        repo_root,
        &["config", "--local", "remote.pushDefault", push_remote],
        quiet,
    )?;
    git_run_in(
        repo_root,
        &[
            "config",
            "--local",
            &format!("branch.{default_branch}.pushRemote"),
            push_remote,
        ],
        quiet,
    )?;
    git_run_in(
        repo_root,
        &[
            "config",
            "--local",
            &format!("branch.{home_branch}.pushRemote"),
            push_remote,
        ],
        quiet,
    )?;
    Ok(())
}

fn private_mirror_status(repo_root: &Path, branch: &str) -> Result<PrivateMirrorStatusResponse> {
    let output = run_private_mirror_script(
        repo_root,
        &[
            "status",
            "--repo-root",
            &repo_root.display().to_string(),
            "--branch",
            branch,
        ],
    )?;
    serde_json::from_str(output.trim()).context("failed to parse private mirror status JSON")
}

fn run_private_mirror_script(repo_root: &Path, args: &[&str]) -> Result<String> {
    let script_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("scripts")
        .join("private_mirror.py");
    let output = Command::new("python3")
        .arg(&script_path)
        .args(args)
        .current_dir(repo_root)
        .output()
        .with_context(|| format!("failed to run {}", script_path.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let message = stderr.trim();
        if !message.is_empty() {
            bail!("{}", message);
        }
        let message = stdout.trim();
        if !message.is_empty() {
            bail!("{}", message);
        }
        bail!("{} failed", script_path.display());
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn parse_github_remote_url(url: &str) -> Option<GithubRemoteRef> {
    let trimmed = url.trim().trim_end_matches('/');
    let path = if let Some(rest) = trimmed.strip_prefix("git@github.com:") {
        rest.trim_end_matches(".git")
    } else if let Some(rest) = trimmed.strip_prefix("https://") {
        let rest = rest.strip_prefix("github.com/").or_else(|| {
            let (_userinfo, host_and_path) = rest.split_once('@')?;
            host_and_path.strip_prefix("github.com/")
        })?;
        rest.trim_end_matches(".git")
    } else if let Some(rest) = trimmed.strip_prefix("https://github.com/") {
        rest.trim_end_matches(".git")
    } else {
        return None;
    };
    let (owner, repo) = path.split_once('/')?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(GithubRemoteRef {
        owner: owner.to_string(),
        repo: repo.to_string(),
    })
}

fn normalize_git_url(url: &str) -> String {
    url.trim()
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .to_string()
}

fn github_login() -> Result<String> {
    let output = Command::new("gh")
        .args(["api", "user", "-q", ".login"])
        .output()
        .context("failed to run gh api user")?;
    if !output.status.success() {
        bail!("gh api user failed");
    }
    let login = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if login.is_empty() {
        bail!("gh login was empty");
    }
    Ok(login)
}

fn resolve_remote_default_branch_in(repo_root: &Path, remote: &str) -> Option<String> {
    let head_ref = format!("refs/remotes/{remote}/HEAD");
    if let Ok(symbolic) = git_capture_in(repo_root, &["symbolic-ref", &head_ref]) {
        let prefix = format!("refs/remotes/{remote}/");
        if let Some(branch) = symbolic.trim().strip_prefix(&prefix)
            && !branch.is_empty()
        {
            return Some(branch.to_string());
        }
    }

    for candidate in ["main", "master", "dev", "trunk"] {
        if git_ref_exists_in(repo_root, &format!("refs/remotes/{remote}/{candidate}")) {
            return Some(candidate.to_string());
        }
    }

    None
}

fn git_ref_exists_in(repo_root: &Path, reference: &str) -> bool {
    Command::new("git")
        .current_dir(repo_root)
        .args(["rev-parse", "--verify", reference])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn git_capture_in(repo_root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(args)
        .output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;
    if !output.status.success() {
        bail!("git {} failed", args.join(" "));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git_config_get(repo_root: &Path, key: &str) -> Option<String> {
    git_capture_in(repo_root, &["config", "--get", key])
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn git_run_in(repo_root: &Path, args: &[&str], quiet: bool) -> Result<()> {
    let mut command = Command::new("git");
    command
        .current_dir(repo_root)
        .args(args)
        .stdin(Stdio::inherit());
    if quiet {
        command.stdout(Stdio::null()).stderr(Stdio::null());
    } else {
        command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    }
    let status = command
        .status()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;
    if !status.success() {
        bail!("git {} failed", args.join(" "));
    }
    Ok(())
}

fn print_home_branch_status(status: &HomeBranchRepoStatus) {
    println!("{}", status.repo_display);
    println!("  category: {:?}", status.category);
    println!(
        "  workflow mode: {}",
        match status.workflow_mode {
            HomeBranchWorkflowMode::PrivateMirror => "private_mirror",
            HomeBranchWorkflowMode::DirectPush => "direct_push",
        }
    );
    println!("  current branch: {}", status.current_branch);
    println!("  home branch: {}", status.home_branch);
    if let Some(public_remote) = status.public_remote.as_deref() {
        println!("  public remote: {}", public_remote);
    }
    if let Some(public_default_branch) = status.public_default_branch.as_deref() {
        println!("  public default branch: {}", public_default_branch);
    }
    println!(
        "  fork remote: {}",
        if status.fork_remote_exists {
            "present"
        } else {
            "missing"
        }
    );
    println!(
        "  push default: {}",
        status.remote_push_default.as_deref().unwrap_or("<unset>")
    );
    println!(
        "  home branch push remote: {}",
        status
            .home_branch_push_remote
            .as_deref()
            .unwrap_or("<unset>")
    );
    println!(
        "  trunk push remote: {}",
        status
            .default_branch_push_remote
            .as_deref()
            .unwrap_or("<unset>")
    );
}

fn print_home_branch_bootstrap_result(result: &HomeBranchBootstrapResult) {
    println!("{}", result.repo_root);
    for step in &result.steps {
        println!("  - {}", step);
    }
    println!("  changed: {}", result.changed);
    println!(
        "  switched_to_home_branch: {}",
        result.switched_to_home_branch
    );
}

fn parse_repo_target(input: &str) -> Result<RepoTarget> {
    if is_github_input(input) {
        return parse_github_repo(input).map(RepoTarget::GitHub);
    }

    let generic = parse_generic_repo(input)?;
    Ok(RepoTarget::Generic(generic))
}

fn resolve_git_like_clone_url(input: &str) -> Result<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("missing repository URL");
    }

    if trimmed.starts_with("git@github.com:")
        || trimmed.contains("github.com/")
        || looks_like_github_shorthand(trimmed)
    {
        let repo_ref = parse_github_repo(trimmed)?;
        return Ok(format!(
            "git@github.com:{}/{}.git",
            repo_ref.owner, repo_ref.repo
        ));
    }

    Ok(trimmed.to_string())
}

fn looks_like_github_shorthand(input: &str) -> bool {
    if input.contains("://")
        || input.contains('@')
        || input.starts_with('/')
        || input.starts_with("./")
        || input.starts_with("../")
        || input.starts_with("~/")
    {
        return false;
    }

    let mut parts = input.split('/');
    let Some(owner) = parts.next() else {
        return false;
    };
    let Some(repo) = parts.next() else {
        return false;
    };
    if parts.next().is_some() {
        return false;
    }

    if owner.is_empty() || repo.is_empty() {
        return false;
    }

    owner != "." && owner != ".." && repo != "." && repo != ".."
}

fn is_github_input(input: &str) -> bool {
    let trimmed = input.trim();
    if trimmed.starts_with("git@github.com:") || trimmed.contains("github.com/") {
        return true;
    }
    !trimmed.contains("://") && !trimmed.contains('@')
}

fn parse_generic_repo(input: &str) -> Result<GenericRepoRef> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("missing repository URL");
    }

    if let Ok(url) = Url::parse(trimmed) {
        let path = url
            .path()
            .trim_matches('/')
            .split('/')
            .filter(|p| !p.is_empty())
            .map(|p| p.trim_end_matches(".git").to_string())
            .collect::<Vec<_>>();
        if path.is_empty() {
            bail!("unable to parse repository path from: {}", input);
        }
        return Ok(GenericRepoRef {
            path,
            clone_url: trimmed.to_string(),
        });
    }

    if let Some(at) = trimmed.find('@') {
        if let Some(colon) = trimmed[at + 1..].find(':') {
            let rest = &trimmed[at + 1 + colon + 1..];
            let path = rest
                .trim_matches('/')
                .split('/')
                .filter(|p| !p.is_empty())
                .map(|p| p.trim_end_matches(".git").to_string())
                .collect::<Vec<_>>();
            if path.is_empty() {
                bail!("unable to parse repository from: {}", input);
            }
            return Ok(GenericRepoRef {
                path,
                clone_url: trimmed.to_string(),
            });
        }
    }

    bail!("unable to parse repository URL: {}", input)
}

pub(crate) fn parse_github_repo(input: &str) -> Result<RepoRef> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("missing repository URL");
    }

    let path = if let Some(rest) = trimmed.strip_prefix("git@github.com:") {
        rest
    } else if let Some(idx) = trimmed.find("github.com/") {
        &trimmed[idx + "github.com/".len()..]
    } else {
        trimmed
    };

    let path = path
        .trim_start_matches('/')
        .split(&['?', '#'][..])
        .next()
        .unwrap_or(path)
        .trim_end_matches('/');

    let mut parts = path.split('/');
    let owner = parts.next().unwrap_or("").trim();
    let repo = parts.next().unwrap_or("").trim();

    if owner.is_empty() || repo.is_empty() {
        bail!("unable to parse GitHub repo from: {}", input);
    }

    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    if repo.is_empty() {
        bail!("unable to parse GitHub repo from: {}", input);
    }

    Ok(RepoRef {
        owner: owner.to_string(),
        repo: repo.to_string(),
    })
}

pub(crate) fn normalize_root(raw: &str) -> Result<PathBuf> {
    let expanded = config::expand_path(raw);
    let cwd = std::env::current_dir().context("failed to resolve current directory")?;
    let root = if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    };

    let default_repos_root = config::expand_path(DEFAULT_REPOS_ROOT);
    let default_code_root = config::expand_path(DEFAULT_CODE_ROOT);
    if root != default_repos_root && root != default_code_root && !repos_root_override_enabled() {
        bail!(
            "repos root is immutable; use {} or {} or set {}=1 to override",
            default_repos_root.display(),
            default_code_root.display(),
            REPOS_ROOT_OVERRIDE_ENV
        );
    }

    Ok(root)
}

fn repos_root_override_enabled() -> bool {
    match std::env::var(REPOS_ROOT_OVERRIDE_ENV) {
        Ok(value) => {
            let trimmed = value.trim();
            !trimmed.is_empty() && trimmed != "0"
        }
        Err(_) => false,
    }
}

fn run_git_clone(url: &str, target_dir: &Path, shallow: bool) -> Result<()> {
    let mut cmd = Command::new("git");
    cmd.arg("clone");
    if shallow {
        cmd.args(["--depth", "1"]);
    }
    let status = cmd
        .arg(url)
        .arg(target_dir)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to run git clone")?;

    if !status.success() {
        bail!("git clone failed");
    }

    Ok(())
}

fn resolve_upstream_url(repo_ref: &RepoRef, prefer_ssh: bool) -> Result<Option<String>> {
    let output = match Command::new("gh")
        .args([
            "api",
            &format!("repos/{}/{}", repo_ref.owner, repo_ref.repo),
        ])
        .output()
    {
        Ok(output) => output,
        Err(err) => {
            println!("gh not available; skipping upstream auto-setup ({})", err);
            return Ok(None);
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = stderr.trim();
        if message.is_empty() {
            println!("gh api failed; skipping upstream auto-setup");
        } else {
            println!("gh api failed; skipping upstream auto-setup: {}", message);
        }
        println!("Authenticate with: gh auth login");
        return Ok(None);
    }

    let info: RepoInfo =
        serde_json::from_slice(&output.stdout).context("failed to parse gh api response")?;

    if !info.fork {
        return Ok(None);
    }

    let parent = info.parent.or(info.source).map(|parent| {
        if prefer_ssh {
            parent.ssh_url
        } else {
            parent.clone_url.unwrap_or_else(|| parent.ssh_url)
        }
    });

    Ok(parent)
}

fn configure_upstream(repo_dir: &Path, upstream_url: &str, depth: Option<u32>) -> Result<()> {
    println!("Setting up upstream: {}", upstream_url);

    let cwd = std::env::current_dir().context("failed to capture current directory")?;
    std::env::set_current_dir(repo_dir)
        .with_context(|| format!("failed to enter {}", repo_dir.display()))?;

    let result = upstream::setup_upstream_with_depth(Some(upstream_url), None, depth);

    if let Err(err) = std::env::set_current_dir(&cwd) {
        println!("warning: failed to restore working directory: {}", err);
    }

    result
}

fn spawn_background_history_fetch(repo_dir: &Path, has_upstream: bool) -> Result<()> {
    let mut remotes = history_fetch_remotes(repo_dir);
    if remotes.is_empty() {
        remotes.push("origin".to_string());
    }
    if !has_upstream {
        remotes.truncate(1);
    }

    let mut commands = Vec::new();
    if let Some(primary_remote) = remotes.first() {
        commands.push(format!("git fetch --unshallow --tags {primary_remote}"));
        for remote in remotes.iter().skip(1) {
            commands.push(format!("git fetch --tags {remote}"));
        }
    }
    let command = commands.join(" && ");

    let _child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(repo_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to spawn background history fetch")?;

    println!("Fetching full history in background...");

    Ok(())
}

fn fetch_complete_history_for_clone(repo_dir: &Path, has_upstream: bool) -> Result<()> {
    println!("Completing git history before private mirror bootstrap...");
    fetch_complete_history(repo_dir, has_upstream, false)
}

fn fetch_complete_history(repo_dir: &Path, has_upstream: bool, quiet: bool) -> Result<()> {
    let mut remotes = history_fetch_remotes(repo_dir);
    if remotes.is_empty() {
        remotes.push("origin".to_string());
    }
    if !has_upstream {
        remotes.truncate(1);
    }

    if git_repo_is_shallow(repo_dir)? {
        if let Some(primary_remote) = remotes.first() {
            git_run_in(
                repo_dir,
                &["fetch", "--unshallow", "--tags", primary_remote],
                quiet,
            )?;
        }
    }
    for remote in remotes.iter().skip(1) {
        git_run_in(repo_dir, &["fetch", "--tags", remote], quiet)?;
    }
    Ok(())
}

fn git_repo_is_shallow(repo_dir: &Path) -> Result<bool> {
    Ok(git_capture_in(repo_dir, &["rev-parse", "--is-shallow-repository"])?.trim() == "true")
}

fn repo_has_distinct_upstream_remote(repo_dir: &Path) -> bool {
    let Some(upstream_url) = git_config_get(repo_dir, "remote.upstream.url") else {
        return false;
    };
    let origin_url = git_config_get(repo_dir, "remote.origin.url");
    origin_url
        .as_deref()
        .map(|origin| normalize_git_url(origin) != normalize_git_url(&upstream_url))
        .unwrap_or(true)
}

fn history_fetch_remotes(repo_dir: &Path) -> Vec<String> {
    let origin_url = git_config_get(repo_dir, "remote.origin.url");
    let upstream_url = git_config_get(repo_dir, "remote.upstream.url");
    let configured_public_remote = config::configured_public_remote_for_repo(repo_dir);
    let public_remote = resolve_public_remote_name(
        configured_public_remote.as_deref(),
        origin_url.as_deref(),
        upstream_url.as_deref(),
    )
    .unwrap_or_else(|| "origin".to_string());

    let mut remotes = vec![public_remote.clone()];
    if repo_has_distinct_upstream_remote(repo_dir) {
        let secondary = if public_remote == "origin" {
            "upstream"
        } else {
            "origin"
        };
        if git_config_get(repo_dir, &format!("remote.{secondary}.url")).is_some() {
            remotes.push(secondary.to_string());
        }
    }
    remotes
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn git_ok(repo_root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(repo_root)
            .status()
            .expect("run git");
        assert!(status.success(), "git {} failed", args.join(" "));
    }

    #[test]
    fn preflight_clone_target_detects_git_checkout() {
        let dir = tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join(".git")).expect("git dir");

        let already_cloned = preflight_clone_target(dir.path()).expect("preflight");

        assert!(already_cloned);
    }

    #[test]
    fn preflight_clone_target_allows_empty_dir() {
        let dir = tempdir().expect("tempdir");

        let already_cloned = preflight_clone_target(dir.path()).expect("preflight");

        assert!(!already_cloned);
    }

    #[test]
    fn preflight_clone_target_rejects_non_repo_dir() {
        let dir = tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("user_files")).expect("user_files dir");

        let err = preflight_clone_target(dir.path()).expect_err("expected non-repo error");

        assert!(
            err.to_string()
                .contains("target path exists but is not a git checkout")
        );
    }

    #[test]
    fn discover_repos_supports_flat_roots() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path();
        fs::create_dir_all(root.join("flow").join(".git")).expect("git dir");
        fs::create_dir_all(root.join("seq").join(".git")).expect("git dir");

        let repos = discover_repos(root).expect("discover repos");
        let displays = repos
            .into_iter()
            .map(|repo| repo.display)
            .collect::<Vec<_>>();

        assert_eq!(displays, vec!["flow".to_string(), "seq".to_string()]);
    }

    #[test]
    fn discover_repos_supports_owner_repo_roots() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path();
        fs::create_dir_all(root.join("foo").join("bar").join(".git")).expect("git dir");
        fs::create_dir_all(root.join("zed-industries").join("zed").join(".git")).expect("git dir");

        let repos = discover_repos(root).expect("discover repos");
        let displays = repos
            .into_iter()
            .map(|repo| repo.display)
            .collect::<Vec<_>>();

        assert_eq!(
            displays,
            vec!["foo/bar".to_string(), "zed-industries/zed".to_string()]
        );
    }

    #[test]
    fn parse_github_remote_url_supports_https_with_username() {
        let parsed =
            parse_github_remote_url("https://nikivdev@github.com/fl2024008/prometheus.git")
                .expect("parse github remote");

        assert_eq!(parsed.owner, "fl2024008");
        assert_eq!(parsed.repo, "prometheus");
    }

    #[test]
    fn history_fetch_remotes_supports_upstream_only_repos() {
        let dir = tempdir().expect("tempdir");
        let repo_root = dir.path();
        git_ok(repo_root, &["init"]);
        git_ok(
            repo_root,
            &[
                "remote",
                "add",
                "upstream",
                "https://github.com/astral-sh/ruff.git",
            ],
        );

        assert_eq!(
            history_fetch_remotes(repo_root),
            vec!["upstream".to_string()]
        );
    }

    #[test]
    fn private_mirror_clone_bootstrap_requires_complete_history() {
        let opts = ReposCloneOpts {
            url: "astral-sh/ruff".to_string(),
            root: "~/repos".to_string(),
            full: false,
            no_upstream: false,
            upstream_url: None,
            no_home_branch_bootstrap: false,
        };

        assert!(should_complete_history_before_clone_bootstrap(
            &opts,
            true,
            HomeBranchWorkflowMode::PrivateMirror
        ));
        assert!(!should_complete_history_before_clone_bootstrap(
            &opts,
            true,
            HomeBranchWorkflowMode::DirectPush
        ));
        assert!(!should_complete_history_before_clone_bootstrap(
            &opts,
            false,
            HomeBranchWorkflowMode::PrivateMirror
        ));
    }

    #[test]
    fn skipping_home_branch_bootstrap_keeps_fast_background_fetch() {
        let opts = ReposCloneOpts {
            url: "astral-sh/ruff".to_string(),
            root: "~/repos".to_string(),
            full: false,
            no_upstream: false,
            upstream_url: None,
            no_home_branch_bootstrap: true,
        };

        assert!(!should_complete_history_before_clone_bootstrap(
            &opts,
            true,
            HomeBranchWorkflowMode::PrivateMirror
        ));
    }

    #[test]
    fn repo_has_distinct_upstream_remote_detects_same_and_different_urls() {
        let dir = tempdir().expect("tempdir");
        git_run_in(dir.path(), &["init", "-q"], true).expect("git init");

        git_run_in(
            dir.path(),
            &[
                "remote",
                "add",
                "origin",
                "git@github.com:astral-sh/ruff.git",
            ],
            true,
        )
        .expect("add origin");
        git_run_in(
            dir.path(),
            &[
                "remote",
                "add",
                "upstream",
                "git@github.com:astral-sh/ruff.git",
            ],
            true,
        )
        .expect("add upstream");
        assert!(!repo_has_distinct_upstream_remote(dir.path()));

        git_run_in(
            dir.path(),
            &[
                "remote",
                "set-url",
                "upstream",
                "git@github.com:python/cpython.git",
            ],
            true,
        )
        .expect("set upstream url");
        assert!(repo_has_distinct_upstream_remote(dir.path()));
    }
}
