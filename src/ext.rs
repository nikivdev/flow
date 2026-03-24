use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::cli::{ExtAction, ExtCommand};
use crate::code;
use crate::config;
use crate::flow_config;
use crate::setup::add_gitignore_entry;
use anyhow::{Context, Result, bail};

pub fn run(cmd: ExtCommand) -> Result<()> {
    match (cmd.action, cmd.path) {
        (Some(ExtAction::List { json }), None) => list_extensions(json),
        (Some(ExtAction::Doctor { json }), None) => doctor_extensions(json),
        (Some(ExtAction::Enable { name }), None) => enable_extension(&name),
        (Some(ExtAction::Disable { name }), None) => disable_extension(&name),
        (Some(ExtAction::Init { name, force }), None) => init_extension(&name, force),
        (Some(ExtAction::Import { path }), None) => import_external_path(&path),
        (None, Some(path)) => import_external_path(&path),
        (Some(_), Some(_)) => bail!("pass either a subcommand or a bare import path, not both"),
        (None, None) => list_extensions(false),
    }
}

fn list_extensions(as_json: bool) -> Result<()> {
    let extensions = flow_config::discover_extensions()?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&extensions)?);
        return Ok(());
    }

    if extensions.is_empty() {
        println!(
            "No Flow extensions discovered. Create one with `f ext init <name>` under {}",
            flow_config::flow_root_dir().join("extensions").display()
        );
        return Ok(());
    }

    println!("Flow extensions:\n");
    for extension in extensions {
        let status = if extension.enabled {
            "enabled"
        } else {
            "disabled"
        };
        let source = extension
            .source_path
            .unwrap_or_else(|| "<builtin>".to_string());
        println!("  {:<20} {:<8} {}", extension.name, status, source);
    }
    Ok(())
}

fn doctor_extensions(as_json: bool) -> Result<()> {
    let report = flow_config::extension_doctor_report()?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    println!("Flow extension doctor");
    println!(
        "  Root config: {}",
        report
            .get("rootConfigPath")
            .and_then(|value| value.as_str())
            .unwrap_or("unknown")
    );
    println!(
        "  Extensions root: {}",
        report
            .get("extensionsRoot")
            .and_then(|value| value.as_str())
            .unwrap_or("unknown")
    );
    println!(
        "  Runtime helper: {}",
        report
            .get("runtimeHelperPath")
            .and_then(|value| value.as_str())
            .unwrap_or("unknown")
    );
    let extensions = report
        .get("extensions")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    println!("  Extensions: {}", extensions.len());
    for extension in extensions {
        let name = extension
            .get("name")
            .and_then(|value| value.as_str())
            .unwrap_or("unknown");
        let enabled = extension
            .get("enabled")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        let kind = extension
            .get("kind")
            .and_then(|value| value.as_str())
            .unwrap_or("unknown");
        println!(
            "  - {:<20} {:<8} {}",
            name,
            if enabled { "enabled" } else { "disabled" },
            kind
        );
    }
    Ok(())
}

fn enable_extension(name: &str) -> Result<()> {
    flow_config::enable_extension(name)?;
    println!("Enabled extension {}", name);
    Ok(())
}

fn disable_extension(name: &str) -> Result<()> {
    flow_config::disable_extension(name)?;
    println!("Disabled extension {}", name);
    Ok(())
}

fn init_extension(name: &str, force: bool) -> Result<()> {
    let dir = flow_config::init_extension(name, force)?;
    println!("Initialized extension {} at {}", name, dir.display());
    Ok(())
}

fn import_external_path(path: &str) -> Result<()> {
    let source = normalize_path(path)?;
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

    let source_workspace = prepare_source_workspace(&source, &project_root)?;
    copy_dir_all(&source_workspace, &dest)?;
    add_gitignore_entry(&project_root, "ext/")?;
    if let Err(err) = code::migrate_sessions_between_paths(&source, &dest, false, false, false) {
        eprintln!("WARN failed to migrate sessions: {err}");
    }

    println!(
        "Copied {} -> {}",
        source_workspace.display(),
        dest.display()
    );
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

fn prepare_source_workspace(source: &Path, project_root: &Path) -> Result<PathBuf> {
    let repo_root = match jj_root(source) {
        Ok(root) => root,
        Err(_) => {
            bail!(
                "Source is not a jj workspace. Run `jj git init --colocate` in {} and retry.",
                source.display()
            );
        }
    };

    let workspace = workspace_name_for_project(project_root)?;
    if workspace.is_empty() {
        return Ok(source.to_path_buf());
    }

    let status = git_capture_in(&repo_root, &["status", "--porcelain"]).unwrap_or_default();
    if !status.trim().is_empty() {
        println!("Source repo has uncommitted changes:");
        for line in status.lines().take(20) {
            println!("  {line}");
        }
        let continue_anyway = prompt_yes_no(
            &format!("Continue and use jj workspace \"{}\"?", workspace),
            false,
        )?;
        if !continue_anyway {
            bail!("Aborted; commit or stash changes before continuing.");
        }
    }

    let workspaces = jj_workspace_list(&repo_root).unwrap_or_default();
    if let Some(existing_path) = workspaces.get(&workspace) {
        return Ok(PathBuf::from(existing_path));
    }

    let base = workspace_base(&repo_root)?;
    fs::create_dir_all(&base).with_context(|| format!("failed to create {}", base.display()))?;
    let workspace_path = base.join(&workspace);
    jj_run_in(
        &repo_root,
        &[
            "workspace",
            "add",
            workspace_path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("invalid workspace path"))?,
            "--name",
            &workspace,
        ],
    )?;

    println!(
        "Created jj workspace {} at {}",
        workspace,
        workspace_path.display()
    );
    Ok(workspace_path)
}

fn workspace_name_for_project(project_root: &Path) -> Result<String> {
    let home = std::env::var("HOME").ok();
    let mut relative = None;
    if let Some(home) = home.as_deref() {
        if let Ok(stripped) = project_root.strip_prefix(home) {
            relative = Some(stripped.to_path_buf());
        }
    }
    let name = if let Some(rel) = relative {
        rel.to_string_lossy().trim_start_matches('/').to_string()
    } else {
        project_root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("external")
            .to_string()
    };

    let mut sanitized = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '/' || ch == '.' || ch == '-' || ch == '_' {
            sanitized.push(ch);
        } else {
            sanitized.push('-');
        }
    }
    Ok(sanitized.trim_matches('/').to_string())
}

fn workspace_base(repo_root: &Path) -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    let repo_name = repo_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("repo");
    Ok(PathBuf::from(home)
        .join(".jj")
        .join("workspaces")
        .join(repo_name))
}

fn jj_root(source: &Path) -> Result<PathBuf> {
    let root = jj_capture_in(source, &["root"])?;
    Ok(PathBuf::from(root.trim()))
}

fn jj_workspace_list(repo_root: &Path) -> Result<std::collections::HashMap<String, String>> {
    let output = jj_capture_in(repo_root, &["workspace", "list"])?;
    let mut map = std::collections::HashMap::new();
    for line in output.lines() {
        let line = line.trim().trim_start_matches('*').trim();
        let Some((name, path)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_string();
        let path = path.trim().to_string();
        if !name.is_empty() && !path.is_empty() {
            map.insert(name, path);
        }
    }
    Ok(map)
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
        bail!("git {} failed", args.join(" "));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn prompt_yes_no(message: &str, default_yes: bool) -> Result<bool> {
    let prompt = if default_yes { "[Y/n]" } else { "[y/N]" };
    print!("{message} {prompt}: ");
    io::stdout().flush()?;
    if !io::stdin().is_terminal() {
        bail!("Non-interactive session; cannot confirm action.");
    }
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_ascii_lowercase();
    if answer.is_empty() {
        return Ok(default_yes);
    }
    Ok(answer == "y" || answer == "yes")
}
