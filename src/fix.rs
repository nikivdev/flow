use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::cli::FixOpts;
use crate::opentui_prompt;

pub fn run(opts: FixOpts) -> Result<()> {
    let message = resolve_fix_message(&opts.message)?;

    let repo_root = git_top_level()?;
    if try_run_commit_repair(&repo_root, &message)? {
        return Ok(());
    }

    if !opts.no_agent {
        let lines = vec![
            format!(
                "`f fix` no longer runs the retired Hive agent step (`{}`).",
                opts.agent
            ),
            "Flow can still prepare the repo for a manual or Codex-driven fix.".to_string(),
            "After the unroll, apply the fix yourself and finish with `f commit`.".to_string(),
        ];
        if !confirm_with_tui("Fix", &lines, "Continue in manual fix mode? [Y/n]: ")? {
            bail!("Aborted.");
        }
    }

    let unroll = !opts.no_unroll;
    let mut stashed = false;

    if unroll {
        ensure_clean_or_stash(&repo_root, opts.stash, &mut stashed)?;
        ensure_has_parent_commit(&repo_root)?;
        let head = git_output(&repo_root, &["rev-parse", "HEAD"])?;
        let head_short = head.trim().chars().take(7).collect::<String>();
        println!("Unrolling last commit ({head_short})...");
        git_status(&repo_root, &["reset", "--soft", "HEAD~1"])?;
    }

    if !opts.no_agent {
        print_fix_agent_retired_notice(&opts.agent, &message);
    } else {
        println!("Skipped retired fix-agent step; continuing in manual fix mode.");
    }

    if stashed {
        println!("Restoring stashed changes...");
        let _ = git_status(&repo_root, &["stash", "pop"]);
    }

    print_manual_fix_next_steps(&message);

    Ok(())
}

fn resolve_fix_message(parts: &[String]) -> Result<String> {
    let joined = parts.join(" ").trim().to_string();
    if joined.is_empty() {
        bail!("provide a fix message, e.g. `f fix last commit had spotify api leaked`");
    }

    let Some(path) = detect_fix_input_file(parts) else {
        return Ok(joined);
    };

    let content = fs::read_to_string(&path)
        .with_context(|| format!("failed to read fix input file {}", path.display()))?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        bail!("fix input file is empty: {}", path.display());
    }

    println!("Loaded fix context from {}", path.display());
    Ok(format!(
        "Use this report as the source of truth for what to fix.\n\nReport file: {}\n\n{}",
        path.display(),
        trimmed
    ))
}

fn detect_fix_input_file(parts: &[String]) -> Option<PathBuf> {
    if parts.len() != 1 {
        return None;
    }
    let raw = parts[0].trim();
    if raw.is_empty() {
        return None;
    }
    let candidate = raw.strip_prefix('@').unwrap_or(raw);
    let path = PathBuf::from(candidate);
    if !path.is_file() {
        return None;
    }
    Some(path.canonicalize().unwrap_or(path))
}

fn try_run_commit_repair(repo_root: &std::path::Path, message: &str) -> Result<bool> {
    if !matches_recommit_request(message) {
        return Ok(false);
    }

    let status = git_output(repo_root, &["status", "--porcelain"])?;
    if !status.trim().is_empty() {
        let lines = vec![
            "Working tree has uncommitted changes that will be included in the new commit."
                .to_string(),
        ];
        if !confirm_with_tui("Re-commit", &lines, "Continue with re-commit? [Y/n]: ")? {
            bail!("Aborted.");
        }
    }

    let plan_lines = vec![
        "Plan:".to_string(),
        "  1) git reset --soft HEAD~1  (undo last commit, keep changes staged)".to_string(),
        "  2) f commit                 (recreate commit with updated hygiene)".to_string(),
    ];
    if !confirm_with_tui("Re-commit", &plan_lines, "Proceed? [Y/n]: ")? {
        bail!("Aborted.");
    }

    git_status(repo_root, &["reset", "--soft", "HEAD~1"])?;
    let status = Command::new("f")
        .arg("commit")
        .current_dir(repo_root)
        .status()
        .context("failed to run f commit")?;
    if !status.success() {
        bail!("f commit failed with status {}", status);
    }

    Ok(true)
}

fn confirm_with_tui(title: &str, lines: &[String], prompt: &str) -> Result<bool> {
    if let Some(answer) = opentui_prompt::confirm(title, lines, true) {
        return Ok(answer);
    }

    if !lines.is_empty() {
        for line in lines {
            println!("{}", line);
        }
    }

    confirm_default_yes(prompt)
}

fn matches_recommit_request(message: &str) -> bool {
    let lowered = message.to_ascii_lowercase();
    let undo = lowered.contains("undo last commit")
        || lowered.contains("undo the last commit")
        || lowered.contains("reset last commit")
        || lowered.contains("reset the last commit")
        || lowered.contains("recommit");
    let rerun = lowered.contains("run f commit")
        || lowered.contains("rerun f commit")
        || lowered.contains("run f commit again")
        || lowered.contains("re-run f commit")
        || lowered.contains("recommit and run f commit");
    undo && rerun
}

fn confirm_default_yes(prompt: &str) -> Result<bool> {
    print!("{}", prompt);
    io::stdout().flush()?;

    if std::io::stdin().is_terminal() {
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Ok(true);
        }
        return Ok(matches!(trimmed.to_ascii_lowercase().as_str(), "y" | "yes"));
    }

    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(true);
    }
    Ok(matches!(trimmed.to_ascii_lowercase().as_str(), "y" | "yes"))
}

fn build_fix_task(message: &str) -> String {
    format!(
        "Fix this repo. Task: {message}\n\n\
If the issue involves leaked secrets, remove them from tracked files, \
update .gitignore if needed, and ensure the repo is safe to recommit."
    )
}

fn print_fix_agent_retired_notice(agent: &str, message: &str) {
    println!(
        "Retired agent step: `{}` is no longer run by `f fix` while Hive is being removed.",
        agent
    );
    println!();
    println!("Suggested prompt for your model:");
    println!("────────────────────────────────────────");
    println!("{}", build_fix_task(message));
    println!("────────────────────────────────────────");
}

fn print_manual_fix_next_steps(message: &str) {
    let preview = message.lines().next().unwrap_or(message).trim();
    println!();
    println!("Next steps:");
    println!("  1) Make the requested repair manually or in Codex");
    println!("  2) Review the diff");
    println!("  3) Run `f commit` to recreate the commit cleanly");
    if !preview.is_empty() {
        println!("  Context: {}", preview);
    }
}

fn git_top_level() -> Result<std::path::PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("failed to run git")?;
    if !output.status.success() {
        bail!("not a git repository (or git not available)");
    }
    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if root.is_empty() {
        bail!("failed to resolve git repository root");
    }
    Ok(std::path::PathBuf::from(root))
}

fn ensure_clean_or_stash(
    repo_root: &std::path::Path,
    allow_stash: bool,
    stashed: &mut bool,
) -> Result<()> {
    let status = git_output(repo_root, &["status", "--porcelain"])?;
    if status.trim().is_empty() {
        return Ok(());
    }

    if !allow_stash {
        bail!("working tree has uncommitted changes; commit/stash them or rerun with --stash");
    }

    println!("Stashing local changes...");
    git_status(
        repo_root,
        &["stash", "push", "-u", "-m", "f fix auto-stash"],
    )?;
    *stashed = true;
    Ok(())
}

fn ensure_has_parent_commit(repo_root: &std::path::Path) -> Result<()> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD~1"])
        .current_dir(repo_root)
        .output()
        .context("failed to check git history")?;
    if !output.status.success() {
        bail!("cannot unroll: repository has no parent commit");
    }
    Ok(())
}

fn git_output(repo_root: &std::path::Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn git_status(repo_root: &std::path::Path, args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .status()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;
    if !status.success() {
        bail!("git {} failed", args.join(" "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::build_fix_task;

    #[test]
    fn build_fix_task_mentions_recommit_safety() {
        let task = build_fix_task("last commit leaked a token");
        assert!(task.contains("last commit leaked a token"));
        assert!(task.contains("remove them from tracked files"));
        assert!(task.contains(".gitignore"));
    }
}
