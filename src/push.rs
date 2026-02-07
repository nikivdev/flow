use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::cli::PushCommand;
use crate::{env, ssh, ssh_keys};

pub fn run(cmd: PushCommand) -> Result<()> {
    let repo_root = git_root()?;
    let current_branch = current_branch(&repo_root)?;

    let upstream_url = git_capture_in(&repo_root, &["remote", "get-url", "upstream"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let origin_url = git_capture_in(&repo_root, &["remote", "get-url", "origin"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let owner = resolve_push_owner(cmd.owner.as_deref())?;
    let repo_name = if let Some(repo) = cmd.repo.as_deref() {
        repo.trim().to_string()
    } else {
        derive_repo_name(&repo_root, upstream_url.as_deref(), origin_url.as_deref())?
    };
    if repo_name.is_empty() {
        bail!("could not determine repo name (use --repo)");
    }

    let target_url = choose_github_remote_url(&owner, &repo_name, &cmd)?;

    if cmd.dry_run {
        println!("Repo: {}", repo_root.display());
        println!("Branch: {}", current_branch);
        println!("Remote: {}", cmd.remote);
        println!("Target: {}", target_url);
        return Ok(());
    }

    ensure_remote_points_to_target(
        &repo_root,
        &cmd.remote,
        &target_url,
        upstream_url.as_deref(),
        cmd.force,
    )?;

    if cmd.create_repo {
        ensure_github_repo_exists(&owner, &repo_name)?;
    }

    println!("==> Pushing {} to {}...", current_branch, cmd.remote);
    git_run_in(&repo_root, &["push", "-u", &cmd.remote, &current_branch])?;
    println!("✓ Pushed to {}/{}", owner, repo_name);
    Ok(())
}

fn resolve_push_owner(cli: Option<&str>) -> Result<String> {
    if let Some(value) = cli {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }

    if let Ok(value) = std::env::var("FLOW_PUSH_OWNER") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }

    if let Ok(Some(value)) = env::get_personal_env_var("FLOW_PUSH_OWNER") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }

    bail!(
        "FLOW_PUSH_OWNER not set. Configure it via:\n  f env set FLOW_PUSH_OWNER=<owner> --personal\nor pass --owner <owner>"
    );
}

fn derive_repo_name(
    repo_root: &Path,
    upstream_url: Option<&str>,
    origin_url: Option<&str>,
) -> Result<String> {
    if let Some(url) = upstream_url {
        if let Some((_owner, repo)) = parse_github_owner_repo(url) {
            return Ok(repo);
        }
    }
    if let Some(url) = origin_url {
        if let Some((_owner, repo)) = parse_github_owner_repo(url) {
            return Ok(repo);
        }
    }
    Ok(repo_root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("repo")
        .to_string())
}

fn build_github_ssh_url(owner: &str, repo: &str) -> String {
    let owner = owner.trim();
    let repo = repo.trim();
    format!("git@github.com:{}/{}.git", owner, repo)
}

fn build_github_https_url(owner: &str, repo: &str) -> String {
    let owner = owner.trim();
    let repo = repo.trim();
    format!("https://github.com/{}/{}.git", owner, repo)
}

fn choose_github_remote_url(owner: &str, repo: &str, cmd: &PushCommand) -> Result<String> {
    let ssh_url = build_github_ssh_url(owner, repo);
    let https_url = build_github_https_url(owner, repo);

    match ssh::ssh_mode() {
        ssh::SshMode::Https => Ok(https_url),
        ssh::SshMode::Force => {
            if !cmd.no_ssh {
                if let Err(err) = ssh_keys::ensure_default_identity(cmd.ttl_hours) {
                    eprintln!(
                        "Warning: could not unlock Flow SSH key (continuing): {}",
                        err
                    );
                }
            }
            Ok(ssh_url)
        }
        ssh::SshMode::Auto => {
            if !cmd.no_ssh {
                if let Err(err) = ssh_keys::ensure_default_identity(cmd.ttl_hours) {
                    eprintln!(
                        "Warning: could not unlock Flow SSH key (continuing): {}",
                        err
                    );
                }
            }
            if ssh::has_identities() {
                Ok(ssh_url)
            } else {
                Ok(https_url)
            }
        }
    }
}

fn parse_github_owner_repo(url: &str) -> Option<(String, String)> {
    let trimmed = url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }

    if let Some(rest) = trimmed.strip_prefix("git@github.com:") {
        let rest = rest.trim_end_matches(".git");
        let (owner, repo) = rest.split_once('/')?;
        return Some((owner.to_string(), repo.to_string()));
    }

    if let Some(rest) = trimmed.strip_prefix("https://github.com/") {
        let rest = rest.trim_end_matches(".git");
        let (owner, repo) = rest.split_once('/')?;
        return Some((owner.to_string(), repo.to_string()));
    }

    None
}

fn ensure_remote_points_to_target(
    repo_root: &Path,
    remote: &str,
    target_url: &str,
    upstream_url: Option<&str>,
    force: bool,
) -> Result<()> {
    let existing = git_capture_in(repo_root, &["remote", "get-url", remote])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    if let Some(existing) = existing {
        if normalize_git_url(&existing) == normalize_git_url(target_url) {
            return Ok(());
        }

        // Safe override when the remote points at upstream (read-only clone).
        let is_upstream = upstream_url
            .map(|u| normalize_git_url(u) == normalize_git_url(&existing))
            .unwrap_or(false);
        if is_upstream || force {
            println!("==> Updating remote {} url...", remote);
            git_run_in(repo_root, &["remote", "set-url", remote, target_url])?;
            return Ok(());
        }

        bail!(
            "remote '{}' already points to {}\nrefusing to overwrite without --force\n(target would be {})",
            remote,
            existing,
            target_url
        );
    }

    println!("==> Adding remote {}...", remote);
    git_run_in(repo_root, &["remote", "add", remote, target_url])?;
    Ok(())
}

fn ensure_github_repo_exists(owner: &str, repo: &str) -> Result<()> {
    let full_name = format!("{}/{}", owner.trim(), repo.trim());

    let view = Command::new("gh")
        .args(["repo", "view", &full_name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    if matches!(view, Ok(s) if s.success()) {
        return Ok(());
    }

    println!("==> Creating GitHub repo {} (private)...", full_name);
    let status = Command::new("gh")
        .args(["repo", "create", &full_name, "--private", "--confirm"])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status();

    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(_) => bail!("failed to create repo via gh (is it installed/authenticated?)"),
        Err(err) => Err(err).context("failed to run gh"),
    }
}

fn normalize_git_url(url: &str) -> String {
    let url = url.trim();
    let url = if url.starts_with("git@github.com:") {
        url.replace("git@github.com:", "github.com/")
    } else if url.starts_with("https://github.com/") {
        url.replace("https://github.com/", "github.com/")
    } else {
        url.to_string()
    };
    url.trim_end_matches(".git").to_lowercase()
}

fn git_root() -> Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("failed to locate git root")?;
    if !output.status.success() {
        bail!("not inside a git repository");
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(PathBuf::from(path))
}

fn current_branch(repo_root: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(repo_root)
        .output()
        .context("failed to read current branch")?;
    if !output.status.success() {
        bail!("git rev-parse --abbrev-ref HEAD failed");
    }
    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if name.is_empty() || name == "HEAD" {
        bail!("detached HEAD (checkout a branch first)");
    }
    Ok(name)
}

fn git_capture_in(repo_root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;
    if !output.status.success() {
        bail!("git {} failed", args.join(" "));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn git_run_in(repo_root: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;
    if !status.success() {
        bail!("git {} failed", args.join(" "));
    }
    Ok(())
}
