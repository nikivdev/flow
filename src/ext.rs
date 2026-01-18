use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::cli::ExtCommand;
use crate::code;
use crate::config;
use crate::setup::add_gitignore_entry;

pub fn run(cmd: ExtCommand) -> Result<()> {
    let source = normalize_path(&cmd.path)?;
    if !source.exists() {
        bail!("Path not found: {}", source.display());
    }
    if !source.is_dir() {
        bail!("Path must be a directory: {}", source.display());
    }

    let project_root = project_root_from_cwd();
    let ext_dir = project_root.join("ext");
    fs::create_dir_all(&ext_dir)?;

    let name = source
        .file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_string())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "external".to_string());

    let dest = ext_dir.join(&name);
    if dest.exists() {
        bail!("Destination already exists: {}", dest.display());
    }

    copy_dir_all(&source, &dest)?;
    add_gitignore_entry(&project_root, "ext/")?;
    if let Err(err) = code::migrate_sessions_between_paths(&source, &dest, false, false, false) {
        eprintln!("WARN failed to migrate sessions: {err}");
    }

    println!("Copied {} -> {}", source.display(), dest.display());
    Ok(())
}

fn normalize_path(path: &str) -> Result<PathBuf> {
    let expanded = config::expand_path(path);
    let canonical = expanded.canonicalize().unwrap_or(expanded);
    Ok(canonical)
}

fn project_root_from_cwd() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut current = cwd.clone();
    loop {
        let candidate = current.join("flow.toml");
        if candidate.exists() {
            return current;
        }
        if !current.pop() {
            return cwd;
        }
    }
}

fn copy_dir_all(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to).with_context(|| format!("failed to create {}", to.display()))?;
    for entry in fs::read_dir(from).with_context(|| format!("failed to read {}", from.display()))? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        let target = to.join(entry.file_name());

        if target.exists() {
            bail!("Refusing to overwrite {}", target.display());
        }

        if file_type.is_dir() {
            copy_dir_all(&path, &target)?;
        } else if file_type.is_file() {
            fs::copy(&path, &target)
                .with_context(|| format!("failed to copy {}", path.display()))?;
        } else if file_type.is_symlink() {
            let link_target = fs::read_link(&path)
                .with_context(|| format!("failed to read link {}", path.display()))?;
            copy_symlink(&link_target, &target)?;
        }
    }
    Ok(())
}

fn copy_symlink(target: &Path, dest: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, dest)
            .with_context(|| format!("failed to create symlink {}", dest.display()))?;
        return Ok(());
    }
    #[cfg(not(unix))]
    {
        let metadata =
            fs::metadata(target).with_context(|| format!("failed to read {}", target.display()))?;
        if metadata.is_dir() {
            copy_dir_all(target, dest)?;
        } else {
            fs::copy(target, dest)
                .with_context(|| format!("failed to copy {}", target.display()))?;
        }
        Ok(())
    }
}
