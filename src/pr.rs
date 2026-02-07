use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::cli::PrOpts;
use crate::commit;

pub fn run(opts: PrOpts) -> Result<()> {
    commit::ensure_git_repo()?;
    commit::ensure_gh_available()?;

    let repo_root = commit::git_root_or_cwd();
    let is_jj = which::which("jj").is_ok() && repo_root.join(".jj").is_dir();

    let origin_repo = remote_repo(&repo_root, "origin");
    let parsed = parse_pr_args(&opts.args);
    let base_repo = if parsed.preview {
        // Preview mode: PR exists only on your repo (origin).
        origin_repo
            .clone()
            .or_else(|| commit::resolve_github_repo(&repo_root).ok())
            .context("failed to resolve origin repo for preview PR")?
    } else {
        // Default: PR base repo prefers upstream (fork workflow).
        resolve_base_repo(&repo_root)?
    };

    if is_jj && is_rise_context(&repo_root) {
        return run_rise_pr(
            &repo_root,
            &base_repo,
            origin_repo.as_deref(),
            &parsed,
            &opts,
        );
    }

    if is_jj {
        return run_jj_pr(
            &repo_root,
            &base_repo,
            origin_repo.as_deref(),
            &parsed,
            &opts,
        );
    }

    run_git_pr(
        &repo_root,
        &base_repo,
        origin_repo.as_deref(),
        &parsed,
        &opts,
    )
}

#[derive(Debug, Clone, Default)]
struct ParsedPrArgs {
    preview: bool,
    title: Option<String>,
}

fn parse_pr_args(args: &[String]) -> ParsedPrArgs {
    let mut preview = false;
    let mut rest: Vec<&str> = Vec::new();

    for (i, a) in args.iter().enumerate() {
        if i == 0 && a == "preview" {
            preview = true;
            continue;
        }
        rest.push(a.as_str());
    }

    let title = rest
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    ParsedPrArgs {
        preview,
        title: if title.is_empty() { None } else { Some(title) },
    }
}

fn run_git_pr(
    repo_root: &Path,
    base_repo: &str,
    origin_repo: Option<&str>,
    parsed: &ParsedPrArgs,
    opts: &PrOpts,
) -> Result<()> {
    let dirty = git_is_dirty(repo_root);

    let (sha, title, body, branch_name) = if dirty {
        let message = parsed
            .title
            .as_deref()
            .map(|s| s.to_string())
            .context("working copy has uncommitted changes; provide a PR title (e.g. `f pr \"...\"`) or commit first")?;

        let (title, body) = commit::commit_message_title_body(&message);
        let branch_name = opts
            .branch
            .clone()
            .unwrap_or_else(|| derive_branch_name(&title));

        ensure_on_git_branch(repo_root, &branch_name)?;
        let sha = git_commit_all(repo_root, &message)?;
        (sha, title, body, branch_name)
    } else {
        let sha = commit::git_capture_in(repo_root, &["rev-parse", "HEAD"])?
            .trim()
            .to_string();
        let message = commit::git_capture_in(repo_root, &["log", "-1", "--format=%B"])?
            .trim()
            .to_string();
        if message.is_empty() && parsed.title.is_none() {
            bail!("HEAD commit has no message");
        }

        let (title, body) = match parsed.title.as_deref().filter(|s| !s.is_empty()) {
            Some(m) => commit::commit_message_title_body(m),
            None => commit::commit_message_title_body(&message),
        };
        let branch_name = opts
            .branch
            .clone()
            .unwrap_or_else(|| derive_branch_name(&title));
        (sha, title, body, branch_name)
    };

    let head_ref = head_ref(base_repo, origin_repo, &branch_name);

    let existing_pr = commit::gh_find_open_pr_by_head(repo_root, base_repo, &head_ref)?;
    if let Some((_number, url)) = &existing_pr {
        println!("Updating existing PR: {}", url);
        push_branch(repo_root, &branch_name, &sha, false)?;
        println!("Force-pushed {} to {}", short_sha(&sha), &branch_name);
        if !opts.no_open {
            let _ = commit::open_in_browser(url);
        }
        return Ok(());
    }

    push_branch(repo_root, &branch_name, &sha, false)?;
    println!("Pushed branch {}", &branch_name);

    let (_number, url) = commit::gh_create_pr(
        repo_root, base_repo, &head_ref, &opts.base, &title, &body, opts.draft,
    )?;
    println!("Created PR: {}", url);
    if !opts.no_open {
        let _ = commit::open_in_browser(&url);
    }

    Ok(())
}

fn run_jj_pr(
    repo_root: &Path,
    base_repo: &str,
    origin_repo: Option<&str>,
    parsed: &ParsedPrArgs,
    opts: &PrOpts,
) -> Result<()> {
    // If a title is provided and the current change is undescribed, set it.
    if let Some(title) = parsed.title.as_deref().filter(|s| !s.is_empty()) {
        let description = jj_description(repo_root).unwrap_or_default();
        if description.trim().is_empty() {
            commit::jj_run_in(repo_root, &["describe", "-m", title])?;
        }
    }

    let description = jj_description(repo_root)?;
    let (title, body) = match parsed.title.as_deref().filter(|s| !s.is_empty()) {
        Some(m) => commit::commit_message_title_body(m),
        None => commit::commit_message_title_body(&description),
    };

    let branch_name = opts
        .branch
        .clone()
        .unwrap_or_else(|| derive_branch_name(&title));
    let head_ref = head_ref(base_repo, origin_repo, &branch_name);

    let sha = commit::jj_capture_in(
        repo_root,
        &["log", "-r", "@", "--no-graph", "-T", "commit_id"],
    )?
    .trim()
    .to_string();
    if sha.is_empty() {
        bail!("failed to resolve jj commit_id for current change");
    }

    let existing_pr = commit::gh_find_open_pr_by_head(repo_root, base_repo, &head_ref)?;
    if let Some((_number, url)) = &existing_pr {
        println!("Updating existing PR: {}", url);
        push_branch(repo_root, &branch_name, &sha, true)?;
        println!("Force-pushed {} to {}", short_sha(&sha), &branch_name);
        if !opts.no_open {
            let _ = commit::open_in_browser(url);
        }
        return Ok(());
    }

    push_branch(repo_root, &branch_name, &sha, true)?;
    println!("Pushed bookmark {}", &branch_name);

    let (_number, url) = commit::gh_create_pr(
        repo_root, base_repo, &head_ref, &opts.base, &title, &body, opts.draft,
    )?;
    println!("Created PR: {}", url);

    if !opts.no_open {
        let _ = commit::open_in_browser(&url);
    }

    Ok(())
}

fn jj_description(repo_root: &Path) -> Result<String> {
    let description = commit::jj_capture_in(
        repo_root,
        &["log", "-r", "@", "--no-graph", "-T", "description"],
    )?
    .trim()
    .to_string();
    if description.is_empty() {
        bail!(
            "Current change has no description. Set one with `jj describe` (or pass a title to `f pr \"...\"`)."
        );
    }
    Ok(description)
}

/// Push a branch/bookmark for the given commit.
fn push_branch(repo_root: &Path, branch: &str, sha: &str, is_jj: bool) -> Result<()> {
    if is_jj {
        // Use jj bookmarks: set the bookmark, then push.
        commit::jj_run_in(
            repo_root,
            &["bookmark", "set", branch, "-r", sha, "--allow-backwards"],
        )?;
        commit::jj_run_in(
            repo_root,
            &["git", "push", "--bookmark", branch, "--allow-new"],
        )?;
    } else {
        // Pure git: create/update branch and push to origin.
        commit::git_run_in(repo_root, &["branch", "-f", branch, sha])?;
        commit::git_run_in(
            repo_root,
            &["push", "-u", "origin", branch, "--force-with-lease"],
        )?;
    }
    Ok(())
}

fn resolve_base_repo(repo_root: &Path) -> Result<String> {
    if let Some(repo) = remote_repo(repo_root, "upstream") {
        return Ok(repo);
    }
    // If upstream remote is missing, prefer the GitHub parent repo when this is a fork.
    if let Ok(repo) = commit::gh_capture_in(
        repo_root,
        &[
            "repo",
            "view",
            "--json",
            "parent,nameWithOwner",
            "-q",
            ".parent.nameWithOwner // .nameWithOwner",
        ],
    ) {
        let repo = repo.trim();
        if !repo.is_empty() {
            return Ok(repo.to_string());
        }
    }
    commit::resolve_github_repo(repo_root)
}

fn remote_repo(repo_root: &Path, remote: &str) -> Option<String> {
    let url = commit::git_capture_in(repo_root, &["remote", "get-url", remote]).ok()?;
    commit::github_repo_from_remote_url(&url)
}

fn head_ref(base_repo: &str, origin_repo: Option<&str>, branch: &str) -> String {
    let Some(origin_repo) = origin_repo else {
        return branch.to_string();
    };
    if origin_repo == base_repo {
        return branch.to_string();
    }
    let owner = origin_repo.split('/').next().unwrap_or(origin_repo);
    format!("{}:{}", owner, branch)
}

fn git_is_dirty(repo_root: &Path) -> bool {
    commit::git_capture_in(repo_root, &["status", "--porcelain"])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

fn ensure_on_git_branch(repo_root: &Path, branch: &str) -> Result<()> {
    let current = commit::git_capture_in(repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])?
        .trim()
        .to_string();
    if current == branch {
        return Ok(());
    }

    if git_branch_exists(repo_root, branch) {
        commit::git_run_in(repo_root, &["checkout", branch])?;
    } else {
        commit::git_run_in(repo_root, &["checkout", "-b", branch])?;
    }
    Ok(())
}

fn git_branch_exists(repo_root: &Path, branch: &str) -> bool {
    Command::new("git")
        .current_dir(repo_root)
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{}", branch),
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn git_commit_all(repo_root: &Path, message: &str) -> Result<String> {
    commit::git_run_in(repo_root, &["add", "-A"])?;

    let staged = commit::git_capture_in(repo_root, &["diff", "--cached", "--name-only"])?
        .trim()
        .to_string();
    if staged.is_empty() {
        bail!("no changes to commit");
    }

    let (title, body) = commit::commit_message_title_body(message);
    if title.trim().is_empty() {
        bail!("PR title/commit message is empty");
    }

    if body.trim().is_empty() {
        commit::git_run_in(repo_root, &["commit", "-m", &title])?;
    } else {
        commit::git_run_in(repo_root, &["commit", "-m", &title, "-m", &body])?;
    }

    Ok(commit::git_capture_in(repo_root, &["rev-parse", "HEAD"])?
        .trim()
        .to_string())
}

fn short_sha(sha: &str) -> &str {
    if sha.len() <= 7 { sha } else { &sha[..7] }
}

/// Derive a branch name from a title.
/// e.g. "Reduce latency with external engine" → "reduce-latency-with-external-engine"
fn derive_branch_name(title: &str) -> String {
    let lowered = title.to_lowercase();
    let sanitized = commit::sanitize_ref_component(&lowered);
    // Truncate to a reasonable length.
    let max_len = 60;
    if sanitized.len() > max_len {
        match sanitized[..max_len].rfind('-') {
            Some(pos) => sanitized[..pos].to_string(),
            None => sanitized[..max_len].to_string(),
        }
    } else {
        sanitized
    }
}

/// Check if the current working copy is a descendant of the rise bookmark.
fn is_rise_context(repo_root: &Path) -> bool {
    if !repo_root.join(".rise").is_dir() {
        return false;
    }
    let check = commit::jj_capture_in(
        repo_root,
        &[
            "log",
            "-r",
            "@ & descendants(rise)",
            "--no-graph",
            "-T",
            "change_id",
        ],
    )
    .unwrap_or_default();
    !check.trim().is_empty()
}

/// Create a PR from a rise child by extracting changed files onto a clean branch off main.
fn run_rise_pr(
    repo_root: &Path,
    base_repo: &str,
    origin_repo: Option<&str>,
    parsed: &ParsedPrArgs,
    opts: &PrOpts,
) -> Result<()> {
    // Capture current change's description.
    let mut description = commit::jj_capture_in(
        repo_root,
        &["log", "-r", "@", "--no-graph", "-T", "description"],
    )?
    .trim()
    .to_string();

    if let Some(title) = parsed.title.as_deref().filter(|s| !s.is_empty()) {
        description = title.to_string();
    }

    if description.is_empty() {
        bail!(
            "Current change has no description. Set one with `jj describe` (or pass a title to `f pr \"...\"`)."
        );
    }

    // Capture changed files.
    let diff_summary = commit::jj_capture_in(repo_root, &["diff", "--summary", "-r", "@"])?;
    let changed_files: Vec<&str> = diff_summary
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.len() > 2 && line.as_bytes()[1] == b' ' {
                Some(line[2..].trim())
            } else {
                None
            }
        })
        .collect();
    if changed_files.is_empty() {
        bail!("No changed files in current change.");
    }

    let original_change = commit::jj_capture_in(
        repo_root,
        &["log", "-r", "@", "--no-graph", "-T", "change_id"],
    )?
    .trim()
    .to_string();

    let (title, body) = commit::commit_message_title_body(&description);
    let branch_name = opts
        .branch
        .clone()
        .unwrap_or_else(|| derive_branch_name(&title));
    let head_ref = head_ref(base_repo, origin_repo, &branch_name);

    // Determine the base revision — use the PR base option to find the right remote ref.
    let base = &opts.base;
    // Try upstream first (fork workflow), then origin. In preview mode: origin only.
    let base_rev = if parsed.preview {
        format!("{}@origin", base)
    } else if commit::jj_capture_in(
        repo_root,
        &[
            "log",
            "-r",
            &format!("{}@upstream", base),
            "--no-graph",
            "-T",
            "change_id",
        ],
    )
    .is_ok()
    {
        format!("{}@upstream", base)
    } else {
        format!("{}@origin", base)
    };

    println!("==> Rise PR: creating clean change off {}...", base_rev);

    // Create a temp change off the base.
    commit::jj_run_in(
        repo_root,
        &["new", &base_rev, "--no-edit", "-m", &description],
    )?;

    // Find the temp change (latest empty child of base).
    let temp_change = commit::jj_capture_in(
        repo_root,
        &[
            "log",
            "-r",
            &format!("latest(children({}) & empty())", base_rev),
            "--no-graph",
            "-T",
            "change_id",
        ],
    )?
    .trim()
    .to_string();
    if temp_change.is_empty() {
        bail!("Failed to create temp change for rise PR");
    }

    let result = (|| -> Result<()> {
        // Restore changed files from original into temp.
        let mut restore_args: Vec<&str> =
            vec!["restore", "--from", &original_change, "--to", &temp_change];
        for f in &changed_files {
            restore_args.push(f);
        }
        commit::jj_run_in(repo_root, &restore_args)?;

        // Get the git commit SHA for the temp change.
        let sha = commit::jj_capture_in(
            repo_root,
            &["log", "-r", &temp_change, "--no-graph", "-T", "commit_id"],
        )?
        .trim()
        .to_string();

        let existing_pr = commit::gh_find_open_pr_by_head(repo_root, base_repo, &head_ref)?;
        if let Some((_number, url)) = &existing_pr {
            println!("Updating existing PR: {}", url);
            push_branch(repo_root, &branch_name, &sha, true)?;
            println!("Force-pushed {} to {}", short_sha(&sha), &branch_name);
            if !opts.no_open {
                let _ = commit::open_in_browser(url);
            }
            return Ok(());
        }

        push_branch(repo_root, &branch_name, &sha, true)?;
        println!("Pushed bookmark {}", &branch_name);

        let (_number, url) = commit::gh_create_pr(
            repo_root, base_repo, &head_ref, &opts.base, &title, &body, opts.draft,
        )?;
        println!("Created PR: {}", url);
        if !opts.no_open {
            let _ = commit::open_in_browser(&url);
        }
        Ok(())
    })();

    // Always abandon the temp change.
    let _ = commit::jj_run_in(repo_root, &["abandon", &temp_change]);

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_derive_branch_name() {
        assert_eq!(
            derive_branch_name("Reduce latency with external engine"),
            "reduce-latency-with-external-engine"
        );
        assert_eq!(derive_branch_name("A/B.C"), "a-b.c");
    }

    #[test]
    fn test_head_ref_fork() {
        assert_eq!(
            head_ref("jj-vcs/jj", Some("nikivdev/jj"), "my-branch"),
            "nikivdev:my-branch"
        );
        assert_eq!(
            head_ref("nikivdev/jj", Some("nikivdev/jj"), "my-branch"),
            "my-branch"
        );
        assert_eq!(head_ref("x/y", None, "b"), "b");
    }

    #[test]
    fn test_parse_pr_args() {
        let p = parse_pr_args(&[]);
        assert!(!p.preview);
        assert!(p.title.is_none());

        let p = parse_pr_args(&[String::from("preview")]);
        assert!(p.preview);
        assert!(p.title.is_none());

        let p = parse_pr_args(&[String::from("preview"), String::from("hello world")]);
        assert!(p.preview);
        assert_eq!(p.title.as_deref(), Some("hello world"));

        let p = parse_pr_args(&[
            String::from("preview"),
            String::from("reduce"),
            String::from("latency"),
        ]);
        assert!(p.preview);
        assert_eq!(p.title.as_deref(), Some("reduce latency"));
    }
}
