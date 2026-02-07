use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::cli::GitRepairOpts;

/// Returns true when the repo has a `.jj` directory (jj colocated mode).
fn is_jj_colocated(repo_root: &Path) -> bool {
    repo_root.join(".jj").is_dir()
}

/// In a jj-colocated repo, detached HEAD is normal. Attach it to main/master
/// so git operations (add, commit, push) work. This is safe because jj keeps
/// git HEAD at the same commit as the main bookmark.
fn jj_auto_checkout(repo_root: &Path) -> Result<bool> {
    if !is_jj_colocated(repo_root) {
        return Ok(false);
    }
    let target = if git_ref_exists(repo_root, "main") {
        "main"
    } else if git_ref_exists(repo_root, "master") {
        "master"
    } else {
        return Ok(false);
    };
    // Silently attach HEAD — no file changes, just moves HEAD to the branch.
    let status = Command::new("git")
        .current_dir(repo_root)
        .args(["checkout", target])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => Ok(true),
        _ => Ok(false),
    }
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
        print_state(&state, branch);
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
        // Try jj auto-checkout first (silent, safe).
        if jj_auto_checkout(&repo_root).unwrap_or(false) {
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

fn print_state(state: &GitState, branch: &str) {
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
