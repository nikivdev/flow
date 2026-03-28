use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::cli::{
    JjAction, JjBookmarkAction, JjCommand, JjOverviewOpts, JjPushOpts, JjRebaseOpts, JjStatusOpts,
    JjSyncOpts, JjWorkspaceAction,
};
use crate::config;
use crate::vcs;

struct JjContext {
    workspace_root: PathBuf,
    repo_root: PathBuf,
}

const JJ_OVERVIEW_CACHE_TTL_SECS: u64 = 20;

pub fn run(cmd: JjCommand) -> Result<()> {
    match cmd
        .action
        .unwrap_or(JjAction::Status(JjStatusOpts::default()))
    {
        JjAction::Init { path } => run_init(path),
        JjAction::Overview(opts) => run_overview(opts),
        JjAction::Status(opts) => run_status(opts),
        JjAction::Fetch => run_fetch(),
        JjAction::Rebase(opts) => run_rebase(opts),
        JjAction::Push(opts) => run_push(opts),
        JjAction::Sync(opts) => run_sync(opts),
        JjAction::Workspace(action) => run_workspace(action),
        JjAction::Bookmark(action) => run_bookmark(action),
    }
}

pub fn run_workflow_status(raw: bool, compact: bool) -> Result<()> {
    run_status(JjStatusOpts { raw, compact })
}

pub fn load_overview_for_path(path: Option<&Path>, op_limit: usize) -> Result<JjWorkflowOverview> {
    load_overview_for_path_with_cache(path, op_limit, true)
}

pub fn load_overview_for_path_uncached(
    path: Option<&Path>,
    op_limit: usize,
) -> Result<JjWorkflowOverview> {
    load_overview_for_path_with_cache(path, op_limit, false)
}

fn load_overview_for_path_with_cache(
    path: Option<&Path>,
    op_limit: usize,
    use_cache: bool,
) -> Result<JjWorkflowOverview> {
    let ctx = context_for_path(path)?;
    let op_limit = op_limit.clamp(1, 32);
    let cache_key = JjOverviewCacheKey {
        workspace_root: ctx.workspace_root.clone(),
        op_limit,
    };
    if use_cache && let Some(snapshot) = cached_overview(&cache_key) {
        return Ok(snapshot);
    }
    let workflow = collect_status_snapshot(&ctx)?;
    let recent_operations = recent_operations(&ctx.workspace_root, op_limit)?;
    let summary = workflow_summary(&workflow, &recent_operations);
    let stacks = stack_summaries(&workflow);
    let attention = attention_items(&workflow, &recent_operations);
    let overview = JjWorkflowOverview {
        generated_at_unix: now_unix_secs(),
        target_path: ctx.repo_root.display().to_string(),
        workflow,
        summary,
        attention,
        stacks,
        recent_operations,
    };
    if use_cache {
        store_cached_overview(cache_key, overview.clone());
    }
    Ok(overview)
}

fn run_overview(opts: JjOverviewOpts) -> Result<()> {
    let overview = load_overview_for_path(opts.path.as_deref(), opts.op_limit)?;
    if opts.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&overview).context("failed to encode JJ overview")?
        );
        return Ok(());
    }

    print_status_snapshot(&overview.workflow);
    println!();
    println!(
        "Summary: {} leaf(s) • {} tracked • {} stack(s) • {} attention item(s)",
        overview.summary.leaf_count,
        overview.summary.tracked_leaf_count,
        overview.summary.stack_count,
        overview.summary.attention_count
    );
    if !overview.attention.is_empty() {
        println!();
        println!("Attention:");
        for item in overview.attention.iter().take(5) {
            println!("  [{}] {} — {}", item.severity, item.title, item.detail);
        }
    }
    if !overview.recent_operations.is_empty() {
        println!();
        println!("Recent Operations:");
        for entry in &overview.recent_operations {
            let flag = if entry.risky { " !" } else { "" };
            println!("  {} [{}] {}{}", entry.id, entry.kind, entry.summary, flag);
        }
    }
    Ok(())
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
    context_for_path(None)
}

fn context_for_path(path: Option<&Path>) -> Result<JjContext> {
    let workspace_root = match path {
        Some(path) => {
            let start = resolve_context_path(path)?;
            vcs::ensure_jj_repo_in(&start)?
        }
        None => vcs::ensure_jj_repo()?,
    };
    let repo_root = repo_root_for_workspace(&workspace_root)?;
    Ok(JjContext {
        workspace_root,
        repo_root,
    })
}

fn resolve_context_path(path: &Path) -> Result<PathBuf> {
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("failed to read current dir")?
            .join(path)
    };
    Ok(resolved.canonicalize().unwrap_or(resolved))
}

fn repo_root_for_workspace(workspace_root: &Path) -> Result<PathBuf> {
    let git_root = jj_read_in(workspace_root, &["git", "root"])?;
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
    let workspace_root = vcs::ensure_jj_repo()?;
    let ctx = JjContext {
        repo_root: repo_root_for_workspace(&workspace_root)?,
        workspace_root,
    };
    if opts.raw {
        return jj_run_in(&ctx.workspace_root, &["status"]);
    }

    if opts.compact {
        let overview = load_overview_for_path(Some(&ctx.workspace_root), 8)?;
        print_compact_status(&overview);
        return Ok(());
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
    jj_run_in(&ctx.workspace_root, &["rebase", "-d", &target.target])
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
    let snapshot = collect_status_snapshot(&ctx)?;
    let remote = opts
        .remote
        .unwrap_or_else(|| default_remote(&ctx.repo_root));
    let dest = opts.dest.unwrap_or_else(|| default_branch(&ctx.repo_root));
    let initial_target = resolve_rebase_target(&ctx.workspace_root, &dest, &remote);
    let sync_plan = build_sync_plan(
        &snapshot,
        &remote,
        &dest,
        &initial_target,
        opts.bookmark.as_deref(),
        opts.no_push,
    );
    if opts.plan {
        print_sync_plan(&sync_plan);
        return Ok(());
    }

    jj_run_in(&ctx.workspace_root, &["git", "fetch"])?;
    let target = resolve_rebase_target(&ctx.workspace_root, &dest, &remote);
    let sync_mode = sync_plan.mode.clone().ok_or_else(|| {
        anyhow::anyhow!(
            sync_plan
                .blocked_reason
                .clone()
                .unwrap_or_else(|| "sync is blocked".to_string())
        )
    })?;
    let rebase_args = sync_rebase_args(&sync_mode, &target.target);
    jj_run_owned_in(&ctx.workspace_root, &rebase_args)?;
    if let SyncMode::RebaseHomeBookmark { home_branch } = &sync_mode {
        jj_run_in(
            &ctx.workspace_root,
            &["edit", "--ignore-immutable", home_branch],
        )?;
    }

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

#[derive(Debug, Clone, PartialEq, Eq)]
enum SyncMode {
    RebaseCurrentCheckout,
    RebaseHomeBookmark { home_branch: String },
}

#[derive(Debug, Clone)]
struct JjSyncPlan {
    mode: Option<SyncMode>,
    mode_label: String,
    reason: String,
    working_copy_effect: String,
    remote: String,
    requested_dest: String,
    resolved_target: String,
    resolved_target_reason: String,
    push_bookmark: Option<String>,
    blocked_reason: Option<String>,
    commands: Vec<String>,
}

#[derive(Debug, Clone)]
struct ResolvedRebaseTarget {
    target: String,
    reason: String,
}

fn determine_sync_mode(snapshot: &WorkflowStatusSnapshot) -> Result<SyncMode> {
    let is_default_home_checkout =
        snapshot.workspace_name == "default" && snapshot.current_role == "home";
    if !is_default_home_checkout {
        return Ok(SyncMode::RebaseCurrentCheckout);
    }
    if anonymous_home_checkout_requires_repair(
        snapshot.current_role,
        snapshot.current_commit_is_anonymous,
        snapshot.current_commit_conflicted,
        snapshot.working_copy_change_count,
    ) {
        let state = anonymous_checkout_label(snapshot.current_commit_conflicted);
        bail!(
            "Default home checkout is unsafe for `flow sync`: {state} on top of {}. Preserve or abandon this child, then `jj edit {}` before retrying.",
            snapshot.current_ref,
            snapshot.home_branch
        );
    }
    Ok(SyncMode::RebaseHomeBookmark {
        home_branch: snapshot.home_branch.clone(),
    })
}

fn build_sync_plan(
    snapshot: &WorkflowStatusSnapshot,
    remote: &str,
    requested_dest: &str,
    resolved_target: &ResolvedRebaseTarget,
    push_bookmark: Option<&str>,
    no_push: bool,
) -> JjSyncPlan {
    match determine_sync_mode(snapshot) {
        Ok(mode) => {
            let mode_label = match &mode {
                SyncMode::RebaseCurrentCheckout => {
                    format!("current checkout {}", snapshot.current_ref)
                }
                SyncMode::RebaseHomeBookmark { home_branch } => {
                    format!("home bookmark {}", home_branch)
                }
            };
            let reason = match &mode {
                SyncMode::RebaseCurrentCheckout => format!(
                    "{} lanes sync the current checkout instead of rebasing the home bookmark.",
                    snapshot.current_role
                ),
                SyncMode::RebaseHomeBookmark { home_branch } => {
                    if snapshot.current_commit_is_anonymous {
                        format!(
                            "default home lane is clean; sync will rebase {} and then re-anchor the working copy off the anonymous child.",
                            home_branch
                        )
                    } else {
                        format!(
                            "default home lane sync rebases {} directly so detached Git HEAD never decides the branch.",
                            home_branch
                        )
                    }
                }
            };
            let working_copy_effect = match &mode {
                SyncMode::RebaseCurrentCheckout => {
                    format!("stay on {}", snapshot.current_ref)
                }
                SyncMode::RebaseHomeBookmark { home_branch } => {
                    format!("re-anchor on {}", home_branch)
                }
            };
            let mut commands = vec![
                "jj git fetch".to_string(),
                sync_rebase_args(&mode, &resolved_target.target).join(" "),
            ];
            if let SyncMode::RebaseHomeBookmark { home_branch } = &mode {
                commands.push(format!("jj edit --ignore-immutable {}", home_branch));
            }
            let push_bookmark = if no_push {
                None
            } else {
                push_bookmark.map(ToOwned::to_owned)
            };
            if let Some(bookmark) = push_bookmark.as_deref() {
                commands.push(format!("jj git push --bookmark {}", bookmark));
            }
            JjSyncPlan {
                mode: Some(mode),
                mode_label,
                reason,
                working_copy_effect,
                remote: remote.to_string(),
                requested_dest: requested_dest.to_string(),
                resolved_target: resolved_target.target.clone(),
                resolved_target_reason: resolved_target.reason.clone(),
                push_bookmark,
                blocked_reason: None,
                commands,
            }
        }
        Err(err) => JjSyncPlan {
            mode: None,
            mode_label: "blocked".to_string(),
            reason: "repair the home lane before syncing".to_string(),
            working_copy_effect:
                "no sync will run until the anonymous home child is preserved or abandoned"
                    .to_string(),
            remote: remote.to_string(),
            requested_dest: requested_dest.to_string(),
            resolved_target: resolved_target.target.clone(),
            resolved_target_reason: resolved_target.reason.clone(),
            push_bookmark: if no_push {
                None
            } else {
                push_bookmark.map(ToOwned::to_owned)
            },
            blocked_reason: Some(err.to_string()),
            commands: snapshot.suggested_next.clone(),
        },
    }
}

fn print_sync_plan(plan: &JjSyncPlan) {
    println!("JJ Sync Plan");
    println!();
    println!("Mode:       {}", plan.mode_label);
    println!("Remote:     {}", plan.remote);
    println!(
        "Dest:       {} -> {}",
        plan.requested_dest, plan.resolved_target
    );
    println!("Dest Why:   {}", plan.resolved_target_reason);
    println!("Reason:     {}", plan.reason);
    println!("Working:    {}", plan.working_copy_effect);
    println!(
        "Push:       {}",
        plan.push_bookmark.as_deref().unwrap_or("none")
    );
    println!("Note:       resolved from current local refs; real sync will fetch first");
    if let Some(blocked_reason) = plan.blocked_reason.as_deref() {
        println!();
        println!("Blocked:");
        println!("  {}", blocked_reason);
    }
    println!();
    println!("Plan:");
    for command in &plan.commands {
        println!("  {}", command);
    }
}

fn sync_rebase_args(mode: &SyncMode, target: &str) -> Vec<String> {
    match mode {
        SyncMode::RebaseCurrentCheckout => {
            vec!["rebase".to_string(), "-d".to_string(), target.to_string()]
        }
        SyncMode::RebaseHomeBookmark { home_branch } => vec![
            "rebase".to_string(),
            "-b".to_string(),
            home_branch.clone(),
            "-d".to_string(),
            target.to_string(),
        ],
    }
}

fn run_workspace(action: JjWorkspaceAction) -> Result<()> {
    let ctx = current_context()?;
    match action {
        JjWorkspaceAction::List => jj_run_in(&ctx.workspace_root, &["workspace", "list"]),
        JjWorkspaceAction::Doctor => {
            let overview = load_overview_for_path(Some(&ctx.workspace_root), 8)?;
            print_workspace_doctor(&overview);
            Ok(())
        }
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
                resolve_rebase_target(&ctx.workspace_root, &dest, &remote).target
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

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JjWorkflowOverview {
    generated_at_unix: u64,
    target_path: String,
    workflow: WorkflowStatusSnapshot,
    summary: JjWorkflowSummary,
    attention: Vec<JjAttentionItem>,
    stacks: Vec<JjStackSummary>,
    recent_operations: Vec<JjOperationSummary>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct JjOperationSummary {
    id: String,
    summary: String,
    kind: &'static str,
    risky: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct JjOverviewCacheKey {
    workspace_root: PathBuf,
    op_limit: usize,
}

#[derive(Debug, Clone)]
struct CachedJjOverview {
    stored_at_unix: u64,
    snapshot: JjWorkflowOverview,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct JjWorkflowSummary {
    leaf_count: usize,
    tracked_leaf_count: usize,
    workspace_count: usize,
    stack_count: usize,
    alert_leaf_count: usize,
    attention_count: usize,
    bookmark_conflict_count: usize,
    target_conflict_count: usize,
    divergent_leaf_count: usize,
    ahead_remote_leaf_count: usize,
    behind_remote_leaf_count: usize,
    missing_workspace_count: usize,
    risky_operation_count: usize,
    working_copy_change_count: usize,
    home_sync_ready: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct JjAttentionItem {
    kind: &'static str,
    severity: &'static str,
    title: String,
    detail: String,
    workspace_name: Option<String>,
    command: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct JjStackSummary {
    id: String,
    title: String,
    kind: &'static str,
    workspace_name: Option<String>,
    workspace_path: Option<PathBuf>,
    path_exists: bool,
    is_current: bool,
    current_ref: Option<String>,
    current_role: Option<&'static str>,
    leaf_count: usize,
    tracked_leaf_count: usize,
    alert_count: usize,
    branches: Vec<JjStackBranch>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct JjStackBranch {
    name: String,
    kind: &'static str,
    unique_commits: usize,
    tracked_remote: bool,
    ahead_of_remote: Option<usize>,
    behind_of_remote: Option<usize>,
    bookmark_conflicted: bool,
    target_commit_conflicted: bool,
    target_commit_divergent: bool,
    is_current: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowStatusSnapshot {
    workspace_root: PathBuf,
    repo_root: PathBuf,
    workspace_name: String,
    current_ref: String,
    current_role: &'static str,
    current_commit_is_anonymous: bool,
    current_commit_conflicted: bool,
    checkout_summary: Option<String>,
    home_branch: String,
    intake_branch: String,
    remote: String,
    trunk_ref: String,
    home_unique_to_trunk: usize,
    trunk_unique_to_home: usize,
    leaves: Vec<LeafBranchStatus>,
    workspaces: Vec<WorkspaceStatus>,
    working_copy_lines: Vec<String>,
    working_copy_source: &'static str,
    working_copy_change_count: usize,
    safety_warning: Option<String>,
    suggested_next: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LeafBranchStatus {
    name: String,
    kind: &'static str,
    unique_commits: usize,
    tracked_remote: bool,
    remote_ref: Option<String>,
    workspace_name: Option<String>,
    workspace_path: Option<PathBuf>,
    is_current: bool,
    bookmark_conflicted: bool,
    target_commit_conflicted: bool,
    target_commit_divergent: bool,
    ahead_of_remote: Option<usize>,
    behind_of_remote: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceStatus {
    name: String,
    is_current: bool,
    path_exists: bool,
    path: PathBuf,
}

struct WorkingCopySnapshot {
    lines: Vec<String>,
    source: &'static str,
}

fn collect_status_snapshot(ctx: &JjContext) -> Result<WorkflowStatusSnapshot> {
    let default_branch = default_branch(&ctx.repo_root);
    let remote = default_remote(&ctx.repo_root);
    let workspace_name = current_workspace_name(&ctx.workspace_root)?;
    let current_bookmarks = local_bookmarks_at_rev(&ctx.workspace_root, "@");
    let parent_bookmarks = local_bookmarks_at_rev(&ctx.workspace_root, "@-");
    let all_bookmarks = jj_bookmark_names(&ctx.workspace_root)?;
    let conflicted_bookmarks = jj_conflicted_bookmark_names(&ctx.workspace_root)?;
    let all_local_bookmarks = all_bookmarks
        .iter()
        .filter(|name| !name.contains('@'))
        .cloned()
        .collect::<HashSet<_>>();
    let current_commit_is_anonymous = current_bookmarks.is_empty();
    let current_commit_conflicted = rev_has_conflict(&ctx.workspace_root, "@");
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
        let workspace_path_for_branch = workspace_name_for_branch
            .as_ref()
            .map(|candidate| inferred_workspace_path(&ctx.repo_root, candidate));
        let remote_ref = all_bookmarks
            .contains(&format!("{name}@{remote}"))
            .then(|| format!("{name}@{remote}"));
        let bookmark_revset = exact_bookmark_revset(&name);
        leaves.push(LeafBranchStatus {
            kind: leaf_branch_kind(&name),
            unique_commits: count_unique_commits(&ctx.workspace_root, &name, &home_branch),
            tracked_remote: remote_ref.is_some(),
            remote_ref: remote_ref.clone(),
            workspace_name: workspace_name_for_branch,
            workspace_path: workspace_path_for_branch,
            is_current: name == current_ref,
            bookmark_conflicted: conflicted_bookmarks.contains(&name),
            target_commit_conflicted: revset_has_conflict(&ctx.workspace_root, &bookmark_revset),
            target_commit_divergent: revset_has_divergence(&ctx.workspace_root, &bookmark_revset),
            ahead_of_remote: remote_ref
                .as_ref()
                .map(|remote_ref| count_unique_commits(&ctx.workspace_root, &name, remote_ref)),
            behind_of_remote: remote_ref
                .as_ref()
                .map(|remote_ref| count_unique_commits(&ctx.workspace_root, remote_ref, &name)),
            name,
        });
    }
    leaves.sort_by(|left, right| left.name.cmp(&right.name));

    let mut workspaces: Vec<WorkspaceStatus> = workspace_names
        .into_iter()
        .map(|name| {
            let path = inferred_workspace_path(&ctx.repo_root, &name);
            WorkspaceStatus {
                path_exists: path.exists(),
                is_current: name == workspace_name,
                path,
                name,
            }
        })
        .collect();
    workspaces.sort_by(|left, right| left.name.cmp(&right.name));

    let working_copy = collect_working_copy_status(ctx)?;
    let working_copy_change_count = working_copy_change_count(&working_copy.lines);
    let checkout_summary = current_checkout_summary(
        &current_ref,
        current_commit_is_anonymous,
        current_commit_conflicted,
    );
    let suggested_next = suggested_next_lines(
        current_role,
        &current_ref,
        &home_branch,
        current_commit_is_anonymous,
        current_commit_conflicted,
        working_copy_change_count,
    );
    let mut snapshot = WorkflowStatusSnapshot {
        workspace_root: ctx.workspace_root.clone(),
        repo_root: ctx.repo_root.clone(),
        workspace_name,
        current_ref,
        current_role,
        current_commit_is_anonymous,
        current_commit_conflicted,
        checkout_summary,
        home_branch: home_branch.clone(),
        intake_branch,
        remote,
        trunk_ref: trunk_ref.clone(),
        home_unique_to_trunk: count_unique_commits(&ctx.workspace_root, &home_branch, &trunk_ref),
        trunk_unique_to_home: count_unique_commits(&ctx.workspace_root, &trunk_ref, &home_branch),
        leaves,
        workspaces,
        working_copy_lines: working_copy.lines,
        working_copy_source: working_copy.source,
        working_copy_change_count,
        safety_warning: None,
        suggested_next,
    };
    snapshot.safety_warning = status_safety_warning(&snapshot);
    Ok(snapshot)
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
    if let Some(summary) = snapshot.checkout_summary.as_deref() {
        println!("Checkout:   {}", summary);
    }
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
                let remote_ref = leaf
                    .remote_ref
                    .as_deref()
                    .unwrap_or(snapshot.remote.as_str());
                let drift = match (leaf.ahead_of_remote, leaf.behind_of_remote) {
                    (Some(ahead), Some(behind)) if ahead > 0 || behind > 0 => {
                        format!(" ahead={ahead} behind={behind}")
                    }
                    _ => String::new(),
                };
                format!(" tracked {}{}", remote_ref, drift)
            } else {
                " local-only".to_string()
            };
            let workspace = leaf
                .workspace_name
                .as_deref()
                .map(|name| format!(" workspace={name}"))
                .unwrap_or_default();
            let flags = [
                leaf.bookmark_conflicted.then_some(" bookmark-conflict"),
                leaf.target_commit_conflicted.then_some(" target-conflict"),
                leaf.target_commit_divergent.then_some(" divergent"),
            ]
            .into_iter()
            .flatten()
            .collect::<String>();
            println!(
                "  {} [{}] {} commit(s) over {}{}{}{}{}",
                leaf.name,
                leaf.kind,
                leaf.unique_commits,
                snapshot.home_branch,
                tracked,
                workspace,
                current,
                flags,
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
        println!(
            "  {} {}{} ({})",
            marker,
            workspace.name,
            suffix,
            workspace.path.display()
        );
    }
    println!();
    println!("Working Copy [{}]:", snapshot.working_copy_source);
    for line in &snapshot.working_copy_lines {
        println!("  {}", line);
    }
    if let Some(warning) = snapshot.safety_warning.as_deref() {
        println!();
        println!("Safety:");
        println!("  {}", warning);
    }
    println!();
    println!("Suggested Next:");
    for line in &snapshot.suggested_next {
        println!("  {}", line);
    }
    if let Some(hint) = status_compact_hint(snapshot) {
        println!();
        println!("Tip:");
        println!("  {}", hint);
    }
}

fn print_compact_status(overview: &JjWorkflowOverview) {
    for line in compact_status_lines(overview) {
        println!("{}", line);
    }
}

fn print_workspace_doctor(overview: &JjWorkflowOverview) {
    for line in workspace_doctor_lines(overview) {
        println!("{}", line);
    }
}

fn compact_status_lines(overview: &JjWorkflowOverview) -> Vec<String> {
    let workflow = &overview.workflow;
    let summary = &overview.summary;
    let mut lines = vec![
        "JJ Workflow Status".to_string(),
        String::new(),
        format!("Repo:       {}", workflow.repo_root.display()),
        format!(
            "Workspace:  {} [{}]",
            workflow.workspace_name, workflow.current_role
        ),
        format!("Current:    {}", workflow.current_ref),
    ];
    if let Some(checkout_summary) = workflow.checkout_summary.as_deref() {
        lines.push(format!("Checkout:   {}", checkout_summary));
    }
    lines.push(format!(
        "Home/Trunk: {} ahead={} behind={} vs {}",
        workflow.home_branch,
        workflow.home_unique_to_trunk,
        workflow.trunk_unique_to_home,
        workflow.trunk_ref
    ));
    lines.push(format!(
        "Working:    {}",
        compact_working_copy_summary(workflow)
    ));
    lines.push(String::new());
    lines.push(format!(
        "Summary:    {} leaf(s) • {} tracked • {} workspace(s) • {} attention",
        summary.leaf_count,
        summary.tracked_leaf_count,
        summary.workspace_count,
        summary.attention_count
    ));

    let mut flags = Vec::new();
    if summary.bookmark_conflict_count > 0 {
        flags.push(format!(
            "{} bookmark-conflict",
            summary.bookmark_conflict_count
        ));
    }
    if summary.target_conflict_count > 0 {
        flags.push(format!("{} target-conflict", summary.target_conflict_count));
    }
    if summary.divergent_leaf_count > 0 {
        flags.push(format!("{} divergent", summary.divergent_leaf_count));
    }
    if summary.missing_workspace_count > 0 {
        flags.push(format!(
            "{} missing-workspace",
            summary.missing_workspace_count
        ));
    }
    if summary.risky_operation_count > 0 {
        flags.push(format!("{} risky-op", summary.risky_operation_count));
    }
    if !flags.is_empty() {
        lines.push(format!("Flags:      {}", flags.join(" • ")));
    }

    if !overview.attention.is_empty() {
        lines.push(String::new());
        lines.push("Attention:".to_string());
        for item in overview.attention.iter().take(6) {
            lines.push(format!(
                "  [{}] {} — {}",
                item.severity, item.title, item.detail
            ));
            if let Some(command) = item.command.as_deref() {
                lines.push(format!("    next: {}", command));
            }
        }
    }

    lines.push(String::new());
    lines.push("Suggested Next:".to_string());
    for line in workflow.suggested_next.iter().take(3) {
        lines.push(format!("  {}", line));
    }

    lines.push(String::new());
    lines.push("More:".to_string());
    lines.push("  Re-run without `--compact` for the full leaf/workspace listing.".to_string());
    if workflow.safety_warning.is_some() || summary.missing_workspace_count > 0 {
        lines.push(
            "  Use `f jj workspace doctor` for lane repair and missing-workspace guidance."
                .to_string(),
        );
    }
    lines.push(format!(
        "  Use `f jj overview --json --path {}` for machine-readable state.",
        workflow.repo_root.display()
    ));

    lines
}

fn workspace_doctor_lines(overview: &JjWorkflowOverview) -> Vec<String> {
    let workflow = &overview.workflow;
    let current_stack = overview.stacks.iter().find(|stack| stack.is_current);
    let missing_stacks = overview
        .stacks
        .iter()
        .filter(|stack| stack.workspace_name.is_some() && !stack.path_exists)
        .collect::<Vec<_>>();
    let unassigned_stack = overview
        .stacks
        .iter()
        .find(|stack| stack.kind == "unassigned");
    let issue_count = usize::from(workflow.safety_warning.is_some())
        + usize::from(workflow.working_copy_change_count > 0)
        + missing_stacks.len()
        + usize::from(unassigned_stack.is_some_and(|stack| !stack.branches.is_empty()));

    let mut lines = vec![
        "JJ Workspace Doctor".to_string(),
        String::new(),
        format!("Repo:       {}", workflow.repo_root.display()),
        format!(
            "Workspace:  {} ({})",
            workflow.workspace_name,
            workflow.workspace_root.display()
        ),
        format!(
            "Current:    {} [{}]",
            workflow.current_ref, workflow.current_role
        ),
        format!(
            "Health:     {} ({issue_count} issue(s))",
            if issue_count == 0 {
                "ok"
            } else {
                "needs attention"
            }
        ),
    ];

    lines.push(String::new());
    lines.push("Current Lane:".to_string());
    if let Some(warning) = workflow.safety_warning.as_deref() {
        lines.push(format!("  - safety: {}", warning));
    } else {
        lines.push("  - safety: no blocking checkout issue detected".to_string());
    }
    if workflow.working_copy_change_count > 0 {
        lines.push(format!(
            "  - working copy: {} tracked change(s) via {}",
            workflow.working_copy_change_count, workflow.working_copy_source
        ));
    } else {
        lines.push(format!(
            "  - working copy: clean via {}",
            workflow.working_copy_source
        ));
    }
    if let Some(stack) = current_stack {
        let branch_count = stack.branches.len();
        lines.push(format!(
            "  - stack: {} leaf(s) in {}",
            branch_count, stack.title
        ));
    }

    lines.push(String::new());
    lines.push("Workspace Paths:".to_string());
    if missing_stacks.is_empty() {
        lines.push("  - all expected workspace paths exist".to_string());
    } else {
        for stack in missing_stacks {
            let path = stack
                .workspace_path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "(no path recorded)".to_string());
            lines.push(format!("  - missing: {} -> {}", stack.title, path));
            if let Some(branch) = stack.branches.first() {
                lines.push(format!("    next: f jj workspace review {}", branch.name));
            } else {
                lines.push("    next: jj workspace list".to_string());
            }
        }
    }

    lines.push(String::new());
    lines.push("Unassigned Leaves:".to_string());
    if let Some(stack) = unassigned_stack.filter(|stack| !stack.branches.is_empty()) {
        for branch in stack.branches.iter().take(6) {
            lines.push(format!(
                "  - {} ({} over {}{})",
                branch.name,
                branch.unique_commits,
                workflow.home_branch,
                if branch.target_commit_conflicted {
                    " • target conflict"
                } else if branch.target_commit_divergent {
                    " • divergent"
                } else {
                    ""
                }
            ));
        }
    } else {
        lines.push("  - none".to_string());
    }

    lines.push(String::new());
    lines.push("Suggested Next:".to_string());
    for line in workflow.suggested_next.iter().take(4) {
        lines.push(format!("  {}", line));
    }
    lines.push("  f status --compact".to_string());

    lines
}

fn compact_working_copy_summary(snapshot: &WorkflowStatusSnapshot) -> String {
    if snapshot.working_copy_change_count == 0 {
        return format!("clean via {}", snapshot.working_copy_source);
    }
    format!(
        "{} tracked change(s) via {}",
        snapshot.working_copy_change_count, snapshot.working_copy_source
    )
}

fn status_compact_hint(snapshot: &WorkflowStatusSnapshot) -> Option<&'static str> {
    if snapshot.leaves.len() >= 12 || snapshot.workspaces.len() >= 12 {
        Some(
            "Use `f status --compact` for a focused summary or `f jj overview --json` for machine-readable state.",
        )
    } else {
        None
    }
}

fn workflow_summary(
    snapshot: &WorkflowStatusSnapshot,
    recent_operations: &[JjOperationSummary],
) -> JjWorkflowSummary {
    JjWorkflowSummary {
        leaf_count: snapshot.leaves.len(),
        tracked_leaf_count: snapshot
            .leaves
            .iter()
            .filter(|leaf| leaf.tracked_remote)
            .count(),
        workspace_count: snapshot.workspaces.len(),
        stack_count: stack_summaries(snapshot).len(),
        alert_leaf_count: snapshot
            .leaves
            .iter()
            .filter(|leaf| leaf_alert_score(leaf) > 0)
            .count(),
        attention_count: attention_items(snapshot, recent_operations).len(),
        bookmark_conflict_count: snapshot
            .leaves
            .iter()
            .filter(|leaf| leaf.bookmark_conflicted)
            .count(),
        target_conflict_count: snapshot
            .leaves
            .iter()
            .filter(|leaf| leaf.target_commit_conflicted)
            .count(),
        divergent_leaf_count: snapshot
            .leaves
            .iter()
            .filter(|leaf| leaf.target_commit_divergent)
            .count(),
        ahead_remote_leaf_count: snapshot
            .leaves
            .iter()
            .filter(|leaf| leaf.ahead_of_remote.unwrap_or(0) > 0)
            .count(),
        behind_remote_leaf_count: snapshot
            .leaves
            .iter()
            .filter(|leaf| leaf.behind_of_remote.unwrap_or(0) > 0)
            .count(),
        missing_workspace_count: snapshot
            .workspaces
            .iter()
            .filter(|workspace| !workspace.path_exists)
            .count(),
        risky_operation_count: recent_operations.iter().filter(|op| op.risky).count(),
        working_copy_change_count: snapshot.working_copy_change_count,
        home_sync_ready: snapshot.workspace_name == "default"
            && snapshot.current_role == "home"
            && !anonymous_home_checkout_requires_repair(
                snapshot.current_role,
                snapshot.current_commit_is_anonymous,
                snapshot.current_commit_conflicted,
                snapshot.working_copy_change_count,
            ),
    }
}

fn attention_items(
    snapshot: &WorkflowStatusSnapshot,
    recent_operations: &[JjOperationSummary],
) -> Vec<JjAttentionItem> {
    let mut items = Vec::new();

    if let Some(warning) = snapshot.safety_warning.as_deref() {
        items.push(JjAttentionItem {
            kind: "safety",
            severity: "high",
            title: format!("Unsafe home checkout on {}", snapshot.current_ref),
            detail: warning.to_string(),
            workspace_name: Some(snapshot.workspace_name.clone()),
            command: snapshot.suggested_next.first().cloned(),
        });
    }

    if snapshot.trunk_unique_to_home > 0 {
        items.push(JjAttentionItem {
            kind: "home-drift",
            severity: if snapshot.current_role == "home" {
                "high"
            } else {
                "medium"
            },
            title: format!("{} is behind {}", snapshot.home_branch, snapshot.trunk_ref),
            detail: format!(
                "{} commit(s) from {} are not in {}",
                snapshot.trunk_unique_to_home, snapshot.trunk_ref, snapshot.home_branch
            ),
            workspace_name: Some(snapshot.workspace_name.clone()),
            command: Some("flow sync".to_string()),
        });
    }

    if snapshot.working_copy_change_count > 0 {
        items.push(JjAttentionItem {
            kind: "working-copy",
            severity: if snapshot.current_role == "home" {
                "medium"
            } else {
                "low"
            },
            title: format!("{} has working-copy changes", snapshot.workspace_name),
            detail: format!(
                "{} tracked change(s) in {}",
                snapshot.working_copy_change_count,
                snapshot.workspace_root.display()
            ),
            workspace_name: Some(snapshot.workspace_name.clone()),
            command: Some("jj status".to_string()),
        });
    }

    for workspace in snapshot
        .workspaces
        .iter()
        .filter(|workspace| !workspace.path_exists)
    {
        items.push(JjAttentionItem {
            kind: "workspace",
            severity: "medium",
            title: format!("Workspace {} is missing on disk", workspace.name),
            detail: workspace.path.display().to_string(),
            workspace_name: Some(workspace.name.clone()),
            command: None,
        });
    }

    let mut risky_leaves = snapshot
        .leaves
        .iter()
        .filter(|leaf| leaf_alert_score(leaf) > 0)
        .collect::<Vec<_>>();
    risky_leaves.sort_by(|left, right| {
        leaf_alert_score(right)
            .cmp(&leaf_alert_score(left))
            .then_with(|| right.unique_commits.cmp(&left.unique_commits))
            .then_with(|| left.name.cmp(&right.name))
    });
    for leaf in risky_leaves.into_iter().take(6) {
        let severity = if leaf.bookmark_conflicted || leaf.target_commit_conflicted {
            "high"
        } else if leaf.target_commit_divergent || leaf.behind_of_remote.unwrap_or(0) > 0 {
            "medium"
        } else {
            "low"
        };
        items.push(JjAttentionItem {
            kind: "leaf",
            severity,
            title: leaf.name.clone(),
            detail: leaf_alert_detail(leaf, &snapshot.home_branch),
            workspace_name: leaf.workspace_name.clone(),
            command: leaf
                .workspace_name
                .as_ref()
                .map(|_| format!("e {}", leaf.name)),
        });
    }

    for op in recent_operations.iter().filter(|op| op.risky).take(3) {
        items.push(JjAttentionItem {
            kind: "operation",
            severity: "low",
            title: format!("Recent {} operation {}", op.kind, op.id),
            detail: if op.summary.is_empty() {
                "No description.".to_string()
            } else {
                op.summary.clone()
            },
            workspace_name: Some(snapshot.workspace_name.clone()),
            command: Some("jj op log".to_string()),
        });
    }

    items
}

fn stack_summaries(snapshot: &WorkflowStatusSnapshot) -> Vec<JjStackSummary> {
    let mut stacks = snapshot
        .workspaces
        .iter()
        .map(|workspace| JjStackSummary {
            id: workspace.name.clone(),
            title: workspace.name.clone(),
            kind: "workspace",
            workspace_name: Some(workspace.name.clone()),
            workspace_path: Some(workspace.path.clone()),
            path_exists: workspace.path_exists,
            is_current: workspace.is_current,
            current_ref: workspace.is_current.then(|| snapshot.current_ref.clone()),
            current_role: workspace.is_current.then_some(snapshot.current_role),
            leaf_count: 0,
            tracked_leaf_count: 0,
            alert_count: 0,
            branches: Vec::new(),
        })
        .collect::<Vec<_>>();

    let mut by_workspace = HashMap::new();
    for (index, stack) in stacks.iter().enumerate() {
        if let Some(workspace_name) = stack.workspace_name.as_deref() {
            by_workspace.insert(workspace_name.to_string(), index);
        }
    }

    let mut unassigned = Vec::new();
    for leaf in &snapshot.leaves {
        let branch = JjStackBranch {
            name: leaf.name.clone(),
            kind: leaf.kind,
            unique_commits: leaf.unique_commits,
            tracked_remote: leaf.tracked_remote,
            ahead_of_remote: leaf.ahead_of_remote,
            behind_of_remote: leaf.behind_of_remote,
            bookmark_conflicted: leaf.bookmark_conflicted,
            target_commit_conflicted: leaf.target_commit_conflicted,
            target_commit_divergent: leaf.target_commit_divergent,
            is_current: leaf.is_current,
        };
        if let Some(index) = leaf
            .workspace_name
            .as_ref()
            .and_then(|workspace_name| by_workspace.get(workspace_name))
        {
            stacks[*index].branches.push(branch);
        } else {
            unassigned.push(branch);
        }
    }

    if !unassigned.is_empty() {
        stacks.push(JjStackSummary {
            id: "unassigned".to_string(),
            title: "Unassigned review leaves".to_string(),
            kind: "unassigned",
            workspace_name: None,
            workspace_path: None,
            path_exists: false,
            is_current: false,
            current_ref: None,
            current_role: None,
            leaf_count: 0,
            tracked_leaf_count: 0,
            alert_count: 0,
            branches: unassigned,
        });
    }

    for stack in &mut stacks {
        stack.branches.sort_by(|left, right| {
            right
                .is_current
                .cmp(&left.is_current)
                .then_with(|| stack_branch_alert_score(right).cmp(&stack_branch_alert_score(left)))
                .then_with(|| right.unique_commits.cmp(&left.unique_commits))
                .then_with(|| left.name.cmp(&right.name))
        });
        stack.leaf_count = stack.branches.len();
        stack.tracked_leaf_count = stack
            .branches
            .iter()
            .filter(|branch| branch.tracked_remote)
            .count();
        stack.alert_count = stack
            .branches
            .iter()
            .filter(|branch| stack_branch_alert_score(branch) > 0)
            .count();
        if !stack.path_exists && stack.workspace_name.is_some() {
            stack.alert_count += 1;
        }
        if stack.is_current
            && (snapshot.safety_warning.is_some() || snapshot.working_copy_change_count > 0)
        {
            stack.alert_count += 1;
        }
    }

    stacks.sort_by(|left, right| {
        right
            .is_current
            .cmp(&left.is_current)
            .then_with(|| right.alert_count.cmp(&left.alert_count))
            .then_with(|| right.leaf_count.cmp(&left.leaf_count))
            .then_with(|| left.title.cmp(&right.title))
    });
    stacks
}

fn leaf_alert_score(leaf: &LeafBranchStatus) -> usize {
    usize::from(leaf.bookmark_conflicted) * 8
        + usize::from(leaf.target_commit_conflicted) * 5
        + usize::from(leaf.target_commit_divergent) * 3
        + leaf.ahead_of_remote.unwrap_or(0)
        + leaf.behind_of_remote.unwrap_or(0)
}

fn leaf_alert_detail(leaf: &LeafBranchStatus, home_branch: &str) -> String {
    let mut parts = vec![format!("{} over {}", leaf.unique_commits, home_branch)];
    if let Some(workspace_name) = leaf.workspace_name.as_deref() {
        parts.push(format!("workspace {workspace_name}"));
    }
    if let Some(behind) = leaf.behind_of_remote.filter(|value| *value > 0) {
        parts.push(format!("behind {behind}"));
    }
    if let Some(ahead) = leaf.ahead_of_remote.filter(|value| *value > 0) {
        parts.push(format!("ahead {ahead}"));
    }
    if leaf.bookmark_conflicted {
        parts.push("bookmark conflict".to_string());
    }
    if leaf.target_commit_conflicted {
        parts.push("target conflict".to_string());
    }
    if leaf.target_commit_divergent {
        parts.push("divergent".to_string());
    }
    parts.join(" • ")
}

fn stack_branch_alert_score(branch: &JjStackBranch) -> usize {
    usize::from(branch.bookmark_conflicted) * 8
        + usize::from(branch.target_commit_conflicted) * 5
        + usize::from(branch.target_commit_divergent) * 3
        + branch.ahead_of_remote.unwrap_or(0)
        + branch.behind_of_remote.unwrap_or(0)
}

fn current_workspace_name(workspace_root: &Path) -> Result<String> {
    let output = jj_read_in(
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
    config::configured_home_branch_for_repo(repo_root)
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
    for bookmark in current_bookmarks.iter().chain(parent_bookmarks.iter()) {
        if is_home_branch_candidate(bookmark, default_branch) {
            return bookmark.clone();
        }
    }
    let mut fallback_candidates = all_bookmarks
        .iter()
        .filter(|bookmark| is_home_branch_candidate(bookmark, default_branch))
        .cloned()
        .collect::<Vec<_>>();
    let score_candidates = fallback_candidates.clone();
    fallback_candidates.sort_by(|left, right| {
        let left_score = home_branch_fallback_score(left, &score_candidates);
        let right_score = home_branch_fallback_score(right, &score_candidates);
        right_score
            .cmp(&left_score)
            .then_with(|| left.len().cmp(&right.len()))
            .then_with(|| left.cmp(right))
    });
    if let Some(best) = fallback_candidates.first() {
        return best.clone();
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

fn current_checkout_summary(
    current_ref: &str,
    current_commit_is_anonymous: bool,
    current_commit_conflicted: bool,
) -> Option<String> {
    if !current_commit_is_anonymous {
        return None;
    }
    let state = anonymous_checkout_label(current_commit_conflicted);
    Some(format!("{state} on top of {current_ref}"))
}

fn collect_working_copy_status(ctx: &JjContext) -> Result<WorkingCopySnapshot> {
    if ctx.workspace_root == ctx.repo_root && git_current_branch(&ctx.repo_root).is_some() {
        if let Ok(lines) = live_git_working_copy_lines(&ctx.repo_root) {
            return Ok(WorkingCopySnapshot {
                lines,
                source: "git-live",
            });
        }
    }

    let output = jj_read_in(&ctx.workspace_root, &["status"])?;
    Ok(WorkingCopySnapshot {
        lines: output.lines().map(|line| line.to_string()).collect(),
        source: "jj-last-op",
    })
}

fn live_git_working_copy_lines(repo_root: &Path) -> Result<Vec<String>> {
    let output = git_capture_in(repo_root, &["status", "--short"])?;
    if output.trim().is_empty() {
        return Ok(vec!["The working copy has no changes.".to_string()]);
    }
    Ok(output.lines().map(|line| line.to_string()).collect())
}

fn suggested_next_lines(
    current_role: &str,
    current_ref: &str,
    home_branch: &str,
    current_commit_is_anonymous: bool,
    current_commit_conflicted: bool,
    working_copy_change_count: usize,
) -> Vec<String> {
    if current_commit_conflicted {
        return vec![
            "jj resolve".to_string(),
            "jj diff".to_string(),
            "jj squash".to_string(),
        ];
    }
    if anonymous_home_checkout_requires_repair(
        current_role,
        current_commit_is_anonymous,
        current_commit_conflicted,
        working_copy_change_count,
    ) {
        return vec![
            "jj squash".to_string(),
            format!("jj edit {home_branch}"),
            "jj abandon @".to_string(),
        ];
    }
    if current_role == "review" || current_role == "codex" {
        return vec![
            format!("f jj push --bookmark {current_ref}"),
            format!("jj rebase -b {current_ref} -o {home_branch}"),
        ];
    }
    vec![
        format!("f jj sync --bookmark {home_branch}"),
        format!(
            "f jj workspace review review/{}-topic",
            review_workspace_name(home_branch)
        ),
    ]
}

fn working_copy_change_count(lines: &[String]) -> usize {
    lines
        .iter()
        .filter(|line| {
            let trimmed = line.trim_start();
            matches!(trimmed.chars().next(), Some('A' | 'M' | 'D' | 'R' | 'C'))
                || trimmed.starts_with("?? ")
        })
        .count()
}

fn status_safety_warning(snapshot: &WorkflowStatusSnapshot) -> Option<String> {
    if snapshot.workspace_name != "default" || snapshot.current_role != "home" {
        return None;
    }
    if !anonymous_home_checkout_requires_repair(
        snapshot.current_role,
        snapshot.current_commit_is_anonymous,
        snapshot.current_commit_conflicted,
        snapshot.working_copy_change_count,
    ) {
        return None;
    }

    let state = anonymous_checkout_label(snapshot.current_commit_conflicted);
    let mut warning = format!(
        "Default home checkout is unsafe for normal work: {state} on top of {}.",
        snapshot.current_ref
    );
    if snapshot.working_copy_change_count > 25 {
        warning.push_str(&format!(
            " Working copy also has {} tracked changes.",
            snapshot.working_copy_change_count
        ));
    }
    warning.push_str(
        " Preserve or abandon this child before syncing or editing here; prefer review workspaces for branch-specific work.",
    );
    warning.push_str(&format!(
        " If `{}@{}` is the last clean home tip, preserve `@` under a recovery bookmark, reset `{}` to that clean tip, then `jj edit {}`.",
        snapshot.home_branch, snapshot.remote, snapshot.home_branch, snapshot.home_branch
    ));
    Some(warning)
}

fn anonymous_home_checkout_requires_repair(
    current_role: &str,
    current_commit_is_anonymous: bool,
    current_commit_conflicted: bool,
    working_copy_change_count: usize,
) -> bool {
    current_role == "home"
        && current_commit_is_anonymous
        && (current_commit_conflicted || working_copy_change_count > 0)
}

fn anonymous_checkout_label(current_commit_conflicted: bool) -> &'static str {
    if current_commit_conflicted {
        "anonymous conflicted @"
    } else {
        "anonymous @"
    }
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
        && !is_hidden_branch(name)
        && name != default_branch
        && !name.contains('@')
}

fn is_hidden_branch(name: &str) -> bool {
    name.starts_with("backup/")
        || name.starts_with("recovery/")
        || name.starts_with("jj/keep/")
        || name.ends_with("-jj-export-backup")
}

fn home_branch_fallback_score(candidate: &str, candidates: &[String]) -> usize {
    candidates
        .iter()
        .filter(|other| home_branch_family_child(other, candidate))
        .count()
}

fn home_branch_family_child(name: &str, parent: &str) -> bool {
    if name == parent {
        return false;
    }
    let Some(suffix) = name.strip_prefix(parent) else {
        return false;
    };
    matches!(suffix.chars().next(), Some('-' | '/' | '_'))
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
    let output = jj_read_in(
        workspace_root,
        &["log", "-r", rev, "--no-graph", "-T", "bookmarks"],
    )
    .unwrap_or_default();
    parse_bookmark_tokens(&output)
}

fn rev_has_conflict(workspace_root: &Path, rev: &str) -> bool {
    jj_read_in(
        workspace_root,
        &[
            "log",
            "-r",
            rev,
            "--no-graph",
            "-T",
            "if(conflict, \"true\", \"false\")",
        ],
    )
    .map(|output| output.trim() == "true")
    .unwrap_or(false)
}

fn jj_bookmark_names(workspace_root: &Path) -> Result<HashSet<String>> {
    let output = jj_read_in(
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

fn jj_conflicted_bookmark_names(workspace_root: &Path) -> Result<HashSet<String>> {
    let output = jj_read_in(
        workspace_root,
        &["bookmark", "list", "--conflicted", "-T", "name ++ \"\\n\""],
    )?;
    Ok(parse_bookmark_list_names(&output)
        .into_iter()
        .collect::<HashSet<_>>())
}

fn parse_bookmark_tokens(output: &str) -> Vec<String> {
    output
        .split_whitespace()
        .filter(|token| !token.contains('@'))
        .filter_map(sanitize_bookmark_token)
        .map(ToOwned::to_owned)
        .collect()
}

fn parse_bookmark_list_names(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with("Hint:") {
                return None;
            }
            let name = trimmed
                .split_once(':')
                .map(|(name, _)| name.trim())
                .unwrap_or(trimmed);
            sanitize_bookmark_token(name).map(ToOwned::to_owned)
        })
        .filter(|name| !name.is_empty())
        .collect()
}

fn sanitize_bookmark_token(token: &str) -> Option<&str> {
    let trimmed = token.trim().trim_end_matches(['*', '?']);
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn jj_workspace_names(workspace_root: &Path) -> Result<HashSet<String>> {
    let output = jj_read_in(
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
    jj_read_in(
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

fn revset_has_conflict(workspace_root: &Path, revset: &str) -> bool {
    let query = format!("({revset}) & conflicts()");
    jj_read_in(
        workspace_root,
        &[
            "log",
            "-r",
            &query,
            "-n",
            "1",
            "--no-graph",
            "-T",
            "commit_id",
        ],
    )
    .map(|output| !output.trim().is_empty())
    .unwrap_or(false)
}

fn revset_has_divergence(workspace_root: &Path, revset: &str) -> bool {
    let query = format!("({revset}) & divergent()");
    jj_read_in(
        workspace_root,
        &[
            "log",
            "-r",
            &query,
            "-n",
            "1",
            "--no-graph",
            "-T",
            "commit_id",
        ],
    )
    .map(|output| !output.trim().is_empty())
    .unwrap_or(false)
}

fn exact_bookmark_revset(name: &str) -> String {
    format!("bookmarks(exact:{})", revset_string_literal(name))
}

fn revset_string_literal(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn resolve_rebase_target(repo_root: &Path, dest: &str, remote: &str) -> ResolvedRebaseTarget {
    let remote_target = format!("{}@{}", dest, remote);
    let local_exists = jj_bookmark_exists(repo_root, dest);
    let remote_exists = jj_bookmark_exists(repo_root, &remote_target);

    if local_exists {
        return ResolvedRebaseTarget {
            target: dest.to_string(),
            reason: if remote_exists {
                format!(
                    "using local bookmark {} because it is already tracked alongside {}",
                    dest, remote_target
                )
            } else {
                format!(
                    "using local bookmark {} because {} is not tracked locally yet",
                    dest, remote_target
                )
            },
        };
    }

    if remote_exists {
        return ResolvedRebaseTarget {
            target: remote_target.clone(),
            reason: format!(
                "using tracked remote bookmark {} because local {} is missing",
                remote_target, dest
            ),
        };
    }

    ResolvedRebaseTarget {
        target: remote_target.clone(),
        reason: format!(
            "defaulting to {} because local {} is missing; fetch may materialize the remote bookmark before rebase",
            remote_target, dest
        ),
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
    config::effective_jj_config_for_repo(repo_root)
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
        rev: fallback.target.clone(),
        source: format!("fallback trunk {} ({})", fallback.target, fallback.reason),
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
            review_workspace_name("review/home-rust-first"),
            "review-home-rust-first"
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
    fn parse_bookmark_tokens_strip_jj_markers() {
        let parsed = parse_bookmark_tokens("review/demo* review/other?? main@origin");

        assert_eq!(parsed, vec!["review/demo", "review/other"]);
    }

    #[test]
    fn repo_root_from_git_root_handles_colocated_git_dir() {
        let repo_root = repo_root_from_git_root("/tmp/repo/.git").expect("repo root");
        assert_eq!(repo_root, "/tmp/repo");
    }

    #[test]
    fn infer_current_ref_prefers_workspace_named_leaf_branch() {
        let current = infer_current_ref(
            "review/home-feature",
            "home",
            "home",
            "home-main-intake",
            "main",
            &[],
            &[String::from("home"), String::from("review/home-feature")],
        );

        assert_eq!(current, "review/home-feature");
    }

    #[test]
    fn infer_home_branch_ignores_backup_aliases() {
        let home = infer_home_branch(
            Path::new("/tmp/flow-home-branch-test"),
            "main",
            &[],
            &[
                String::from("backup/home-pre-origin-main-20260318T2330Z"),
                String::from("home"),
            ],
            &HashSet::from([
                String::from("backup/home-pre-origin-main-20260318T2330Z"),
                String::from("home"),
            ]),
        );

        assert_eq!(home, "home");
    }

    #[test]
    fn infer_home_branch_ignores_recovery_and_export_aliases() {
        let home = infer_home_branch(
            Path::new("/tmp/flow-home-branch-test"),
            "main",
            &[],
            &[],
            &HashSet::from([
                String::from("recovery/home-pre-repair-20260319"),
                String::from("home-jj-export-backup"),
                String::from("home"),
                String::from("home-wip-20260319"),
            ]),
        );

        assert_eq!(home, "home");
    }

    #[test]
    fn current_checkout_summary_reports_anonymous_conflicted_child() {
        let summary = current_checkout_summary("home", true, true);

        assert_eq!(
            summary.as_deref(),
            Some("anonymous conflicted @ on top of home")
        );
    }

    #[test]
    fn suggested_next_lines_prioritize_conflict_resolution() {
        let lines = suggested_next_lines("home", "home", "home", true, true, 0);

        assert_eq!(
            lines,
            vec![
                "jj resolve".to_string(),
                "jj diff".to_string(),
                "jj squash".to_string(),
            ]
        );
    }

    #[test]
    fn suggested_next_lines_use_sanitized_workspace_name() {
        let lines = suggested_next_lines(
            "home",
            "home",
            "backup/home-pre-origin-main",
            false,
            false,
            0,
        );

        assert_eq!(
            lines,
            vec![
                "f jj sync --bookmark backup/home-pre-origin-main".to_string(),
                "f jj workspace review review/backup-home-pre-origin-main-topic".to_string(),
            ]
        );
    }

    #[test]
    fn suggested_next_lines_prefer_home_checkout_repair_over_sync() {
        let lines = suggested_next_lines("home", "home", "home", true, false, 1);

        assert_eq!(
            lines,
            vec![
                "jj squash".to_string(),
                "jj edit home".to_string(),
                "jj abandon @".to_string(),
            ]
        );
    }

    #[test]
    fn suggested_next_lines_keep_clean_anonymous_home_checkout_on_normal_lane() {
        let lines = suggested_next_lines("home", "home", "home", true, false, 0);

        assert_eq!(
            lines,
            vec![
                "f jj sync --bookmark home".to_string(),
                "f jj workspace review review/home-topic".to_string(),
            ]
        );
    }

    #[test]
    fn status_safety_warning_flags_unsafe_default_home_checkout() {
        let snapshot = WorkflowStatusSnapshot {
            workspace_root: PathBuf::from("/tmp/repo"),
            repo_root: PathBuf::from("/tmp/repo"),
            workspace_name: "default".to_string(),
            current_ref: "home".to_string(),
            current_role: "home",
            current_commit_is_anonymous: true,
            current_commit_conflicted: true,
            checkout_summary: None,
            home_branch: "home".to_string(),
            intake_branch: "home-main-intake".to_string(),
            remote: "origin".to_string(),
            trunk_ref: "main@origin".to_string(),
            home_unique_to_trunk: 0,
            trunk_unique_to_home: 0,
            leaves: Vec::new(),
            workspaces: Vec::new(),
            working_copy_lines: vec![
                "Working copy changes:".to_string(),
                "A foo".to_string(),
                "M bar".to_string(),
            ],
            working_copy_source: "jj",
            working_copy_change_count: 2,
            safety_warning: None,
            suggested_next: Vec::new(),
        };

        let warning = status_safety_warning(&snapshot).expect("warning");
        assert!(warning.contains("unsafe for normal work"));
        assert!(warning.contains("anonymous conflicted @ on top of home"));
    }

    #[test]
    fn status_safety_warning_flags_clean_anonymous_default_home_checkout() {
        let snapshot = WorkflowStatusSnapshot {
            workspace_root: PathBuf::from("/tmp/repo"),
            repo_root: PathBuf::from("/tmp/repo"),
            workspace_name: "default".to_string(),
            current_ref: "home".to_string(),
            current_role: "home",
            current_commit_is_anonymous: true,
            current_commit_conflicted: false,
            checkout_summary: None,
            home_branch: "home".to_string(),
            intake_branch: "home-main-intake".to_string(),
            remote: "origin".to_string(),
            trunk_ref: "main@origin".to_string(),
            home_unique_to_trunk: 0,
            trunk_unique_to_home: 0,
            leaves: Vec::new(),
            workspaces: Vec::new(),
            working_copy_lines: vec!["M foo".to_string()],
            working_copy_source: "jj",
            working_copy_change_count: 1,
            safety_warning: None,
            suggested_next: Vec::new(),
        };

        let warning = status_safety_warning(&snapshot).expect("warning");
        assert!(warning.contains("anonymous @ on top of home"));
        assert!(warning.contains("Preserve or abandon this child"));
    }

    #[test]
    fn status_safety_warning_skips_clean_anonymous_default_home_checkout() {
        let snapshot = WorkflowStatusSnapshot {
            workspace_root: PathBuf::from("/tmp/repo"),
            repo_root: PathBuf::from("/tmp/repo"),
            workspace_name: "default".to_string(),
            current_ref: "home".to_string(),
            current_role: "home",
            current_commit_is_anonymous: true,
            current_commit_conflicted: false,
            checkout_summary: None,
            home_branch: "home".to_string(),
            intake_branch: "home-main-intake".to_string(),
            remote: "origin".to_string(),
            trunk_ref: "main@origin".to_string(),
            home_unique_to_trunk: 0,
            trunk_unique_to_home: 0,
            leaves: Vec::new(),
            workspaces: Vec::new(),
            working_copy_lines: vec!["The working copy has no changes.".to_string()],
            working_copy_source: "jj",
            working_copy_change_count: 0,
            safety_warning: None,
            suggested_next: Vec::new(),
        };

        assert!(status_safety_warning(&snapshot).is_none());
    }

    #[test]
    fn status_safety_warning_mentions_large_change_count() {
        let snapshot = WorkflowStatusSnapshot {
            workspace_root: PathBuf::from("/tmp/repo"),
            repo_root: PathBuf::from("/tmp/repo"),
            workspace_name: "default".to_string(),
            current_ref: "home".to_string(),
            current_role: "home",
            current_commit_is_anonymous: true,
            current_commit_conflicted: true,
            checkout_summary: None,
            home_branch: "home".to_string(),
            intake_branch: "home-main-intake".to_string(),
            remote: "origin".to_string(),
            trunk_ref: "main@origin".to_string(),
            home_unique_to_trunk: 0,
            trunk_unique_to_home: 0,
            leaves: Vec::new(),
            workspaces: Vec::new(),
            working_copy_lines: (0..30).map(|index| format!("M file-{index}")).collect(),
            working_copy_source: "jj",
            working_copy_change_count: 30,
            safety_warning: None,
            suggested_next: Vec::new(),
        };

        let warning = status_safety_warning(&snapshot).expect("warning");
        assert!(warning.contains("30 tracked changes"));
    }

    #[test]
    fn status_safety_warning_mentions_clean_remote_repair_path() {
        let snapshot = WorkflowStatusSnapshot {
            workspace_root: PathBuf::from("/tmp/repo"),
            repo_root: PathBuf::from("/tmp/repo"),
            workspace_name: "default".to_string(),
            current_ref: "home".to_string(),
            current_role: "home",
            current_commit_is_anonymous: true,
            current_commit_conflicted: true,
            checkout_summary: None,
            home_branch: "home".to_string(),
            intake_branch: "home-main-intake".to_string(),
            remote: "origin".to_string(),
            trunk_ref: "main@origin".to_string(),
            home_unique_to_trunk: 0,
            trunk_unique_to_home: 0,
            leaves: Vec::new(),
            workspaces: Vec::new(),
            working_copy_lines: vec!["M foo".to_string()],
            working_copy_source: "jj",
            working_copy_change_count: 1,
            safety_warning: None,
            suggested_next: Vec::new(),
        };

        let warning = status_safety_warning(&snapshot).expect("warning");
        assert!(warning.contains("home@origin"));
        assert!(warning.contains("jj edit home"));
    }

    #[test]
    fn determine_sync_mode_rebases_home_bookmark_for_default_home_checkout() {
        let snapshot = WorkflowStatusSnapshot {
            workspace_root: PathBuf::from("/tmp/repo"),
            repo_root: PathBuf::from("/tmp/repo"),
            workspace_name: "default".to_string(),
            current_ref: "home".to_string(),
            current_role: "home",
            current_commit_is_anonymous: false,
            current_commit_conflicted: false,
            checkout_summary: None,
            home_branch: "home".to_string(),
            intake_branch: "home-main-intake".to_string(),
            remote: "origin".to_string(),
            trunk_ref: "main@origin".to_string(),
            home_unique_to_trunk: 0,
            trunk_unique_to_home: 0,
            leaves: Vec::new(),
            workspaces: Vec::new(),
            working_copy_lines: Vec::new(),
            working_copy_source: "jj",
            working_copy_change_count: 0,
            safety_warning: None,
            suggested_next: Vec::new(),
        };

        assert_eq!(
            determine_sync_mode(&snapshot).expect("mode"),
            SyncMode::RebaseHomeBookmark {
                home_branch: "home".to_string(),
            }
        );
        assert_eq!(
            sync_rebase_args(
                &SyncMode::RebaseHomeBookmark {
                    home_branch: "home".to_string(),
                },
                "main@origin",
            ),
            vec!["rebase", "-b", "home", "-d", "main@origin"]
        );
    }

    #[test]
    fn determine_sync_mode_rejects_anonymous_default_home_checkout() {
        let snapshot = WorkflowStatusSnapshot {
            workspace_root: PathBuf::from("/tmp/repo"),
            repo_root: PathBuf::from("/tmp/repo"),
            workspace_name: "default".to_string(),
            current_ref: "home".to_string(),
            current_role: "home",
            current_commit_is_anonymous: true,
            current_commit_conflicted: false,
            checkout_summary: None,
            home_branch: "home".to_string(),
            intake_branch: "home-main-intake".to_string(),
            remote: "origin".to_string(),
            trunk_ref: "main@origin".to_string(),
            home_unique_to_trunk: 0,
            trunk_unique_to_home: 0,
            leaves: Vec::new(),
            workspaces: Vec::new(),
            working_copy_lines: vec!["M foo".to_string()],
            working_copy_source: "jj",
            working_copy_change_count: 1,
            safety_warning: None,
            suggested_next: Vec::new(),
        };

        let err = determine_sync_mode(&snapshot).expect_err("should fail");
        assert!(err.to_string().contains("unsafe for `flow sync`"));
        assert!(err.to_string().contains("jj edit home"));
    }

    #[test]
    fn determine_sync_mode_allows_clean_anonymous_default_home_checkout() {
        let snapshot = WorkflowStatusSnapshot {
            workspace_root: PathBuf::from("/tmp/repo"),
            repo_root: PathBuf::from("/tmp/repo"),
            workspace_name: "default".to_string(),
            current_ref: "home".to_string(),
            current_role: "home",
            current_commit_is_anonymous: true,
            current_commit_conflicted: false,
            checkout_summary: Some("anonymous @ on top of home".to_string()),
            home_branch: "home".to_string(),
            intake_branch: "home-main-intake".to_string(),
            remote: "origin".to_string(),
            trunk_ref: "main@origin".to_string(),
            home_unique_to_trunk: 0,
            trunk_unique_to_home: 0,
            leaves: Vec::new(),
            workspaces: Vec::new(),
            working_copy_lines: vec!["The working copy has no changes.".to_string()],
            working_copy_source: "jj",
            working_copy_change_count: 0,
            safety_warning: None,
            suggested_next: vec!["f jj sync --bookmark home".to_string()],
        };

        assert_eq!(
            determine_sync_mode(&snapshot).expect("mode"),
            SyncMode::RebaseHomeBookmark {
                home_branch: "home".to_string(),
            }
        );
    }

    #[test]
    fn build_sync_plan_blocks_dirty_anonymous_home_checkout_with_repair_steps() {
        let snapshot = WorkflowStatusSnapshot {
            workspace_root: PathBuf::from("/tmp/repo"),
            repo_root: PathBuf::from("/tmp/repo"),
            workspace_name: "default".to_string(),
            current_ref: "home".to_string(),
            current_role: "home",
            current_commit_is_anonymous: true,
            current_commit_conflicted: false,
            checkout_summary: Some("anonymous @ on top of home".to_string()),
            home_branch: "home".to_string(),
            intake_branch: "home-main-intake".to_string(),
            remote: "origin".to_string(),
            trunk_ref: "main@origin".to_string(),
            home_unique_to_trunk: 0,
            trunk_unique_to_home: 0,
            leaves: Vec::new(),
            workspaces: Vec::new(),
            working_copy_lines: vec!["M foo".to_string()],
            working_copy_source: "jj",
            working_copy_change_count: 1,
            safety_warning: None,
            suggested_next: vec![
                "jj squash".to_string(),
                "jj edit home".to_string(),
                "jj abandon @".to_string(),
            ],
        };

        let plan = build_sync_plan(
            &snapshot,
            "origin",
            "main",
            &ResolvedRebaseTarget {
                target: "main@origin".to_string(),
                reason: "using tracked remote bookmark main@origin because local main is missing"
                    .to_string(),
            },
            Some("home"),
            false,
        );

        assert!(plan.mode.is_none());
        assert!(
            plan.blocked_reason
                .as_deref()
                .expect("blocked reason")
                .contains("unsafe for `flow sync`")
        );
        assert_eq!(
            plan.commands,
            vec![
                "jj squash".to_string(),
                "jj edit home".to_string(),
                "jj abandon @".to_string(),
            ]
        );
    }

    #[test]
    fn build_sync_plan_reanchors_clean_anonymous_home_checkout() {
        let snapshot = WorkflowStatusSnapshot {
            workspace_root: PathBuf::from("/tmp/repo"),
            repo_root: PathBuf::from("/tmp/repo"),
            workspace_name: "default".to_string(),
            current_ref: "home".to_string(),
            current_role: "home",
            current_commit_is_anonymous: true,
            current_commit_conflicted: false,
            checkout_summary: Some("anonymous @ on top of home".to_string()),
            home_branch: "home".to_string(),
            intake_branch: "home-main-intake".to_string(),
            remote: "origin".to_string(),
            trunk_ref: "main@origin".to_string(),
            home_unique_to_trunk: 0,
            trunk_unique_to_home: 0,
            leaves: Vec::new(),
            workspaces: Vec::new(),
            working_copy_lines: vec!["The working copy has no changes.".to_string()],
            working_copy_source: "jj",
            working_copy_change_count: 0,
            safety_warning: None,
            suggested_next: vec!["f jj sync --bookmark home".to_string()],
        };

        let plan = build_sync_plan(
            &snapshot,
            "origin",
            "main",
            &ResolvedRebaseTarget {
                target: "main@origin".to_string(),
                reason: "using tracked remote bookmark main@origin because local main is missing"
                    .to_string(),
            },
            Some("home"),
            false,
        );

        assert_eq!(
            plan.mode,
            Some(SyncMode::RebaseHomeBookmark {
                home_branch: "home".to_string(),
            })
        );
        assert!(plan.reason.contains("rebase home"));
        assert!(plan.working_copy_effect.contains("re-anchor on home"));
        assert!(
            plan.resolved_target_reason
                .contains("tracked remote bookmark main@origin")
        );
        assert_eq!(
            plan.commands,
            vec![
                "jj git fetch".to_string(),
                "rebase -b home -d main@origin".to_string(),
                "jj edit --ignore-immutable home".to_string(),
                "jj git push --bookmark home".to_string(),
            ]
        );
    }

    #[test]
    fn workflow_summary_counts_alerts_and_risky_ops() {
        let snapshot = sample_snapshot();
        let recent_operations = vec![
            JjOperationSummary {
                id: "abc123".to_string(),
                summary: "rebase bookmark home onto main@origin".to_string(),
                kind: "rebase",
                risky: true,
            },
            JjOperationSummary {
                id: "def456".to_string(),
                summary: "describe commit".to_string(),
                kind: "history",
                risky: false,
            },
        ];

        let summary = workflow_summary(&snapshot, &recent_operations);

        assert_eq!(summary.leaf_count, 2);
        assert_eq!(summary.tracked_leaf_count, 1);
        assert_eq!(summary.alert_leaf_count, 2);
        assert_eq!(summary.attention_count, 4);
        assert_eq!(summary.bookmark_conflict_count, 1);
        assert_eq!(summary.behind_remote_leaf_count, 1);
        assert_eq!(summary.risky_operation_count, 1);
        assert!(summary.home_sync_ready);
    }

    #[test]
    fn workflow_summary_marks_clean_anonymous_home_checkout_as_sync_ready() {
        let snapshot = WorkflowStatusSnapshot {
            workspace_root: PathBuf::from("/tmp/repo"),
            repo_root: PathBuf::from("/tmp/repo"),
            workspace_name: "default".to_string(),
            current_ref: "home".to_string(),
            current_role: "home",
            current_commit_is_anonymous: true,
            current_commit_conflicted: false,
            checkout_summary: Some("anonymous @ on top of home".to_string()),
            home_branch: "home".to_string(),
            intake_branch: "home-main-intake".to_string(),
            remote: "origin".to_string(),
            trunk_ref: "main@origin".to_string(),
            home_unique_to_trunk: 0,
            trunk_unique_to_home: 0,
            leaves: Vec::new(),
            workspaces: vec![WorkspaceStatus {
                name: "default".to_string(),
                is_current: true,
                path_exists: true,
                path: PathBuf::from("/tmp/repo"),
            }],
            working_copy_lines: vec!["The working copy has no changes.".to_string()],
            working_copy_source: "jj",
            working_copy_change_count: 0,
            safety_warning: None,
            suggested_next: vec!["f jj sync --bookmark home".to_string()],
        };

        let summary = workflow_summary(&snapshot, &[]);

        assert!(summary.home_sync_ready);
    }

    #[test]
    fn stack_summaries_group_leaves_by_workspace_and_keep_current_home_lane() {
        let stacks = stack_summaries(&sample_snapshot());

        assert_eq!(
            stacks.first().map(|stack| stack.id.as_str()),
            Some("default")
        );
        let review_stack = stacks
            .iter()
            .find(|stack| stack.workspace_name.as_deref() == Some("review-demo"))
            .expect("review stack");
        assert_eq!(review_stack.leaf_count, 1);
        assert_eq!(review_stack.alert_count, 1);
        assert_eq!(review_stack.branches[0].name, "review/demo");

        let unassigned = stacks
            .iter()
            .find(|stack| stack.kind == "unassigned")
            .expect("unassigned stack");
        assert_eq!(unassigned.leaf_count, 1);
        assert_eq!(unassigned.branches[0].name, "review/no-workspace");
    }

    #[test]
    fn attention_items_include_home_drift_and_leaf_alerts() {
        let items = attention_items(&sample_snapshot(), &[]);

        assert_eq!(items[0].kind, "home-drift");
        assert!(items.iter().any(|item| item.title == "review/demo"));
        assert!(
            items
                .iter()
                .any(|item| item.detail.contains("behind 2") && item.kind == "leaf")
        );
    }

    #[test]
    fn operation_kind_marks_rebases_and_bookmark_changes_as_risky() {
        assert_eq!(
            operation_kind("rebase bookmark home onto main@origin"),
            "rebase"
        );
        assert!(operation_is_risky(
            operation_kind("rebase bookmark home onto main@origin"),
            "rebase bookmark home onto main@origin"
        ));
        assert_eq!(operation_kind("describe commit"), "history");
        assert!(!operation_is_risky(
            operation_kind("describe commit"),
            "describe commit"
        ));
    }

    #[test]
    fn compact_status_lines_surface_summary_attention_and_next_steps() {
        let overview = sample_overview();
        let lines = compact_status_lines(&overview);
        let rendered = lines.join("\n");

        assert!(rendered.contains("Summary:    2 leaf(s)"));
        assert!(rendered.contains("Attention:"));
        assert!(rendered.contains("[high] home is behind main@origin"));
        assert!(rendered.contains("next: flow sync"));
        assert!(rendered.contains("Use `f jj overview --json --path /tmp/repo`"));
    }

    #[test]
    fn workspace_doctor_lines_surface_missing_paths_and_lane_repairs() {
        let mut workflow = sample_snapshot();
        workflow.current_commit_is_anonymous = true;
        workflow.checkout_summary = Some("anonymous @ on top of home".to_string());
        workflow.working_copy_lines = vec!["M foo".to_string()];
        workflow.working_copy_change_count = 1;
        workflow.safety_warning =
            Some("Default home checkout is unsafe for normal work.".to_string());
        workflow.workspaces.push(WorkspaceStatus {
            name: "review-missing".to_string(),
            is_current: false,
            path_exists: false,
            path: PathBuf::from("/tmp/review-missing"),
        });
        let recent_operations = Vec::new();
        let overview = JjWorkflowOverview {
            generated_at_unix: 0,
            target_path: "/tmp/repo".to_string(),
            summary: workflow_summary(&workflow, &recent_operations),
            attention: attention_items(&workflow, &recent_operations),
            stacks: stack_summaries(&workflow),
            workflow,
            recent_operations,
        };

        let rendered = workspace_doctor_lines(&overview).join("\n");
        assert!(rendered.contains("JJ Workspace Doctor"));
        assert!(rendered.contains("Health:     needs attention"));
        assert!(rendered.contains("safety: Default home checkout is unsafe"));
        assert!(rendered.contains("working copy: 1 tracked change(s)"));
        assert!(rendered.contains("missing: review-missing -> /tmp/review-missing"));
        assert!(rendered.contains("jj workspace list"));
    }

    #[test]
    fn status_compact_hint_recommends_compact_for_large_repos() {
        let mut snapshot = sample_snapshot();
        for index in 0..12 {
            snapshot.workspaces.push(WorkspaceStatus {
                name: format!("review-extra-{index}"),
                is_current: false,
                path_exists: true,
                path: PathBuf::from(format!("/tmp/review-extra-{index}")),
            });
        }

        assert_eq!(
            status_compact_hint(&snapshot),
            Some(
                "Use `f status --compact` for a focused summary or `f jj overview --json` for machine-readable state."
            )
        );
    }

    fn sample_snapshot() -> WorkflowStatusSnapshot {
        WorkflowStatusSnapshot {
            workspace_root: PathBuf::from("/tmp/repo"),
            repo_root: PathBuf::from("/tmp/repo"),
            workspace_name: "default".to_string(),
            current_ref: "home".to_string(),
            current_role: "home",
            current_commit_is_anonymous: false,
            current_commit_conflicted: false,
            checkout_summary: None,
            home_branch: "home".to_string(),
            intake_branch: "home-main-intake".to_string(),
            remote: "origin".to_string(),
            trunk_ref: "main@origin".to_string(),
            home_unique_to_trunk: 2,
            trunk_unique_to_home: 5,
            leaves: vec![
                LeafBranchStatus {
                    name: "review/demo".to_string(),
                    kind: "review",
                    unique_commits: 3,
                    tracked_remote: true,
                    remote_ref: Some("review/demo@origin".to_string()),
                    workspace_name: Some("review-demo".to_string()),
                    workspace_path: Some(PathBuf::from("/tmp/review-demo")),
                    is_current: false,
                    bookmark_conflicted: false,
                    target_commit_conflicted: false,
                    target_commit_divergent: false,
                    ahead_of_remote: Some(0),
                    behind_of_remote: Some(2),
                },
                LeafBranchStatus {
                    name: "review/no-workspace".to_string(),
                    kind: "review",
                    unique_commits: 1,
                    tracked_remote: false,
                    remote_ref: None,
                    workspace_name: None,
                    workspace_path: None,
                    is_current: false,
                    bookmark_conflicted: true,
                    target_commit_conflicted: false,
                    target_commit_divergent: false,
                    ahead_of_remote: None,
                    behind_of_remote: None,
                },
            ],
            workspaces: vec![
                WorkspaceStatus {
                    name: "default".to_string(),
                    is_current: true,
                    path_exists: true,
                    path: PathBuf::from("/tmp/repo"),
                },
                WorkspaceStatus {
                    name: "review-demo".to_string(),
                    is_current: false,
                    path_exists: true,
                    path: PathBuf::from("/tmp/review-demo"),
                },
            ],
            working_copy_lines: vec!["The working copy has no changes.".to_string()],
            working_copy_source: "jj",
            working_copy_change_count: 0,
            safety_warning: None,
            suggested_next: vec!["flow sync".to_string()],
        }
    }

    fn sample_overview() -> JjWorkflowOverview {
        let workflow = sample_snapshot();
        let recent_operations = vec![JjOperationSummary {
            id: "op-1".to_string(),
            summary: "rebase bookmark home onto main@origin".to_string(),
            kind: "rebase",
            risky: true,
        }];
        let summary = workflow_summary(&workflow, &recent_operations);
        let attention = attention_items(&workflow, &recent_operations);
        let stacks = stack_summaries(&workflow);

        JjWorkflowOverview {
            generated_at_unix: 0,
            target_path: "/tmp/repo".to_string(),
            workflow,
            summary,
            attention,
            stacks,
            recent_operations,
        }
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

fn git_capture_in(repo_root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(args)
        .output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn jj_read_in(repo_root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("jj")
        .current_dir(repo_root)
        .args(["--at-op=@", "--ignore-working-copy"])
        .args(args)
        .output()
        .with_context(|| format!("failed to run jj {}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("jj {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn jj_overview_cache() -> &'static Mutex<HashMap<JjOverviewCacheKey, CachedJjOverview>> {
    static CACHE: OnceLock<Mutex<HashMap<JjOverviewCacheKey, CachedJjOverview>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cached_overview(cache_key: &JjOverviewCacheKey) -> Option<JjWorkflowOverview> {
    let now = now_unix_secs();
    let cache = jj_overview_cache().lock().ok()?;
    let entry = cache.get(cache_key)?;
    if now.saturating_sub(entry.stored_at_unix) > JJ_OVERVIEW_CACHE_TTL_SECS {
        return None;
    }
    Some(entry.snapshot.clone())
}

fn store_cached_overview(cache_key: JjOverviewCacheKey, snapshot: JjWorkflowOverview) {
    if let Ok(mut cache) = jj_overview_cache().lock() {
        cache.insert(
            cache_key,
            CachedJjOverview {
                stored_at_unix: now_unix_secs(),
                snapshot,
            },
        );
    }
}

fn jj_run_owned_in(repo_root: &Path, args: &[String]) -> Result<()> {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    jj_run_in(repo_root, &refs)
}

fn recent_operations(workspace_root: &Path, limit: usize) -> Result<Vec<JjOperationSummary>> {
    let output = jj_read_in(
        workspace_root,
        &[
            "op",
            "log",
            "-n",
            &limit.to_string(),
            "--no-graph",
            "-T",
            "id.short() ++ \"\\t\" ++ description.first_line() ++ \"\\n\"",
        ],
    )?;
    Ok(output
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return None;
            }
            let (id, summary) = trimmed
                .split_once('\t')
                .map(|(id, summary)| (id.trim(), summary.trim()))
                .unwrap_or((trimmed, ""));
            let kind = operation_kind(summary);
            Some(JjOperationSummary {
                id: id.to_string(),
                summary: summary.to_string(),
                kind,
                risky: operation_is_risky(kind, summary),
            })
        })
        .collect())
}

fn operation_kind(summary: &str) -> &'static str {
    let lowered = summary.to_ascii_lowercase();
    if lowered.contains("rebase") || lowered.contains("backout") {
        "rebase"
    } else if lowered.contains("bookmark") || lowered.contains("branch") {
        "bookmark"
    } else if lowered.contains("workspace") {
        "workspace"
    } else if lowered.contains("describe")
        || lowered.contains("squash")
        || lowered.contains("edit")
        || lowered.contains("new")
    {
        "history"
    } else if lowered.contains("git ") {
        "git"
    } else {
        "other"
    }
}

fn operation_is_risky(kind: &str, summary: &str) -> bool {
    let lowered = summary.to_ascii_lowercase();
    matches!(kind, "rebase" | "bookmark")
        || lowered.contains("abandon")
        || lowered.contains("resolve")
        || lowered.contains("restore")
        || lowered.contains("rewind")
        || lowered.contains("undo")
        || lowered.contains("track")
        || lowered.contains("untrack")
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0)
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
