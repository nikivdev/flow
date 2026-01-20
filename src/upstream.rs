//! Upstream fork management.
//!
//! Provides automated workflows for managing forks with upstream repositories.
//! - `f upstream setup` - Configure upstream remote and local tracking branch
//! - `f upstream pull` - Pull changes from upstream into local branch
//! - `f upstream sync` - Full sync: pull upstream, merge to dev, merge to main, push

use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::cli::{UpstreamAction, UpstreamCommand};

/// Run the upstream subcommand.
pub fn run(cmd: UpstreamCommand) -> Result<()> {
    let action = cmd.action.unwrap_or(UpstreamAction::Status);

    match action {
        UpstreamAction::Status => show_status(),
        UpstreamAction::Setup {
            upstream_url,
            upstream_branch,
        } => setup_upstream(upstream_url.as_deref(), upstream_branch.as_deref()),
        UpstreamAction::Pull { branch } => pull_upstream(branch.as_deref()),
        UpstreamAction::Check => check_upstream(),
        UpstreamAction::Sync {
            no_push,
            create_repo,
        } => sync_upstream(!no_push, create_repo),
        UpstreamAction::Open => open_upstream(),
    }
}

/// Set up upstream remote and local tracking branch, with optional fetch depth.
pub fn setup_upstream_with_depth(
    upstream_url: Option<&str>,
    upstream_branch: Option<&str>,
    depth: Option<u32>,
) -> Result<()> {
    setup_upstream_internal(upstream_url, upstream_branch, depth)
}

/// Show current upstream configuration status.
fn show_status() -> Result<()> {
    println!("Upstream Fork Status\n");

    // Check for upstream remote
    let upstream_url = git_capture(&["remote", "get-url", "upstream"]).ok();
    let origin_url = git_capture(&["remote", "get-url", "origin"]).ok();

    if let Some(url) = &upstream_url {
        println!("✓ upstream remote: {}", url.trim());
    } else {
        println!("✗ upstream remote: not configured");
    }

    if let Some(url) = &origin_url {
        println!("✓ origin remote: {}", url.trim());
    }

    // Check for local upstream branch
    let has_upstream_branch =
        git_capture(&["rev-parse", "--verify", "refs/heads/upstream"]).is_ok();
    if has_upstream_branch {
        let tracking = git_capture(&["config", "--get", "branch.upstream.remote"])
            .ok()
            .map(|s| s.trim().to_string());
        println!("✓ local 'upstream' branch: exists (tracks {:?})", tracking);
    } else {
        println!("✗ local 'upstream' branch: not created");
    }

    // Current branch
    let current = git_capture(&["rev-parse", "--abbrev-ref", "HEAD"])
        .ok()
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    println!("\nCurrent branch: {}", current);

    // Show divergence if upstream exists
    if upstream_url.is_some() {
        println!("\nTo set up: f upstream setup");
        println!("To pull:   f upstream pull");
        println!("To sync:   f upstream sync");
    } else {
        println!("\nTo set up upstream:");
        println!("  f upstream setup --url <upstream-repo-url>");
        println!("  f upstream setup --url https://github.com/original/repo");
    }

    Ok(())
}

/// Open upstream repository URL in browser.
fn open_upstream() -> Result<()> {
    let upstream_url = git_capture(&["remote", "get-url", "upstream"])?;
    let upstream_url = upstream_url.trim();

    // Convert git URL to https URL if needed
    let https_url = if upstream_url.starts_with("git@github.com:") {
        upstream_url
            .replace("git@github.com:", "https://github.com/")
            .trim_end_matches(".git")
            .to_string()
    } else if upstream_url.starts_with("https://") {
        upstream_url.trim_end_matches(".git").to_string()
    } else {
        upstream_url.to_string()
    };

    println!("Opening {}", https_url);

    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg(&https_url)
            .status()
            .context("failed to open URL")?;
    }

    #[cfg(target_os = "linux")]
    {
        Command::new("xdg-open")
            .arg(&https_url)
            .status()
            .context("failed to open URL")?;
    }

    #[cfg(target_os = "windows")]
    {
        Command::new("cmd")
            .args(["/C", "start", &https_url])
            .status()
            .context("failed to open URL")?;
    }

    Ok(())
}

/// Set up upstream remote and local tracking branch.
fn setup_upstream(upstream_url: Option<&str>, upstream_branch: Option<&str>) -> Result<()> {
    setup_upstream_internal(upstream_url, upstream_branch, None)
}

fn setup_upstream_internal(
    upstream_url: Option<&str>,
    upstream_branch: Option<&str>,
    depth: Option<u32>,
) -> Result<()> {
    // Check if upstream remote exists
    let has_upstream = git_capture(&["remote", "get-url", "upstream"]).is_ok();

    if !has_upstream {
        if let Some(url) = upstream_url {
            println!("Adding upstream remote: {}", url);
            git_run(&["remote", "add", "upstream", url])?;
        } else {
            // Try to detect from origin
            if let Ok(origin_url) = git_capture(&["remote", "get-url", "origin"]) {
                println!("No upstream remote configured.");
                println!("Current origin: {}", origin_url.trim());
                println!("\nTo add upstream, run:");
                println!("  f upstream setup --url <original-repo-url>");
                return Ok(());
            }
            bail!("No upstream remote. Use: f upstream setup --url <upstream-repo-url>");
        }
    } else {
        let url = git_capture(&["remote", "get-url", "upstream"])?;
        println!("✓ upstream remote exists: {}", url.trim());
    }

    // Fetch upstream
    println!("\nFetching upstream...");
    if let Some(depth) = depth {
        let depth_str = depth.to_string();
        git_run(&["fetch", "upstream", "--prune", "--depth", &depth_str])?;
    } else {
        git_run(&["fetch", "upstream", "--prune"])?;
    }

    // Determine upstream branch (explicit > HEAD > main > master)
    let upstream_branch = if let Some(branch) = upstream_branch {
        branch.to_string()
    } else if let Ok(head_ref) = git_capture(&["symbolic-ref", "refs/remotes/upstream/HEAD"]) {
        head_ref.trim().replace("refs/remotes/upstream/", "")
    } else if git_capture(&["rev-parse", "--verify", "refs/remotes/upstream/main"]).is_ok() {
        "main".to_string()
    } else if git_capture(&["rev-parse", "--verify", "refs/remotes/upstream/master"]).is_ok() {
        "master".to_string()
    } else {
        // List available branches
        let branches = git_capture(&["branch", "-r", "--list", "upstream/*"])?;
        println!("Cannot auto-detect upstream branch.");
        println!("Available upstream branches:");
        for line in branches.lines() {
            println!("  {}", line.trim());
        }
        bail!("Specify branch with: f upstream setup --branch <branch-name>");
    };

    // Check if upstream branch exists on remote
    let remote_ref = format!("refs/remotes/upstream/{}", upstream_branch);
    if git_capture(&["rev-parse", "--verify", &remote_ref]).is_err() {
        let branches = git_capture(&["branch", "-r", "--list", "upstream/*"])?;
        println!("Branch 'upstream/{}' not found.", upstream_branch);
        println!("Available upstream branches:");
        for line in branches.lines() {
            println!("  {}", line.trim());
        }
        bail!("Specify branch with: f upstream setup --branch <branch-name>");
    }

    // Create or update local upstream branch
    let local_upstream_exists =
        git_capture(&["rev-parse", "--verify", "refs/heads/upstream"]).is_ok();
    let upstream_ref = format!("upstream/{}", upstream_branch);

    if local_upstream_exists {
        println!(
            "Updating local 'upstream' branch to match {}...",
            upstream_ref
        );
        let current = git_capture(&["rev-parse", "--abbrev-ref", "HEAD"])?;
        let current = current.trim();

        if current == "upstream" {
            // Already on upstream, just reset
            git_run(&["reset", "--hard", &upstream_ref])?;
        } else {
            // Update without switching
            git_run(&["branch", "-f", "upstream", &upstream_ref])?;
        }
    } else {
        println!(
            "Creating local 'upstream' branch tracking {}...",
            upstream_ref
        );
        git_run(&["branch", "upstream", &upstream_ref])?;
    }

    // Set up tracking
    git_run(&["config", "branch.upstream.remote", "upstream"])?;
    git_run(&[
        "config",
        "branch.upstream.merge",
        &format!("refs/heads/{}", upstream_branch),
    ])?;

    println!("\n✓ Upstream setup complete!");
    println!("\nWorkflow:");
    println!("  1. f upstream pull     - Pull latest from upstream into 'upstream' branch");
    println!("  2. f upstream sync     - Pull, merge to dev/main, and push");
    println!("\nThe local 'upstream' branch is a clean snapshot of the original repo.");
    println!("Your changes stay on dev/main, making merges cleaner.");

    Ok(())
}

/// Pull changes from upstream into the local upstream branch.
fn pull_upstream(target_branch: Option<&str>) -> Result<()> {
    // Check upstream remote exists
    if git_capture(&["remote", "get-url", "upstream"]).is_err() {
        bail!("No upstream remote. Run: f upstream setup --url <url>");
    }

    // Fetch upstream
    println!("Fetching upstream...");
    git_run(&["fetch", "upstream", "--prune"])?;

    // Determine the upstream branch to track (check config, then HEAD, then try main/master)
    let upstream_branch = resolve_upstream_branch()?;

    let upstream_ref = format!("upstream/{}", upstream_branch);

    // Update local upstream branch
    let current = git_capture(&["rev-parse", "--abbrev-ref", "HEAD"])?;
    let current = current.trim();

    // Check for uncommitted changes and stash if needed
    let mut stashed = false;
    let stash_count_before = git_capture(&["stash", "list"])
        .map(|s| s.lines().count())
        .unwrap_or(0);

    let status = git_capture(&["status", "--porcelain"])?;
    if !status.trim().is_empty() {
        println!("Stashing local changes...");
        let _ = git_run(&["stash", "push", "-m", "upstream-pull auto-stash"]);

        // Check if stash actually added an entry
        let stash_count_after = git_capture(&["stash", "list"])
            .map(|s| s.lines().count())
            .unwrap_or(0);
        stashed = stash_count_after > stash_count_before;
    }

    // Update local upstream branch
    let local_upstream_exists =
        git_capture(&["rev-parse", "--verify", "refs/heads/upstream"]).is_ok();

    if local_upstream_exists {
        if current == "upstream" {
            git_run(&["reset", "--hard", &upstream_ref])?;
        } else {
            git_run(&["branch", "-f", "upstream", &upstream_ref])?;
        }
        println!("✓ Updated local 'upstream' branch to {}", upstream_ref);
    } else {
        git_run(&["branch", "upstream", &upstream_ref])?;
        println!("✓ Created local 'upstream' branch from {}", upstream_ref);
    }

    // Optionally merge into target branch
    if let Some(target) = target_branch {
        println!("\nMerging upstream into {}...", target);

        if current != target {
            git_run(&["checkout", target])?;
        }

        if git_run(&["merge", "--ff-only", "upstream"]).is_err() {
            println!("Fast-forward failed, trying regular merge...");
            if let Err(e) = git_run(&["merge", "upstream", "--no-edit"]) {
                if stashed {
                    println!("Your changes are stashed. Run 'git stash pop' after resolving.");
                }
                return Err(e);
            }
        }
        println!("✓ Merged upstream into {}", target);

        // Return to original branch if different
        if current != target && current != "upstream" {
            git_run(&["checkout", current])?;
        }
    }

    // Restore stashed changes
    if stashed {
        println!("Restoring stashed changes...");
        git_run(&["stash", "pop"])?;
    }

    // Show what changed
    let behind = git_capture(&["rev-list", "--count", &format!("HEAD..{}", upstream_ref)])
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0);

    if behind > 0 {
        println!("\nYour branch is {} commit(s) behind upstream.", behind);
        println!("Run 'f upstream sync' to merge and push.");
    } else {
        println!("\n✓ Up to date with upstream!");
    }

    Ok(())
}

fn resolve_upstream_branch() -> Result<String> {
    if let Ok(merge_ref) = git_capture(&["config", "--get", "branch.upstream.merge"]) {
        return Ok(merge_ref.trim().replace("refs/heads/", ""));
    }
    if let Ok(head_ref) = git_capture(&["symbolic-ref", "refs/remotes/upstream/HEAD"]) {
        // Parse "refs/remotes/upstream/master" -> "master"
        return Ok(head_ref.trim().replace("refs/remotes/upstream/", ""));
    }
    if git_capture(&["rev-parse", "--verify", "refs/remotes/upstream/main"]).is_ok() {
        return Ok("main".to_string());
    }
    if git_capture(&["rev-parse", "--verify", "refs/remotes/upstream/master"]).is_ok() {
        return Ok("master".to_string());
    }
    if git_capture(&["rev-parse", "--verify", "refs/remotes/upstream/dev"]).is_ok() {
        return Ok("dev".to_string());
    }
    bail!("Cannot determine upstream branch. Run: f upstream setup --branch <branch>");
}

fn check_upstream() -> Result<()> {
    if git_capture(&["remote", "get-url", "upstream"]).is_err() {
        bail!("No upstream remote. Run: f upstream setup --url <url>");
    }

    println!("Fetching upstream...");
    git_run(&["fetch", "upstream", "--prune"])?;

    let upstream_branch = resolve_upstream_branch()?;
    let upstream_ref = format!("upstream/{}", upstream_branch);

    let current = git_capture(&["rev-parse", "--abbrev-ref", "HEAD"])?;
    let current = current.trim();

    let mut stashed = false;
    let stash_count_before = git_capture(&["stash", "list"])
        .map(|s| s.lines().count())
        .unwrap_or(0);

    let status = git_capture(&["status", "--porcelain"])?;
    if !status.trim().is_empty() {
        println!("Stashing local changes to check upstream...");
        let _ = git_run(&["stash", "push", "-m", "upstream-check auto-stash"]);
        let stash_count_after = git_capture(&["stash", "list"])
            .map(|s| s.lines().count())
            .unwrap_or(0);
        stashed = stash_count_after > stash_count_before;
    }

    let local_upstream_exists =
        git_capture(&["rev-parse", "--verify", "refs/heads/upstream"]).is_ok();
    if local_upstream_exists {
        if current == "upstream" {
            git_run(&["reset", "--hard", &upstream_ref])?;
        } else {
            git_run(&["branch", "-f", "upstream", &upstream_ref])?;
        }
        println!("✓ Updated local 'upstream' branch to {}", upstream_ref);
    } else {
        git_run(&["branch", "upstream", &upstream_ref])?;
        println!("✓ Created local 'upstream' branch from {}", upstream_ref);
    }

    git_run(&["checkout", "upstream"])?;
    println!("Now on 'upstream' (tracking {}).", upstream_ref);
    if stashed {
        println!("Your changes are stashed. Run 'git stash pop' when you're ready.");
    }

    Ok(())
}

/// Full sync: pull upstream, merge to dev, merge to main, push.
fn sync_upstream(push: bool, create_repo: bool) -> Result<()> {
    // Check upstream remote exists
    if git_capture(&["remote", "get-url", "upstream"]).is_err() {
        bail!("No upstream remote. Run: f upstream setup --url <url>");
    }

    let current = git_capture(&["rev-parse", "--abbrev-ref", "HEAD"])?;
    let current = current.trim().to_string();

    // Check for uncommitted changes and stash if needed
    let mut stashed = false;
    let stash_count_before = git_capture(&["stash", "list"])
        .map(|s| s.lines().count())
        .unwrap_or(0);

    let status = git_capture(&["status", "--porcelain"])?;
    if !status.trim().is_empty() {
        println!("Stashing local changes...");
        let _ = git_run(&["stash", "push", "-m", "upstream-sync auto-stash"]);

        // Check if stash actually added an entry
        let stash_count_after = git_capture(&["stash", "list"])
            .map(|s| s.lines().count())
            .unwrap_or(0);
        stashed = stash_count_after > stash_count_before;
    }

    // Fetch upstream
    println!("==> Fetching upstream...");
    git_run(&["fetch", "upstream", "--prune"])?;

    // Determine upstream branch (check config, then HEAD, then try main/master)
    let upstream_branch =
        if let Ok(merge_ref) = git_capture(&["config", "--get", "branch.upstream.merge"]) {
            merge_ref.trim().replace("refs/heads/", "")
        } else if let Ok(head_ref) = git_capture(&["symbolic-ref", "refs/remotes/upstream/HEAD"]) {
            // Parse "refs/remotes/upstream/master" -> "master"
            head_ref.trim().replace("refs/remotes/upstream/", "")
        } else if git_capture(&["rev-parse", "--verify", "refs/remotes/upstream/main"]).is_ok() {
            "main".to_string()
        } else if git_capture(&["rev-parse", "--verify", "refs/remotes/upstream/master"]).is_ok() {
            "master".to_string()
        } else {
            "main".to_string()
        };
    let upstream_ref = format!("upstream/{}", upstream_branch);

    // Update local upstream branch
    println!("==> Updating local 'upstream' branch...");
    let local_upstream_exists =
        git_capture(&["rev-parse", "--verify", "refs/heads/upstream"]).is_ok();
    if local_upstream_exists {
        git_run(&["branch", "-f", "upstream", &upstream_ref])?;
    } else {
        git_run(&["branch", "upstream", &upstream_ref])?;
    }

    // Detect branch structure (dev+main or just main)
    let has_dev = git_capture(&["rev-parse", "--verify", "refs/heads/dev"]).is_ok();
    let has_main = git_capture(&["rev-parse", "--verify", "refs/heads/main"]).is_ok();

    if has_dev {
        // Merge upstream -> dev -> main
        println!("==> Merging upstream into dev...");
        git_run(&["checkout", "dev"])?;
        merge_branch("upstream", "dev")?;

        if has_main {
            println!("==> Merging dev into main...");
            git_run(&["checkout", "main"])?;
            merge_branch("dev", "main")?;
        }
    } else if has_main {
        // Just merge upstream -> main
        println!("==> Merging upstream into main...");
        git_run(&["checkout", "main"])?;
        merge_branch("upstream", "main")?;
    } else {
        // Merge into current branch
        println!("==> Merging upstream into {}...", current);
        git_run(&["checkout", &current])?;
        merge_branch("upstream", &current)?;
    }

    // Push if requested
    if push {
        println!("==> Pushing to origin...");

        // Try push, auto-create repo if it doesn't exist
        let branches_to_push: Vec<&str> = if has_dev && has_main {
            vec!["dev", "main"]
        } else if has_main {
            vec!["main"]
        } else if has_dev {
            vec!["dev"]
        } else {
            vec![current.as_str()]
        };

        for branch in &branches_to_push {
            if let Err(e) = git_run(&["push", "origin", branch]) {
                // Only try to create repo if explicitly requested
                if create_repo && try_create_origin_repo()? {
                    // Repo created, retry push
                    git_run(&["push", "-u", "origin", branch])?;
                } else {
                    return Err(e);
                }
            }
        }
    }

    // Return to original branch
    if current != "main" && current != "dev" {
        git_run(&["checkout", &current])?;
    }

    // Restore stashed changes
    if stashed {
        println!("Restoring stashed changes...");
        git_run(&["stash", "pop"])?;
    }

    println!("\n✓ Sync complete!");
    if has_dev && has_main {
        println!("  upstream, dev, and main are updated.");
    } else if has_main {
        println!("  upstream and main are updated.");
    }

    Ok(())
}

/// Merge a source branch into the current branch.
fn merge_branch(source: &str, target: &str) -> Result<()> {
    // Try fast-forward first
    if git_run(&["merge", "--ff-only", source]).is_ok() {
        return Ok(());
    }

    println!("Fast-forward failed, trying regular merge...");
    if let Err(_) = git_run(&["merge", source, "--no-edit"]) {
        bail!(
            "Merge conflicts in {}. Resolve manually:\n  git status\n  # fix conflicts\n  git add . && git commit",
            target
        );
    }

    Ok(())
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
/// Returns true if repo was created, false if it already exists or creation failed.
fn try_create_origin_repo() -> Result<bool> {
    // Get origin URL to extract repo name
    let origin_url = match git_capture(&["remote", "get-url", "origin"]) {
        Ok(url) => url.trim().to_string(),
        Err(_) => return Ok(false),
    };

    // Extract repo name from URL (supports both SSH and HTTPS formats)
    // SSH: git@github.com:user/repo.git
    // HTTPS: https://github.com/user/repo.git
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

    // Use gh CLI to create the repo
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
            println!("  Run: gh auth login");
            Ok(false)
        }
        Err(e) => {
            println!("Failed to run gh CLI: {}", e);
            println!("  Install with: brew install gh");
            Ok(false)
        }
    }
}
