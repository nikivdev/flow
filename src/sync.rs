//! Git sync command - comprehensive repo synchronization.
//!
//! Provides a single command to sync a git repository:
//! - Pull from origin (with rebase)
//! - Sync upstream if configured (fetch, merge)
//! - Push to origin

use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::cli::SyncCommand;

/// Run the sync command.
pub fn run(cmd: SyncCommand) -> Result<()> {
    // Check we're in a git repo
    if git_capture(&["rev-parse", "--git-dir"]).is_err() {
        bail!("Not a git repository");
    }

    let current = git_capture(&["rev-parse", "--abbrev-ref", "HEAD"])?;
    let current = current.trim();

    // Check for uncommitted changes
    let status = git_capture(&["status", "--porcelain"])?;
    let has_changes = !status.trim().is_empty();

    if has_changes && !cmd.stash {
        println!("You have uncommitted changes. Use --stash to auto-stash them.");
        bail!("Uncommitted changes");
    }

    // Stash if needed
    let mut stashed = false;
    if has_changes && cmd.stash {
        println!("==> Stashing local changes...");
        let stash_count_before = git_capture(&["stash", "list"])
            .map(|s| s.lines().count())
            .unwrap_or(0);

        let _ = git_run(&["stash", "push", "-m", "f sync auto-stash"]);

        let stash_count_after = git_capture(&["stash", "list"])
            .map(|s| s.lines().count())
            .unwrap_or(0);
        stashed = stash_count_after > stash_count_before;
    }

    // Check if we have an origin remote
    let has_origin = git_capture(&["remote", "get-url", "origin"]).is_ok();
    let has_upstream = git_capture(&["remote", "get-url", "upstream"]).is_ok();

    // Check if origin remote is reachable (repo exists on remote)
    let origin_reachable = has_origin && git_capture(&["ls-remote", "--exit-code", "-q", "origin"]).is_ok();

    // Step 1: Pull from origin (if tracking branch exists and repo is reachable)
    if has_origin && origin_reachable {
        let tracking = git_capture(&["rev-parse", "--abbrev-ref", "@{upstream}"]);
        if tracking.is_ok() {
            println!("==> Pulling from origin...");
            if cmd.rebase {
                if let Err(e) = git_run(&["pull", "--rebase", "origin", current]) {
                    restore_stash(stashed);
                    return Err(e);
                }
            } else {
                if let Err(e) = git_run(&["pull", "origin", current]) {
                    restore_stash(stashed);
                    return Err(e);
                }
            }
        } else {
            println!("==> No tracking branch, skipping pull");
        }
    } else if has_origin && !origin_reachable {
        println!("==> Origin repo not found, skipping pull");
    }

    // Step 2: Sync upstream if it exists
    if has_upstream {
        println!("==> Syncing upstream...");
        if let Err(e) = sync_upstream_internal(current) {
            restore_stash(stashed);
            return Err(e);
        }
    }

    // Step 3: Push to origin
    if has_origin && !cmd.no_push {
        if !origin_reachable {
            // Origin repo doesn't exist
            if cmd.create_repo {
                println!("==> Creating origin repo...");
                if try_create_origin_repo()? {
                    println!("==> Pushing to origin...");
                    git_run(&["push", "-u", "origin", current])?;
                } else {
                    println!("  Could not create repo, skipping push");
                }
            } else {
                println!("==> Origin repo not found, skipping push");
                println!("  Use --create-repo to create it");
            }
        } else {
            println!("==> Pushing to origin...");
            if let Err(e) = git_run(&["push", "origin", current]) {
                restore_stash(stashed);
                return Err(e);
            }
        }
    }

    // Restore stash
    restore_stash(stashed);

    println!("\n✓ Sync complete!");

    Ok(())
}

/// Sync from upstream remote into current branch.
fn sync_upstream_internal(current_branch: &str) -> Result<()> {
    // Fetch upstream
    git_run(&["fetch", "upstream", "--prune"])?;

    // Determine upstream branch
    let upstream_branch = if let Ok(merge_ref) = git_capture(&["config", "--get", "branch.upstream.merge"]) {
        merge_ref.trim().replace("refs/heads/", "")
    } else if let Ok(head_ref) = git_capture(&["symbolic-ref", "refs/remotes/upstream/HEAD"]) {
        head_ref.trim().replace("refs/remotes/upstream/", "")
    } else if git_capture(&["rev-parse", "--verify", "refs/remotes/upstream/main"]).is_ok() {
        "main".to_string()
    } else if git_capture(&["rev-parse", "--verify", "refs/remotes/upstream/master"]).is_ok() {
        "master".to_string()
    } else {
        println!("  Cannot determine upstream branch, skipping upstream sync");
        return Ok(());
    };

    let upstream_ref = format!("upstream/{}", upstream_branch);

    // Update local upstream branch if it exists
    let local_upstream_exists = git_capture(&["rev-parse", "--verify", "refs/heads/upstream"]).is_ok();
    if local_upstream_exists {
        git_run(&["branch", "-f", "upstream", &upstream_ref])?;
    }

    // Check if current branch is behind upstream
    let behind = git_capture(&["rev-list", "--count", &format!("{}..{}", current_branch, upstream_ref)])
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0);

    if behind > 0 {
        println!("  Merging {} commits from upstream...", behind);

        // Try fast-forward first
        if git_run(&["merge", "--ff-only", &upstream_ref]).is_err() {
            // Fall back to regular merge
            if let Err(_) = git_run(&["merge", &upstream_ref, "--no-edit"]) {
                bail!("Merge conflicts with upstream. Resolve manually:\n  git status\n  # fix conflicts\n  git add . && git commit");
            }
        }
    } else {
        println!("  Already up to date with upstream");
    }

    Ok(())
}

fn restore_stash(stashed: bool) {
    if stashed {
        println!("==> Restoring stashed changes...");
        let _ = git_run(&["stash", "pop"]);
    }
}

/// Run a git command and capture stdout.
fn git_capture(args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .output()
        .context("failed to run git")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Run a git command with inherited stdio.
fn git_run(args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to run git")?;

    if !status.success() {
        bail!("git {} failed", args.join(" "));
    }

    Ok(())
}

/// Try to create the origin repo on GitHub if it doesn't exist.
fn try_create_origin_repo() -> Result<bool> {
    let origin_url = match git_capture(&["remote", "get-url", "origin"]) {
        Ok(url) => url.trim().to_string(),
        Err(_) => return Ok(false),
    };

    let repo_path = if origin_url.starts_with("git@github.com:") {
        origin_url
            .strip_prefix("git@github.com:")
            .and_then(|s| s.strip_suffix(".git").or(Some(s)))
    } else if origin_url.contains("github.com/") {
        origin_url
            .split("github.com/")
            .nth(1)
            .and_then(|s| s.strip_suffix(".git").or(Some(s)))
    } else {
        None
    };

    let Some(repo_path) = repo_path else {
        println!("Cannot parse origin URL for auto-creation: {}", origin_url);
        return Ok(false);
    };

    println!("\nOrigin repo doesn't exist. Creating: {}", repo_path);

    let status = Command::new("gh")
        .args(["repo", "create", repo_path, "--private", "--source=."])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status();

    match status {
        Ok(s) if s.success() => {
            println!("✓ Created GitHub repo: {}", repo_path);
            Ok(true)
        }
        Ok(_) => {
            println!("Failed to create repo. Is `gh` installed and authenticated?");
            Ok(false)
        }
        Err(e) => {
            println!("Failed to run gh CLI: {}", e);
            Ok(false)
        }
    }
}
