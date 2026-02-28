use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::cli::{
    JjAction, JjBookmarkAction, JjCommand, JjPushOpts, JjRebaseOpts, JjSyncOpts, JjWorkspaceAction,
};
use crate::config;
use crate::vcs;

pub fn run(cmd: JjCommand) -> Result<()> {
    match cmd.action.unwrap_or(JjAction::Status) {
        JjAction::Init { path } => run_init(path),
        JjAction::Status => run_status(),
        JjAction::Fetch => run_fetch(),
        JjAction::Rebase(opts) => run_rebase(opts),
        JjAction::Push(opts) => run_push(opts),
        JjAction::Sync(opts) => run_sync(opts),
        JjAction::Workspace(action) => run_workspace(action),
        JjAction::Bookmark(action) => run_bookmark(action),
    }
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

fn run_status() -> Result<()> {
    let repo_root = vcs::ensure_jj_repo()?;
    jj_run_in(&repo_root, &["st"])
}

fn run_fetch() -> Result<()> {
    let repo_root = vcs::ensure_jj_repo()?;
    ensure_git_not_busy(&repo_root)?;
    jj_run_in(&repo_root, &["git", "fetch"])
}

fn run_rebase(opts: JjRebaseOpts) -> Result<()> {
    let repo_root = vcs::ensure_jj_repo()?;
    ensure_git_not_busy(&repo_root)?;
    let remote = default_remote(&repo_root);
    let dest = opts.dest.unwrap_or_else(|| default_branch(&repo_root));
    let target = resolve_rebase_target(&repo_root, &dest, &remote);
    jj_run_in(&repo_root, &["rebase", "-d", &target])
}

fn run_push(opts: JjPushOpts) -> Result<()> {
    let repo_root = vcs::ensure_jj_repo()?;
    ensure_git_not_busy(&repo_root)?;
    if opts.all {
        return jj_run_in(&repo_root, &["git", "push", "--all"]);
    }
    let Some(bookmark) = opts.bookmark else {
        bail!("Specify a bookmark or pass --all");
    };
    jj_run_in(&repo_root, &["git", "push", "--bookmark", &bookmark])
}

fn run_sync(opts: JjSyncOpts) -> Result<()> {
    let repo_root = vcs::ensure_jj_repo()?;
    ensure_git_not_busy(&repo_root)?;
    let remote = opts.remote.unwrap_or_else(|| default_remote(&repo_root));
    let dest = opts.dest.unwrap_or_else(|| default_branch(&repo_root));

    jj_run_in(&repo_root, &["git", "fetch"])?;
    let target = resolve_rebase_target(&repo_root, &dest, &remote);
    jj_run_in(&repo_root, &["rebase", "-d", &target])?;

    // Check for conflicts after rebase
    let has_conflicts = jj_capture_in(
        &repo_root,
        &["log", "-r", "conflicts()", "--no-graph", "-T", "commit_id"],
    )
    .map(|out| !out.trim().is_empty())
    .unwrap_or(false);
    if has_conflicts {
        let details = jj_capture_in(&repo_root, &["log", "-r", "conflicts()", "--no-graph"])
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
    jj_run_in(&repo_root, &["git", "push", "--bookmark", &bookmark])
}

fn run_workspace(action: JjWorkspaceAction) -> Result<()> {
    let repo_root = vcs::ensure_jj_repo()?;
    match action {
        JjWorkspaceAction::List => jj_run_in(&repo_root, &["workspace", "list"]),
        JjWorkspaceAction::Add { name, path, rev } => {
            let workspace_path = match path {
                Some(p) => p,
                None => workspace_default_path(&repo_root, &name)?,
            };
            run_workspace_add(&repo_root, &name, workspace_path, rev.as_deref())
        }
        JjWorkspaceAction::Lane {
            name,
            path,
            base,
            remote,
            no_fetch,
        } => {
            ensure_git_not_busy(&repo_root)?;
            let remote = remote.unwrap_or_else(|| default_remote(&repo_root));
            if !no_fetch {
                if let Err(err) = jj_run_in(&repo_root, &["git", "fetch"]) {
                    eprintln!("⚠ jj git fetch failed: {err}");
                    eprintln!("  continuing with current local refs");
                }
            }
            let workspace_path = match path {
                Some(p) => p,
                None => workspace_default_path(&repo_root, &name)?,
            };
            let base_rev = base.unwrap_or_else(|| {
                let dest = default_branch(&repo_root);
                resolve_rebase_target(&repo_root, &dest, &remote)
            });
            run_workspace_add(&repo_root, &name, workspace_path.clone(), Some(&base_rev))?;
            println!("Lane {} is anchored at {}", name, base_rev);
            println!("Next: cd {}", workspace_path.display());
            println!(
                "Optional bookmark: f jj bookmark create {} --rev @ --track --remote {}",
                name, remote
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
    let repo_root = vcs::ensure_jj_repo()?;
    match action {
        JjBookmarkAction::List => jj_run_in(&repo_root, &["bookmark", "list"]),
        JjBookmarkAction::Track { name, remote } => {
            let remote = remote.unwrap_or_else(|| default_remote(&repo_root));
            let track_ref = format!("{}@{}", name, remote);
            jj_run_in(&repo_root, &["bookmark", "track", &track_ref])
        }
        JjBookmarkAction::Create {
            name,
            rev,
            track,
            remote,
        } => {
            let rev = rev.unwrap_or_else(|| "@".to_string());
            jj_run_in(&repo_root, &["bookmark", "create", &name, "-r", &rev])?;

            let should_track = track.unwrap_or_else(|| auto_track_enabled(&repo_root));
            if should_track {
                let remote = remote.unwrap_or_else(|| default_remote(&repo_root));
                let track_ref = format!("{}@{}", name, remote);
                if jj_run_in(&repo_root, &["bookmark", "track", &track_ref]).is_err() {
                    println!("⚠ Failed to track {}", track_ref);
                }
            }
            Ok(())
        }
    }
}

fn resolve_rebase_target(repo_root: &Path, dest: &str, remote: &str) -> String {
    if jj_bookmark_exists(repo_root, dest) {
        dest.to_string()
    } else {
        format!("{}@{}", dest, remote)
    }
}

fn jj_bookmark_exists(repo_root: &Path, name: &str) -> bool {
    let output = jj_capture_in(repo_root, &["bookmark", "list"]).unwrap_or_default();
    output
        .lines()
        .any(|line| line.trim_start().starts_with(name))
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
