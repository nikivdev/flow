//! Git sync command - comprehensive repo synchronization.
//!
//! Provides a single command to sync a git repository:
//! - Pull from origin (with rebase)
//! - Sync upstream if configured (fetch, merge)
//! - Push to origin

use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    terminal,
};

use crate::cli::SyncCommand;

/// Run the sync command.
pub fn run(cmd: SyncCommand) -> Result<()> {
    // Check we're in a git repo
    if git_capture(&["rev-parse", "--git-dir"]).is_err() {
        bail!("Not a git repository");
    }

    // Determine if auto-fix is enabled (--fix is default, --no-fix disables)
    let auto_fix = cmd.fix && !cmd.no_fix;

    // Check for unmerged files (can exist even without active merge/rebase)
    let unmerged = git_capture(&["diff", "--name-only", "--diff-filter=U"]).unwrap_or_default();
    if !unmerged.trim().is_empty() {
        let unmerged_files: Vec<&str> = unmerged.lines().filter(|l| !l.is_empty()).collect();
        println!(
            "==> Found {} unmerged files, resolving...",
            unmerged_files.len()
        );

        let should_fix = auto_fix || prompt_for_auto_fix()?;
        if should_fix {
            if try_resolve_conflicts()? {
                let _ = git_run(&["add", "-A"]);
                // Check if we're in a merge
                if is_merge_in_progress() {
                    let _ = Command::new("git").args(["commit", "--no-edit"]).output();
                }
                println!("  ✓ Unmerged files resolved");
            } else {
                // Couldn't resolve - reset the conflicted files to HEAD
                println!("  Could not auto-resolve. Resetting unmerged files...");
                for file in &unmerged_files {
                    let _ = Command::new("git")
                        .args(["checkout", "HEAD", "--", file])
                        .output();
                }
                if is_merge_in_progress() {
                    let _ = Command::new("git").args(["merge", "--abort"]).output();
                }
            }
        } else {
            // User declined - reset the files
            println!("  Resetting unmerged files...");
            for file in &unmerged_files {
                let _ = Command::new("git")
                    .args(["checkout", "HEAD", "--", file])
                    .output();
            }
            if is_merge_in_progress() {
                let _ = Command::new("git").args(["merge", "--abort"]).output();
            }
        }
    }

    // Check for in-progress rebase/merge and handle it
    if is_rebase_in_progress() {
        println!("==> Rebase in progress, attempting to resolve...");
        let should_fix = auto_fix || prompt_for_rebase_action()?;
        if should_fix {
            if try_resolve_rebase_conflicts()? {
                println!("  ✓ Rebase completed");
            } else {
                println!("  Could not auto-resolve. Aborting rebase...");
                let _ = Command::new("git").args(["rebase", "--abort"]).output();
            }
        } else {
            println!("  Aborting rebase...");
            let _ = Command::new("git").args(["rebase", "--abort"]).output();
        }
    }

    // Check for in-progress merge
    if is_merge_in_progress() {
        println!("==> Merge in progress, attempting to resolve...");
        let should_fix = auto_fix || prompt_for_auto_fix()?;
        if should_fix {
            if try_resolve_conflicts()? {
                let _ = git_run(&["add", "-A"]);
                let _ = Command::new("git").args(["commit", "--no-edit"]).output();
                println!("  ✓ Merge completed");
            } else {
                println!("  Could not auto-resolve. Aborting merge...");
                let _ = Command::new("git").args(["merge", "--abort"]).output();
            }
        } else {
            println!("  Aborting merge...");
            let _ = Command::new("git").args(["merge", "--abort"]).output();
        }
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
    let origin_reachable =
        has_origin && git_capture(&["ls-remote", "--exit-code", "-q", "origin"]).is_ok();

    // Step 1: Pull from origin (if tracking branch exists and repo is reachable)
    if has_origin && origin_reachable {
        let tracking = git_capture(&["rev-parse", "--abbrev-ref", "@{upstream}"]);
        if tracking.is_ok() {
            println!("==> Pulling from origin...");
            if cmd.rebase {
                if let Err(_) = git_run(&["pull", "--rebase", "origin", current]) {
                    // Check if we're in a rebase conflict
                    if is_rebase_in_progress() {
                        let should_fix = auto_fix || prompt_for_auto_fix()?;
                        if should_fix {
                            if try_resolve_rebase_conflicts()? {
                                println!("  ✓ Rebase conflicts auto-resolved");
                            } else {
                                restore_stash(stashed);
                                bail!(
                                    "Rebase conflicts. Resolve manually:\n  git status\n  # fix conflicts\n  git add . && git rebase --continue"
                                );
                            }
                        } else {
                            restore_stash(stashed);
                            bail!(
                                "Rebase conflicts. Resolve manually:\n  git status\n  # fix conflicts\n  git add . && git rebase --continue"
                            );
                        }
                    } else {
                        restore_stash(stashed);
                        bail!("git pull --rebase failed");
                    }
                }
            } else {
                if let Err(_) = git_run(&["pull", "origin", current]) {
                    // Check for merge conflicts
                    let conflicts = git_capture(&["diff", "--name-only", "--diff-filter=U"])
                        .unwrap_or_default();
                    if !conflicts.trim().is_empty() {
                        let should_fix = auto_fix || prompt_for_auto_fix()?;
                        if should_fix {
                            if try_resolve_conflicts()? {
                                let _ = git_run(&["add", "-A"]);
                                let _ = Command::new("git").args(["commit", "--no-edit"]).output();
                                println!("  ✓ Merge conflicts auto-resolved");
                            } else {
                                restore_stash(stashed);
                                bail!(
                                    "Merge conflicts. Resolve manually:\n  git status\n  # fix conflicts\n  git add . && git commit"
                                );
                            }
                        } else {
                            restore_stash(stashed);
                            bail!(
                                "Merge conflicts. Resolve manually:\n  git status\n  # fix conflicts\n  git add . && git commit"
                            );
                        }
                    } else {
                        restore_stash(stashed);
                        bail!("git pull failed");
                    }
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
        if let Err(e) = sync_upstream_internal(current, auto_fix) {
            restore_stash(stashed);
            return Err(e);
        }
    }

    // Step 3: Push to origin
    if has_origin && !cmd.no_push {
        // Check if origin == upstream (read-only clone, no fork)
        let origin_url = git_capture(&["remote", "get-url", "origin"]).unwrap_or_default();
        let upstream_url = git_capture(&["remote", "get-url", "upstream"]).unwrap_or_default();
        let is_read_only =
            has_upstream && normalize_git_url(&origin_url) == normalize_git_url(&upstream_url);

        if is_read_only {
            println!("==> Skipping push (origin == upstream, read-only clone)");
            println!("  To push, create a fork first: gh repo fork --remote");
        } else if !origin_reachable {
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
            let push_result = push_with_autofix(current, auto_fix, cmd.max_fix_attempts);
            if let Err(e) = push_result {
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
fn sync_upstream_internal(current_branch: &str, auto_fix: bool) -> Result<()> {
    // Fetch upstream
    git_run(&["fetch", "upstream", "--prune"])?;

    // Determine upstream branch
    let upstream_branch =
        if let Ok(merge_ref) = git_capture(&["config", "--get", "branch.upstream.merge"]) {
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
    let local_upstream_exists =
        git_capture(&["rev-parse", "--verify", "refs/heads/upstream"]).is_ok();
    if local_upstream_exists {
        git_run(&["branch", "-f", "upstream", &upstream_ref])?;
    }

    // Check if current branch is behind upstream
    let behind = git_capture(&[
        "rev-list",
        "--count",
        &format!("{}..{}", current_branch, upstream_ref),
    ])
    .ok()
    .and_then(|s| s.trim().parse::<u32>().ok())
    .unwrap_or(0);

    if behind > 0 {
        println!("  Merging {} commits from upstream...", behind);

        // Try fast-forward first
        if git_run(&["merge", "--ff-only", &upstream_ref]).is_err() {
            // Fall back to regular merge
            if let Err(_) = git_run(&["merge", &upstream_ref, "--no-edit"]) {
                // Merge failed - check for conflicts
                let should_fix = auto_fix || prompt_for_auto_fix()?;
                if should_fix {
                    println!("  Attempting auto-fix...");
                    if try_resolve_conflicts()? {
                        // Conflicts resolved, commit
                        let _ = git_run(&["add", "-A"]);
                        let _ = Command::new("git").args(["commit", "--no-edit"]).output();
                        println!("  ✓ Conflicts auto-resolved");
                        return Ok(());
                    }
                }
                bail!(
                    "Merge conflicts with upstream. Resolve manually:\n  git status\n  # fix conflicts\n  git add . && git commit"
                );
            }
        }
    } else {
        println!("  Already up to date with upstream");
    }

    Ok(())
}

/// Check if a rebase is in progress.
fn is_rebase_in_progress() -> bool {
    let git_dir = git_capture(&["rev-parse", "--git-dir"]).unwrap_or_else(|_| ".git".to_string());
    let git_dir = git_dir.trim();
    std::path::Path::new(&format!("{}/rebase-merge", git_dir)).exists()
        || std::path::Path::new(&format!("{}/rebase-apply", git_dir)).exists()
}

/// Check if a merge is in progress.
fn is_merge_in_progress() -> bool {
    let git_dir = git_capture(&["rev-parse", "--git-dir"]).unwrap_or_else(|_| ".git".to_string());
    let git_dir = git_dir.trim();
    std::path::Path::new(&format!("{}/MERGE_HEAD", git_dir)).exists()
}

/// Read a single keypress (y/n) without waiting for Enter.
fn read_yes_no() -> Result<bool> {
    terminal::enable_raw_mode()?;
    let result = loop {
        if event::poll(std::time::Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('y') | KeyCode::Char('Y') => break Ok(true),
                        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Enter | KeyCode::Esc => {
                            break Ok(false);
                        }
                        _ => {}
                    }
                }
            }
        }
    };
    terminal::disable_raw_mode()?;
    println!(); // Move to next line after keypress
    result
}

/// Prompt user for rebase action.
fn prompt_for_rebase_action() -> Result<bool> {
    let conflicts = git_capture(&["diff", "--name-only", "--diff-filter=U"]).unwrap_or_default();
    let conflicted_files: Vec<&str> = conflicts.lines().filter(|l| !l.is_empty()).collect();

    if !conflicted_files.is_empty() {
        println!("\n  Conflicted files:");
        for file in &conflicted_files {
            println!("    - {}", file);
        }
        println!();
    }

    print!("  Try auto-fix with Claude? [y/N] ");
    std::io::Write::flush(&mut std::io::stdout())?;

    read_yes_no()
}

/// Try to resolve rebase conflicts and continue.
fn try_resolve_rebase_conflicts() -> Result<bool> {
    loop {
        // Get conflicted files
        let conflicts = git_capture(&["diff", "--name-only", "--diff-filter=U"])?;
        let conflicted_files: Vec<&str> = conflicts.lines().filter(|l| !l.is_empty()).collect();

        if conflicted_files.is_empty() {
            // No more conflicts, try to continue rebase
            let result = Command::new("git")
                .args(["rebase", "--continue"])
                .env("GIT_EDITOR", "true") // Skip commit message editing
                .output();

            match result {
                Ok(out) if out.status.success() => return Ok(true),
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    // Check if rebase is complete
                    if stderr.contains("No rebase in progress") || !is_rebase_in_progress() {
                        return Ok(true);
                    }
                    // Still conflicts, continue loop
                }
                Err(_) => return Ok(false),
            }
            continue;
        }

        println!("  Resolving {} conflicted files...", conflicted_files.len());

        // Try to resolve each conflict
        let mut all_resolved = true;
        for file in &conflicted_files {
            if !try_resolve_single_conflict(file)? {
                all_resolved = false;
                println!("  ✗ Could not resolve {}", file);
            }
        }

        if !all_resolved {
            return Ok(false);
        }

        // Stage resolved files
        let _ = git_run(&["add", "-A"]);

        // Try to continue rebase
        let result = Command::new("git")
            .args(["rebase", "--continue"])
            .env("GIT_EDITOR", "true")
            .output();

        match result {
            Ok(out) if out.status.success() => return Ok(true),
            Ok(_) => {
                // More conflicts from next commit, continue loop
                if !is_rebase_in_progress() {
                    return Ok(true);
                }
            }
            Err(_) => return Ok(false),
        }
    }
}

/// Try to resolve a single conflicted file.
fn try_resolve_single_conflict(file: &str) -> Result<bool> {
    let filename = file.rsplit('/').next().unwrap_or(file);

    // Auto-generated files - accept theirs (upstream/incoming)
    let auto_generated = [
        "STATS.md",
        "stats.md",
        "CHANGELOG.md",
        "changelog.md",
        "package-lock.json",
        "yarn.lock",
        "bun.lock",
        "pnpm-lock.yaml",
        "Cargo.lock",
        "Gemfile.lock",
        "poetry.lock",
        "composer.lock",
    ];

    if auto_generated
        .iter()
        .any(|&ag| filename.eq_ignore_ascii_case(ag))
    {
        println!("  Auto-resolving {} (accepting theirs)", file);
        let _ = Command::new("git")
            .args(["checkout", "--theirs", file])
            .output();
        let _ = Command::new("git").args(["add", file]).output();
        return Ok(true);
    }

    // Try Claude for code conflicts
    let content = std::fs::read_to_string(file).unwrap_or_default();
    if content.contains("<<<<<<<") {
        println!("  Trying Claude for {}...", file);
        let prompt = format!(
            "This file has git merge conflicts. Resolve them by keeping the best of both versions. Output ONLY the resolved file content, no explanations:\n\n{}",
            if content.len() > 8000 {
                &content[..8000]
            } else {
                &content
            }
        );

        let output = Command::new("claude")
            .args(["--print", "--dangerously-skip-permissions", &prompt])
            .output();

        if let Ok(out) = output {
            if out.status.success() {
                let resolved = String::from_utf8_lossy(&out.stdout);
                if !resolved.contains("<<<<<<<") && !resolved.contains(">>>>>>>") {
                    if std::fs::write(file, resolved.as_ref()).is_ok() {
                        let _ = Command::new("git").args(["add", file]).output();
                        println!("  ✓ Resolved {}", file);
                        return Ok(true);
                    }
                }
            }
        }
    }

    Ok(false)
}

/// Prompt user to try auto-fix for push failures.
fn prompt_for_push_fix() -> Result<bool> {
    println!();
    print!("  Try auto-fix with Claude? [y/N] ");
    std::io::Write::flush(&mut std::io::stdout())?;
    read_yes_no()
}

/// Prompt user to try auto-fix for conflicts.
fn prompt_for_auto_fix() -> Result<bool> {
    // Get list of conflicted files
    let conflicts = git_capture(&["diff", "--name-only", "--diff-filter=U"])?;
    let conflicted_files: Vec<&str> = conflicts.lines().filter(|l| !l.is_empty()).collect();

    if conflicted_files.is_empty() {
        return Ok(false);
    }

    println!("\n  Conflicted files:");
    for file in &conflicted_files {
        println!("    - {}", file);
    }
    println!();

    print!("  Try auto-fix with Claude? [y/N] ");
    std::io::Write::flush(&mut std::io::stdout())?;
    read_yes_no()
}

/// Try to resolve merge conflicts automatically.
fn try_resolve_conflicts() -> Result<bool> {
    // Get list of conflicted files
    let conflicts = git_capture(&["diff", "--name-only", "--diff-filter=U"])?;
    let conflicted_files: Vec<&str> = conflicts.lines().filter(|l| !l.is_empty()).collect();

    if conflicted_files.is_empty() {
        return Ok(true);
    }

    println!("  Conflicted files: {}", conflicted_files.join(", "));

    // Auto-generated files - accept theirs (upstream)
    let auto_generated = [
        "STATS.md",
        "stats.md",
        "CHANGELOG.md",
        "changelog.md",
        "package-lock.json",
        "yarn.lock",
        "bun.lock",
        "pnpm-lock.yaml",
        "Cargo.lock",
        "Gemfile.lock",
        "poetry.lock",
        "composer.lock",
    ];

    let mut resolved_count = 0;
    let mut needs_claude = Vec::new();

    for file in &conflicted_files {
        let filename = file.rsplit('/').next().unwrap_or(file);

        if auto_generated
            .iter()
            .any(|&ag| filename.eq_ignore_ascii_case(ag))
        {
            // Accept theirs for auto-generated files
            println!("  Auto-resolving {} (accepting upstream)", file);
            let _ = Command::new("git")
                .args(["checkout", "--theirs", file])
                .output();
            let _ = Command::new("git").args(["add", file]).output();
            resolved_count += 1;
        } else {
            needs_claude.push(*file);
        }
    }

    // If all conflicts were auto-generated files, we're done
    if needs_claude.is_empty() {
        return Ok(true);
    }

    // Try Claude for remaining conflicts
    println!(
        "  Trying Claude for {} remaining conflicts...",
        needs_claude.len()
    );

    for file in &needs_claude {
        let content = std::fs::read_to_string(file).unwrap_or_default();
        if content.contains("<<<<<<<") {
            let prompt = format!(
                "This file has git merge conflicts. Resolve them by keeping the best of both versions. Output ONLY the resolved file content, no explanations:\n\n{}",
                if content.len() > 8000 {
                    &content[..8000]
                } else {
                    &content
                }
            );

            let output = Command::new("claude")
                .args(["--print", "--dangerously-skip-permissions", &prompt])
                .output();

            if let Ok(out) = output {
                if out.status.success() {
                    let resolved = String::from_utf8_lossy(&out.stdout);
                    // Only use if it doesn't contain conflict markers
                    if !resolved.contains("<<<<<<<") && !resolved.contains(">>>>>>>") {
                        if std::fs::write(file, resolved.as_ref()).is_ok() {
                            let _ = Command::new("git").args(["add", file]).output();
                            resolved_count += 1;
                            println!("  ✓ Resolved {}", file);
                            continue;
                        }
                    }
                }
            }
            println!("  ✗ Could not resolve {}", file);
        }
    }

    Ok(resolved_count == conflicted_files.len())
}

fn restore_stash(stashed: bool) {
    if stashed {
        println!("==> Restoring stashed changes...");
        let _ = git_run(&["stash", "pop"]);
    }
}

/// Normalize a git URL for comparison (handle ssh vs https, trailing .git).
fn normalize_git_url(url: &str) -> String {
    let url = url.trim();
    // Convert SSH to HTTPS format for comparison
    let url = if url.starts_with("git@github.com:") {
        url.replace("git@github.com:", "github.com/")
    } else if url.starts_with("https://github.com/") {
        url.replace("https://github.com/", "github.com/")
    } else {
        url.to_string()
    };
    // Remove trailing .git
    url.trim_end_matches(".git").to_lowercase()
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

/// Push to origin with optional auto-fix on failure.
fn push_with_autofix(branch: &str, auto_fix: bool, max_attempts: u32) -> Result<()> {
    let mut attempts = 0;

    loop {
        // Try push and capture output
        let output = Command::new("git")
            .args(["push", "origin", branch])
            .output()
            .context("failed to run git push")?;

        if output.status.success() {
            return Ok(());
        }

        attempts += 1;
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let combined = format!("{}\n{}", stdout, stderr);

        // Check if this looks like a pre-push hook failure
        let is_hook_failure = combined.contains("pre-push")
            || combined.contains("husky")
            || combined.contains("typecheck")
            || combined.contains("error TS")
            || combined.contains("eslint")
            || combined.contains("error:")
            || combined.contains("failed to push");

        // Prompt user if not already in auto-fix mode
        let should_fix = if auto_fix {
            true
        } else if is_hook_failure && attempts == 1 {
            println!("{}", combined);
            prompt_for_push_fix()?
        } else {
            false
        };

        if !should_fix || attempts > max_attempts {
            if !should_fix {
                println!("{}", combined);
            }
            bail!("git push origin {} failed", branch);
        }

        println!(
            "\n==> Push failed (attempt {}/{}), attempting auto-fix with Claude...",
            attempts, max_attempts
        );

        // Run Claude to fix the errors
        if !try_claude_fix(&combined)? {
            println!("{}", combined);
            bail!("Auto-fix failed. Run manually:\n  claude 'fix these errors: ...'");
        }

        // Stage and commit the fix
        let status = git_capture(&["status", "--porcelain"])?;
        if !status.trim().is_empty() {
            println!("==> Committing auto-fix...");
            let _ = git_run(&["add", "-A"]);
            let commit_msg = format!("fix: auto-fix sync errors (attempt {})", attempts);
            let _ = Command::new("git")
                .args(["commit", "-m", &commit_msg, "--no-verify"])
                .output();
        }

        println!("==> Retrying push...");
    }
}

/// Try to fix errors using Claude CLI.
fn try_claude_fix(error_output: &str) -> Result<bool> {
    // Check if claude is available
    let claude_check = Command::new("which").arg("claude").output();

    if claude_check.is_err() || !claude_check.unwrap().status.success() {
        println!("  Claude CLI not found. Install with: npm i -g @anthropic-ai/claude-code");
        return Ok(false);
    }

    // Build a focused prompt
    let prompt = format!(
        "Fix these errors so the code compiles/passes checks. Make minimal changes. Do not explain, just fix:\n\n{}",
        // Truncate if too long
        if error_output.len() > 4000 {
            &error_output[error_output.len() - 4000..]
        } else {
            error_output
        }
    );

    // Run claude with the fix prompt
    let status = Command::new("claude")
        .args(["--print", "--dangerously-skip-permissions", &prompt])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to run claude")?;

    Ok(status.success())
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
