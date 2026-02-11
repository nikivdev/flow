use std::path::Path;
use std::process::Command;

use anyhow::{Result, bail};

use crate::cli::StashOpts;
use crate::commit;

pub fn run(opts: StashOpts) -> Result<()> {
    let repo_root = commit::git_root_or_cwd();
    if !repo_root.join(".jj").is_dir() || which::which("jj").is_err() {
        bail!("f stash requires a jj-managed repository");
    }

    let name = opts
        .name
        .unwrap_or_else(|| format!("stash-{}", chrono::Utc::now().format("%Y%m%d-%H%M%S")));

    // Save current work to a named bookmark.
    println!("==> Bookmarking current change as {}...", name);
    commit::jj_run_in(&repo_root, &["bookmark", "create", &name, "-r", "@"])?;

    // Determine the target to reset to.
    let target = opts.target.unwrap_or_else(|| resolve_main_ref(&repo_root));

    println!("==> Moving working copy to {}...", target);
    commit::jj_run_in(&repo_root, &["new", &target])?;

    println!("\n✓ Stashed as '{}'. To restore: jj edit {}", name, name);
    Ok(())
}

/// Pick the freshest main ref — upstream first, then origin.
fn resolve_main_ref(repo_root: &Path) -> String {
    let has_upstream = Command::new("git")
        .current_dir(repo_root)
        .args(["remote", "get-url", "upstream"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if has_upstream {
        return "main@upstream".to_string();
    }
    "main@origin".to_string()
}
