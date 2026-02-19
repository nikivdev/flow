//! Git sync command - comprehensive repo synchronization.
//!
//! Provides a single command to sync a git repository:
//! - Pull from tracking/default remote (with rebase)
//! - Sync upstream if configured (fetch, merge)
//! - Push to configured git remote (default: origin)

use std::env;
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
use serde::{Deserialize, Serialize};

use crate::ai_context;
use crate::cli::{CheckoutCommand, SwitchCommand, SyncCommand};
use crate::commit;
use crate::config;
use crate::git_guard;
use crate::push;
use crate::secret_redact;
use crate::todo;

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

/// Check the review-todo push gate. Returns `true` if push should proceed.
/// Only P1+P2 items trigger the gate; P3/P4 are non-blocking.
/// Reads `[commit].review-push-gate` from config: "warn" (default) | "block" | "off".
/// `--allow-review-issues` overrides any mode.
/// Fails open if todos can't be loaded.
fn check_review_todo_push_gate(
    repo_root: &Path,
    allow_review_issues: bool,
    recorder: &mut SyncRecorder,
) -> bool {
    if allow_review_issues {
        return true;
    }

    let (p1, p2, _p3, _p4, _total) = match todo::count_open_review_todos_by_priority(repo_root) {
        Ok(counts) => counts,
        Err(_) => return true, // fail open
    };

    let blocking = p1 + p2;
    if blocking == 0 {
        return true;
    }

    // Read gate mode from config
    let config_path = repo_root.join("flow.toml");
    let gate_mode = if config_path.exists() {
        config::load(&config_path)
            .ok()
            .and_then(|cfg| cfg.commit)
            .and_then(|c| c.review_push_gate)
            .unwrap_or_else(|| "warn".to_string())
    } else {
        "warn".to_string()
    };

    match gate_mode.as_str() {
        "off" => true,
        "block" => {
            eprintln!(
                "✗ Push blocked: {} open review todos (P1:{}, P2:{})",
                blocking, p1, p2
            );
            eprintln!(
                "  Resolve with `f reviews-todo list` or use --allow-review-issues to override."
            );
            recorder.record("review-gate", format!("blocked (P1:{}, P2:{})", p1, p2));
            false
        }
        _ => {
            // "warn" (default)
            eprintln!(
                "⚠  {} open review todos (P1:{}, P2:{}) — consider reviewing before push",
                blocking, p1, p2
            );
            eprintln!("  Run `f reviews-todo list` to see details.");
            recorder.record("review-gate", format!("warned (P1:{}, P2:{})", p1, p2));
            true
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
        let preferred_remote = config::preferred_git_remote_for_repo(repo_root_path);
        let mut use_jj = should_use_jj(repo_root_path);
        let mut jj_disabled_by_custom_tracking = false;
        if use_jj && preferred_remote != "origin" && preferred_remote != "upstream" {
            println!(
                "⚠️  Configured git.remote '{}' detected; using git sync flow.",
                preferred_remote
            );
            recorder.record(
                "mode",
                format!("jj bypassed (configured git.remote {})", preferred_remote),
            );
            use_jj = false;
        }
        if use_jj {
            if let Ok(branch) =
                git_capture_in(repo_root_path, &["rev-parse", "--abbrev-ref", "HEAD"])
            {
                let branch = branch.trim();
                if branch != "HEAD" {
                    if let Some((remote, _)) =
                        resolve_tracking_remote_branch_in(repo_root_path, Some(branch))
                    {
                        if remote != "origin" && remote != "upstream" {
                            println!(
                                "⚠️  Tracking remote '{}' detected; using git sync flow for reliable branch + upstream sync.",
                                remote
                            );
                            recorder.record(
                                "mode",
                                format!("jj bypassed (custom tracking remote {})", remote),
                            );
                            use_jj = false;
                            jj_disabled_by_custom_tracking = true;
                        }
                    }
                }
            }
        }
        if repo_root_path.join(".jj").exists() && !use_jj && !jj_disabled_by_custom_tracking {
            println!("⚠️  jj workspace appears unhealthy; falling back to git sync flow.");
            println!(
                "   Fix: `jj git import` (or if still broken: `rm -rf .jj && jj git init --colocate`)"
            );
            recorder.record("mode", "jj unavailable/unhealthy; fallback to git");
        }
        let current_branch_for_queue = resolve_sync_branch_for_queue_guard(repo_root_path);
        let queue_present = match current_branch_for_queue.as_deref() {
            Some(branch) => commit::commit_queue_has_entries_on_branch(repo_root_path, branch),
            None => commit::commit_queue_has_entries_reachable_from_head(repo_root_path),
        };
        if queue_present && !cmd.allow_queue && (cmd.rebase || use_jj) {
            recorder.record("queue", "blocked (commit queue present)");
            bail!(
                "Commit queue is not empty. Rebase-based sync can rewrite commit SHAs.\n\
Use `f commit-queue list` to review, or re-run with `--allow-queue`."
            );
        }

        if use_jj {
            recorder.record("mode", "using jj sync flow");
            match run_jj_sync(repo_root_path, &cmd, auto_fix, &mut recorder) {
                Ok(()) => return Ok(()),
                Err(err) if is_jj_corruption_error(&err) => {
                    println!("⚠️  jj sync failed due workspace/store issues; retrying with git.");
                    println!(
                        "   Fix: `jj git import` (or if still broken: `rm -rf .jj && jj git init --colocate`)"
                    );
                    recorder.record("mode", "jj failed (corruption); fallback to git");
                }
                Err(err) => return Err(err),
            }
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

            if let Err(err) = git_run(&["stash", "push", "-u", "-m", "f sync auto-stash"]) {
                recorder.record("stash", format!("stash failed: {}", err));
                bail!(
                    "Failed to stash local changes: {}. Resolve the issue and re-run sync.",
                    err
                );
            }

            let stash_count_after = git_capture(&["stash", "list"])
                .map(|s| s.lines().count())
                .unwrap_or(0);
            stashed = stash_count_after > stash_count_before;
        }
        recorder.set_stashed(stashed);
        if has_changes && cmd.stash {
            recorder.record("stash", format!("stashed={}", stashed));
        }

        // Resolve remotes for sync.
        let push_remote = preferred_remote.clone();
        let has_push_remote = git_capture(&["remote", "get-url", &push_remote]).is_ok();

        // Keep explicit origin/upstream detection for fork-sync heuristics.
        let has_origin = git_capture(&["remote", "get-url", "origin"]).is_ok();
        let has_upstream = git_capture(&["remote", "get-url", "upstream"]).is_ok();

        // Check if remotes are reachable (repo exists on remote)
        let push_remote_reachable = has_push_remote
            && git_capture(&["ls-remote", "--exit-code", "-q", &push_remote]).is_ok();
        let origin_reachable =
            has_origin && git_capture(&["ls-remote", "--exit-code", "-q", "origin"]).is_ok();

        // Step 1: Pull from tracking branch.
        let mut tracking = resolve_tracking_remote_branch_in(repo_root_path, Some(current));
        if current != "HEAD" && has_push_remote {
            let should_retarget = tracking
                .as_ref()
                .map(|(remote, _)| remote != &push_remote)
                .unwrap_or(true);
            if should_retarget && remote_branch_exists(repo_root_path, &push_remote, current) {
                let branch_remote_key = format!("branch.{}.remote", current);
                let branch_merge_key = format!("branch.{}.merge", current);
                let merge_ref = format!("refs/heads/{}", current);
                let _ = git_run_in(
                    repo_root_path,
                    &["config", "--local", &branch_remote_key, &push_remote],
                );
                let _ = git_run_in(
                    repo_root_path,
                    &["config", "--local", &branch_merge_key, &merge_ref],
                );
                tracking = Some((push_remote.clone(), current.to_string()));
            }
        }

        if let Some((tracking_remote, tracking_branch)) = tracking {
            let tracking_reachable = git_capture_in(
                repo_root_path,
                &["ls-remote", "--exit-code", "-q", &tracking_remote],
            )
            .is_ok();
            if tracking_reachable {
                println!(
                    "==> Pulling from {}/{}...",
                    tracking_remote, tracking_branch
                );
                recorder.record(
                    "pull",
                    format!(
                        "pulling from {}/{} (rebase={})",
                        tracking_remote, tracking_branch, cmd.rebase
                    ),
                );
                if cmd.rebase {
                    let pull = Command::new("git")
                        .current_dir(repo_root_path)
                        .args([
                            "pull",
                            "--rebase",
                            tracking_remote.as_str(),
                            tracking_branch.as_str(),
                        ])
                        .output()
                        .context("failed to run git pull --rebase")?;
                    if !pull.status.success() {
                        let pull_stdout = String::from_utf8_lossy(&pull.stdout);
                        let pull_stderr = String::from_utf8_lossy(&pull.stderr);
                        let pull_text = format!("{}\n{}", pull_stdout, pull_stderr).to_lowercase();
                        if pull_text.contains("cannot rebase: you have unstaged changes")
                            || pull_text
                                .contains("cannot pull with rebase: you have unstaged changes")
                        {
                            let _ = Command::new("git")
                                .current_dir(repo_root_path)
                                .args(["rebase", "--abort"])
                                .output();
                            restore_stash(repo_root_path, stashed);
                            recorder.record("pull", "blocked by unstaged changes");
                            bail!(
                                "git pull --rebase refused due unstaged changes. \
Clean local file conflicts/case-only path conflicts, then re-run `f sync`."
                            );
                        }
                        // Check if we're in a rebase conflict
                        if is_rebase_in_progress() {
                            let should_fix = auto_fix || prompt_for_auto_fix()?;
                            if should_fix {
                                if try_resolve_rebase_conflicts()? {
                                    println!("  ✓ Rebase conflicts auto-resolved");
                                    recorder.record("pull", "rebase conflicts auto-resolved");
                                } else {
                                    restore_stash(repo_root_path, stashed);
                                    recorder.record("pull", "rebase conflicts unresolved");
                                    bail!(
                                        "Rebase conflicts. Resolve manually:\n  git status\n  # fix conflicts\n  git add . && git rebase --continue"
                                    );
                                }
                            } else {
                                restore_stash(repo_root_path, stashed);
                                recorder.record("pull", "rebase conflicts unresolved");
                                bail!(
                                    "Rebase conflicts. Resolve manually:\n  git status\n  # fix conflicts\n  git add . && git rebase --continue"
                                );
                            }
                        } else {
                            restore_stash(repo_root_path, stashed);
                            recorder.record("pull", "git pull --rebase failed");
                            bail!("git pull --rebase failed");
                        }
                    }
                } else {
                    if let Err(_) = git_run_in(
                        repo_root_path,
                        &[
                            "pull",
                            "--no-rebase",
                            tracking_remote.as_str(),
                            tracking_branch.as_str(),
                        ],
                    ) {
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
                                    restore_stash(repo_root_path, stashed);
                                    recorder.record("pull", "merge conflicts unresolved");
                                    bail!(
                                        "Merge conflicts. Resolve manually:\n  git status\n  # fix conflicts\n  git add . && git commit"
                                    );
                                }
                            } else {
                                restore_stash(repo_root_path, stashed);
                                recorder.record("pull", "merge conflicts unresolved");
                                bail!(
                                    "Merge conflicts. Resolve manually:\n  git status\n  # fix conflicts\n  git add . && git commit"
                                );
                            }
                        } else {
                            restore_stash(repo_root_path, stashed);
                            recorder.record("pull", "git pull failed");
                            bail!("git pull failed");
                        }
                    }
                }
                recorder.record("pull", "pull complete");
            } else {
                println!(
                    "==> Tracking remote '{}' unreachable, skipping pull",
                    tracking_remote
                );
                recorder.record(
                    "pull",
                    format!("skipped (tracking remote unreachable: {})", tracking_remote),
                );
            }
        } else {
            println!("==> No tracking branch, skipping pull");
            recorder.record("pull", "skipped (no tracking branch)");
        }

        // Step 2: Sync upstream if it exists. If no upstream remote is configured and we're on
        // a feature branch, fall back to syncing from origin's default branch.
        if has_upstream {
            println!("==> Syncing upstream...");
            recorder.record("upstream", "syncing upstream");
            if let Err(e) = sync_upstream_internal(repo_root_path, current, auto_fix, &mut recorder)
            {
                restore_stash(repo_root_path, stashed);
                return Err(e);
            }
        } else if has_origin && origin_reachable {
            if let Some(default_branch) =
                origin_default_branch_for_feature_sync(repo_root_path, current)
            {
                println!("==> Syncing origin/{} into {}...", default_branch, current);
                recorder.record(
                    "upstream",
                    format!("syncing origin/{} into {}", default_branch, current),
                );
                if let Err(e) = sync_origin_default_internal(
                    repo_root_path,
                    current,
                    &default_branch,
                    auto_fix,
                    &mut recorder,
                ) {
                    restore_stash(repo_root_path, stashed);
                    return Err(e);
                }
            } else {
                recorder.record("upstream", "skipped (no upstream remote)");
            }
        } else {
            recorder.record("upstream", "skipped (no upstream remote)");
        }

        // Step 3: Push to configured remote (defaults to origin).
        // Fork push override: redirect to private fork remote if configured.
        let (mut push_remote, mut has_push_remote, mut push_remote_reachable) =
            (push_remote, has_push_remote, push_remote_reachable);
        if should_push {
            if let Some((fork_remote, fork_owner, fork_repo)) =
                resolve_fork_push_target(repo_root_path)
            {
                let target_url = push::build_github_ssh_url(&fork_owner, &fork_repo);
                if let Err(e) = push::ensure_remote_points_to_target(
                    repo_root_path,
                    &fork_remote,
                    &target_url,
                    None,
                    true,
                ) {
                    eprintln!("Warning: could not set up fork remote: {}", e);
                } else {
                    push::ensure_github_repo_exists(&fork_owner, &fork_repo).ok();
                    println!(
                        "==> Fork push enabled: {}/{}  (remote: {})",
                        fork_owner, fork_repo, fork_remote
                    );
                    push_remote = fork_remote;
                    has_push_remote = true;
                    push_remote_reachable = true;
                }
            }
        }

        // Review-todo push gate (git sync path)
        if should_push
            && !check_review_todo_push_gate(repo_root_path, cmd.allow_review_issues, &mut recorder)
        {
            recorder.record("push", "blocked by review-todo gate");
            bail!("Push blocked by open review todos. Use --allow-review-issues to override.");
        }

        if has_push_remote && should_push {
            // Check if push remote == upstream (read-only clone, no fork)
            let push_remote_url =
                git_capture(&["remote", "get-url", &push_remote]).unwrap_or_default();
            let upstream_url = git_capture(&["remote", "get-url", "upstream"]).unwrap_or_default();
            let is_read_only = has_upstream
                && normalize_git_url(&push_remote_url) == normalize_git_url(&upstream_url);

            if is_read_only {
                println!(
                    "==> Skipping push (remote '{}' == upstream, read-only clone)",
                    push_remote
                );
                println!("  To push, create a fork first: gh repo fork --remote");
                recorder.record("push", "skipped (push remote == upstream)");
            } else if !push_remote_reachable {
                // Remote repo doesn't exist or is unreachable.
                if cmd.create_repo && push_remote == "origin" {
                    println!("==> Creating origin repo...");
                    if try_create_origin_repo()? {
                        println!("==> Pushing to {}...", push_remote);
                        git_run(&["push", "-u", &push_remote, current])?;
                        recorder.record(
                            "push",
                            format!("created repo and pushed to {}", push_remote),
                        );
                    } else {
                        println!("  Could not create repo, skipping push");
                        recorder.record("push", "skipped (create repo failed)");
                    }
                } else {
                    println!("==> Remote '{}' unreachable, skipping push", push_remote);
                    println!("  The remote may be missing, private, or auth/network failed.");
                    if push_remote == "origin" {
                        println!("  Use --create-repo if origin does not exist yet.");
                    } else {
                        println!(
                            "  Create/fix remote '{}' and re-run sync (or set [git].remote).",
                            push_remote
                        );
                    }
                    recorder.record(
                        "push",
                        format!("skipped (remote unreachable: {})", push_remote),
                    );
                }
            } else {
                println!("==> Pushing to {}...", push_remote);
                let push_result =
                    push_with_autofix(current, &push_remote, auto_fix, cmd.max_fix_attempts);
                if let Err(e) = push_result {
                    restore_stash(repo_root_path, stashed);
                    recorder.record("push", "push failed");
                    return Err(e);
                }
                recorder.record("push", "push complete");
            }
        } else if cmd.no_push {
            recorder.record("push", "skipped (--no-push)");
        } else if has_push_remote {
            recorder.record("push", "skipped (default; use --push)");
        } else {
            recorder.record("push", format!("skipped (missing remote: {})", push_remote));
        }

        // Restore stash
        restore_stash(repo_root_path, stashed);
        if stashed {
            recorder.record("stash", "stash restored");
        }

        // Explain new commits if configured
        let head_after_sha = git_capture(&["rev-parse", "HEAD"])
            .unwrap_or_default()
            .trim()
            .to_string();
        if recorder.head_before != head_after_sha {
            if let Err(e) =
                crate::explain_commits::maybe_run_after_sync(repo_root_path, &recorder.head_before)
            {
                eprintln!("warn: commit explanation failed: {e}");
            }
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

    let target_input = cmd.branch.trim();
    if target_input.is_empty() {
        bail!("Branch name cannot be empty");
    }

    let repo_root = git_capture(&["rev-parse", "--show-toplevel"])?
        .trim()
        .to_string();
    let repo_root_path = PathBuf::from(repo_root);
    let resolution = resolve_switch_target(&repo_root_path, target_input)?;
    let target_branch = resolution.branch;
    git_guard::ensure_clean_for_push(&repo_root_path)?;

    let stash_enabled = cmd.stash && !cmd.no_stash;
    let has_changes = !git_capture_in(&repo_root_path, &["status", "--porcelain"])?
        .trim()
        .is_empty();
    let current_branch = git_capture_in(&repo_root_path, &["rev-parse", "--abbrev-ref", "HEAD"])
        .unwrap_or_else(|_| "HEAD".to_string())
        .trim()
        .to_string();
    let mut stashed = false;

    let preserve_enabled = cmd.preserve && !cmd.no_preserve;
    if preserve_enabled && current_branch != "HEAD" && current_branch != target_branch {
        let preserve_reason = switch_preserve_reason(
            &repo_root_path,
            &current_branch,
            &target_branch,
            has_changes,
        );
        if let Some(reason) = preserve_reason {
            let snapshot_name = create_switch_safety_snapshot(&repo_root_path, &current_branch);
            match snapshot_name {
                Some(snapshot) => {
                    println!(
                        "==> Preserved branch '{}' as '{}' ({})",
                        current_branch, snapshot, reason
                    );
                }
                None => {
                    eprintln!(
                        "warning: failed to create safety snapshot for '{}'; continuing switch",
                        current_branch
                    );
                }
            }
        }
    }

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

    let mut switched_branch = target_branch.clone();
    let switch_result = (|| -> Result<()> {
        let tracking_remote = resolve_tracking_remote_and_fetch(
            &repo_root_path,
            &switched_branch,
            cmd.remote.as_deref(),
        )?;

        if git_ref_exists_in(&repo_root_path, &format!("refs/heads/{}", switched_branch)) {
            println!("==> Switching to local branch {}...", switched_branch);
            git_run_in(&repo_root_path, &["switch", &switched_branch])?;
        } else if let Some(remote) = tracking_remote.as_deref() {
            println!(
                "==> Creating {} from {}/{}...",
                switched_branch, remote, switched_branch
            );
            let remote_branch = format!("{}/{}", remote, switched_branch);
            git_run_in(
                &repo_root_path,
                &["switch", "-c", &switched_branch, "--track", &remote_branch],
            )?;
        } else if let Some(pr_target) = resolution.pr.as_ref() {
            println!(
                "==> Branch '{}' not found on remotes; checking out {} via gh...",
                switched_branch, pr_target.display
            );
            ensure_gh_available()?;
            let gh_args =
                build_gh_pr_checkout_args(&pr_target.checkout_target, cmd.remote.as_deref());
            run_gh_in(&repo_root_path, &gh_args)?;
            let checked_out =
                git_capture_in(&repo_root_path, &["rev-parse", "--abbrev-ref", "HEAD"])
                    .unwrap_or_else(|_| "HEAD".to_string())
                    .trim()
                    .to_string();
            if checked_out.is_empty() || checked_out == "HEAD" {
                bail!(
                    "checked out {}, but git is detached; please run `gh pr checkout {}` manually",
                    pr_target.display,
                    pr_target.checkout_target
                );
            }
            switched_branch = checked_out;
        } else {
            let preferred = cmd.remote.as_deref().unwrap_or("upstream/origin");
            bail!(
                "Branch '{}' not found locally or on remotes (searched: {}).",
                switched_branch,
                preferred
            );
        }

        if remote_branch_exists(&repo_root_path, "upstream", &switched_branch) {
            println!(
                "==> Updating flow upstream tracking to upstream/{}...",
                switched_branch
            );
            git_run_in(
                &repo_root_path,
                &["config", "branch.upstream.remote", "upstream"],
            )?;
            let merge_ref = format!("refs/heads/{}", switched_branch);
            git_run_in(
                &repo_root_path,
                &["config", "branch.upstream.merge", &merge_ref],
            )?;
            sync_local_upstream_branch(&repo_root_path, &switched_branch)?;
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

    println!("✓ Switched to {}", switched_branch);

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
            allow_review_issues: false,
            compact: false,
        }) {
            let _ = ensure_branch_attached(&repo_root_path, &switched_branch);
            return Err(sync_err);
        }
        ensure_branch_attached(&repo_root_path, &switched_branch)?;
    }

    Ok(())
}

#[derive(Debug, Clone)]
struct SwitchPrTarget {
    checkout_target: String,
    display: String,
}

#[derive(Debug, Clone)]
struct SwitchTargetResolution {
    branch: String,
    pr: Option<SwitchPrTarget>,
}

#[derive(Debug, Deserialize)]
struct SwitchPrView {
    #[serde(rename = "headRefName")]
    head_ref_name: String,
}

fn resolve_switch_target(repo_root: &Path, target: &str) -> Result<SwitchTargetResolution> {
    let trimmed = target.trim();
    if let Some((repo, number)) = parse_github_pr_url(trimmed) {
        ensure_gh_available()?;
        let branch = resolve_pr_head_branch(repo_root, &number, Some(&repo))?;
        println!("==> Resolved PR {repo}#{number} to branch {branch}");
        return Ok(SwitchTargetResolution {
            branch,
            pr: Some(SwitchPrTarget {
                checkout_target: number.clone(),
                display: format!("{repo}#{number}"),
            }),
        });
    }

    let pr_number = if let Some(stripped) = trimmed.strip_prefix('#') {
        stripped.trim()
    } else {
        trimmed
    };
    if !pr_number.is_empty() && pr_number.chars().all(|c| c.is_ascii_digit()) {
        ensure_gh_available()?;
        let branch = resolve_pr_head_branch(repo_root, pr_number, None)?;
        println!("==> Resolved PR #{pr_number} to branch {branch}");
        return Ok(SwitchTargetResolution {
            branch,
            pr: Some(SwitchPrTarget {
                checkout_target: pr_number.to_string(),
                display: format!("#{pr_number}"),
            }),
        });
    }

    Ok(SwitchTargetResolution {
        branch: trimmed.to_string(),
        pr: None,
    })
}

fn resolve_pr_head_branch(repo_root: &Path, number: &str, repo: Option<&str>) -> Result<String> {
    let mut args: Vec<String> = vec![
        "pr".to_string(),
        "view".to_string(),
        number.to_string(),
        "--json".to_string(),
        "headRefName".to_string(),
    ];
    if let Some(repo) = repo.map(str::trim).filter(|s| !s.is_empty()) {
        args.push("--repo".to_string());
        args.push(repo.to_string());
    }

    let ref_args: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = gh_capture_in(repo_root, &ref_args)?;
    let parsed: SwitchPrView = serde_json::from_str(out.trim())
        .with_context(|| format!("failed to parse gh pr view JSON for #{number}"))?;
    let branch = parsed.head_ref_name.trim();
    if branch.is_empty() {
        bail!("PR #{} returned empty head branch", number);
    }
    Ok(branch.to_string())
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

fn switch_preserve_reason(
    repo_root: &Path,
    current_branch: &str,
    target_branch: &str,
    has_changes: bool,
) -> Option<String> {
    if has_changes {
        return Some("uncommitted changes present".to_string());
    }

    if switch_branch_has_commits_not_in_target(repo_root, current_branch, target_branch) {
        return Some(format!("contains commits not in {}", target_branch));
    }

    if let Some((tracking_remote, tracking_branch)) =
        resolve_tracking_remote_branch_in(repo_root, Some(current_branch))
    {
        let tracking_ref = format!("{}/{}", tracking_remote, tracking_branch);
        let tracking_git_ref = format!("refs/remotes/{}", tracking_ref);
        if !git_ref_exists_in(repo_root, &tracking_git_ref) {
            return Some(format!("tracking ref {} not fetched", tracking_ref));
        }

        let ahead = git_capture_in(
            repo_root,
            &[
                "rev-list",
                "--count",
                &format!("{}..{}", tracking_ref, current_branch),
            ],
        )
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(0);

        if ahead > 0 {
            return Some(format!(
                "ahead of tracking {} by {} commit(s)",
                tracking_ref, ahead
            ));
        }
    }

    None
}

fn switch_branch_has_commits_not_in_target(
    repo_root: &Path,
    current_branch: &str,
    target_branch: &str,
) -> bool {
    let target_ref = if git_ref_exists_in(repo_root, &format!("refs/heads/{}", target_branch)) {
        target_branch.to_string()
    } else if git_ref_exists_in(
        repo_root,
        &format!("refs/remotes/upstream/{}", target_branch),
    ) {
        format!("upstream/{}", target_branch)
    } else if git_ref_exists_in(repo_root, &format!("refs/remotes/origin/{}", target_branch)) {
        format!("origin/{}", target_branch)
    } else {
        return true;
    };

    git_capture_in(
        repo_root,
        &[
            "rev-list",
            "--count",
            &format!("{}..{}", target_ref, current_branch),
        ],
    )
    .ok()
    .and_then(|v| v.trim().parse::<u32>().ok())
    .map(|count| count > 0)
    .unwrap_or(true)
}

fn create_switch_safety_snapshot(repo_root: &Path, current_branch: &str) -> Option<String> {
    let snapshot_base = format!(
        "f-switch-save/{}-{}",
        sanitize_checkout_label(current_branch),
        Utc::now().format("%Y%m%d-%H%M%S")
    );
    let use_jj = should_use_jj(repo_root);
    let mut snapshot_name = snapshot_base.clone();
    let mut suffix = 1;

    loop {
        let git_exists = git_ref_exists_in(repo_root, &format!("refs/heads/{}", snapshot_name));
        let jj_exists = use_jj && jj_bookmark_exists(repo_root, &snapshot_name);
        if !git_exists && !jj_exists {
            break;
        }
        snapshot_name = format!("{}-{}", snapshot_base, suffix);
        suffix += 1;
    }

    if git_run_in(repo_root, &["branch", &snapshot_name, current_branch]).is_err() {
        return None;
    }

    if use_jj {
        if let Err(err) = jj_bookmark_create_or_set(repo_root, &snapshot_name, current_branch) {
            eprintln!(
                "warning: created git snapshot '{}' but failed to create/update jj bookmark: {}",
                snapshot_name, err
            );
        }
    }

    Some(snapshot_name)
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

fn gh_capture_in(repo_root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("gh")
        .current_dir(repo_root)
        .args(args)
        .output()
        .with_context(|| format!("failed to run gh {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "gh {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn remote_branch_exists(repo_root: &Path, remote: &str, branch: &str) -> bool {
    let reference = format!("refs/remotes/{}/{}", remote, branch);
    git_ref_exists_in(repo_root, &reference)
}

fn git_ref_exists_in(repo_root: &Path, reference: &str) -> bool {
    git_capture_in(repo_root, &["rev-parse", "--verify", reference]).is_ok()
}

#[derive(Clone, Debug)]
struct TrackedRemoteRef {
    remote: String,
    branch: String,
    before_tip: Option<String>,
}

fn track_remote_ref(
    tracked: &mut Vec<TrackedRemoteRef>,
    repo_root: &Path,
    remote: &str,
    branch: &str,
) {
    if remote.trim().is_empty() || branch.trim().is_empty() {
        return;
    }
    if tracked
        .iter()
        .any(|item| item.remote == remote && item.branch == branch)
    {
        return;
    }
    tracked.push(TrackedRemoteRef {
        remote: remote.to_string(),
        branch: branch.to_string(),
        before_tip: remote_branch_tip(repo_root, remote, branch),
    });
}

fn remote_branch_tip(repo_root: &Path, remote: &str, branch: &str) -> Option<String> {
    let reference = format!("refs/remotes/{}/{}", remote, branch);
    git_capture_in(repo_root, &["rev-parse", "--verify", &reference])
        .ok()
        .map(|out| out.trim().to_string())
        .filter(|sha| !sha.is_empty())
}

fn short_commit_id(sha: &str) -> &str {
    let trimmed = sha.trim();
    if trimmed.len() <= 8 {
        trimmed
    } else {
        &trimmed[..8]
    }
}

fn print_fetched_remote_commits(
    repo_root: &Path,
    tracked: &[TrackedRemoteRef],
    recorder: &mut SyncRecorder,
    compact: bool,
) {
    let mut printed_header = false;
    for item in tracked {
        let Some(after_tip) = remote_branch_tip(repo_root, &item.remote, &item.branch) else {
            continue;
        };
        if item.before_tip.as_deref() == Some(after_tip.as_str()) {
            continue;
        }

        if !printed_header {
            println!("==> Fetched remote commits:");
            printed_header = true;
        }

        let label = format!("{}/{}", item.remote, item.branch);
        match item.before_tip.as_deref() {
            None => {
                println!("  {} -> {}", label, short_commit_id(&after_tip));
                recorder.record(
                    "jj",
                    format!("fetched {} at {}", label, short_commit_id(&after_tip)),
                );
            }
            Some(before_tip) => {
                let range = format!("{}..{}", before_tip, after_tip);
                let lines = git_capture_in(
                    repo_root,
                    &["log", "--oneline", "--no-decorate", "--reverse", &range],
                )
                .unwrap_or_default();
                let commits: Vec<&str> = lines
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .collect();
                if commits.is_empty() {
                    println!(
                        "  {} updated {} -> {}",
                        label,
                        short_commit_id(before_tip),
                        short_commit_id(&after_tip)
                    );
                    recorder.record(
                        "jj",
                        format!(
                            "fetched {} {} -> {}",
                            label,
                            short_commit_id(before_tip),
                            short_commit_id(&after_tip)
                        ),
                    );
                } else {
                    if compact {
                        println!("  {} (+{})", label, commits.len());
                    } else {
                        println!("  {} (+{}):", label, commits.len());
                        for line in commits.iter().take(8) {
                            println!("    {}", line);
                        }
                        if commits.len() > 8 {
                            println!("    ... +{} more", commits.len() - 8);
                        }
                    }
                    recorder.record(
                        "jj",
                        format!("fetched {} (+{} commits)", label, commits.len()),
                    );
                }
            }
        }
    }
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

    let push_remote = config::preferred_git_remote_for_repo(repo_root);
    let has_push_remote = git_capture_in(repo_root, &["remote", "get-url", &push_remote]).is_ok();
    let push_remote_reachable = has_push_remote
        && git_capture_in(repo_root, &["ls-remote", "--exit-code", "-q", &push_remote]).is_ok();
    let has_origin = git_capture_in(repo_root, &["remote", "get-url", "origin"]).is_ok();
    let has_upstream = git_capture_in(repo_root, &["remote", "get-url", "upstream"]).is_ok();
    let origin_reachable = has_origin
        && git_capture_in(repo_root, &["ls-remote", "--exit-code", "-q", "origin"]).is_ok();
    let should_push = sync_should_push(cmd);
    let origin_default_branch = if !has_upstream && has_origin && origin_reachable {
        origin_default_branch_for_feature_sync(repo_root, &current_branch)
    } else {
        None
    };

    // Keep jj fetch output small. In most workflows, only the current branch + upstream trunk are
    // needed for a sync/rebase.
    let mut upstream_branch_opt = resolve_upstream_branch_in(repo_root, Some(&current_branch));
    let upstream_branch_for_fetch = upstream_branch_opt
        .clone()
        .unwrap_or_else(|| jj_default_branch(repo_root));

    let mut tracked_refs: Vec<TrackedRemoteRef> = Vec::new();
    if has_origin || has_upstream {
        println!("==> Fetching remotes via jj...");
        let mut fetched_any = false;
        let mut failures: Vec<String> = Vec::new();

        if has_origin && origin_reachable {
            track_remote_ref(&mut tracked_refs, repo_root, "origin", &current_branch);
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
            if let Some(default_branch) = origin_default_branch.as_deref() {
                track_remote_ref(&mut tracked_refs, repo_root, "origin", default_branch);
                recorder.record(
                    "jj",
                    format!("jj git fetch --remote origin --branch {}", default_branch),
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
                        default_branch,
                    ],
                ) {
                    failures.push(format!("origin default {}: {}", default_branch, err));
                } else {
                    fetched_any = true;
                }
            }
        } else if has_origin {
            recorder.record("jj", "skip origin (unreachable)");
        }

        if has_push_remote
            && push_remote != "origin"
            && push_remote != "upstream"
            && push_remote_reachable
        {
            track_remote_ref(&mut tracked_refs, repo_root, &push_remote, &current_branch);
            recorder.record(
                "jj",
                format!(
                    "jj git fetch --remote {} --branch {}",
                    push_remote, current_branch
                ),
            );
            if let Err(err) = jj_run_in(
                repo_root,
                &[
                    "--quiet",
                    "git",
                    "fetch",
                    "--remote",
                    &push_remote,
                    "--branch",
                    &current_branch,
                ],
            ) {
                failures.push(format!("{}: {}", push_remote, err));
            } else {
                fetched_any = true;
            }
        } else if has_push_remote
            && push_remote != "origin"
            && push_remote != "upstream"
            && !push_remote_reachable
        {
            recorder.record("jj", format!("skip {} (unreachable)", push_remote));
        }

        if has_upstream {
            track_remote_ref(
                &mut tracked_refs,
                repo_root,
                "upstream",
                &upstream_branch_for_fetch,
            );
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
            print_fetched_remote_commits(repo_root, &tracked_refs, recorder, cmd.compact);
            // Re-resolve after fetch/import so we can pick up newly discovered upstream refs.
            upstream_branch_opt = resolve_upstream_branch_in(repo_root, Some(&current_branch));
        } else if !failures.is_empty() {
            bail!("jj git fetch failed: {}", failures.join(", "));
        }
    }

    let push_remote_url =
        git_capture_in(repo_root, &["remote", "get-url", &push_remote]).unwrap_or_default();
    let upstream_url =
        git_capture_in(repo_root, &["remote", "get-url", "upstream"]).unwrap_or_default();
    let is_read_only =
        has_upstream && normalize_git_url(&push_remote_url) == normalize_git_url(&upstream_url);

    let mut dest_ref: Option<String> = None;
    if has_upstream {
        if let Some(branch) = upstream_branch_opt {
            dest_ref = Some(format!("{}@upstream", branch));
        }
    } else if let Some(default_branch) = origin_default_branch {
        dest_ref = Some(format!("{}@origin", default_branch));
    }

    if dest_ref.is_none() && has_push_remote {
        dest_ref = Some(format!("{}@{}", current_branch, push_remote));
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
                    let preempt_ignore_immutable = !is_read_only
                        && branch_tip_matches_remote(repo_root, &current_branch, &push_remote);
                    if preempt_ignore_immutable {
                        recorder.record(
                            "jj",
                            format!(
                                "branch {} matches {}/{}; preemptively using --ignore-immutable",
                                current_branch, push_remote, current_branch
                            ),
                        );
                    }
                    let initial_rebase_args: Vec<&str> = if preempt_ignore_immutable {
                        vec![
                            "rebase",
                            "--ignore-immutable",
                            "-b",
                            &current_branch,
                            "-d",
                            &dest,
                        ]
                    } else {
                        vec!["rebase", "-b", &current_branch, "-d", &dest]
                    };
                    recorder.record(
                        "jj",
                        if preempt_ignore_immutable {
                            format!(
                                "jj rebase --ignore-immutable -b {} -d {}",
                                current_branch, dest
                            )
                        } else {
                            format!("jj rebase -b {} -d {}", current_branch, dest)
                        },
                    );
                    if let Err(err) = jj_run_in(repo_root, &initial_rebase_args) {
                        recorder.record("jj", "jj branch rebase failed");
                        if !preempt_ignore_immutable && !is_read_only {
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
                    recorder.record(
                        "jj",
                        format!("jj bookmark set {} -r {}", current_branch, dest),
                    );
                    let ff_output = jj_capture_in(
                        repo_root,
                        &["bookmark", "set", &current_branch, "-r", &dest],
                    )?;
                    let ff_trimmed = ff_output.trim();
                    if ff_trimmed.is_empty()
                        || ff_trimmed.contains("Nothing changed")
                        || ff_trimmed.contains("nothing changed")
                    {
                        println!("  {} already up to date with {}", current_branch, dest);
                    } else {
                        println!("==> Fast-forwarded {} to {}", current_branch, dest);
                    }
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

            // After jj git export, git HEAD may be detached (or on a jj/keep/ ref)
            // because the jj working copy commit isn't on any bookmark. Re-attach
            // HEAD to the current branch so the user's shell prompt stays sane.
            let git_head = git_capture_in(repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])
                .unwrap_or_default();
            let git_head = git_head.trim();
            if git_head == "HEAD" || git_head.starts_with("jj/keep/") {
                if git_ref_exists_in(repo_root, &format!("refs/heads/{}", current_branch)) {
                    let branch_sha = git_capture_in(
                        repo_root,
                        &["rev-parse", &format!("refs/heads/{}", current_branch)],
                    )
                    .unwrap_or_default();
                    let branch_sha = branch_sha.trim();
                    if !branch_sha.is_empty() {
                        // Point HEAD at the branch symbolically, then reset to its tip.
                        let _ = git_run_in(
                            repo_root,
                            &[
                                "symbolic-ref",
                                "HEAD",
                                &format!("refs/heads/{}", current_branch),
                            ],
                        );
                        let _ = git_run_in(repo_root, &["reset", "--mixed", "--quiet", branch_sha]);
                        recorder.record("jj", format!("re-attached HEAD to {}", current_branch));
                    }
                }
            }
        }
    } else {
        println!("==> No remotes configured, skipping rebase");
        recorder.record("jj", "skipped (no remotes)");
    }

    // Fork push override: redirect to private fork remote if configured.
    let (mut push_remote, mut has_push_remote, mut push_remote_reachable, mut is_read_only) = (
        push_remote,
        has_push_remote,
        push_remote_reachable,
        is_read_only,
    );
    if should_push {
        if let Some((fork_remote, fork_owner, fork_repo)) = resolve_fork_push_target(repo_root) {
            let target_url = push::build_github_ssh_url(&fork_owner, &fork_repo);
            if let Err(e) = push::ensure_remote_points_to_target(
                repo_root,
                &fork_remote,
                &target_url,
                None,
                true,
            ) {
                eprintln!("Warning: could not set up fork remote: {}", e);
            } else {
                push::ensure_github_repo_exists(&fork_owner, &fork_repo).ok();
                // Let jj know about the new remote.
                let _ = jj_capture_in(repo_root, &["git", "fetch", "--remote", &fork_remote]);
                println!(
                    "==> Fork push enabled: {}/{}  (remote: {})",
                    fork_owner, fork_repo, fork_remote
                );
                push_remote = fork_remote;
                has_push_remote = true;
                push_remote_reachable = true;
                is_read_only = false;
            }
        }
    }

    // Review-todo push gate (jj sync path)
    if should_push && !check_review_todo_push_gate(repo_root, cmd.allow_review_issues, recorder) {
        recorder.record("push", "blocked by review-todo gate");
        bail!("Push blocked by open review todos. Use --allow-review-issues to override.");
    }

    if has_push_remote && should_push {
        if is_read_only {
            println!(
                "==> Skipping push (remote '{}' == upstream, read-only clone)",
                push_remote
            );
            println!("  To push, create a fork first: gh repo fork --remote");
            recorder.record("push", "skipped (push remote == upstream)");
        } else if !push_remote_reachable {
            if cmd.create_repo && push_remote == "origin" {
                println!("==> Creating origin repo...");
                if try_create_origin_repo()? {
                    println!("==> Pushing to {}...", push_remote);
                    git_run(&["push", "-u", &push_remote, &current_branch])?;
                    recorder.record(
                        "push",
                        format!("created repo and pushed to {}", push_remote),
                    );
                } else {
                    println!("  Could not create repo, skipping push");
                    recorder.record("push", "skipped (create repo failed)");
                }
            } else {
                println!("==> Remote '{}' unreachable, skipping push", push_remote);
                println!("  The remote may be missing, private, or auth/network failed.");
                if push_remote == "origin" {
                    println!("  Use --create-repo if origin does not exist yet.");
                } else {
                    println!(
                        "  Create/fix remote '{}' and re-run sync (or set [git].remote).",
                        push_remote
                    );
                }
                recorder.record(
                    "push",
                    format!("skipped (remote unreachable: {})", push_remote),
                );
            }
        } else {
            println!("==> Pushing to {}...", push_remote);
            let push_result = if did_rebase {
                push_with_autofix_force(
                    &current_branch,
                    &push_remote,
                    auto_fix,
                    cmd.max_fix_attempts,
                )
            } else {
                push_with_autofix(
                    &current_branch,
                    &push_remote,
                    auto_fix,
                    cmd.max_fix_attempts,
                )
            };
            if let Err(e) = push_result {
                recorder.record("push", "push failed");
                return Err(e);
            }
            recorder.record("push", "push complete");
        }
    } else if cmd.no_push {
        recorder.record("push", "skipped (--no-push)");
    } else if has_push_remote {
        recorder.record("push", "skipped (default; use --push)");
    } else {
        recorder.record("push", format!("skipped (missing remote: {})", push_remote));
    }

    // Check for jj conflicts left after rebase
    let has_conflicts = jj_capture_in(
        repo_root,
        &["log", "-r", "conflicts()", "--no-graph", "-T", "commit_id"],
    )
    .map(|out| !out.trim().is_empty())
    .unwrap_or(false);

    if has_conflicts {
        let conflict_details =
            jj_capture_in(repo_root, &["log", "-r", "conflicts()", "--no-graph"])
                .unwrap_or_default();
        println!("\n⚠ Sync complete (jj) but conflicts remain:");
        for line in conflict_details.lines().filter(|l| !l.trim().is_empty()) {
            println!("  {}", line.trim());
        }
        println!("\nResolve with: jj resolve");
        recorder.record("complete", "sync complete (jj) with conflicts");
    } else {
        println!("\n✓ Sync complete (jj)!");
        recorder.record("complete", "sync complete (jj)");
    }
    Ok(())
}

/// Sync from upstream remote into current branch.
fn sync_upstream_internal(
    repo_root: &Path,
    current_branch: &str,
    auto_fix: bool,
    recorder: &mut SyncRecorder,
) -> Result<()> {
    // Fetch upstream — tolerate case-insensitive ref collisions (macOS)
    let fetch = Command::new("git")
        .current_dir(repo_root)
        .args(["fetch", "upstream", "--prune"])
        .output()
        .context("failed to run git fetch upstream")?;
    if !fetch.status.success() {
        let stderr = String::from_utf8_lossy(&fetch.stderr);
        if stderr.contains("case-insensitive filesystem") {
            eprintln!(
                "  Warning: upstream has refs that differ only in case; fetch continued anyway"
            );
        } else {
            bail!("git fetch upstream --prune failed: {}", stderr.trim());
        }
    }
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

    // Update local upstream branch if it exists
    let local_upstream_exists =
        git_capture_in(repo_root, &["rev-parse", "--verify", "refs/heads/upstream"]).is_ok();
    if local_upstream_exists {
        let upstream_ref = format!("upstream/{}", upstream_branch);
        git_run_in(repo_root, &["branch", "-f", "upstream", &upstream_ref])?;
    }

    merge_remote_branch_into_current(
        repo_root,
        "upstream",
        &upstream_branch,
        current_branch,
        auto_fix,
        recorder,
        "upstream",
    )
}

fn sync_origin_default_internal(
    repo_root: &Path,
    current_branch: &str,
    origin_default_branch: &str,
    auto_fix: bool,
    recorder: &mut SyncRecorder,
) -> Result<()> {
    let refspec = format!(
        "+refs/heads/{}:refs/remotes/origin/{}",
        origin_default_branch, origin_default_branch
    );
    git_run_in(repo_root, &["fetch", "origin", "--prune", &refspec])?;
    recorder.record(
        "upstream",
        format!("fetched origin {}", origin_default_branch),
    );

    merge_remote_branch_into_current(
        repo_root,
        "origin",
        origin_default_branch,
        current_branch,
        auto_fix,
        recorder,
        "origin-default",
    )
}

fn merge_remote_branch_into_current(
    repo_root: &Path,
    remote: &str,
    remote_branch: &str,
    current_branch: &str,
    auto_fix: bool,
    recorder: &mut SyncRecorder,
    stage: &str,
) -> Result<()> {
    let remote_ref = format!("{}/{}", remote, remote_branch);
    let behind = git_capture_in(
        repo_root,
        &[
            "rev-list",
            "--count",
            &format!("{}..{}", current_branch, remote_ref),
        ],
    )
    .ok()
    .and_then(|s| s.trim().parse::<u32>().ok())
    .unwrap_or(0);

    if behind == 0 {
        println!("  Already up to date with {}", remote_ref);
        recorder.record(stage, format!("already up to date with {}", remote_ref));
        return Ok(());
    }

    println!("  Merging {} commits from {}...", behind, remote_ref);
    recorder.record(
        stage,
        format!("merging {} commits from {}", behind, remote_ref),
    );

    match git_run_in(repo_root, &["merge", "--ff-only", &remote_ref]) {
        Ok(()) => {
            recorder.record(stage, format!("fast-forwarded to {}", remote_ref));
            return Ok(());
        }
        Err(err) if is_git_index_lock_error(&err.to_string()) => {
            bail!(
                "Git index lock detected during merge. Remove stale .git/index.lock (if no git process is running) and re-run."
            );
        }
        Err(_) => {}
    }

    match git_run_in(repo_root, &["merge", &remote_ref, "--no-edit"]) {
        Ok(()) => {
            recorder.record(stage, format!("merged {} with commit", remote_ref));
            return Ok(());
        }
        Err(err) if is_git_index_lock_error(&err.to_string()) => {
            bail!(
                "Git index lock detected during merge. Remove stale .git/index.lock (if no git process is running) and re-run."
            );
        }
        Err(_) => {}
    }

    let should_fix = auto_fix || prompt_for_auto_fix()?;
    if should_fix {
        println!("  Attempting auto-fix...");
        if try_resolve_conflicts()? {
            let _ = git_run_in(repo_root, &["add", "-A"]);
            let _ = Command::new("git")
                .current_dir(repo_root)
                .args(["commit", "--no-edit"])
                .output();
            println!("  ✓ Conflicts auto-resolved");
            recorder.record(stage, "conflicts auto-resolved");
            return Ok(());
        }
    }

    recorder.record(stage, "merge conflicts unresolved");
    bail!(
        "Merge conflicts with {}. Resolve manually:\n  git status\n  # fix conflicts\n  git add . && git commit",
        remote_ref
    );
}

fn origin_default_branch_for_feature_sync(
    repo_root: &Path,
    current_branch: &str,
) -> Option<String> {
    let current = current_branch.trim();
    if current.is_empty() || current == "HEAD" {
        return None;
    }
    let default_branch = resolve_remote_default_branch_in(repo_root, "origin")?;
    if current == default_branch {
        return None;
    }
    if !remote_branch_exists(repo_root, "origin", &default_branch) {
        return None;
    }
    Some(default_branch)
}

fn resolve_remote_default_branch_in(repo_root: &Path, remote: &str) -> Option<String> {
    let head_ref = format!("refs/remotes/{}/HEAD", remote);
    if let Ok(symbolic) = git_capture_in(repo_root, &["symbolic-ref", &head_ref]) {
        let prefix = format!("refs/remotes/{}/", remote);
        if let Some(branch) = symbolic.trim().strip_prefix(&prefix) {
            if !branch.is_empty() {
                return Some(branch.to_string());
            }
        }
    }

    let preferred = jj_default_branch(repo_root);
    if remote_branch_exists(repo_root, remote, &preferred) {
        return Some(preferred);
    }
    for candidate in ["main", "master", "dev", "trunk"] {
        if remote_branch_exists(repo_root, remote, candidate) {
            return Some(candidate.to_string());
        }
    }

    None
}

fn is_jj_corruption_error(err: &anyhow::Error) -> bool {
    let msg = err.to_string().to_lowercase();
    msg.contains("failed to load short-prefixes index")
        || msg.contains("unexpected error from commit backend")
        || msg.contains("current working-copy commit not found")
        || msg.contains("failed to check out a commit")
        || (msg.contains("object ") && msg.contains(" not found"))
        || (msg.contains("jj git fetch failed") && msg.contains("object"))
}

fn is_git_index_lock_error(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("index.lock")
        || lower.contains("another git process seems to be running")
        || lower.contains("could not write index")
}

fn parse_branch_merge_ref(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.trim_start_matches("refs/heads/").to_string())
}

fn parse_tracking_ref(value: &str) -> Option<(String, String)> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    let (remote, branch) = trimmed.split_once('/')?;
    if remote.is_empty() || branch.is_empty() {
        return None;
    }

    Some((remote.to_string(), branch.to_string()))
}

fn resolve_tracking_remote_branch_in(
    repo_root: &Path,
    current_branch: Option<&str>,
) -> Option<(String, String)> {
    let current_branch = current_branch
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "HEAD");

    if let Some(branch) = current_branch {
        if let Ok(remote) = git_capture_in(
            repo_root,
            &["config", "--get", &format!("branch.{}.remote", branch)],
        ) {
            let remote = remote.trim();
            if !remote.is_empty() {
                if let Ok(merge_ref) = git_capture_in(
                    repo_root,
                    &["config", "--get", &format!("branch.{}.merge", branch)],
                ) {
                    if let Some(merge_branch) = parse_branch_merge_ref(&merge_ref) {
                        if !merge_branch.is_empty() {
                            return Some((remote.to_string(), merge_branch));
                        }
                    }
                }
            }
        }
    }

    if let Ok(upstream) = git_capture_in(repo_root, &["rev-parse", "--abbrev-ref", "@{upstream}"]) {
        return parse_tracking_ref(&upstream);
    }

    None
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

fn resolve_sync_branch_for_queue_guard(repo_root: &Path) -> Option<String> {
    let head = git_capture_in(repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .ok()
        .map(|value| value.trim().to_string())
        .unwrap_or_default();
    if !head.is_empty() && head != "HEAD" {
        return Some(head);
    }

    if let Some(branch) = resolve_rebase_head_branch(repo_root) {
        return Some(branch);
    }

    resolve_branch_containing_head(repo_root)
}

fn resolve_rebase_head_branch(repo_root: &Path) -> Option<String> {
    let git_dir = git_capture_in(repo_root, &["rev-parse", "--git-dir"])
        .ok()
        .map(|value| value.trim().to_string())?;
    let git_dir_path = if Path::new(&git_dir).is_absolute() {
        PathBuf::from(git_dir)
    } else {
        repo_root.join(git_dir)
    };

    for rel in ["rebase-merge/head-name", "rebase-apply/head-name"] {
        let path = git_dir_path.join(rel);
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let branch = raw.trim().trim_start_matches("refs/heads/").trim();
        if !branch.is_empty() && branch != "HEAD" {
            return Some(branch.to_string());
        }
    }
    None
}

fn resolve_branch_containing_head(repo_root: &Path) -> Option<String> {
    let output = git_capture_in(
        repo_root,
        &["branch", "--format=%(refname:short)", "--contains", "HEAD"],
    )
    .ok()?;
    output
        .lines()
        .map(str::trim)
        .find(|value| !value.is_empty() && *value != "(no branch)" && *value != "HEAD")
        .map(|value| value.to_string())
}

fn should_use_jj(repo_root: &Path) -> bool {
    has_jj_workspace(repo_root) && jj_cli_available() && jj_workspace_healthy(repo_root)
}

fn has_jj_workspace(repo_root: &Path) -> bool {
    repo_root.join(".jj").exists()
}

fn jj_cli_available() -> bool {
    let status = Command::new("jj")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    status.map(|s| s.success()).unwrap_or(false)
}

fn jj_workspace_healthy(repo_root: &Path) -> bool {
    if env::var("FLOW_SYNC_SKIP_JJ_HEALTHCHECK")
        .ok()
        .map(|v| {
            let t = v.trim();
            t == "1" || t.eq_ignore_ascii_case("true")
        })
        .unwrap_or(false)
    {
        return true;
    }

    Command::new("jj")
        .current_dir(repo_root)
        .arg("status")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
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

fn jj_bookmark_create_or_set(repo_root: &Path, name: &str, rev: &str) -> Result<()> {
    if jj_bookmark_exists(repo_root, name) {
        return jj_run_in(repo_root, &["bookmark", "set", name, "-r", rev]);
    }

    match jj_run_in(repo_root, &["bookmark", "create", name, "-r", rev]) {
        Ok(()) => Ok(()),
        Err(create_err) => {
            if jj_bookmark_exists(repo_root, name) {
                jj_run_in(repo_root, &["bookmark", "set", name, "-r", rev]).with_context(|| {
                    format!("create failed ({create_err}); bookmark exists, but set also failed")
                })
            } else {
                Err(create_err)
            }
        }
    }
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

fn branch_tip_matches_remote(repo_root: &Path, branch: &str, remote: &str) -> bool {
    let local_ref = format!("refs/heads/{}", branch);
    let remote_ref = format!("refs/remotes/{}/{}", remote, branch);
    let local_sha = match git_capture_in(repo_root, &["rev-parse", &local_ref]) {
        Ok(value) => value.trim().to_string(),
        Err(_) => return false,
    };
    let remote_sha = match git_capture_in(repo_root, &["rev-parse", &remote_ref]) {
        Ok(value) => value.trim().to_string(),
        Err(_) => return false,
    };
    !local_sha.is_empty() && local_sha == remote_sha
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

fn restore_stash(repo_root: &Path, stashed: bool) {
    if stashed {
        println!("==> Restoring stashed changes...");
        let output = Command::new("git")
            .current_dir(repo_root)
            .args(["stash", "pop"])
            .output();
        match output {
            Ok(out) if out.status.success() => {}
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                let combined = format!("{}\n{}", stdout, stderr).to_lowercase();
                if stash_pop_untracked_conflict(&combined) {
                    match drop_stash_if_untracked_restored(repo_root) {
                        Ok(true) => {
                            println!(
                                "  ✓ Kept local untracked files and dropped redundant auto-stash"
                            );
                            return;
                        }
                        Ok(false) => {}
                        Err(err) => {
                            eprintln!("warning: stash cleanup failed: {}", err);
                        }
                    }
                }
                eprintln!(
                    "warning: failed to restore stash automatically: git stash pop failed\nRun `git stash list` and restore manually if needed."
                );
            }
            Err(err) => {
                eprintln!(
                    "warning: failed to restore stash automatically: {}\nRun `git stash list` and restore manually if needed.",
                    err
                );
            }
        }
    }
}

fn stash_pop_untracked_conflict(output: &str) -> bool {
    output.contains("could not restore untracked files from stash")
        || (output.contains("already exists, no checkout") && output.contains("stash"))
}

fn drop_stash_if_untracked_restored(repo_root: &Path) -> Result<bool> {
    if git_capture_in(repo_root, &["rev-parse", "--verify", "stash@{0}"]).is_err() {
        return Ok(false);
    }

    let has_untracked_parent = git_capture_in(repo_root, &["rev-parse", "--verify", "stash@{0}^3"]);
    if has_untracked_parent.is_err() {
        return Ok(false);
    }

    let files = git_capture_in(repo_root, &["ls-tree", "-r", "--name-only", "stash@{0}^3"])
        .unwrap_or_default();
    let untracked_paths: Vec<String> = files
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| line.to_string())
        .collect();
    if untracked_paths.is_empty() {
        return Ok(false);
    }

    let all_present = untracked_paths
        .iter()
        .all(|path| repo_root.join(path).exists());
    if !all_present {
        return Ok(false);
    }

    git_run_in(repo_root, &["stash", "drop", "stash@{0}"])?;
    Ok(true)
}

/// If fork-push is enabled in config, resolve the target remote name, owner, and fork repo name.
///
/// Returns `Some((remote_name, owner, fork_repo_name))` when fork push should be used.
fn resolve_fork_push_target(repo_root: &Path) -> Option<(String, String, String)> {
    // Check local config first, then global.
    let cfg = {
        let local = repo_root.join("flow.toml");
        if local.exists() {
            config::load(&local).ok()
        } else {
            None
        }
        .or_else(|| {
            let global = config::default_config_path();
            if global.exists() {
                config::load(&global).ok()
            } else {
                None
            }
        })
    };
    let git_cfg = cfg.as_ref().and_then(|c| c.git.as_ref());
    if git_cfg.map(|g| g.fork_push.unwrap_or(false)) != Some(true) {
        return None;
    }
    let git_cfg = git_cfg.unwrap();

    let owner = push::resolve_fork_owner(git_cfg.fork_push_owner.as_deref()).ok()?;
    let suffix = git_cfg.fork_push_suffix.as_deref().unwrap_or("-i");

    // Derive base repo name from upstream or origin URL.
    let upstream_url = git_capture_in(repo_root, &["remote", "get-url", "upstream"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let origin_url = git_capture_in(repo_root, &["remote", "get-url", "origin"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let base_name =
        push::derive_repo_name(repo_root, upstream_url.as_deref(), origin_url.as_deref()).ok()?;

    let fork_repo = format!("{}{}", base_name, suffix);
    let remote_name = format!("fork{}", suffix);
    Some((remote_name, owner, fork_repo))
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

/// Push to a remote with optional auto-fix on failure.
fn push_with_autofix(branch: &str, remote: &str, auto_fix: bool, max_attempts: u32) -> Result<()> {
    let mut attempts = 0;

    loop {
        // Try push and capture output
        let output = Command::new("git")
            .args(["push", remote, branch])
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
            bail!("git push {} {} failed", remote, branch);
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

/// Push to a remote with --force-with-lease, with optional auto-fix on failure.
fn push_with_autofix_force(
    branch: &str,
    remote: &str,
    auto_fix: bool,
    max_attempts: u32,
) -> Result<()> {
    let mut attempts = 0;

    loop {
        let output = Command::new("git")
            .args(["push", "--force-with-lease", remote, branch])
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
            bail!("git push --force-with-lease {} {} failed", remote, branch);
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
        dirs.push(home.join("repos").join("garden-co").join("jazz2"));
        dirs.push(home.join("code").join("org").join("1f").join("jazz2"));
    }
    dirs
}

fn write_sync_snapshot(snapshot: &SyncSnapshot) -> Result<()> {
    let mut value = serde_json::to_value(snapshot)?;
    secret_redact::redact_json_value(&mut value);
    let payload = serde_json::to_string(&value)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_branch_merge_ref_strips_heads_prefix() {
        assert_eq!(
            parse_branch_merge_ref("refs/heads/feature/socket-command"),
            Some("feature/socket-command".to_string())
        );
        assert_eq!(parse_branch_merge_ref("main"), Some("main".to_string()));
    }

    #[test]
    fn parse_tracking_ref_parses_remote_and_branch() {
        assert_eq!(
            parse_tracking_ref("fork/socket-command"),
            Some(("fork".to_string(), "socket-command".to_string()))
        );
        assert_eq!(
            parse_tracking_ref("origin/feature/latency/tune"),
            Some(("origin".to_string(), "feature/latency/tune".to_string()))
        );
    }

    #[test]
    fn parse_tracking_ref_rejects_invalid_values() {
        assert_eq!(parse_tracking_ref(""), None);
        assert_eq!(parse_tracking_ref("origin"), None);
        assert_eq!(parse_tracking_ref("/main"), None);
        assert_eq!(parse_tracking_ref("origin/"), None);
    }
}
