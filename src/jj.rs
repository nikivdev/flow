use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::cli::{
    JjAction, JjBookmarkAction, JjCommand, JjPushOpts, JjRebaseOpts, JjStatusOpts, JjSyncOpts,
    JjWorkspaceAction,
};
use crate::config;
use crate::vcs;

struct JjContext {
    workspace_root: PathBuf,
    repo_root: PathBuf,
}

pub fn run(cmd: JjCommand) -> Result<()> {
    match cmd
        .action
        .unwrap_or(JjAction::Status(JjStatusOpts::default()))
    {
        JjAction::Init { path } => run_init(path),
        JjAction::Status(opts) => run_status(opts),
        JjAction::Fetch => run_fetch(),
        JjAction::Rebase(opts) => run_rebase(opts),
        JjAction::Push(opts) => run_push(opts),
        JjAction::Sync(opts) => run_sync(opts),
        JjAction::Workspace(action) => run_workspace(action),
        JjAction::Bookmark(action) => run_bookmark(action),
    }
}

pub fn run_workflow_status(raw: bool) -> Result<()> {
    run_status(JjStatusOpts { raw })
}

fn run_init(path: Option<PathBuf>) -> Result<()> {
    vcs::ensure_jj_installed()?;
    let root = path.unwrap_or(std::env::current_dir().context("failed to read current dir")?);
    let root = root.canonicalize().unwrap_or(root);

    if is_jj_repo(&root) {
        println!("JJ already initialized at {}", root.display());
        return Ok(());
    }

    let has_git = root.join(".git").exists();
    if has_git {
        jj_run_in(&root, &["git", "init", "--colocate"])?;
    } else {
        jj_run_in(&root, &["git", "init"])?;
    }

    let repo_root = vcs::ensure_jj_repo_in(&root)?;
    let branch = default_branch(&repo_root);
    let remote = default_remote(&repo_root);
    let auto_track = auto_track_enabled(&repo_root);

    if jj_run_in(&repo_root, &["git", "fetch"]).is_err() {
        println!("⚠ jj git fetch failed (no remote yet?)");
        return Ok(());
    }

    if auto_track {
        let track_ref = format!("{}@{}", branch, remote);
        if jj_run_in(&repo_root, &["bookmark", "track", &track_ref]).is_err() {
            println!("⚠ Failed to track {}", track_ref);
        }
    }

    println!("✓ JJ initialized (colocated: {})", has_git);
    Ok(())
}

fn current_context() -> Result<JjContext> {
    let workspace_root = vcs::ensure_jj_repo()?;
    let repo_root = repo_root_for_workspace(&workspace_root)?;
    Ok(JjContext {
        workspace_root,
        repo_root,
    })
}

fn repo_root_for_workspace(workspace_root: &Path) -> Result<PathBuf> {
    let git_root = jj_capture_in(workspace_root, &["git", "root"])?;
    repo_root_from_git_root(git_root.trim()).map(PathBuf::from)
}

fn repo_root_from_git_root(git_root: &str) -> Result<&str> {
    let trimmed = git_root.trim();
    if trimmed.is_empty() {
        bail!("jj git root returned an empty path");
    }
    if let Some(parent) = trimmed.strip_suffix("/.git") {
        return Ok(parent);
    }
    if let Some(parent) = trimmed.strip_suffix("\\.git") {
        return Ok(parent);
    }
    Ok(trimmed)
}

fn run_status(opts: JjStatusOpts) -> Result<()> {
    let ctx = current_context()?;
    if opts.raw {
        return jj_run_in(&ctx.workspace_root, &["status"]);
    }

    let snapshot = collect_status_snapshot(&ctx)?;
    print_status_snapshot(&snapshot);
    Ok(())
}

fn run_fetch() -> Result<()> {
    let ctx = current_context()?;
    ensure_git_not_busy(&ctx.repo_root)?;
    jj_run_in(&ctx.workspace_root, &["git", "fetch"])
}

fn run_rebase(opts: JjRebaseOpts) -> Result<()> {
    let ctx = current_context()?;
    ensure_git_not_busy(&ctx.repo_root)?;
    let remote = default_remote(&ctx.repo_root);
    let dest = opts.dest.unwrap_or_else(|| default_branch(&ctx.repo_root));
    let target = resolve_rebase_target(&ctx.workspace_root, &dest, &remote);
    jj_run_in(&ctx.workspace_root, &["rebase", "-d", &target])
}

fn run_push(opts: JjPushOpts) -> Result<()> {
    let ctx = current_context()?;
    ensure_git_not_busy(&ctx.repo_root)?;
    if opts.all {
        return jj_run_in(&ctx.workspace_root, &["git", "push", "--all"]);
    }
    let Some(bookmark) = opts.bookmark else {
        bail!("Specify a bookmark or pass --all");
    };
    jj_run_in(
        &ctx.workspace_root,
        &["git", "push", "--bookmark", &bookmark],
    )
}

fn run_sync(opts: JjSyncOpts) -> Result<()> {
    let ctx = current_context()?;
    ensure_git_not_busy(&ctx.repo_root)?;
    let remote = opts
        .remote
        .unwrap_or_else(|| default_remote(&ctx.repo_root));
    let dest = opts.dest.unwrap_or_else(|| default_branch(&ctx.repo_root));

    jj_run_in(&ctx.workspace_root, &["git", "fetch"])?;
    let target = resolve_rebase_target(&ctx.workspace_root, &dest, &remote);
    jj_run_in(&ctx.workspace_root, &["rebase", "-d", &target])?;

    // Check for conflicts after rebase
    let has_conflicts = jj_capture_in(
        &ctx.workspace_root,
        &["log", "-r", "conflicts()", "--no-graph", "-T", "commit_id"],
    )
    .map(|out| !out.trim().is_empty())
    .unwrap_or(false);
    if has_conflicts {
        let details = jj_capture_in(
            &ctx.workspace_root,
            &["log", "-r", "conflicts()", "--no-graph"],
        )
        .unwrap_or_default();
        eprintln!("\n⚠ Rebase produced conflicts:");
        for line in details.lines().filter(|l| !l.trim().is_empty()) {
            eprintln!("  {}", line.trim());
        }
        eprintln!("\nResolve with: jj resolve");
    }

    if opts.no_push {
        return Ok(());
    }

    let Some(bookmark) = opts.bookmark else {
        return Ok(());
    };
    jj_run_in(
        &ctx.workspace_root,
        &["git", "push", "--bookmark", &bookmark],
    )
}

fn run_workspace(action: JjWorkspaceAction) -> Result<()> {
    let ctx = current_context()?;
    match action {
        JjWorkspaceAction::List => jj_run_in(&ctx.workspace_root, &["workspace", "list"]),
        JjWorkspaceAction::Add { name, path, rev } => {
            let workspace_path = match path {
                Some(p) => p,
                None => workspace_default_path(&ctx.repo_root, &name)?,
            };
            run_workspace_add(&ctx.workspace_root, &name, workspace_path, rev.as_deref())
        }
        JjWorkspaceAction::Lane {
            name,
            path,
            base,
            remote,
            no_fetch,
        } => {
            ensure_git_not_busy(&ctx.repo_root)?;
            let remote = remote.unwrap_or_else(|| default_remote(&ctx.repo_root));
            if !no_fetch {
                if let Err(err) = jj_run_in(&ctx.workspace_root, &["git", "fetch"]) {
                    eprintln!("⚠ jj git fetch failed: {err}");
                    eprintln!("  continuing with current local refs");
                }
            }
            let workspace_path = match path {
                Some(p) => p,
                None => workspace_default_path(&ctx.repo_root, &name)?,
            };
            let base_rev = base.unwrap_or_else(|| {
                let dest = default_branch(&ctx.repo_root);
                resolve_rebase_target(&ctx.workspace_root, &dest, &remote)
            });
            run_workspace_add(
                &ctx.workspace_root,
                &name,
                workspace_path.clone(),
                Some(&base_rev),
            )?;
            println!("Lane {} is anchored at {}", name, base_rev);
            println!("Next: cd {}", workspace_path.display());
            println!(
                "Optional bookmark: f jj bookmark create {} --rev @ --track --remote {}",
                name, remote
            );
            Ok(())
        }
        JjWorkspaceAction::Review {
            branch,
            path,
            base,
            remote,
            no_fetch,
        } => {
            let remote = remote.unwrap_or_else(|| default_remote(&ctx.repo_root));
            if !no_fetch {
                ensure_git_not_busy(&ctx.repo_root)?;
                if let Err(err) = jj_run_in(&ctx.workspace_root, &["git", "fetch"]) {
                    eprintln!("⚠ jj git fetch failed: {err}");
                    eprintln!("  continuing with current local refs");
                }
            }

            let workspace_name = review_workspace_name(&branch);
            if workspace_name.is_empty() {
                bail!("Invalid review branch name: {}", branch);
            }

            let workspace_path = match path {
                Some(p) => p,
                None => workspace_default_path(&ctx.repo_root, &workspace_name)?,
            };

            if let Some(existing_path) =
                existing_workspace_path(&ctx.workspace_root, &ctx.repo_root, &workspace_name)?
            {
                if existing_path != workspace_path {
                    bail!(
                        "Workspace {} already exists at {}",
                        workspace_name,
                        existing_path.display()
                    );
                }
                println!(
                    "Reusing review workspace {} at {}",
                    workspace_name,
                    existing_path.display()
                );
            } else {
                let resolution = resolve_review_workspace_base(
                    &ctx.repo_root,
                    &branch,
                    &remote,
                    base.as_deref(),
                );
                run_workspace_add(
                    &ctx.workspace_root,
                    &workspace_name,
                    workspace_path.clone(),
                    Some(&resolution.rev),
                )?;
                println!(
                    "Review branch {} resolved via {}",
                    branch, resolution.source
                );
            }

            println!("Next: cd {}", workspace_path.display());
            println!(
                "Use `jj` / `f jj` inside this workspace. Git commands still point at the colocated main checkout."
            );
            println!(
                "When you are ready to publish from JJ: f jj bookmark create {} --rev @ --track --remote {}",
                branch, remote
            );
            Ok(())
        }
    }
}

fn run_workspace_add(
    repo_root: &Path,
    name: &str,
    workspace_path: PathBuf,
    rev: Option<&str>,
) -> Result<()> {
    if let Some(parent) = workspace_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let path_str = workspace_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("invalid workspace path"))?
        .to_string();
    let args = workspace_add_args(&path_str, name, rev);
    jj_run_owned_in(repo_root, &args)?;
    if let Some(rev) = rev.filter(|v| !v.trim().is_empty()) {
        println!(
            "Created workspace {} at {} (base: {})",
            name,
            workspace_path.display(),
            rev.trim()
        );
    } else {
        println!("Created workspace {} at {}", name, workspace_path.display());
    }
    Ok(())
}

fn workspace_add_args(destination: &str, name: &str, rev: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "workspace".to_string(),
        "add".to_string(),
        destination.to_string(),
        "--name".to_string(),
        name.to_string(),
    ];
    if let Some(rev) = rev {
        let trimmed = rev.trim();
        if !trimmed.is_empty() {
            args.push("--revision".to_string());
            args.push(trimmed.to_string());
        }
    }
    args
}

fn run_bookmark(action: JjBookmarkAction) -> Result<()> {
    let ctx = current_context()?;
    match action {
        JjBookmarkAction::List => jj_run_in(&ctx.workspace_root, &["bookmark", "list"]),
        JjBookmarkAction::Track { name, remote } => {
            let remote = remote.unwrap_or_else(|| default_remote(&ctx.repo_root));
            let track_ref = format!("{}@{}", name, remote);
            jj_run_in(&ctx.workspace_root, &["bookmark", "track", &track_ref])
        }
        JjBookmarkAction::Create {
            name,
            rev,
            track,
            remote,
        } => {
            let rev = rev.unwrap_or_else(|| "@".to_string());
            jj_run_in(
                &ctx.workspace_root,
                &["bookmark", "create", &name, "-r", &rev],
            )?;

            let should_track = track.unwrap_or_else(|| auto_track_enabled(&ctx.repo_root));
            if should_track {
                let remote = remote.unwrap_or_else(|| default_remote(&ctx.repo_root));
                let track_ref = format!("{}@{}", name, remote);
                if jj_run_in(&ctx.workspace_root, &["bookmark", "track", &track_ref]).is_err() {
                    println!("⚠ Failed to track {}", track_ref);
                }
            }
            Ok(())
        }
    }
}

struct WorkflowStatusSnapshot {
    workspace_root: PathBuf,
    repo_root: PathBuf,
    workspace_name: String,
    current_ref: String,
    current_role: &'static str,
    home_branch: String,
    intake_branch: String,
    remote: String,
    trunk_ref: String,
    home_unique_to_trunk: usize,
    trunk_unique_to_home: usize,
    leaves: Vec<LeafBranchStatus>,
    workspaces: Vec<WorkspaceStatus>,
    working_copy_lines: Vec<String>,
}

struct LeafBranchStatus {
    name: String,
    kind: &'static str,
    unique_commits: usize,
    tracked_remote: bool,
    workspace_name: Option<String>,
    is_current: bool,
}

struct WorkspaceStatus {
    name: String,
    is_current: bool,
    path_exists: bool,
}

fn collect_status_snapshot(ctx: &JjContext) -> Result<WorkflowStatusSnapshot> {
    let default_branch = default_branch(&ctx.repo_root);
    let remote = default_remote(&ctx.repo_root);
    let workspace_name = current_workspace_name(&ctx.workspace_root)?;
    let current_bookmarks = local_bookmarks_at_rev(&ctx.workspace_root, "@");
    let parent_bookmarks = local_bookmarks_at_rev(&ctx.workspace_root, "@-");
    let all_bookmarks = jj_bookmark_names(&ctx.workspace_root)?;
    let all_local_bookmarks = all_bookmarks
        .iter()
        .filter(|name| !name.contains('@'))
        .cloned()
        .collect::<HashSet<_>>();
    let current_git_branch = git_current_branch(&ctx.repo_root).unwrap_or_default();
    let home_branch = infer_home_branch(
        &ctx.repo_root,
        &default_branch,
        &current_bookmarks,
        &parent_bookmarks,
        &all_local_bookmarks,
    );
    let intake_branch = derive_intake_branch(&home_branch);
    let current_ref = infer_current_ref(
        &workspace_name,
        &current_git_branch,
        &home_branch,
        &intake_branch,
        &default_branch,
        &current_bookmarks,
        &parent_bookmarks,
    );
    let current_role = classify_branch_role(&current_ref, &home_branch, &intake_branch);
    let remote_trunk_ref = format!("{default_branch}@{remote}");
    let trunk_ref = if all_bookmarks.contains(&remote_trunk_ref) {
        remote_trunk_ref
    } else {
        default_branch.clone()
    };
    let leaf_names: Vec<String> = all_local_bookmarks
        .iter()
        .filter(|name| is_leaf_branch(name))
        .cloned()
        .collect();
    let workspace_names = jj_workspace_names(&ctx.workspace_root)?;
    let mut leaves = Vec::new();
    for name in leaf_names {
        let workspace_name_for_branch = workspace_name_for_branch(&name)
            .filter(|candidate| workspace_names.contains(candidate));
        leaves.push(LeafBranchStatus {
            kind: leaf_branch_kind(&name),
            unique_commits: count_unique_commits(&ctx.workspace_root, &name, &home_branch),
            tracked_remote: all_bookmarks.contains(&format!("{name}@{remote}")),
            workspace_name: workspace_name_for_branch,
            is_current: name == current_ref,
            name,
        });
    }
    leaves.sort_by(|left, right| left.name.cmp(&right.name));

    let mut workspaces: Vec<WorkspaceStatus> = workspace_names
        .into_iter()
        .map(|name| WorkspaceStatus {
            path_exists: inferred_workspace_path(&ctx.repo_root, &name).exists(),
            is_current: name == workspace_name,
            name,
        })
        .collect();
    workspaces.sort_by(|left, right| left.name.cmp(&right.name));

    let working_copy_output = jj_capture_in(&ctx.workspace_root, &["status"])?;
    Ok(WorkflowStatusSnapshot {
        workspace_root: ctx.workspace_root.clone(),
        repo_root: ctx.repo_root.clone(),
        workspace_name,
        current_ref,
        current_role,
        home_branch: home_branch.clone(),
        intake_branch,
        remote,
        trunk_ref: trunk_ref.clone(),
        home_unique_to_trunk: count_unique_commits(&ctx.workspace_root, &home_branch, &trunk_ref),
        trunk_unique_to_home: count_unique_commits(&ctx.workspace_root, &trunk_ref, &home_branch),
        leaves,
        workspaces,
        working_copy_lines: working_copy_output
            .lines()
            .map(|line| line.to_string())
            .collect(),
    })
}

fn print_status_snapshot(snapshot: &WorkflowStatusSnapshot) {
    println!("JJ Workflow Status");
    println!();
    println!("Repo:       {}", snapshot.repo_root.display());
    println!(
        "Workspace:  {} ({})",
        snapshot.workspace_name,
        snapshot.workspace_root.display()
    );
    println!(
        "Current:    {} [{}]",
        snapshot.current_ref, snapshot.current_role
    );
    println!(
        "Home:       {} ({} commit(s) not in {})",
        snapshot.home_branch, snapshot.home_unique_to_trunk, snapshot.trunk_ref
    );
    println!("Intake:     {}", snapshot.intake_branch);
    println!(
        "Trunk:      {} ({} commit(s) not in {})",
        snapshot.trunk_ref, snapshot.trunk_unique_to_home, snapshot.home_branch
    );
    println!("Remote:     {}", snapshot.remote);
    println!();
    println!("Leaf Branches:");
    if snapshot.leaves.is_empty() {
        println!("  none");
    } else {
        for leaf in &snapshot.leaves {
            let current = if leaf.is_current { " current" } else { "" };
            let tracked = if leaf.tracked_remote {
                format!(" tracked@{}", snapshot.remote)
            } else {
                " local-only".to_string()
            };
            let workspace = leaf
                .workspace_name
                .as_deref()
                .map(|name| format!(" workspace={name}"))
                .unwrap_or_default();
            println!(
                "  {} [{}] {} commit(s) over {}{}{}{}",
                leaf.name,
                leaf.kind,
                leaf.unique_commits,
                snapshot.home_branch,
                tracked,
                workspace,
                current
            );
        }
    }
    println!();
    println!("Workspaces:");
    for workspace in &snapshot.workspaces {
        let marker = if workspace.is_current { "*" } else { "-" };
        let suffix = if workspace.path_exists {
            ""
        } else {
            " (expected path missing)"
        };
        println!("  {} {}{}", marker, workspace.name, suffix);
    }
    println!();
    println!("Working Copy:");
    for line in &snapshot.working_copy_lines {
        println!("  {}", line);
    }
    println!();
    println!("Suggested Next:");
    if snapshot.current_role == "review" || snapshot.current_role == "codex" {
        println!("  f jj push --bookmark {}", snapshot.current_ref);
        println!(
            "  jj rebase -b {} -o {}",
            snapshot.current_ref, snapshot.home_branch
        );
    } else {
        println!("  f jj sync --bookmark {}", snapshot.home_branch);
        println!(
            "  f jj workspace review review/{}-topic",
            snapshot.home_branch
        );
    }
}

fn current_workspace_name(workspace_root: &Path) -> Result<String> {
    let output = jj_capture_in(
        workspace_root,
        &["log", "-r", "@", "--no-graph", "-T", "working_copies"],
    )?;
    let trimmed = output.trim().trim_end_matches('@').trim();
    if trimmed.is_empty() {
        Ok("default".to_string())
    } else {
        Ok(trimmed.to_string())
    }
}

fn configured_home_branch(repo_root: &Path) -> Option<String> {
    load_jj_config(repo_root)
        .and_then(|cfg| cfg.home_branch)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn git_current_branch(repo_root: &Path) -> Option<String> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(["branch", "--show-current"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() { None } else { Some(value) }
}

fn infer_home_branch(
    repo_root: &Path,
    default_branch: &str,
    current_bookmarks: &[String],
    parent_bookmarks: &[String],
    all_bookmarks: &HashSet<String>,
) -> String {
    if let Some(home_branch) = configured_home_branch(repo_root) {
        return home_branch;
    }
    if let Some(git_branch) =
        git_current_branch(repo_root).filter(|name| is_home_branch_candidate(name, default_branch))
    {
        return git_branch;
    }
    for bookmark in current_bookmarks
        .iter()
        .chain(parent_bookmarks.iter())
        .chain(all_bookmarks.iter())
    {
        if is_home_branch_candidate(bookmark, default_branch) {
            return bookmark.clone();
        }
    }
    default_branch.to_string()
}

fn derive_intake_branch(home_branch: &str) -> String {
    if home_branch.trim().is_empty() || home_branch == "main" {
        "main-intake".to_string()
    } else {
        format!("{home_branch}-main-intake")
    }
}

fn infer_current_ref(
    workspace_name: &str,
    current_git_branch: &str,
    home_branch: &str,
    intake_branch: &str,
    default_branch: &str,
    current_bookmarks: &[String],
    parent_bookmarks: &[String],
) -> String {
    let candidates: Vec<String> = current_bookmarks
        .iter()
        .chain(parent_bookmarks.iter())
        .cloned()
        .collect();
    if let Some(match_by_workspace) = candidates
        .iter()
        .find(|name| workspace_name_for_branch(name).as_deref() == Some(workspace_name))
    {
        return match_by_workspace.clone();
    }
    if workspace_name == "default"
        && !current_git_branch.is_empty()
        && !is_leaf_branch(current_git_branch)
    {
        return current_git_branch.to_string();
    }
    if let Some(leaf) = candidates.iter().find(|name| is_leaf_branch(name)) {
        return leaf.clone();
    }
    if candidates.iter().any(|name| name == home_branch) {
        return home_branch.to_string();
    }
    if candidates.iter().any(|name| name == intake_branch) {
        return intake_branch.to_string();
    }
    if let Some(plain) = candidates
        .iter()
        .find(|name| is_home_branch_candidate(name, default_branch))
    {
        return plain.clone();
    }
    if !home_branch.is_empty() {
        return home_branch.to_string();
    }
    default_branch.to_string()
}

fn classify_branch_role(name: &str, home_branch: &str, intake_branch: &str) -> &'static str {
    if name == home_branch {
        "home"
    } else if name == intake_branch {
        "intake"
    } else if name.starts_with("review/") {
        "review"
    } else if name.starts_with("codex/") {
        "codex"
    } else {
        "other"
    }
}

fn is_home_branch_candidate(name: &str, default_branch: &str) -> bool {
    !name.trim().is_empty()
        && !is_leaf_branch(name)
        && !is_intake_branch(name)
        && name != default_branch
        && !name.contains('@')
}

fn is_intake_branch(name: &str) -> bool {
    name == "main-intake" || name.ends_with("-main-intake")
}

fn is_leaf_branch(name: &str) -> bool {
    name.starts_with("review/") || name.starts_with("codex/")
}

fn leaf_branch_kind(name: &str) -> &'static str {
    if name.starts_with("review/") {
        "review"
    } else {
        "codex"
    }
}

fn workspace_name_for_branch(name: &str) -> Option<String> {
    let value = review_workspace_name(name);
    if value.is_empty() { None } else { Some(value) }
}

fn local_bookmarks_at_rev(workspace_root: &Path, rev: &str) -> Vec<String> {
    let output = jj_capture_in(
        workspace_root,
        &["log", "-r", rev, "--no-graph", "-T", "bookmarks"],
    )
    .unwrap_or_default();
    parse_bookmark_tokens(&output)
}

fn jj_bookmark_names(workspace_root: &Path) -> Result<HashSet<String>> {
    let output = jj_capture_in(
        workspace_root,
        &[
            "bookmark",
            "list",
            "--all-remotes",
            "-T",
            "if(remote, name ++ \"@\" ++ remote, name) ++ \"\\n\"",
        ],
    )?;
    Ok(parse_bookmark_list_names(&output)
        .into_iter()
        .collect::<HashSet<_>>())
}

fn parse_bookmark_tokens(output: &str) -> Vec<String> {
    output
        .split_whitespace()
        .filter(|token| !token.contains('@'))
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
        .collect()
}

fn parse_bookmark_list_names(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return None;
            }
            let name = trimmed
                .split_once(':')
                .map(|(name, _)| name.trim())
                .unwrap_or(trimmed);
            Some(name.to_string())
        })
        .filter(|name| !name.is_empty())
        .collect()
}

fn jj_workspace_names(workspace_root: &Path) -> Result<HashSet<String>> {
    let output = jj_capture_in(
        workspace_root,
        &["workspace", "list", "-T", "name ++ \"\\n\""],
    )?;
    Ok(parse_workspace_names(&output).into_iter().collect())
}

fn parse_workspace_names(output: &str) -> Vec<String> {
    output
        .lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn inferred_workspace_path(repo_root: &Path, name: &str) -> PathBuf {
    if name == "default" {
        repo_root.to_path_buf()
    } else {
        workspace_default_path(repo_root, name).unwrap_or_else(|_| repo_root.to_path_buf())
    }
}

fn count_unique_commits(workspace_root: &Path, branch: &str, base: &str) -> usize {
    if branch == base {
        return 0;
    }
    let revset = format!("ancestors({branch}) ~ ancestors({base})");
    jj_capture_in(
        workspace_root,
        &[
            "log",
            "-r",
            &revset,
            "--no-graph",
            "-T",
            "commit_id ++ \"\\n\"",
        ],
    )
    .map(|output| {
        output
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count()
    })
    .unwrap_or(0)
}

fn resolve_rebase_target(repo_root: &Path, dest: &str, remote: &str) -> String {
    if jj_bookmark_exists(repo_root, dest) {
        dest.to_string()
    } else {
        format!("{}@{}", dest, remote)
    }
}

fn jj_bookmark_exists(repo_root: &Path, name: &str) -> bool {
    jj_bookmark_names(repo_root)
        .map(|bookmarks| bookmarks.contains(name))
        .unwrap_or(false)
}

fn default_branch(repo_root: &Path) -> String {
    if let Some(cfg) = load_jj_config(repo_root) {
        if let Some(branch) = cfg.default_branch {
            return branch;
        }
    }
    if git_ref_exists(repo_root, "refs/heads/main")
        || git_ref_exists(repo_root, "refs/remotes/origin/main")
    {
        return "main".to_string();
    }
    if git_ref_exists(repo_root, "refs/heads/master")
        || git_ref_exists(repo_root, "refs/remotes/origin/master")
    {
        return "master".to_string();
    }
    "main".to_string()
}

fn default_remote(repo_root: &Path) -> String {
    config::preferred_git_remote_for_repo(repo_root)
}

fn auto_track_enabled(repo_root: &Path) -> bool {
    load_jj_config(repo_root)
        .and_then(|cfg| cfg.auto_track)
        .unwrap_or(false)
}

fn load_jj_config(repo_root: &Path) -> Option<config::JjConfig> {
    let local = repo_root.join("flow.toml");
    if local.exists() {
        if let Ok(cfg) = config::load(&local) {
            if cfg.jj.is_some() {
                return cfg.jj;
            }
        }
    }
    let global = config::default_config_path();
    if global.exists() {
        if let Ok(cfg) = config::load(&global) {
            if cfg.jj.is_some() {
                return cfg.jj;
            }
        }
    }
    None
}

fn review_workspace_name(branch: &str) -> String {
    let trimmed = branch.trim().trim_matches('/');
    let mut out = String::new();
    let mut previous_was_dash = false;
    for ch in trimmed.chars() {
        let normalized = if ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' || ch == '-' {
            previous_was_dash = false;
            ch
        } else {
            if previous_was_dash {
                continue;
            }
            previous_was_dash = true;
            '-'
        };
        out.push(normalized);
    }
    out.trim_matches('-').to_string()
}

fn existing_workspace_path(
    workspace_root: &Path,
    repo_root: &Path,
    name: &str,
) -> Result<Option<PathBuf>> {
    if !jj_workspace_names(workspace_root)?.contains(name) {
        return Ok(None);
    }
    Ok(Some(inferred_workspace_path(repo_root, name)))
}

struct ReviewWorkspaceBase {
    rev: String,
    source: String,
}

fn resolve_review_workspace_base(
    repo_root: &Path,
    branch: &str,
    remote: &str,
    explicit_base: Option<&str>,
) -> ReviewWorkspaceBase {
    if let Some(base) = explicit_base.map(str::trim).filter(|base| !base.is_empty()) {
        return ReviewWorkspaceBase {
            rev: base.to_string(),
            source: format!("explicit base {}", base),
        };
    }

    if let Some(commit) = git_ref_commit(repo_root, &format!("refs/heads/{branch}")) {
        return ReviewWorkspaceBase {
            rev: commit.clone(),
            source: format!("local branch {branch} ({})", short_commit(&commit)),
        };
    }

    let remote_ref = format!("refs/remotes/{remote}/{branch}");
    if let Some(commit) = git_ref_commit(repo_root, &remote_ref) {
        return ReviewWorkspaceBase {
            rev: commit.clone(),
            source: format!(
                "remote branch {remote}/{branch} ({})",
                short_commit(&commit)
            ),
        };
    }

    let dest = default_branch(repo_root);
    let fallback = resolve_rebase_target(repo_root, &dest, remote);
    ReviewWorkspaceBase {
        rev: fallback.clone(),
        source: format!("fallback trunk {}", fallback),
    }
}

fn git_ref_commit(repo_root: &Path, reference: &str) -> Option<String> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(["rev-parse", reference])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if sha.is_empty() { None } else { Some(sha) }
}

fn short_commit(commit: &str) -> &str {
    const SHORT_COMMIT_LEN: usize = 12;
    let end = commit
        .char_indices()
        .nth(SHORT_COMMIT_LEN)
        .map(|(idx, _)| idx)
        .unwrap_or(commit.len());
    &commit[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn default_remote_uses_git_remote_when_set() {
        let dir = tempdir().expect("tempdir");
        let repo_root = dir.path();

        std::fs::write(
            repo_root.join("flow.toml"),
            "[git]\nremote = \"myflow-i\"\n",
        )
        .expect("write flow.toml");

        assert_eq!(default_remote(repo_root), "myflow-i");
    }

    #[test]
    fn workspace_add_args_use_modern_jj_shape() {
        let args = workspace_add_args("/tmp/ws-fix-otp", "fix-otp", None);
        assert_eq!(
            args,
            vec!["workspace", "add", "/tmp/ws-fix-otp", "--name", "fix-otp",]
        );
    }

    #[test]
    fn workspace_add_args_include_revision_when_set() {
        let args = workspace_add_args("/tmp/ws-testflight", "testflight", Some("main@upstream"));
        assert_eq!(
            args,
            vec![
                "workspace",
                "add",
                "/tmp/ws-testflight",
                "--name",
                "testflight",
                "--revision",
                "main@upstream",
            ]
        );
    }

    #[test]
    fn review_workspace_name_sanitizes_review_branch() {
        assert_eq!(
            review_workspace_name("review/nikiv-designer-reactron-rs-rust-first"),
            "review-nikiv-designer-reactron-rs-rust-first"
        );
        assert_eq!(
            review_workspace_name("/review//messy branch/"),
            "review-messy-branch"
        );
    }

    #[test]
    fn resolve_review_workspace_base_prefers_explicit_base() {
        let dir = tempdir().expect("tempdir");
        let repo_root = dir.path();

        let resolved =
            resolve_review_workspace_base(repo_root, "review/demo", "origin", Some("main@origin"));

        assert_eq!(resolved.rev, "main@origin");
        assert_eq!(resolved.source, "explicit base main@origin");
    }

    #[test]
    fn resolve_review_workspace_base_uses_local_branch_commit() {
        let dir = tempdir().expect("tempdir");
        let repo_root = dir.path();
        init_git_repo(repo_root);

        git(repo_root, &["checkout", "-q", "-b", "review/demo"]);
        let expected = git_capture(repo_root, &["rev-parse", "refs/heads/review/demo"]);

        let resolved = resolve_review_workspace_base(repo_root, "review/demo", "origin", None);

        assert_eq!(resolved.rev, expected);
        assert!(resolved.source.starts_with("local branch review/demo ("));
    }

    #[test]
    fn parse_workspace_names_skips_blank_lines() {
        let parsed = parse_workspace_names("\ndefault\nreview-demo\n\n");

        assert_eq!(parsed, vec!["default", "review-demo"]);
    }

    #[test]
    fn parse_bookmark_list_names_supports_structured_template_output() {
        let parsed = parse_bookmark_list_names("main\nmain@origin\nreview/demo\n");

        assert_eq!(parsed, vec!["main", "main@origin", "review/demo"]);
    }

    #[test]
    fn repo_root_from_git_root_handles_colocated_git_dir() {
        let repo_root = repo_root_from_git_root("/tmp/prom/.git").expect("repo root");
        assert_eq!(repo_root, "/tmp/prom");
    }

    #[test]
    fn infer_current_ref_prefers_workspace_named_leaf_branch() {
        let current = infer_current_ref(
            "review-nikiv-feature",
            "nikiv",
            "nikiv",
            "nikiv-main-intake",
            "main",
            &[],
            &[String::from("nikiv"), String::from("review/nikiv-feature")],
        );

        assert_eq!(current, "review/nikiv-feature");
    }

    fn init_git_repo(repo_root: &Path) {
        git(repo_root, &["init", "-q"]);
        git(repo_root, &["config", "user.name", "Flow Tests"]);
        git(
            repo_root,
            &["config", "user.email", "flow-tests@example.com"],
        );
        std::fs::write(repo_root.join("README.md"), "init\n").expect("write README");
        git(repo_root, &["add", "README.md"]);
        git(repo_root, &["commit", "-q", "-m", "init"]);
    }

    fn git(repo_root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(repo_root)
            .args(args)
            .status()
            .expect("run git");
        assert!(status.success(), "git {:?} failed", args);
    }

    fn git_capture(repo_root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(repo_root)
            .args(args)
            .output()
            .expect("run git");
        assert!(output.status.success(), "git {:?} failed", args);
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }
}

fn is_jj_repo(path: &Path) -> bool {
    Command::new("jj")
        .current_dir(path)
        .arg("root")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn jj_run_in(repo_root: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("jj")
        .current_dir(repo_root)
        .args(args)
        .output()
        .with_context(|| format!("failed to run jj {}", args.join(" ")))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.trim().is_empty() {
        print!("{}", stdout);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    for line in stderr.lines() {
        if line.contains("Refused to snapshot") {
            continue;
        }
        eprintln!("{}", line);
    }
    if !output.status.success() {
        bail!("jj {} failed", args.join(" "));
    }
    Ok(())
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

fn jj_run_owned_in(repo_root: &Path, args: &[String]) -> Result<()> {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    jj_run_in(repo_root, &refs)
}

fn git_ref_exists(repo_root: &Path, name: &str) -> bool {
    Command::new("git")
        .current_dir(repo_root)
        .args(["show-ref", "--verify", name])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn ensure_git_not_busy(repo_root: &Path) -> Result<()> {
    let git_dir = git_dir(repo_root)?;
    let rebase = git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists();
    let merge = git_dir.join("MERGE_HEAD").exists();
    let cherry_pick = git_dir.join("CHERRY_PICK_HEAD").exists();
    let revert = git_dir.join("REVERT_HEAD").exists();
    let bisect = git_dir.join("BISECT_LOG").exists();
    let unmerged = git_unmerged_files(repo_root);

    if rebase || merge || cherry_pick || revert || bisect || !unmerged.is_empty() {
        bail!("Git operation in progress. Run `f git-repair` first.");
    }
    Ok(())
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

fn workspace_default_path(repo_root: &Path, name: &str) -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    let repo_name = repo_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("repo");
    Ok(PathBuf::from(home)
        .join(".jj")
        .join("workspaces")
        .join(repo_name)
        .join(name))
}
