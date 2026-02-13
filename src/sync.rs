//! Git sync command - comprehensive repo synchronization.
//!
//! Provides a single command to sync a git repository:
//! - Pull from origin (with rebase)
//! - Sync upstream if configured (fetch, merge)
//! - Push to origin

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    terminal,
};
use serde::Serialize;

use crate::ai_context;
use crate::cli::{CheckoutCommand, SwitchCommand, SyncCommand};
use crate::commit;
use crate::config;
use crate::git_guard;

#[derive(Serialize, Clone)]
struct SyncEvent {
    at: String,
    stage: String,
    message: String,
}

#[derive(Serialize)]
struct SyncSnapshot {
    timestamp: String,
    duration_ms: u128,
    repo_root: String,
    repo_name: String,
    branch_before: String,
    branch_after: String,
    head_before: String,
    head_after: String,
    upstream_before: Option<String>,
    upstream_after: Option<String>,
    origin_url: Option<String>,
    upstream_url: Option<String>,
    status_before: String,
    status_after: String,
    stashed: bool,
    rebase: bool,
    pushed: bool,
    success: bool,
    error: Option<String>,
    events: Vec<SyncEvent>,
}

struct SyncRecorder {
    enabled: bool,
    started_at: Instant,
    repo_root: String,
    repo_name: String,
    branch_before: String,
    head_before: String,
    upstream_before: Option<String>,
    origin_url: Option<String>,
    upstream_url: Option<String>,
    status_before: String,
    events: Vec<SyncEvent>,
    stashed: bool,
    rebase: bool,
    pushed: bool,
}

fn sync_should_push(cmd: &SyncCommand) -> bool {
    cmd.push && !cmd.no_push
}

impl SyncRecorder {
    fn new(cmd: &SyncCommand) -> Result<Self> {
        let should_push = sync_should_push(cmd);
        let repo_root =
            git_capture(&["rev-parse", "--show-toplevel"]).unwrap_or_else(|_| ".".to_string());
        let repo_root = repo_root.trim().to_string();
        let repo_name = std::path::Path::new(&repo_root)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("repo")
            .to_string();
        let branch_before = git_capture(&["rev-parse", "--abbrev-ref", "HEAD"])
            .unwrap_or_default()
            .trim()
            .to_string();
        let head_before = git_capture(&["rev-parse", "HEAD"])
            .unwrap_or_default()
            .trim()
            .to_string();
        let upstream_before = git_capture(&["rev-parse", "--abbrev-ref", "@{upstream}"])
            .ok()
            .map(|s| s.trim().to_string());
        let origin_url = git_capture(&["remote", "get-url", "origin"])
            .ok()
            .map(|s| s.trim().to_string());
        let upstream_url = git_capture(&["remote", "get-url", "upstream"])
            .ok()
            .map(|s| s.trim().to_string());
        let status_before = git_capture(&["status", "--porcelain"]).unwrap_or_default();

        let mut recorder = SyncRecorder {
            enabled: true,
            started_at: Instant::now(),
            repo_root,
            repo_name,
            branch_before,
            head_before,
            upstream_before,
            origin_url,
            upstream_url,
            status_before,
            events: Vec::new(),
            stashed: false,
            rebase: cmd.rebase,
            pushed: should_push,
        };
        recorder.record(
            "start",
            format!(
                "sync start (rebase={}, stash={}, push={})",
                cmd.rebase, cmd.stash, should_push
            ),
        );
        Ok(recorder)
    }

    fn disabled() -> Self {
        SyncRecorder {
            enabled: false,
            started_at: Instant::now(),
            repo_root: String::new(),
            repo_name: String::new(),
            branch_before: String::new(),
            head_before: String::new(),
            upstream_before: None,
            origin_url: None,
            upstream_url: None,
            status_before: String::new(),
            events: Vec::new(),
            stashed: false,
            rebase: false,
            pushed: false,
        }
    }

    fn record(&mut self, stage: &str, message: impl Into<String>) {
        if !self.enabled {
            return;
        }
        self.events.push(SyncEvent {
            at: Utc::now().to_rfc3339(),
            stage: stage.to_string(),
            message: message.into(),
        });
    }

    fn set_stashed(&mut self, stashed: bool) {
        self.stashed = stashed;
    }

    fn finish(&mut self, error: Option<&anyhow::Error>) {
        if !self.enabled {
            return;
        }

        let branch_after = git_capture(&["rev-parse", "--abbrev-ref", "HEAD"])
            .unwrap_or_default()
            .trim()
            .to_string();
        let head_after = git_capture(&["rev-parse", "HEAD"])
            .unwrap_or_default()
            .trim()
            .to_string();
        let upstream_after = git_capture(&["rev-parse", "--abbrev-ref", "@{upstream}"])
            .ok()
            .map(|s| s.trim().to_string());
        let status_after = git_capture(&["status", "--porcelain"]).unwrap_or_default();

        let snapshot = SyncSnapshot {
            timestamp: Utc::now().to_rfc3339(),
            duration_ms: self.started_at.elapsed().as_millis(),
            repo_root: self.repo_root.clone(),
            repo_name: self.repo_name.clone(),
            branch_before: self.branch_before.clone(),
            branch_after,
            head_before: self.head_before.clone(),
            head_after,
            upstream_before: self.upstream_before.clone(),
            upstream_after,
            origin_url: self.origin_url.clone(),
            upstream_url: self.upstream_url.clone(),
            status_before: self.status_before.clone(),
            status_after,
            stashed: self.stashed,
            rebase: self.rebase,
            pushed: self.pushed,
            success: error.is_none(),
            error: error.map(|e| e.to_string()),
            events: self.events.clone(),
        };

        if let Err(err) = write_sync_snapshot(&snapshot) {
            eprintln!("warn: failed to write sync snapshot: {err}");
        }
    }
}

/// Run the sync command.
pub fn run(cmd: SyncCommand) -> Result<()> {
    // Check we're in a git repo
    if git_capture(&["rev-parse", "--git-dir"]).is_err() {
        bail!("Not a git repository");
    }

    let mut recorder = SyncRecorder::new(&cmd).unwrap_or_else(|err| {
        eprintln!("warn: unable to init sync recorder: {err}");
        SyncRecorder::disabled()
    });

    let result = (|| -> Result<()> {
        // Determine if auto-fix is enabled (--fix is default, --no-fix disables)
        let auto_fix = cmd.fix && !cmd.no_fix;
        let should_push = sync_should_push(&cmd);
        let repo_root = git_capture(&["rev-parse", "--show-toplevel"])
            .unwrap_or_else(|_| ".".to_string())
            .trim()
            .to_string();
        let repo_root_path = Path::new(&repo_root);
        let queue_present = commit::commit_queue_has_entries(repo_root_path);
        if queue_present && !cmd.allow_queue && (cmd.rebase || should_use_jj(repo_root_path)) {
            recorder.record("queue", "blocked (commit queue present)");
            bail!(
                "Commit queue is not empty. Rebase-based sync can rewrite commit SHAs.\n\
Use `f commit-queue list` to review, or re-run with `--allow-queue`."
            );
        }

        if should_use_jj(repo_root_path) {
            recorder.record("mode", "using jj sync flow");
            return run_jj_sync(repo_root_path, &cmd, auto_fix, &mut recorder);
        }

        // Check for unmerged files (can exist even without active merge/rebase)
        let unmerged = git_capture(&["diff", "--name-only", "--diff-filter=U"]).unwrap_or_default();
        if !unmerged.trim().is_empty() {
            let unmerged_files: Vec<&str> = unmerged.lines().filter(|l| !l.is_empty()).collect();
            println!(
                "==> Found {} unmerged files, resolving...",
                unmerged_files.len()
            );
            recorder.record(
                "unmerged",
                format!("found {} unmerged files", unmerged_files.len()),
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
            recorder.record("stash", "skipped (uncommitted changes without --stash)");
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
        recorder.set_stashed(stashed);
        if has_changes && cmd.stash {
            recorder.record("stash", format!("stashed={}", stashed));
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
                recorder.record(
                    "pull",
                    format!("pulling from origin (rebase={})", cmd.rebase),
                );
                if cmd.rebase {
                    if let Err(_) = git_run(&["pull", "--rebase", "origin", current]) {
                        // Check if we're in a rebase conflict
                        if is_rebase_in_progress() {
                            let should_fix = auto_fix || prompt_for_auto_fix()?;
                            if should_fix {
                                if try_resolve_rebase_conflicts()? {
                                    println!("  ✓ Rebase conflicts auto-resolved");
                                    recorder.record("pull", "rebase conflicts auto-resolved");
                                } else {
                                    restore_stash(stashed);
                                    recorder.record("pull", "rebase conflicts unresolved");
                                    bail!(
                                        "Rebase conflicts. Resolve manually:\n  git status\n  # fix conflicts\n  git add . && git rebase --continue"
                                    );
                                }
                            } else {
                                restore_stash(stashed);
                                recorder.record("pull", "rebase conflicts unresolved");
                                bail!(
                                    "Rebase conflicts. Resolve manually:\n  git status\n  # fix conflicts\n  git add . && git rebase --continue"
                                );
                            }
                        } else {
                            restore_stash(stashed);
                            recorder.record("pull", "git pull --rebase failed");
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
                                    let _ =
                                        Command::new("git").args(["commit", "--no-edit"]).output();
                                    println!("  ✓ Merge conflicts auto-resolved");
                                    recorder.record("pull", "merge conflicts auto-resolved");
                                } else {
                                    restore_stash(stashed);
                                    recorder.record("pull", "merge conflicts unresolved");
                                    bail!(
                                        "Merge conflicts. Resolve manually:\n  git status\n  # fix conflicts\n  git add . && git commit"
                                    );
                                }
                            } else {
                                restore_stash(stashed);
                                recorder.record("pull", "merge conflicts unresolved");
                                bail!(
                                    "Merge conflicts. Resolve manually:\n  git status\n  # fix conflicts\n  git add . && git commit"
                                );
                            }
                        } else {
                            restore_stash(stashed);
                            recorder.record("pull", "git pull failed");
                            bail!("git pull failed");
                        }
                    }
                }
                recorder.record("pull", "pull complete");
            } else {
                println!("==> No tracking branch, skipping pull");
                recorder.record("pull", "skipped (no tracking branch)");
            }
        } else if has_origin && !origin_reachable {
            println!("==> Origin unreachable, skipping pull");
            println!("  The remote may be missing, private, or auth/network failed.");
            recorder.record("pull", "skipped (origin unreachable)");
        }

        // Step 2: Sync upstream if it exists
        if has_upstream {
            println!("==> Syncing upstream...");
            recorder.record("upstream", "syncing upstream");
            if let Err(e) =
                sync_upstream_internal(repo_root_path, current, auto_fix, &mut recorder)
            {
                restore_stash(stashed);
                return Err(e);
            }
        } else {
            recorder.record("upstream", "skipped (no upstream remote)");
        }

        // Step 3: Push to origin
        if has_origin && should_push {
            // Check if origin == upstream (read-only clone, no fork)
            let origin_url = git_capture(&["remote", "get-url", "origin"]).unwrap_or_default();
            let upstream_url = git_capture(&["remote", "get-url", "upstream"]).unwrap_or_default();
            let is_read_only =
                has_upstream && normalize_git_url(&origin_url) == normalize_git_url(&upstream_url);

            if is_read_only {
                println!("==> Skipping push (origin == upstream, read-only clone)");
                println!("  To push, create a fork first: gh repo fork --remote");
                recorder.record("push", "skipped (origin == upstream)");
            } else if !origin_reachable {
                // Origin repo doesn't exist
                if cmd.create_repo {
                    println!("==> Creating origin repo...");
                    if try_create_origin_repo()? {
                        println!("==> Pushing to origin...");
                        git_run(&["push", "-u", "origin", current])?;
                        recorder.record("push", "created repo and pushed to origin");
                    } else {
                        println!("  Could not create repo, skipping push");
                        recorder.record("push", "skipped (create repo failed)");
                    }
                } else {
                    println!("==> Origin unreachable, skipping push");
                    println!("  The remote may be missing, private, or auth/network failed.");
                    println!("  Use --create-repo if origin does not exist yet.");
                    recorder.record("push", "skipped (origin unreachable)");
                }
            } else {
                println!("==> Pushing to origin...");
                let push_result = push_with_autofix(current, auto_fix, cmd.max_fix_attempts);
                if let Err(e) = push_result {
                    restore_stash(stashed);
                    recorder.record("push", "push failed");
                    return Err(e);
                }
                recorder.record("push", "push complete");
            }
        } else if cmd.no_push {
            recorder.record("push", "skipped (--no-push)");
        } else if has_origin {
            recorder.record("push", "skipped (default; use --push)");
        } else {
            recorder.record("push", "skipped (no origin)");
        }

        // Restore stash
        restore_stash(stashed);
        if stashed {
            recorder.record("stash", "stash restored");
        }

        println!("\n✓ Sync complete!");
        recorder.record("complete", "sync complete");

        Ok(())
    })();

    recorder.finish(result.as_ref().err());
    result
}

/// Switch to a branch and align upstream/jj state for flow workflows.
pub fn run_switch(cmd: SwitchCommand) -> Result<()> {
    if git_capture(&["rev-parse", "--git-dir"]).is_err() {
        bail!("Not a git repository");
    }

    let target_branch = cmd.branch.trim();
    if target_branch.is_empty() {
        bail!("Branch name cannot be empty");
    }

    let repo_root = git_capture(&["rev-parse", "--show-toplevel"])?
        .trim()
        .to_string();
    let repo_root_path = PathBuf::from(repo_root);
    git_guard::ensure_clean_for_push(&repo_root_path)?;

    let stash_enabled = cmd.stash && !cmd.no_stash;
    let has_changes = !git_capture_in(&repo_root_path, &["status", "--porcelain"])?
        .trim()
        .is_empty();
    let mut stashed = false;

    if stash_enabled && has_changes {
        let message = format!(
            "flow-switch-{}-{}",
            target_branch,
            Utc::now().format("%Y%m%d-%H%M%S")
        );
        println!("==> Stashing local changes...");
        if let Err(err) = git_run_in(&repo_root_path, &["stash", "push", "-u", "-m", &message]) {
            eprintln!(
                "warning: auto-stash failed, continuing without stash: {}",
                err
            );
        } else {
            stashed = true;
        }
    }

    let switch_result = (|| -> Result<()> {
        let tracking_remote = resolve_tracking_remote_and_fetch(
            &repo_root_path,
            target_branch,
            cmd.remote.as_deref(),
        )?;

        if git_ref_exists_in(&repo_root_path, &format!("refs/heads/{}", target_branch)) {
            println!("==> Switching to local branch {}...", target_branch);
            git_run_in(&repo_root_path, &["switch", target_branch])?;
        } else if let Some(remote) = tracking_remote.as_deref() {
            println!(
                "==> Creating {} from {}/{}...",
                target_branch, remote, target_branch
            );
            let remote_branch = format!("{}/{}", remote, target_branch);
            git_run_in(
                &repo_root_path,
                &["switch", "-c", target_branch, "--track", &remote_branch],
            )?;
        } else {
            let preferred = cmd.remote.as_deref().unwrap_or("upstream/origin");
            bail!(
                "Branch '{}' not found locally or on remotes (searched: {}).",
                target_branch,
                preferred
            );
        }

        if remote_branch_exists(&repo_root_path, "upstream", target_branch) {
            println!(
                "==> Updating flow upstream tracking to upstream/{}...",
                target_branch
            );
            git_run_in(
                &repo_root_path,
                &["config", "branch.upstream.remote", "upstream"],
            )?;
            let merge_ref = format!("refs/heads/{}", target_branch);
            git_run_in(
                &repo_root_path,
                &["config", "branch.upstream.merge", &merge_ref],
            )?;
            sync_local_upstream_branch(&repo_root_path, target_branch)?;
        }

        if should_use_jj(&repo_root_path) {
            println!("==> Importing git refs into jj...");
            jj_run_in(&repo_root_path, &["--quiet", "git", "import"])?;
        }

        Ok(())
    })();

    if let Err(err) = switch_result {
        if stashed {
            eprintln!("==> Restoring stashed changes after failed switch...");
            let _ = git_run_in(&repo_root_path, &["stash", "pop"]);
        }
        return Err(err);
    }

    if stashed {
        println!("==> Restoring stashed changes...");
        if let Err(err) = git_run_in(&repo_root_path, &["stash", "pop"]) {
            eprintln!(
                "warning: failed to restore stash automatically: {}\nRun `git stash list` and restore manually if needed.",
                err
            );
        }
    }

    println!("✓ Switched to {}", target_branch);

    if cmd.sync {
        println!("==> Running sync (default no push)...");
        if let Err(sync_err) = run(SyncCommand {
            rebase: false,
            push: false,
            no_push: true,
            stash: true,
            stash_commits: false,
            allow_queue: false,
            create_repo: false,
            fix: true,
            no_fix: false,
            max_fix_attempts: 3,
        }) {
            let _ = ensure_branch_attached(&repo_root_path, target_branch);
            return Err(sync_err);
        }
        ensure_branch_attached(&repo_root_path, target_branch)?;
    }

    Ok(())
}

/// Checkout a GitHub PR safely while preserving local changes.
pub fn run_checkout(cmd: CheckoutCommand) -> Result<()> {
    if git_capture(&["rev-parse", "--git-dir"]).is_err() {
        bail!("Not a git repository");
    }

    let target = cmd.target.trim();
    if target.is_empty() {
        bail!("Checkout target cannot be empty");
    }

    let repo_root = git_capture(&["rev-parse", "--show-toplevel"])?
        .trim()
        .to_string();
    let repo_root_path = PathBuf::from(repo_root);
    let stash_enabled = cmd.stash && !cmd.no_stash;
    let has_changes = !git_capture_in(&repo_root_path, &["status", "--porcelain"])?
        .trim()
        .is_empty();
    let mut stashed = false;

    if stash_enabled && has_changes {
        let message = format!(
            "flow-checkout-{}-{}",
            sanitize_checkout_label(target),
            Utc::now().format("%Y%m%d-%H%M%S")
        );
        println!("==> Stashing local changes...");
        if let Err(err) = git_run_in(&repo_root_path, &["stash", "push", "-u", "-m", &message]) {
            eprintln!(
                "warning: auto-stash failed, continuing without stash: {}",
                err
            );
        } else {
            stashed = true;
        }
    } else if has_changes && !stash_enabled {
        println!("==> Continuing with local changes (auto-stash disabled)...");
    }

    let checkout_result = (|| -> Result<()> {
        ensure_gh_available()?;
        let gh_args = build_gh_pr_checkout_args(target, cmd.remote.as_deref());
        println!("==> Running: gh {}", gh_args.join(" "));
        run_gh_in(&repo_root_path, &gh_args)?;

        if should_use_jj(&repo_root_path) {
            println!("==> Importing git refs into jj...");
            if let Err(err) = jj_run_preferred_in(&repo_root_path, &["--quiet", "git", "import"]) {
                eprintln!(
                    "warning: jj import failed after checkout: {}\nGit checkout succeeded.",
                    err
                );
            }
        }

        Ok(())
    })();

    if let Err(err) = checkout_result {
        if stashed {
            eprintln!("==> Restoring stashed changes after failed checkout...");
            let _ = git_run_in(&repo_root_path, &["stash", "pop"]);
        }
        return Err(err);
    }

    if stashed {
        println!("==> Restoring stashed changes...");
        if let Err(err) = git_run_in(&repo_root_path, &["stash", "pop"]) {
            eprintln!(
                "warning: failed to restore stash automatically: {}\nRun `git stash list` and restore manually if needed.",
                err
            );
        }
    }

    let current = git_capture_in(&repo_root_path, &["rev-parse", "--abbrev-ref", "HEAD"])
        .unwrap_or_else(|_| "HEAD".to_string())
        .trim()
        .to_string();
    println!("✓ Checked out {}", current);
    Ok(())
}

fn ensure_branch_attached(repo_root: &Path, target_branch: &str) -> Result<()> {
    let current = git_capture_in(repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .unwrap_or_else(|_| "HEAD".to_string());
    let current = current.trim();

    if current == target_branch {
        return Ok(());
    }

    println!(
        "==> Re-attaching Git checkout to {} (current: {})...",
        target_branch, current
    );
    git_run_in(repo_root, &["switch", target_branch]).with_context(|| {
        format!(
            "sync finished but could not switch back to '{}'; run `git switch {}` manually",
            target_branch, target_branch
        )
    })?;
    Ok(())
}

fn resolve_tracking_remote_and_fetch(
    repo_root: &Path,
    branch: &str,
    preferred_remote: Option<&str>,
) -> Result<Option<String>> {
    let mut candidates: Vec<String> = Vec::new();
    if let Some(remote) = preferred_remote.map(str::trim).filter(|s| !s.is_empty()) {
        candidates.push(remote.to_string());
    }
    for remote in ["upstream", "origin"] {
        if !candidates.iter().any(|r| r == remote) {
            candidates.push(remote.to_string());
        }
    }

    for remote in candidates {
        if !remote_exists(repo_root, &remote) {
            continue;
        }
        println!("==> Fetching {}...", remote);
        let args = vec!["fetch", remote.as_str(), "--prune"];
        if let Err(err) = git_run_in(repo_root, &args) {
            if preferred_remote.map(|r| r.trim()) == Some(remote.as_str()) {
                return Err(err);
            }
            continue;
        }
        if remote_branch_exists(repo_root, &remote, branch) {
            return Ok(Some(remote));
        }
    }

    Ok(None)
}

fn sync_local_upstream_branch(repo_root: &Path, branch: &str) -> Result<()> {
    let remote_ref = format!("refs/remotes/upstream/{}", branch);
    if !git_ref_exists_in(repo_root, &remote_ref) {
        return Ok(());
    }
    let upstream_ref = format!("upstream/{}", branch);
    let current = git_capture_in(repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .unwrap_or_else(|_| "HEAD".to_string());
    if git_ref_exists_in(repo_root, "refs/heads/upstream") {
        if current.trim() == "upstream" {
            git_run_in(repo_root, &["reset", "--hard", &upstream_ref])?;
        } else {
            git_run_in(repo_root, &["branch", "-f", "upstream", &upstream_ref])?;
        }
    } else {
        git_run_in(repo_root, &["branch", "upstream", &upstream_ref])?;
    }
    Ok(())
}

fn remote_exists(repo_root: &Path, remote: &str) -> bool {
    git_capture_in(repo_root, &["remote", "get-url", remote]).is_ok()
}

fn sanitize_checkout_label(input: &str) -> String {
    let mut out = String::new();
    let mut last_sep = false;
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
            out.push(ch);
            last_sep = false;
        } else if !last_sep {
            out.push('-');
            last_sep = true;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "target".to_string()
    } else {
        trimmed.chars().take(32).collect()
    }
}

fn parse_github_pr_url(target: &str) -> Option<(String, String)> {
    let raw = target.trim().trim_end_matches('/');
    let rest = raw
        .strip_prefix("https://github.com/")
        .or_else(|| raw.strip_prefix("http://github.com/"))?;
    let parts: Vec<&str> = rest.split('/').collect();
    if parts.len() < 4 {
        return None;
    }
    if parts.get(2).copied() != Some("pull") {
        return None;
    }
    let owner = parts[0].trim();
    let repo = parts[1].trim();
    let number = parts[3].trim();
    if owner.is_empty() || repo.is_empty() || number.is_empty() {
        return None;
    }
    if !number.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((format!("{}/{}", owner, repo), number.to_string()))
}

fn build_gh_pr_checkout_args(target: &str, preferred_remote: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = vec!["pr".to_string(), "checkout".to_string()];
    if let Some((repo, number)) = parse_github_pr_url(target) {
        args.push(number);
        args.push("--repo".to_string());
        args.push(repo);
    } else {
        args.push(target.to_string());
    }
    if let Some(remote) = preferred_remote.map(str::trim).filter(|s| !s.is_empty()) {
        args.push("--remote".to_string());
        args.push(remote.to_string());
    }
    args
}

fn ensure_gh_available() -> Result<()> {
    let status = Command::new("gh")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("failed to run gh --version")?;
    if !status.success() {
        bail!("GitHub CLI (`gh`) is required for `f checkout`");
    }
    Ok(())
}

fn run_gh_in(repo_root: &Path, args: &[String]) -> Result<()> {
    let status = Command::new("gh")
        .current_dir(repo_root)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("failed to run gh {}", args.join(" ")))?;
    if !status.success() {
        bail!("gh {} failed with status {}", args.join(" "), status);
    }
    Ok(())
}

fn remote_branch_exists(repo_root: &Path, remote: &str, branch: &str) -> bool {
    let reference = format!("refs/remotes/{}/{}", remote, branch);
    git_ref_exists_in(repo_root, &reference)
}

fn git_ref_exists_in(repo_root: &Path, reference: &str) -> bool {
    git_capture_in(repo_root, &["rev-parse", "--verify", reference]).is_ok()
}

fn run_jj_sync(
    repo_root: &Path,
    cmd: &SyncCommand,
    auto_fix: bool,
    recorder: &mut SyncRecorder,
) -> Result<()> {
    // Avoid git operations in progress.
    if is_rebase_in_progress() || is_merge_in_progress() {
        bail!("Git operation in progress. Run `f git-repair` first.");
    }
    let unmerged = git_capture(&["diff", "--name-only", "--diff-filter=U"]).unwrap_or_default();
    if !unmerged.trim().is_empty() {
        bail!("Unmerged files detected. Resolve them before syncing.");
    }

    let head_ref = git_capture_in(repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .unwrap_or_else(|_| "HEAD".to_string());
    let head_ref = head_ref.trim();
    let current_branch = if head_ref == "HEAD" || head_ref.is_empty() {
        recorder.record("jj", "detached head (ignored, using default branch)");
        jj_default_branch(repo_root)
    } else {
        head_ref.to_string()
    };

    let has_origin = git_capture_in(repo_root, &["remote", "get-url", "origin"]).is_ok();
    let has_upstream = git_capture_in(repo_root, &["remote", "get-url", "upstream"]).is_ok();
    let origin_reachable = has_origin
        && git_capture_in(repo_root, &["ls-remote", "--exit-code", "-q", "origin"]).is_ok();
    let should_push = sync_should_push(cmd);

    // Keep jj fetch output small. In most workflows, only the current branch + upstream trunk are
    // needed for a sync/rebase.
    let mut upstream_branch_opt = resolve_upstream_branch_in(repo_root, Some(&current_branch));
    let upstream_branch_for_fetch = upstream_branch_opt
        .clone()
        .unwrap_or_else(|| jj_default_branch(repo_root));

    if has_origin || has_upstream {
        println!("==> Fetching remotes via jj...");
        let mut fetched_any = false;
        let mut failures: Vec<String> = Vec::new();

        if has_origin && origin_reachable {
            recorder.record(
                "jj",
                format!("jj git fetch --remote origin --branch {}", current_branch),
            );
            if let Err(err) = jj_run_in(
                repo_root,
                &[
                    "--quiet",
                    "git",
                    "fetch",
                    "--remote",
                    "origin",
                    "--branch",
                    &current_branch,
                ],
            ) {
                failures.push(format!("origin: {}", err));
            } else {
                fetched_any = true;
            }
        } else if has_origin {
            recorder.record("jj", "skip origin (unreachable)");
        }

        if has_upstream {
            recorder.record(
                "jj",
                format!(
                    "jj git fetch --remote upstream --branch {}",
                    upstream_branch_for_fetch
                ),
            );
            if let Err(primary_err) = jj_run_in(
                repo_root,
                &[
                    "--quiet",
                    "git",
                    "fetch",
                    "--remote",
                    "upstream",
                    "--branch",
                    &upstream_branch_for_fetch,
                ],
            ) {
                recorder.record(
                    "jj",
                    format!(
                        "jj git fetch upstream branch {} failed, retrying full fetch",
                        upstream_branch_for_fetch
                    ),
                );
                if let Err(fallback_err) = jj_run_in(
                    repo_root,
                    &["--quiet", "git", "fetch", "--remote", "upstream"],
                ) {
                    failures.push(format!(
                        "upstream: {} (fallback failed: {})",
                        primary_err, fallback_err
                    ));
                } else {
                    fetched_any = true;
                }
            } else {
                fetched_any = true;
            }
        }

        if fetched_any {
            recorder.record("jj", "jj git import");
            let _ = jj_run_in(repo_root, &["--quiet", "git", "import"]);
            // Re-resolve after fetch/import so we can pick up newly discovered upstream refs.
            upstream_branch_opt = resolve_upstream_branch_in(repo_root, Some(&current_branch));
        } else if !failures.is_empty() {
            bail!("jj git fetch failed: {}", failures.join(", "));
        }
    }

    let origin_url = git_capture_in(repo_root, &["remote", "get-url", "origin"]).unwrap_or_default();
    let upstream_url =
        git_capture_in(repo_root, &["remote", "get-url", "upstream"]).unwrap_or_default();
    let is_read_only =
        has_upstream && normalize_git_url(&origin_url) == normalize_git_url(&upstream_url);

    let mut dest_ref: Option<String> = None;
    if has_upstream {
        if let Some(branch) = upstream_branch_opt {
            dest_ref = Some(format!("{}@upstream", branch));
        }
    }

    if dest_ref.is_none() && has_origin {
        let remote = jj_default_remote(repo_root);
        dest_ref = Some(format!("{}@{}", current_branch, remote));
    }

    let mut did_rebase = false;
    let mut did_stash_commits = false;
    let mut needs_git_export = false;
    if let Some(dest) = dest_ref.clone() {
        let has_branch_bookmark = jj_bookmark_exists(repo_root, &current_branch);

        if cmd.stash_commits {
            if jj_has_divergence(repo_root, &current_branch, &dest)? {
                let stash_name = jj_stash_commits(repo_root, &current_branch, &dest)?;
                println!("==> Stashed local JJ commits to {}", stash_name);
                recorder.record("stash", format!("jj stash {}", stash_name));
                recorder.set_stashed(true);
                did_rebase = true;
                did_stash_commits = true;
                needs_git_export = true;
            }
        }

        if !did_stash_commits {
            if has_branch_bookmark {
                if jj_has_divergence(repo_root, &current_branch, &dest)? {
                    println!(
                        "==> Rebasing branch {} with jj onto {}...",
                        current_branch, dest
                    );
                    recorder.record("jj", format!("jj rebase -b {} -d {}", current_branch, dest));
                    if let Err(err) =
                        jj_run_in(repo_root, &["rebase", "-b", &current_branch, "-d", &dest])
                    {
                        recorder.record("jj", "jj branch rebase failed");
                        if !is_read_only {
                            println!(
                                "==> Rebase blocked by immutable commits; retrying with --ignore-immutable..."
                            );
                            recorder.record("jj", "jj branch rebase retry --ignore-immutable");
                            jj_run_in(
                                repo_root,
                                &[
                                    "rebase",
                                    "--ignore-immutable",
                                    "-b",
                                    &current_branch,
                                    "-d",
                                    &dest,
                                ],
                            )?;
                        } else {
                            return Err(err);
                        }
                    }
                    did_rebase = true;
                    needs_git_export = true;
                } else {
                    println!("==> Fast-forwarding {} to {}...", current_branch, dest);
                    recorder.record(
                        "jj",
                        format!("jj bookmark set {} -r {}", current_branch, dest),
                    );
                    jj_run_in(
                        repo_root,
                        &["bookmark", "set", &current_branch, "-r", &dest],
                    )?;
                    needs_git_export = true;
                }

                // After syncing the branch bookmark, also rebase the working
                // copy onto the new destination so files reflect latest state.
                recorder.record("jj", format!("jj rebase -d {} (working copy)", dest));
                match jj_capture_in(repo_root, &["rebase", "-d", &dest]) {
                    Ok(_) => {
                        println!("==> Rebased working copy onto {}", dest);
                        did_rebase = true;
                    }
                    Err(_) => {
                        // Non-fatal: working copy may already be at destination
                    }
                }
            } else {
                println!("==> Rebasing with jj onto {}...", dest);
                recorder.record("jj", format!("jj rebase -d {}", dest));
                if let Err(err) = jj_run_in(repo_root, &["rebase", "-d", &dest]) {
                    recorder.record("jj", "jj rebase failed");
                    if !is_read_only {
                        println!(
                            "==> Rebase blocked by immutable commits; retrying with --ignore-immutable..."
                        );
                        recorder.record("jj", "jj rebase retry --ignore-immutable");
                        jj_run_in(repo_root, &["rebase", "--ignore-immutable", "-d", &dest])?;
                    } else {
                        return Err(err);
                    }
                }
                did_rebase = true;
            }

            if commit::commit_queue_has_entries(repo_root) {
                if let Ok(updated) = commit::refresh_commit_queue(repo_root) {
                    if updated > 0 {
                        recorder.record("queue", format!("refreshed {} queued commits", updated));
                        println!("==> Updated {} queued commit(s) after rebase", updated);
                    }
                }
            }
        }

        if needs_git_export {
            recorder.record("jj", "jj git export");
            jj_run_in(repo_root, &["--quiet", "git", "export"])?;
        }
    } else {
        println!("==> No remotes configured, skipping rebase");
        recorder.record("jj", "skipped (no remotes)");
    }

    if has_origin && should_push {
        if is_read_only {
            println!("==> Skipping push (origin == upstream, read-only clone)");
            println!("  To push, create a fork first: gh repo fork --remote");
            recorder.record("push", "skipped (origin == upstream)");
        } else if !origin_reachable {
            if cmd.create_repo {
                println!("==> Creating origin repo...");
                if try_create_origin_repo()? {
                    println!("==> Pushing to origin...");
                    git_run(&["push", "-u", "origin", &current_branch])?;
                    recorder.record("push", "created repo and pushed to origin");
                } else {
                    println!("  Could not create repo, skipping push");
                    recorder.record("push", "skipped (create repo failed)");
                }
            } else {
                println!("==> Origin unreachable, skipping push");
                println!("  The remote may be missing, private, or auth/network failed.");
                println!("  Use --create-repo if origin does not exist yet.");
                recorder.record("push", "skipped (origin unreachable)");
            }
        } else {
            println!("==> Pushing to origin...");
            let push_result = if did_rebase {
                push_with_autofix_force(&current_branch, auto_fix, cmd.max_fix_attempts)
            } else {
                push_with_autofix(&current_branch, auto_fix, cmd.max_fix_attempts)
            };
            if let Err(e) = push_result {
                recorder.record("push", "push failed");
                return Err(e);
            }
            recorder.record("push", "push complete");
        }
    } else if cmd.no_push {
        recorder.record("push", "skipped (--no-push)");
    } else if has_origin {
        recorder.record("push", "skipped (default; use --push)");
    } else {
        recorder.record("push", "skipped (no origin)");
    }

    println!("\n✓ Sync complete (jj)!");
    recorder.record("complete", "sync complete (jj)");
    Ok(())
}

/// Sync from upstream remote into current branch.
fn sync_upstream_internal(
    repo_root: &Path,
    current_branch: &str,
    auto_fix: bool,
    recorder: &mut SyncRecorder,
) -> Result<()> {
    // Fetch upstream
    git_run_in(repo_root, &["fetch", "upstream", "--prune"])?;
    recorder.record("upstream", "fetched upstream");

    // Determine upstream branch
    let upstream_branch = match resolve_upstream_branch_in(repo_root, Some(current_branch)) {
        Some(branch) => branch,
        None => {
            println!("  Cannot determine upstream branch, skipping upstream sync");
            recorder.record("upstream", "skipped (cannot determine upstream branch)");
            return Ok(());
        }
    };

    let upstream_ref = format!("upstream/{}", upstream_branch);

    // Update local upstream branch if it exists
    let local_upstream_exists = git_capture_in(repo_root, &["rev-parse", "--verify", "refs/heads/upstream"]).is_ok();
    if local_upstream_exists {
        git_run_in(repo_root, &["branch", "-f", "upstream", &upstream_ref])?;
    }

    // Check if current branch is behind upstream
    let behind = git_capture_in(
        repo_root,
        &[
            "rev-list",
            "--count",
            &format!("{}..{}", current_branch, upstream_ref),
        ],
    )
    .ok()
    .and_then(|s| s.trim().parse::<u32>().ok())
    .unwrap_or(0);

    if behind > 0 {
        println!("  Merging {} commits from upstream...", behind);
        recorder.record(
            "upstream",
            format!("merging {} commits from upstream", behind),
        );

        // Try fast-forward first
        if git_run_in(repo_root, &["merge", "--ff-only", &upstream_ref]).is_err() {
            // Fall back to regular merge
            if let Err(_) = git_run_in(repo_root, &["merge", &upstream_ref, "--no-edit"]) {
                // Merge failed - check for conflicts
                let should_fix = auto_fix || prompt_for_auto_fix()?;
                if should_fix {
                    println!("  Attempting auto-fix...");
                    if try_resolve_conflicts()? {
                        // Conflicts resolved, commit
                        let _ = git_run_in(repo_root, &["add", "-A"]);
                        let _ = Command::new("git")
                            .current_dir(repo_root)
                            .args(["commit", "--no-edit"])
                            .output();
                        println!("  ✓ Conflicts auto-resolved");
                        recorder.record("upstream", "conflicts auto-resolved");
                        return Ok(());
                    }
                }
                recorder.record("upstream", "merge conflicts unresolved");
                bail!(
                    "Merge conflicts with upstream. Resolve manually:\n  git status\n  # fix conflicts\n  git add . && git commit"
                );
            }
            recorder.record("upstream", "merged upstream with commit");
        } else {
            recorder.record("upstream", "fast-forwarded to upstream");
        }
    } else {
        println!("  Already up to date with upstream");
        recorder.record("upstream", "already up to date");
    }

    Ok(())
}

fn parse_branch_merge_ref(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.trim_start_matches("refs/heads/").to_string())
}

fn list_upstream_remote_branches(repo_root: &Path) -> Vec<String> {
    let output = git_capture_in(
        repo_root,
        &[
            "for-each-ref",
            "--format=%(refname:short)",
            "refs/remotes/upstream",
        ],
    )
    .unwrap_or_default();
    let mut branches = Vec::new();
    for line in output.lines() {
        let value = line.trim();
        if value.is_empty() || value == "upstream/HEAD" {
            continue;
        }
        if let Some(rest) = value.strip_prefix("upstream/") {
            if !rest.is_empty() {
                branches.push(rest.to_string());
            }
        }
    }
    branches.sort();
    branches.dedup();
    branches
}

fn resolve_upstream_branch_in(repo_root: &Path, current_branch: Option<&str>) -> Option<String> {
    let current_branch = current_branch
        .map(str::trim)
        .filter(|value| !value.is_empty());

    if let Some(branch) = current_branch {
        if let Ok(remote) = git_capture_in(
            repo_root,
            &["config", "--get", &format!("branch.{}.remote", branch)],
        ) {
            if remote.trim() == "upstream" {
                if let Ok(merge_ref) = git_capture_in(
                    repo_root,
                    &["config", "--get", &format!("branch.{}.merge", branch)],
                ) {
                    if let Some(parsed) = parse_branch_merge_ref(&merge_ref) {
                        return Some(parsed);
                    }
                }
            }
        }
    }

    if let Ok(merge_ref) = git_capture_in(repo_root, &["config", "--get", "branch.upstream.merge"])
    {
        if let Some(parsed) = parse_branch_merge_ref(&merge_ref) {
            return Some(parsed);
        }
    }
    if let Ok(head_ref) = git_capture_in(repo_root, &["symbolic-ref", "refs/remotes/upstream/HEAD"])
    {
        let parsed = head_ref.trim().replace("refs/remotes/upstream/", "");
        if !parsed.is_empty() {
            return Some(parsed);
        }
    }

    if let Some(branch) = current_branch {
        let reference = format!("refs/remotes/upstream/{}", branch);
        if git_ref_exists_in(repo_root, &reference) {
            return Some(branch.to_string());
        }
    }

    let remote_branches = list_upstream_remote_branches(repo_root);
    for candidate in ["main", "master", "dev", "trunk"] {
        if remote_branches.iter().any(|b| b == candidate) {
            return Some(candidate.to_string());
        }
    }

    remote_branches.into_iter().next()
}

fn should_use_jj(repo_root: &Path) -> bool {
    let jj_dir = repo_root.join(".jj");
    if !jj_dir.exists() {
        return false;
    }
    let status = Command::new("jj")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    status.map(|s| s.success()).unwrap_or(false)
}

fn jj_default_remote(repo_root: &Path) -> String {
    let local_config = repo_root.join("flow.toml");
    if local_config.exists() {
        if let Ok(cfg) = config::load(&local_config) {
            if let Some(jj_cfg) = cfg.jj {
                if let Some(remote) = jj_cfg.remote {
                    return remote;
                }
            }
        }
    }

    let global_config = config::default_config_path();
    if global_config.exists() {
        if let Ok(cfg) = config::load(&global_config) {
            if let Some(jj_cfg) = cfg.jj {
                if let Some(remote) = jj_cfg.remote {
                    return remote;
                }
            }
        }
    }

    "origin".to_string()
}

fn jj_default_branch(repo_root: &Path) -> String {
    let local_config = repo_root.join("flow.toml");
    if local_config.exists() {
        if let Ok(cfg) = config::load(&local_config) {
            if let Some(jj_cfg) = cfg.jj {
                if let Some(branch) = jj_cfg.default_branch {
                    return branch;
                }
            }
        }
    }

    let global_config = config::default_config_path();
    if global_config.exists() {
        if let Ok(cfg) = config::load(&global_config) {
            if let Some(jj_cfg) = cfg.jj {
                if let Some(branch) = jj_cfg.default_branch {
                    return branch;
                }
            }
        }
    }

    if git_ref_exists("refs/heads/main") || git_ref_exists("refs/remotes/origin/main") {
        return "main".to_string();
    }
    if git_ref_exists("refs/heads/master") || git_ref_exists("refs/remotes/origin/master") {
        return "master".to_string();
    }

    "main".to_string()
}

fn git_ref_exists(reference: &str) -> bool {
    git_capture(&["rev-parse", "--verify", reference]).is_ok()
}

fn jj_run_in(repo_root: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new("jj")
        .current_dir(repo_root)
        .args(args)
        .status()
        .with_context(|| format!("failed to run jj {}", args.join(" ")))?;
    if !status.success() {
        bail!("jj {} failed", args.join(" "));
    }
    Ok(())
}

fn jj_preferred_binary() -> std::path::PathBuf {
    if let Ok(path) = std::env::var("FLOW_JJ_BIN") {
        let candidate = PathBuf::from(path);
        if candidate.exists() {
            return candidate;
        }
    }

    if let Ok(home) = std::env::var("HOME") {
        let local_dev_jj = PathBuf::from(home).join("repos/jj-vcs/jj/target/release/jj");
        if local_dev_jj.exists() {
            return local_dev_jj;
        }
    }

    PathBuf::from("jj")
}

fn jj_run_preferred_in(repo_root: &Path, args: &[&str]) -> Result<()> {
    let jj_bin = jj_preferred_binary();
    let output = Command::new(&jj_bin)
        .current_dir(repo_root)
        .args(args)
        .output()
        .with_context(|| format!("failed to run {} {}", jj_bin.display(), args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let concise = stderr
            .lines()
            .chain(stdout.lines())
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("jj command failed");
        bail!(
            "{} {} failed: {}",
            jj_bin.display(),
            args.join(" "),
            concise
        );
    }
    Ok(())
}

fn jj_bookmark_exists(repo_root: &Path, name: &str) -> bool {
    let output = jj_capture_in(repo_root, &["bookmark", "list"]).unwrap_or_default();
    output
        .lines()
        .any(|line| line.trim_start().starts_with(name))
}

fn jj_capture_in(repo_root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("jj")
        .current_dir(repo_root)
        .args(args)
        .output()
        .with_context(|| format!("failed to run jj {}", args.join(" ")))?;
    if !output.status.success() {
        bail!("jj {} failed", args.join(" "));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn jj_has_divergence(repo_root: &Path, current: &str, dest: &str) -> Result<bool> {
    let revset = format!("{}..{}", dest, current);
    let output = jj_capture_in(
        repo_root,
        &["log", "-r", &revset, "--no-graph", "-T", "commit_id"],
    )?;
    Ok(!output.trim().is_empty())
}

fn jj_stash_commits(repo_root: &Path, current: &str, dest: &str) -> Result<String> {
    let ts = Utc::now().format("%Y%m%d-%H%M%S").to_string();
    let stash_name = format!("f-sync-stash/{}/{}", current, ts);
    jj_run_in(
        repo_root,
        &["bookmark", "create", &stash_name, "-r", current],
    )?;
    jj_run_in(repo_root, &["bookmark", "set", current, "-r", dest])?;
    jj_run_in(repo_root, &["edit", current])?;
    Ok(stash_name)
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

        // Load sync context if available
        let context = ai_context::load_command_context("sync").unwrap_or_default();
        let context_section = if !context.is_empty() {
            format!("## Context\n\n{}\n\n", context)
        } else {
            String::new()
        };

        let prompt = format!(
            "{}This file has git merge conflicts. Resolve them by keeping the best of both versions. Output ONLY the resolved file content, no explanations:\n\n{}",
            context_section,
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

    // Load sync context once for all files
    let context = ai_context::load_command_context("sync").unwrap_or_default();
    let context_section = if !context.is_empty() {
        format!("## Context\n\n{}\n\n", context)
    } else {
        String::new()
    };

    for file in &needs_claude {
        let content = std::fs::read_to_string(file).unwrap_or_default();
        if content.contains("<<<<<<<") {
            let prompt = format!(
                "{}This file has git merge conflicts. Resolve them by keeping the best of both versions. Output ONLY the resolved file content, no explanations:\n\n{}",
                context_section,
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

/// Run a git command in a specific repository and capture stdout.
fn git_capture_in(repo_root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .current_dir(repo_root)
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

/// Run a git command in a specific repository with inherited stdio.
fn git_run_in(repo_root: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .current_dir(repo_root)
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

        // Run Claude to fix the errors (fallback to opencode glm if Claude fails)
        let mut fixed = try_claude_fix(&combined)?;
        if !fixed {
            println!("  Claude fix failed; trying opencode glm...");
            fixed = try_opencode_fix(&combined)?;
        }
        if !fixed {
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

/// Push to origin with --force-with-lease, with optional auto-fix on failure.
fn push_with_autofix_force(branch: &str, auto_fix: bool, max_attempts: u32) -> Result<()> {
    let mut attempts = 0;

    loop {
        let output = Command::new("git")
            .args(["push", "--force-with-lease", "origin", branch])
            .output()
            .context("failed to run git push --force-with-lease")?;

        if output.status.success() {
            return Ok(());
        }

        attempts += 1;
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let combined = format!("{}\n{}", stdout, stderr);

        let is_hook_failure = combined.contains("pre-push")
            || combined.contains("husky")
            || combined.contains("typecheck")
            || combined.contains("error TS")
            || combined.contains("eslint")
            || combined.contains("error:")
            || combined.contains("failed to push");

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
            bail!("git push --force-with-lease origin {} failed", branch);
        }

        println!(
            "\n==> Push failed (attempt {}/{}), attempting auto-fix with Claude...",
            attempts, max_attempts
        );

        let mut fixed = try_claude_fix(&combined)?;
        if !fixed {
            println!("  Claude fix failed; trying opencode glm...");
            fixed = try_opencode_fix(&combined)?;
        }

        if fixed {
            println!("  Changes applied. Retrying push...");
        } else {
            bail!("auto-fix failed; push still failing");
        }
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

    let prompt = build_fix_prompt(error_output);

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

fn try_opencode_fix(error_output: &str) -> Result<bool> {
    let opencode_check = Command::new("which").arg("opencode").output();
    if opencode_check.is_err() || !opencode_check.unwrap().status.success() {
        println!("  opencode CLI not found. Install with: npm i -g opencode");
        return Ok(false);
    }

    let prompt = build_fix_prompt(error_output);
    let mut child = Command::new("opencode")
        .args(["run", "-m", "opencode/glm-4.7-free", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("failed to run opencode")?;

    if let Some(stdin) = child.stdin.as_mut() {
        use std::io::Write;
        stdin
            .write_all(prompt.as_bytes())
            .context("failed to write opencode prompt")?;
    }

    let status = child.wait().context("failed to wait on opencode")?;
    Ok(status.success())
}

fn build_fix_prompt(error_output: &str) -> String {
    let excerpt = if error_output.len() > 4000 {
        &error_output[error_output.len() - 4000..]
    } else {
        error_output
    };
    format!(
        "Fix these errors so the code compiles/passes checks. Make minimal changes. Do not explain, just fix:\n\n{}",
        excerpt
    )
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

fn sync_log_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join("code").join("org").join("linsa").join("base"));
        dirs.push(home.join("code").join("org").join("1f").join("jazz"));
    }
    dirs
}

fn write_sync_snapshot(snapshot: &SyncSnapshot) -> Result<()> {
    let payload = serde_json::to_string(snapshot)?;
    for base in sync_log_dirs() {
        let target_dir = base.join("sync");
        if !target_dir.exists() {
            if let Err(err) = fs::create_dir_all(&target_dir) {
                eprintln!(
                    "warn: unable to create sync log dir {}: {}",
                    target_dir.display(),
                    err
                );
                continue;
            }
        }
        let log_path = target_dir.join("flow-sync.jsonl");
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("open sync log {}", log_path.display()))?;
        writeln!(file, "{}", payload)?;
    }
    Ok(())
}
