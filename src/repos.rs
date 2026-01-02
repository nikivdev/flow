//! Repository management commands.
//!
//! Supports cloning repos into a structured local directory.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::cli::{ReposAction, ReposCloneOpts, ReposCommand};
use crate::{config, upstream};

/// Run the repos subcommand.
pub fn run(cmd: ReposCommand) -> Result<()> {
    match cmd.action {
        Some(ReposAction::Clone(opts)) => clone_repo(opts),
        None => {
            println!("Usage: f repos clone <url>");
            Ok(())
        }
    }
}

#[derive(Debug)]
struct RepoRef {
    owner: String,
    repo: String,
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
}

fn clone_repo(opts: ReposCloneOpts) -> Result<()> {
    let repo_ref = parse_github_repo(&opts.url)?;
    let root = normalize_root(&opts.root)?;
    let owner_dir = root.join(&repo_ref.owner);
    let target_dir = owner_dir.join(&repo_ref.repo);

    if target_dir.exists() {
        bail!("target already exists: {}", target_dir.display());
    }

    fs::create_dir_all(&owner_dir)
        .with_context(|| format!("failed to create {}", owner_dir.display()))?;

    let clone_url = format!("git@github.com:{}/{}.git", repo_ref.owner, repo_ref.repo);
    let shallow = !opts.full;
    let fetch_depth = if shallow { Some(1) } else { None };
    run_git_clone(&clone_url, &target_dir, shallow)?;

    println!("✓ cloned to {}", target_dir.display());

    if opts.no_upstream {
        if shallow {
            spawn_background_history_fetch(&target_dir, false)?;
        }
        return Ok(());
    }

    let upstream_url = if let Some(url) = opts.upstream_url {
        Some(url)
    } else {
        resolve_upstream_url(&repo_ref)?
    };

    let Some(upstream_url) = upstream_url else {
        println!("Upstream not configured. If this is a fork, run:");
        println!("  f upstream setup --url <original-repo-url>");
        if shallow {
            spawn_background_history_fetch(&target_dir, false)?;
        }
        return Ok(());
    };

    if upstream_url.trim() == clone_url {
        println!("Upstream matches origin; skipping upstream setup.");
        if shallow {
            spawn_background_history_fetch(&target_dir, false)?;
        }
        return Ok(());
    }

    configure_upstream(&target_dir, &upstream_url, fetch_depth)?;
    if shallow {
        spawn_background_history_fetch(&target_dir, true)?;
    }

    Ok(())
}

fn parse_github_repo(input: &str) -> Result<RepoRef> {
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

fn normalize_root(raw: &str) -> Result<PathBuf> {
    let expanded = config::expand_path(raw);
    if expanded.is_absolute() {
        return Ok(expanded);
    }

    let cwd = std::env::current_dir().context("failed to resolve current directory")?;
    Ok(cwd.join(expanded))
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

fn resolve_upstream_url(repo_ref: &RepoRef) -> Result<Option<String>> {
    let output = match Command::new("gh")
        .args(["api", &format!("repos/{}/{}", repo_ref.owner, repo_ref.repo)])
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

    let parent = info
        .parent
        .or(info.source)
        .map(|parent| parent.ssh_url);

    Ok(parent)
}

fn configure_upstream(repo_dir: &Path, upstream_url: &str, depth: Option<u32>) -> Result<()> {
    println!("Setting up upstream: {}", upstream_url);

    let cwd = std::env::current_dir().context("failed to capture current directory")?;
    std::env::set_current_dir(repo_dir)
        .with_context(|| format!("failed to enter {}", repo_dir.display()))?;

    let result = upstream::setup_upstream_with_depth(
        Some(upstream_url),
        None,
        depth,
    );

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
