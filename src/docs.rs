//! Auto-generated documentation management.
//!
//! Maintains documentation in `.ai/docs/` that stays in sync with the codebase.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use which::which;

use crate::cli::{DocsAction, DocsCommand, DocsHubOpts, DocsNewOpts};
use crate::config;

/// Docs directory relative to project root.
const DOCS_DIR: &str = ".ai/docs";
const PROJECT_DOCS_DIR: &str = "docs";
const DEFAULT_DOCS_TEMPLATE_ROOT: &str = "~/new/docs";
const HUB_CONTENT_ROOT: &str = "content/docs";
const HUB_PROJECTS_ROOT: &str = "content/docs/projects";

/// Run the docs command.
pub fn run(cmd: DocsCommand) -> Result<()> {
    let project_root = std::env::current_dir()?;
    let docs_dir = project_root.join(DOCS_DIR);

    match cmd.action {
        Some(DocsAction::New(opts)) => create_docs_scaffold(&project_root, opts),
        Some(DocsAction::Hub(opts)) => run_docs_hub(opts),
        Some(DocsAction::List) | None => list_docs(&docs_dir),
        Some(DocsAction::Status) => show_status(&project_root, &docs_dir),
        Some(DocsAction::Sync { commits, dry }) => {
            sync_docs(&project_root, &docs_dir, commits, dry)
        }
        Some(DocsAction::Edit { name }) => edit_doc(&docs_dir, &name),
    }
}

/// List all documentation files.
fn list_docs(docs_dir: &Path) -> Result<()> {
    if !docs_dir.exists() {
        println!("No docs directory. Run `f setup` to create .ai/docs/");
        return Ok(());
    }

    let entries: Vec<_> = fs::read_dir(docs_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|ext| ext == "md").unwrap_or(false))
        .collect();

    if entries.is_empty() {
        println!("No documentation files in .ai/docs/");
        return Ok(());
    }

    println!("Documentation files in .ai/docs/:\n");
    for entry in entries {
        let path = entry.path();
        let name = path.file_stem().unwrap_or_default().to_string_lossy();

        // Read first line as title
        let title = fs::read_to_string(&path)
            .ok()
            .and_then(|content| {
                content
                    .lines()
                    .find(|l| l.starts_with("# "))
                    .map(|l| l.trim_start_matches("# ").to_string())
            })
            .unwrap_or_default();

        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        let size_str = if size > 1024 {
            format!("{:.1}KB", size as f64 / 1024.0)
        } else {
            format!("{}B", size)
        };

        println!("  {:<15} {:>8}  {}", name, size_str, title);
    }

    Ok(())
}

/// Show documentation status.
fn show_status(project_root: &Path, docs_dir: &Path) -> Result<()> {
    if !docs_dir.exists() {
        println!("No docs directory. Run `f setup` to create .ai/docs/");
        return Ok(());
    }

    // Get recent commits
    let output = Command::new("git")
        .args(["log", "--oneline", "-10"])
        .current_dir(project_root)
        .output()
        .context("failed to run git log")?;

    let commits = String::from_utf8_lossy(&output.stdout);

    println!("Recent commits (may need documentation):\n");
    for line in commits.lines() {
        println!("  {}", line);
    }

    // Check last sync marker
    let marker_path = docs_dir.join(".last_sync");
    if marker_path.exists() {
        let last_sync = fs::read_to_string(&marker_path)?;
        println!("\nLast sync: {}", last_sync.trim());
    } else {
        println!("\nNo sync marker found. Run `f docs sync` to update.");
    }

    // List doc files with modification times
    println!("\nDoc files:");
    let entries: Vec<_> = fs::read_dir(docs_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|ext| ext == "md").unwrap_or(false))
        .collect();

    for entry in entries {
        let path = entry.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| {
                let duration = t.elapsed().unwrap_or_default();
                format_duration(duration)
            })
            .unwrap_or_else(|_| "unknown".to_string());

        println!("  {:<20} modified {}", name, modified);
    }

    Ok(())
}

/// Sync documentation with recent commits.
fn sync_docs(project_root: &Path, docs_dir: &Path, commits: usize, dry: bool) -> Result<()> {
    if !docs_dir.exists() {
        bail!("No docs directory. Run `f setup` to create .ai/docs/");
    }

    // Get recent commit messages and diffs
    let output = Command::new("git")
        .args(["log", "--oneline", &format!("-{}", commits)])
        .current_dir(project_root)
        .output()
        .context("failed to run git log")?;

    let commit_list = String::from_utf8_lossy(&output.stdout);

    println!("Analyzing {} recent commits...\n", commits);

    for line in commit_list.lines() {
        println!("  {}", line);
    }

    if dry {
        println!("\n[Dry run] Would analyze commits and update:");
        println!("  - commands.md (if new commands added)");
        println!("  - changelog.md (add entries for new features)");
        println!("  - architecture.md (if structure changed)");
        return Ok(());
    }

    // Update sync marker
    let marker_path = docs_dir.join(".last_sync");
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();

    // Get current HEAD
    let head = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(project_root)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    fs::write(&marker_path, format!("{} ({})\n", now, head))?;

    println!("\n✓ Sync marker updated");
    println!("\nTo fully sync docs, use an AI assistant to:");
    println!("  1. Review recent commits");
    println!("  2. Update changelog.md with new features");
    println!("  3. Update commands.md if CLI changed");
    println!("  4. Update architecture.md if structure changed");

    Ok(())
}

/// Open a doc file in the editor.
fn edit_doc(docs_dir: &Path, name: &str) -> Result<()> {
    let doc_path = docs_dir.join(format!("{}.md", name));

    if !doc_path.exists() {
        bail!("Doc file not found: {}.md", name);
    }

    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vim".to_string());

    Command::new(&editor)
        .arg(&doc_path)
        .status()
        .with_context(|| format!("failed to open {} with {}", doc_path.display(), editor))?;

    Ok(())
}

/// Format a duration as a human-readable string.
fn format_duration(duration: std::time::Duration) -> String {
    let secs = duration.as_secs();
    if secs < 60 {
        format!("{}s ago", secs)
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

fn create_docs_scaffold(project_root: &Path, opts: DocsNewOpts) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let target_root = match opts.path {
        Some(path) => {
            let raw = path.to_string_lossy();
            let expanded = config::expand_path(&raw);
            if expanded.is_absolute() {
                expanded
            } else {
                cwd.join(expanded)
            }
        }
        None => project_root.to_path_buf(),
    };
    create_docs_scaffold_at(&target_root, opts.force)
}

pub fn create_docs_scaffold_at(project_root: &Path, force: bool) -> Result<()> {
    let docs_dir = project_root.join(PROJECT_DOCS_DIR);
    if docs_dir.exists() {
        if !force {
            bail!("docs/ already exists at {}", docs_dir.display());
        }
        fs::remove_dir_all(&docs_dir)
            .with_context(|| format!("failed to remove {}", docs_dir.display()))?;
    }

    let template_root = config::expand_path(DEFAULT_DOCS_TEMPLATE_ROOT);
    let template_docs = template_root.join(HUB_CONTENT_ROOT);
    if !template_docs.exists() {
        bail!(
            "Docs template not found at {}",
            template_docs.display()
        );
    }

    fs::create_dir_all(&docs_dir)
        .with_context(|| format!("failed to create {}", docs_dir.display()))?;
    copy_dir_filtered(&template_docs, &docs_dir, true)?;
    ensure_index_file(&docs_dir, "Docs")?;

    println!("Created {}", docs_dir.display());
    Ok(())
}

fn run_docs_hub(opts: DocsHubOpts) -> Result<()> {
    let hub_root = config::expand_path(&opts.hub_root);
    let template_root = config::expand_path(&opts.template_root);
    ensure_docs_hub(&hub_root, &template_root)?;

    let code_root = config::expand_path(&opts.code_root);
    let org_root = config::expand_path(&opts.org_root);
    let projects = collect_projects(&code_root, &org_root, !opts.no_ai)?;
    sync_docs_hub_content(&hub_root, &projects)?;

    if opts.sync_only {
        println!("Docs hub content synced.");
        return Ok(());
    }

    ensure_docs_hub_deps(&hub_root)?;
    let url = format!("http://{}:{}", opts.host, opts.port);
    if !opts.no_open {
        open_in_browser(&url);
    }
    run_docs_hub_dev(&hub_root, &opts.host, opts.port)
}

fn ensure_docs_hub(hub_root: &Path, template_root: &Path) -> Result<()> {
    if hub_root.join("package.json").exists() {
        return Ok(());
    }
    if !template_root.exists() {
        bail!("Docs template root not found: {}", template_root.display());
    }
    fs::create_dir_all(hub_root)
        .with_context(|| format!("failed to create {}", hub_root.display()))?;
    copy_template_dir(template_root, hub_root)?;
    Ok(())
}

fn ensure_docs_hub_deps(hub_root: &Path) -> Result<()> {
    let node_modules = hub_root.join("node_modules");
    if node_modules.exists() {
        return Ok(());
    }
    if which("bun").is_ok() {
        run_command("bun", &["install"], hub_root)
    } else if which("npm").is_ok() {
        run_command("npm", &["install"], hub_root)
    } else {
        bail!("bun or npm is required to install docs hub dependencies");
    }
}

fn run_docs_hub_dev(hub_root: &Path, host: &str, port: u16) -> Result<()> {
    if which("bun").is_ok() {
        let port_arg = port.to_string();
        let host_arg = host.to_string();
        let status = Command::new("bun")
            .args(["run", "dev", "--", "--port", &port_arg, "--hostname", &host_arg])
            .current_dir(hub_root)
            .status()
            .context("failed to run bun dev")?;
        if !status.success() {
            bail!("docs hub dev server exited with error");
        }
        return Ok(());
    }

    if which("npm").is_ok() {
        let port_arg = port.to_string();
        let host_arg = host.to_string();
        let status = Command::new("npm")
            .args(["run", "dev", "--", "--port", &port_arg, "--hostname", &host_arg])
            .current_dir(hub_root)
            .status()
            .context("failed to run npm dev")?;
        if !status.success() {
            bail!("docs hub dev server exited with error");
        }
        return Ok(());
    }

    bail!("bun or npm is required to run docs hub dev server");
}

fn open_in_browser(url: &str) {
    let _ = Command::new("open").arg(url).status();
}

fn run_command(cmd: &str, args: &[&str], cwd: &Path) -> Result<()> {
    let status = Command::new(cmd)
        .args(args)
        .current_dir(cwd)
        .status()
        .with_context(|| format!("failed to run {}", cmd))?;
    if !status.success() {
        bail!("{} failed", cmd);
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct ProjectDocs {
    name: String,
    slug: String,
    root: PathBuf,
    docs_dir: Option<PathBuf>,
    ai_docs_dir: Option<PathBuf>,
}

fn collect_projects(code_root: &Path, org_root: &Path, include_ai: bool) -> Result<Vec<ProjectDocs>> {
    let mut projects = HashMap::new();
    collect_projects_from_root(&mut projects, code_root, None, include_ai)?;
    collect_projects_from_root(&mut projects, org_root, Some("org"), include_ai)?;
    let mut out: Vec<ProjectDocs> = projects.into_values().collect();
    out.sort_by(|a, b| a.slug.cmp(&b.slug));
    Ok(out)
}

fn collect_projects_from_root(
    projects: &mut HashMap<String, ProjectDocs>,
    root: &Path,
    prefix: Option<&str>,
    include_ai: bool,
) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }

    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let name = dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        if should_skip_dir(name) {
            continue;
        }

        let docs_dir = dir.join(PROJECT_DOCS_DIR);
        let ai_docs_dir = dir.join(".ai").join("docs");
        let has_docs = docs_dir.is_dir();
        let has_ai = include_ai && ai_docs_dir.is_dir();

        if has_docs || has_ai {
            let slug = slug_for_path(&dir, root, prefix);
            let name = dir
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(&slug)
                .to_string();
            projects.entry(slug.clone()).or_insert(ProjectDocs {
                name,
                slug,
                root: dir.clone(),
                docs_dir: if has_docs { Some(docs_dir) } else { None },
                ai_docs_dir: if has_ai { Some(ai_docs_dir) } else { None },
            });
            continue;
        }

        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if !file_type.is_dir() {
                continue;
            }
            let child_name = entry.file_name().to_string_lossy().to_string();
            if should_skip_dir(&child_name) {
                continue;
            }
            stack.push(path);
        }
    }

    Ok(())
}

fn slug_for_path(path: &Path, root: &Path, prefix: Option<&str>) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let mut slug = relative.to_string_lossy().replace('\\', "/");
    slug = slug.trim().trim_start_matches('/').to_string();
    if let Some(prefix) = prefix {
        if slug.is_empty() {
            return prefix.to_string();
        }
        return format!("{prefix}/{slug}");
    }
    if slug.is_empty() {
        "root".to_string()
    } else {
        slug
    }
}

fn should_skip_dir(name: &str) -> bool {
    if name.starts_with('.') {
        return true;
    }
    matches!(
        name,
        "node_modules"
            | "target"
            | "dist"
            | "build"
            | ".git"
            | ".hg"
            | ".svn"
            | "__pycache__"
            | ".pytest_cache"
            | ".mypy_cache"
            | "venv"
            | ".venv"
            | "vendor"
            | "Pods"
            | ".cargo"
            | ".rustup"
            | ".next"
            | ".turbo"
            | ".cache"
    )
}

fn sync_docs_hub_content(hub_root: &Path, projects: &[ProjectDocs]) -> Result<()> {
    let content_root = hub_root.join(HUB_CONTENT_ROOT);
    let projects_root = hub_root.join(HUB_PROJECTS_ROOT);

    if projects_root.exists() {
        fs::remove_dir_all(&projects_root)
            .with_context(|| format!("failed to remove {}", projects_root.display()))?;
    }
    fs::create_dir_all(&projects_root)
        .with_context(|| format!("failed to create {}", projects_root.display()))?;

    for project in projects {
        let project_root = projects_root.join(&project.slug);
        fs::create_dir_all(&project_root)
            .with_context(|| format!("failed to create {}", project_root.display()))?;

        let mut lines = Vec::new();
        lines.push("---".to_string());
        lines.push(format!("title: {}", project.name));
        lines.push("---".to_string());
        lines.push(String::new());
        lines.push(format!("# {}", project.name));
        lines.push(String::new());
        lines.push(format!("Path: `{}`", project.root.display()));
        lines.push(String::new());

        if let Some(docs_dir) = &project.docs_dir {
            let dest = project_root.join("docs");
            copy_dir_filtered(docs_dir, &dest, true)?;
            ensure_index_file(&dest, "Docs")?;
            lines.push("- [Docs](./docs)".to_string());
        }
        if let Some(ai_docs_dir) = &project.ai_docs_dir {
            let dest = project_root.join("ai");
            copy_dir_filtered(ai_docs_dir, &dest, true)?;
            ensure_index_file(&dest, "AI Docs")?;
            lines.push("- [AI Docs](./ai)".to_string());
        }

        if lines.last().map(|line| !line.is_empty()).unwrap_or(false) {
            lines.push(String::new());
        }

        let index_path = project_root.join("index.mdx");
        fs::write(&index_path, lines.join("\n"))
            .with_context(|| format!("failed to write {}", index_path.display()))?;
    }

    fs::create_dir_all(&content_root)
        .with_context(|| format!("failed to create {}", content_root.display()))?;
    let root_index = content_root.join("index.mdx");
    fs::write(&root_index, render_root_index(projects))
        .with_context(|| format!("failed to write {}", root_index.display()))?;

    Ok(())
}

fn render_root_index(projects: &[ProjectDocs]) -> String {
    let mut lines = Vec::new();
    lines.push("---".to_string());
    lines.push("title: Docs".to_string());
    lines.push("---".to_string());
    lines.push(String::new());
    lines.push("# Docs Hub".to_string());
    lines.push(String::new());
    if projects.is_empty() {
        lines.push("No docs found yet.".to_string());
        lines.push(String::new());
        lines.push("Add `docs/` or `.ai/docs` to a project and run `f docs hub` again.".to_string());
        lines.push(String::new());
        return lines.join("\n");
    }
    lines.push("Projects:".to_string());
    lines.push(String::new());
    for project in projects {
        lines.push(format!("- [{}](./projects/{})", project.name, project.slug));
    }
    lines.push(String::new());
    lines.join("\n")
}

fn ensure_index_file(dir: &Path, title: &str) -> Result<()> {
    let index_md = dir.join("index.md");
    let index_mdx = dir.join("index.mdx");
    if index_md.exists() || index_mdx.exists() {
        return Ok(());
    }
    let content = format!("---\ntitle: {}\n---\n", title);
    fs::write(&index_mdx, content)
        .with_context(|| format!("failed to write {}", index_mdx.display()))?;
    Ok(())
}

fn copy_dir_filtered(from: &Path, to: &Path, allow_assets: bool) -> Result<()> {
    fs::create_dir_all(to).with_context(|| format!("failed to create {}", to.display()))?;
    for entry in fs::read_dir(from).with_context(|| format!("failed to read {}", from.display()))? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        let name = entry.file_name().to_string_lossy().to_string();
        if should_skip_dir(&name) {
            continue;
        }
        let dest = to.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_filtered(&path, &dest, allow_assets)?;
        } else if file_type.is_file() {
            if should_copy_doc_file(&path, allow_assets) {
                fs::copy(&path, &dest)
                    .with_context(|| format!("failed to copy {}", path.display()))?;
            }
        }
    }
    Ok(())
}

fn should_copy_doc_file(path: &Path, allow_assets: bool) -> bool {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("md") | Some("mdx") => true,
        _ => allow_assets,
    }
}

fn copy_template_dir(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to).with_context(|| format!("failed to create {}", to.display()))?;
    for entry in fs::read_dir(from).with_context(|| format!("failed to read {}", from.display()))? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        let name = entry.file_name().to_string_lossy().to_string();
        if file_type.is_dir() && should_skip_template_dir(&name) {
            continue;
        }
        let dest = to.join(entry.file_name());
        if file_type.is_dir() {
            copy_template_dir(&path, &dest)?;
        } else if file_type.is_file() {
            fs::copy(&path, &dest)
                .with_context(|| format!("failed to copy {}", path.display()))?;
        }
    }
    Ok(())
}

fn should_skip_template_dir(name: &str) -> bool {
    if matches!(name, ".source") {
        return false;
    }
    if name.starts_with('.') {
        return true;
    }
    matches!(name, "node_modules" | "dist" | "build" | ".next")
}
