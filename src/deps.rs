use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::cli::{DepsAction, DepsCommand, DepsManager};

pub fn run(cmd: DepsCommand) -> Result<()> {
    let action = cmd.action.unwrap_or(DepsAction::Install { args: Vec::new() });
    let project_root = project_root()?;
    let manager = cmd.manager.unwrap_or_else(|| detect_manager(&project_root));

    let (program, args) = build_command(manager, &project_root, &action)?;
    let status = Command::new(program)
        .args(&args)
        .current_dir(&project_root)
        .status()
        .with_context(|| format!("failed to run {}", program))?;

    if !status.success() {
        bail!("dependency command failed");
    }

    Ok(())
}

fn build_command(
    manager: DepsManager,
    project_root: &Path,
    action: &DepsAction,
) -> Result<(&'static str, Vec<String>)> {
    let workspace = is_workspace(project_root);
    let (base, mut args) = match (manager, workspace) {
        (DepsManager::Pnpm, true) => ("pnpm", vec!["-r".to_string()]),
        (DepsManager::Pnpm, false) => ("pnpm", Vec::new()),
        (DepsManager::Yarn, _) => ("yarn", Vec::new()),
        (DepsManager::Bun, _) => ("bun", Vec::new()),
        (DepsManager::Npm, _) => ("npm", Vec::new()),
    };

    match action {
        DepsAction::Install { args: extra } => {
            args.push("install".to_string());
            args.extend(extra.clone());
        }
        DepsAction::Update { args: extra } => {
            match manager {
                DepsManager::Pnpm => {
                    args.push("up".to_string());
                    args.push("--latest".to_string());
                }
                DepsManager::Yarn => {
                    args.push("up".to_string());
                }
                DepsManager::Bun => {
                    args.push("update".to_string());
                }
                DepsManager::Npm => {
                    args.push("update".to_string());
                }
            }
            args.extend(extra.clone());
        }
    }

    Ok((base, args))
}

fn detect_manager(project_root: &Path) -> DepsManager {
    if project_root.join("pnpm-lock.yaml").exists() || project_root.join("pnpm-workspace.yaml").exists() {
        return DepsManager::Pnpm;
    }
    if project_root.join("bun.lockb").exists() || project_root.join("bun.lock").exists() {
        return DepsManager::Bun;
    }
    if project_root.join("yarn.lock").exists() {
        return DepsManager::Yarn;
    }
    if project_root.join("package-lock.json").exists() {
        return DepsManager::Npm;
    }
    DepsManager::Npm
}

fn is_workspace(project_root: &Path) -> bool {
    project_root.join("pnpm-workspace.yaml").exists()
}

fn project_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    if let Some(flow_path) = find_flow_toml(&cwd) {
        return Ok(flow_path.parent().unwrap_or(&cwd).to_path_buf());
    }
    Ok(cwd)
}

fn find_flow_toml(start: &PathBuf) -> Option<PathBuf> {
    let mut current = start.clone();
    loop {
        let candidate = current.join("flow.toml");
        if candidate.exists() {
            return Some(candidate);
        }
        if !current.pop() {
            return None;
        }
    }
}
