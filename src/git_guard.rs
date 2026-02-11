use std::path::{Path, PathBuf};
use std::process::Command;
use std::fs;

use anyhow::{Context, Result, anyhow, bail};

use crate::cli::GitRepairOpts;

/// Returns true when the repo has a `.jj` directory (jj colocated mode).
fn is_jj_colocated(repo_root: &Path) -> bool {
    repo_root.join(".jj").is_dir()
}

fn git_capture_in(repo_root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .current_dir(repo_root)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn has_working_tree_changes(repo_root: &Path) -> bool {
    match git_capture_in(repo_root, &["status", "--porcelain"]) {
        Some(s) => !s.trim().is_empty(),
        None => false,
    }
}

fn short_sha(sha: &str) -> &str {
    if sha.len() <= 7 { sha } else { &sha[..7] }
}

fn commit_queue_has_entries(repo_root: &Path) -> bool {
    let dir = repo_root.join(".ai").join("internal").join("commit-queue");
    if !dir.exists() {
        return false;
    }
    match fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .any(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json")),
        Err(_) => false,
    }
}

fn resolve_land_target_branch(repo_root: &Path, requested: &str) -> Result<String> {
    if git_ref_exists(repo_root, requested) {
        return Ok(requested.to_string());
    }
    if requested == "main" && git_ref_exists(repo_root, "master") {
        return Ok("master".to_string());
    }
    bail!(
        "Target branch '{}' not found (and fallback branch unavailable).",
        requested
    );
}

fn ensure_clean_working_tree_for_land(repo_root: &Path) -> Result<()> {
    let status = git_capture_in(repo_root, &["status", "--porcelain"]).unwrap_or_default();
    if !status.trim().is_empty() {
        bail!(
            "Cannot land onto main with uncommitted changes. Commit or stash first."
        );
    }
    Ok(())
}

fn land_head_to_branch(repo_root: &Path, requested_target: &str) -> Result<()> {
    ensure_clean_working_tree_for_land(repo_root)?;
    if commit_queue_has_entries(repo_root) {
        bail!(
            "Commit queue is not empty. Landing to main rewrites commit SHA; approve/drop queued entries first."
        );
    }

    let current = git_capture_in(repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .unwrap_or_else(|| "HEAD".to_string());
    if current.trim() == "HEAD" {
        bail!("HEAD is detached. Run `f git-repair` first.");
    }
    let current = current.trim().to_string();
    let target = resolve_land_target_branch(repo_root, requested_target)?;
    if current == target {
        println!("Already on {}", target);
        return Ok(());
    }

    let head_sha = git_capture_in(repo_root, &["rev-parse", "HEAD"])
        .ok_or_else(|| anyhow!("failed to resolve HEAD commit"))?;

    git_run_in(repo_root, &["checkout", &target])?;
    match git_run_in(repo_root, &["cherry-pick", head_sha.trim()]) {
        Ok(_) => {
            println!(
                "✓ Landed commit {} from {} onto {}",
                short_sha(head_sha.trim()),
                current,
                target
            );
            Ok(())
        }
        Err(err) => {
            eprintln!("Cherry-pick conflict while landing onto {}.", target);
            eprintln!("Resolve conflicts, then run:");
            eprintln!("  git cherry-pick --continue");
            eprintln!("Or abort with:");
            eprintln!("  git cherry-pick --abort");
            Err(err).context("failed to land commit onto target branch")
        }
    }
}

fn attach_detached_head_to_keep_branch(repo_root: &Path) -> Result<bool> {
    if !is_jj_colocated(repo_root) {
        return Ok(false);
    }
    let Some(head) = git_capture_in(repo_root, &["rev-parse", "HEAD"]) else {
        return Ok(false);
    };
    if head.trim().is_empty() {
        return Ok(false);
    }
    let branch = format!("jj/keep/{}", head.trim());

    // If it already exists, just check it out.
    if git_ref_exists(repo_root, &branch) {
        let status = Command::new("git")
            .current_dir(repo_root)
            .args(["checkout", &branch])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        return Ok(matches!(status, Ok(s) if s.success()));
    }

    // Create and check out (at HEAD) without touching the working tree.
    let status = Command::new("git")
        .current_dir(repo_root)
        .args(["checkout", "-b", &branch, head.trim()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    Ok(matches!(status, Ok(s) if s.success()))
}

/// In a jj-colocated repo, detached HEAD is common. For git-based workflows,
/// we need to ensure HEAD is attached to a local branch.
///
/// Strategy:
/// - If main/master exists AND points at the current HEAD commit, attach to it
///   (no working tree changes).
/// - Otherwise, attach HEAD to a synthetic `jj/keep/<sha>` branch at the current commit.
fn jj_auto_checkout(repo_root: &Path) -> Result<bool> {
    if !is_jj_colocated(repo_root) {
        return Ok(false);
    }

    let Some(head) = git_capture_in(repo_root, &["rev-parse", "HEAD"]) else {
        return Ok(false);
    };
    for target in ["main", "master"] {
        if !git_ref_exists(repo_root, target) {
            continue;
        }
        let Some(target_sha) = git_capture_in(repo_root, &["rev-parse", target]) else {
            continue;
        };
        if target_sha == head {
            // Silently attach HEAD — no file changes, just moves HEAD to the branch.
            let status = Command::new("git")
                .current_dir(repo_root)
                .args(["checkout", target])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            if matches!(status, Ok(s) if s.success()) {
                return Ok(true);
            }
        }
    }

    attach_detached_head_to_keep_branch(repo_root)
}

#[derive(Debug, Clone)]
struct GitState {
    rebase: bool,
    merge: bool,
    cherry_pick: bool,
    revert: bool,
    bisect: bool,
    detached: bool,
    unmerged_files: Vec<String>,
}

pub fn ensure_clean_for_commit(repo_root: &Path) -> Result<()> {
    ensure_clean_state(repo_root, "commit")
}

pub fn ensure_clean_for_push(repo_root: &Path) -> Result<()> {
    ensure_clean_state(repo_root, "push")
}

fn ensure_clean_state(repo_root: &Path, action: &str) -> Result<()> {
    let state = detect_git_state(repo_root)?;
    let mut issues = Vec::new();

    if state.rebase {
        issues.push("rebase in progress".to_string());
    }
    if state.merge {
        issues.push("merge in progress".to_string());
    }
    if state.cherry_pick {
        issues.push("cherry-pick in progress".to_string());
    }
    if state.revert {
        issues.push("revert in progress".to_string());
    }
    if state.bisect {
        issues.push("bisect in progress".to_string());
    }
    if !state.unmerged_files.is_empty() {
        issues.push(format!(
            "unmerged files: {}",
            state.unmerged_files.join(", ")
        ));
    }
    if state.detached {
        // In jj-colocated repos, detached HEAD is normal — auto-fix it.
        if !jj_auto_checkout(repo_root).unwrap_or(false) {
            issues.push("detached HEAD".to_string());
        }
    }

    if !issues.is_empty() {
        let mut msg = format!("Git state not clean for {}:", action);
        for issue in issues {
            msg.push_str(&format!("\n  - {}", issue));
        }
        msg.push_str("\n\nRun `f git-repair` or resolve manually before continuing.");
        bail!(msg);
    }

    Ok(())
}

pub fn run_git_repair(opts: GitRepairOpts) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let repo_root = find_repo_root(&cwd)?;
    let branch = opts.branch.as_deref().unwrap_or("main");
    let state = detect_git_state(&repo_root)?;

    if opts.dry_run {
        print_state(&state, branch, opts.land_main);
        return Ok(());
    }

    let mut did_work = false;
    if state.rebase {
        let _ = git_run_in(&repo_root, &["rebase", "--abort"]);
        did_work = true;
    }
    if state.merge {
        let _ = git_run_in(&repo_root, &["merge", "--abort"]);
        did_work = true;
    }
    if state.cherry_pick {
        let _ = git_run_in(&repo_root, &["cherry-pick", "--abort"]);
        did_work = true;
    }
    if state.revert {
        let _ = git_run_in(&repo_root, &["revert", "--abort"]);
        did_work = true;
    }
    if state.bisect {
        let _ = git_run_in(&repo_root, &["bisect", "reset"]);
        did_work = true;
    }

    if state.detached {
        // In jj-colocated repos, prefer attaching HEAD to a safe local branch.
        // If there are working copy changes, do NOT try to checkout main/master (it can overwrite).
        if is_jj_colocated(&repo_root) && has_working_tree_changes(&repo_root) {
            // Keep working tree intact; just attach HEAD to a branch at the current commit.
            if attach_detached_head_to_keep_branch(&repo_root).unwrap_or(false) {
                did_work = true;
            }
        } else if jj_auto_checkout(&repo_root).unwrap_or(false) {
            did_work = true;
        } else {
            let target = if git_ref_exists(&repo_root, branch) {
                branch.to_string()
            } else if git_ref_exists(&repo_root, "master") {
                "master".to_string()
            } else {
                bail!(
                    "Detached HEAD and branch '{}' not found. Checkout a branch manually.",
                    branch
                );
            };
            git_run_in(&repo_root, &["checkout", &target])?;
            did_work = true;
        }
    }

    if opts.land_main {
        land_head_to_branch(&repo_root, branch)?;
        did_work = true;
    }

    if did_work {
        println!("✓ Git repair complete");
    } else {
        println!("No repair needed");
    }

    Ok(())
}

fn detect_git_state(repo_root: &Path) -> Result<GitState> {
    let git_dir = git_dir(repo_root)?;
    let rebase = git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists();
    let merge = git_dir.join("MERGE_HEAD").exists();
    let cherry_pick = git_dir.join("CHERRY_PICK_HEAD").exists();
    let revert = git_dir.join("REVERT_HEAD").exists();
    let bisect = git_dir.join("BISECT_LOG").exists();
    let detached = is_detached_head(repo_root)?;
    let unmerged_files = git_unmerged_files(repo_root);

    Ok(GitState {
        rebase,
        merge,
        cherry_pick,
        revert,
        bisect,
        detached,
        unmerged_files,
    })
}

fn git_dir(repo_root: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(["rev-parse", "--git-dir"])
        .output()
        .context("failed to locate git directory")?;
    if !output.status.success() {
        bail!("Not a git repository");
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let dir = PathBuf::from(raw);
    if dir.is_absolute() {
        Ok(dir)
    } else {
        Ok(repo_root.join(dir))
    }
}

fn is_detached_head(repo_root: &Path) -> Result<bool> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(["symbolic-ref", "--quiet", "--short", "HEAD"])
        .output()
        .context("failed to check HEAD")?;
    Ok(!output.status.success())
}

fn git_unmerged_files(repo_root: &Path) -> Vec<String> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(["diff", "--name-only", "--diff-filter=U"])
        .output();
    match output {
        Ok(out) => String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.trim().to_string())
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn git_run_in(repo_root: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .current_dir(repo_root)
        .args(args)
        .status()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;
    if !status.success() {
        bail!("git {} failed", args.join(" "));
    }
    Ok(())
}

fn git_ref_exists(repo_root: &Path, name: &str) -> bool {
    Command::new("git")
        .current_dir(repo_root)
        .args(["show-ref", "--verify", &format!("refs/heads/{}", name)])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn print_state(state: &GitState, branch: &str, land_main: bool) {
    println!("Git repair dry-run:");
    println!("  rebase: {}", state.rebase);
    println!("  merge: {}", state.merge);
    println!("  cherry-pick: {}", state.cherry_pick);
    println!("  revert: {}", state.revert);
    println!("  bisect: {}", state.bisect);
    println!("  detached: {}", state.detached);
    if !state.unmerged_files.is_empty() {
        println!("  unmerged files: {}", state.unmerged_files.join(", "));
    }
    println!("  target branch: {}", branch);
    println!("  land main: {}", land_main);
}

fn find_repo_root(start: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .current_dir(start)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("failed to find git repository")?;

    if !output.status.success() {
        bail!("Not in a git repository");
    }

    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(PathBuf::from(path))
}
