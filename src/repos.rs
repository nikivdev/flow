//! Repository management commands.
//!
//! Supports cloning repos into a structured local directory.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::cli::{ReposAction, ReposCloneOpts, ReposCommand};
use crate::{config, publish, ssh, ssh_keys, upstream};

const DEFAULT_REPOS_ROOT: &str = "~/repos";
const REPOS_ROOT_OVERRIDE_ENV: &str = "FLOW_REPOS_ALLOW_ROOT_OVERRIDE";

/// Run the repos subcommand.
pub fn run(cmd: ReposCommand) -> Result<()> {
    match cmd.action {
        Some(ReposAction::Clone(opts)) => {
            let path = clone_repo(opts)?;
            open_in_zed(&path)?;
            Ok(())
        }
        Some(ReposAction::Create(opts)) => publish::run(opts),
        None => fuzzy_select_repo(),
    }
}

fn open_in_zed(path: &std::path::Path) -> Result<()> {
    std::process::Command::new("open")
        .args(["-a", "/Applications/Zed.app"])
        .arg(path)
        .status()
        .context("failed to open Zed")?;
    Ok(())
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

/// Discover all repos in the root directory (owner/repo structure).
fn discover_repos(root: &Path) -> Result<Vec<RepoEntry>> {
    let mut repos = Vec::new();

    let owners = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return Ok(repos),
    };

    for owner_entry in owners.flatten() {
        let owner_path = owner_entry.path();
        if !owner_path.is_dir() {
            continue;
        }

        let owner_name = match owner_path.file_name() {
            Some(name) => name.to_string_lossy().to_string(),
            None => continue,
        };

        // Skip hidden directories
        if owner_name.starts_with('.') {
            continue;
        }

        let repo_entries = match fs::read_dir(&owner_path) {
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
                repos.push(RepoEntry {
                    display: format!("{}/{}", owner_name, repo_name),
                    path: repo_path,
                });
            }
        }
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
    let prefer_ssh = ssh::prefer_ssh();
    let repo_ref = parse_github_repo(&opts.url)?;
    let root = normalize_root(&opts.root)?;
    let owner_dir = root.join(&repo_ref.owner);
    let target_dir = owner_dir.join(&repo_ref.repo);

    if target_dir.exists() {
        println!("Already cloned: {}", target_dir.display());
        return Ok(target_dir);
    }

    fs::create_dir_all(&owner_dir)
        .with_context(|| format!("failed to create {}", owner_dir.display()))?;

    let clone_url = if prefer_ssh {
        format!("git@github.com:{}/{}.git", repo_ref.owner, repo_ref.repo)
    } else {
        format!(
            "https://github.com/{}/{}.git",
            repo_ref.owner, repo_ref.repo
        )
    };
    let shallow = !opts.full;
    let fetch_depth = if shallow { Some(1) } else { None };
    run_git_clone(&clone_url, &target_dir, shallow)?;

    println!("✓ cloned to {}", target_dir.display());

    if opts.no_upstream {
        if shallow {
            spawn_background_history_fetch(&target_dir, false)?;
        }
        return Ok(target_dir);
    }

    let upstream_url = if let Some(url) = opts.upstream_url {
        Some(url)
    } else {
        resolve_upstream_url(&repo_ref, prefer_ssh)?
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
        spawn_background_history_fetch(&target_dir, !upstream_is_origin)?;
    }

    Ok(target_dir)
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

    let default_root = config::expand_path(DEFAULT_REPOS_ROOT);
    if root != default_root && !repos_root_override_enabled() {
        bail!(
            "repos root is immutable; use {} or set {}=1 to override",
            default_root.display(),
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
    let mut command = String::from("git fetch --unshallow --tags origin");
    if has_upstream {
        command.push_str(" && git fetch --tags upstream");
    }

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
