//! Auto-generated documentation management.
//!
//! Maintains documentation in `.ai/docs/` that stays in sync with the codebase.

use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use which::which;

use crate::cli::{
    DeployCommand, DocsAction, DocsCommand, DocsDeployOpts, DocsHubOpts, DocsNewOpts,
};
use crate::{config, deploy};

/// Docs directory relative to project root.
const DOCS_DIR: &str = ".ai/docs";
const PROJECT_DOCS_DIR: &str = "docs";
const DEFAULT_DOCS_TEMPLATE_ROOT: &str = "~/new/docs";
const HUB_CONTENT_ROOT: &str = "content/docs";
const DOCS_HUB_FOCUS_FILE: &str = ".flow-focus";

/// Run the docs command.
pub fn run(cmd: DocsCommand) -> Result<()> {
    let project_root = std::env::current_dir()?;
    let docs_dir = project_root.join(DOCS_DIR);

    match cmd.action {
        Some(DocsAction::New(opts)) => create_docs_scaffold(&project_root, opts),
        Some(DocsAction::Hub(opts)) => run_docs_hub(opts),
        None => open_project_docs(&project_root),
        Some(DocsAction::Deploy(opts)) => deploy_docs_hub(&project_root, opts),
        Some(DocsAction::List) => list_docs(&docs_dir),
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
        if docs_dir.is_file() {
            bail!("docs/ exists but is a file: {}", docs_dir.display());
        }
        if !force {
            let template_root = config::expand_path(DEFAULT_DOCS_TEMPLATE_ROOT);
            let template_docs = template_root.join(HUB_CONTENT_ROOT);
            if !template_docs.exists() {
                bail!("Docs template not found at {}", template_docs.display());
            }
            merge_docs_scaffold(&docs_dir, &template_docs)?;
            ensure_index_file(&docs_dir, "Docs")?;
            println!(
                "Docs already exists; merged template into {}",
                docs_dir.display()
            );
            return Ok(());
        }
        fs::remove_dir_all(&docs_dir)
            .with_context(|| format!("failed to remove {}", docs_dir.display()))?;
    }

    let template_root = config::expand_path(DEFAULT_DOCS_TEMPLATE_ROOT);
    let template_docs = template_root.join(HUB_CONTENT_ROOT);
    if !template_docs.exists() {
        bail!("Docs template not found at {}", template_docs.display());
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
    run_docs_hub_dev(&hub_root, &opts.host, opts.port, opts.no_open)
}

fn ensure_docs_hub(hub_root: &Path, template_root: &Path) -> Result<()> {
    if hub_root.join("package.json").exists() {
        sync_docs_hub_template_file(hub_root, template_root, "mdx-components.tsx", true)?;
        sync_docs_hub_template_file(hub_root, template_root, "next.config.mjs", true)?;
        sync_docs_hub_template_file(hub_root, template_root, "public/favicon.ico", false)?;
        sync_docs_hub_template_file(hub_root, template_root, "wrangler.toml", false)?;
        ensure_docs_hub_flow_toml(hub_root, template_root)?;
        ensure_docs_hub_config(hub_root)?;
        ensure_docs_hub_layout(hub_root)?;
        return Ok(());
    }
    if !template_root.exists() {
        bail!("Docs template root not found: {}", template_root.display());
    }
    fs::create_dir_all(hub_root)
        .with_context(|| format!("failed to create {}", hub_root.display()))?;
    copy_template_dir(template_root, hub_root)?;
    ensure_docs_hub_config(hub_root)?;
    ensure_docs_hub_layout(hub_root)?;
    Ok(())
}

pub fn ensure_docs_hub_daemon(opts: &DocsHubOpts) -> Result<()> {
    let focus_root = focus_project_root_from_env();
    ensure_docs_hub_daemon_with_focus(opts, focus_root.as_deref())
}

fn ensure_docs_hub_daemon_with_focus(opts: &DocsHubOpts, focus_root: Option<&Path>) -> Result<()> {
    let hub_root = config::expand_path(&opts.hub_root);
    let template_root = config::expand_path(&opts.template_root);
    println!(
        "Docs hub: root={} template={}",
        hub_root.display(),
        template_root.display()
    );
    ensure_docs_hub(&hub_root, &template_root)?;

    let code_root = config::expand_path(&opts.code_root);
    let org_root = config::expand_path(&opts.org_root);
    let focus_project =
        focus_root.and_then(|root| project_docs_for_root(root, &code_root, &org_root, !opts.no_ai));

    if let Some(project) = focus_project {
        println!(
            "Docs hub: syncing focused project {} ({})",
            project.name, project.slug
        );
        let expected_path = hub_root.join(HUB_CONTENT_ROOT).join(&project.slug);
        let focus_match = read_docs_hub_focus_marker(&hub_root)
            .map(|slug| slug == project.slug)
            .unwrap_or(false);
        let hub_running = docs_hub_healthy(&opts.host, opts.port);
        if !(focus_match && expected_path.exists() && hub_running) {
            sync_docs_hub_content_focus(&hub_root, &project)?;
        } else {
            println!("Docs hub: already running; skipping sync.");
        }
    } else {
        let projects = collect_projects(&code_root, &org_root, !opts.no_ai)?;
        println!(
            "Docs hub: syncing {} project(s) from {} and {}",
            projects.len(),
            code_root.display(),
            org_root.display()
        );
        sync_docs_hub_content(&hub_root, &projects)?;
    }

    let needs_reset = docs_hub_needs_reset(&hub_root)?;
    let was_running = docs_hub_healthy(&opts.host, opts.port);
    if needs_reset {
        println!("Docs hub: stale index detected; resetting cache.");
        if let Some(pid) = load_docs_hub_pid()? {
            terminate_process(pid).ok();
            remove_docs_hub_pid().ok();
        }
        kill_docs_hub_by_port(opts.port).ok();
        remove_docs_hub_cache(&hub_root).ok();
    }

    if was_running && !needs_reset && focus_root.is_none() {
        println!("Docs hub: restarting to apply latest docs.");
        if let Some(pid) = load_docs_hub_pid()? {
            terminate_process(pid).ok();
            remove_docs_hub_pid().ok();
        }
        kill_docs_hub_by_port(opts.port).ok();
        remove_docs_hub_cache(&hub_root).ok();
    }

    if !needs_reset && !was_running {
        if let Some(pid) = load_docs_hub_pid()? {
            if process_alive(pid)? {
                println!(
                    "Docs hub: already running at http://{}:{}",
                    opts.host, opts.port
                );
                return Ok(());
            }
            remove_docs_hub_pid().ok();
        }
    }

    if was_running && !needs_reset {
        return Ok(());
    }

    ensure_docs_hub_deps(&hub_root)?;
    start_docs_hub_daemon(&hub_root, &opts.host, opts.port)?;
    println!(
        "Docs hub: starting dev server on http://{}:{}",
        opts.host, opts.port
    );
    wait_for_port(&opts.host, opts.port, std::time::Duration::from_secs(10));
    Ok(())
}

pub fn stop_docs_hub_daemon() -> Result<()> {
    if let Some(pid) = load_docs_hub_pid()? {
        terminate_process(pid).ok();
        remove_docs_hub_pid().ok();
    }
    kill_docs_hub_by_port(4410).ok();
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

fn run_docs_hub_dev(hub_root: &Path, host: &str, port: u16, no_open: bool) -> Result<()> {
    let mut cmd = if which("bun").is_ok() {
        let port_arg = port.to_string();
        let host_arg = host.to_string();
        let mut cmd = Command::new("bun");
        cmd.args([
            "run",
            "dev",
            "--",
            "--port",
            &port_arg,
            "--hostname",
            &host_arg,
        ]);
        cmd
    } else if which("npm").is_ok() {
        let port_arg = port.to_string();
        let host_arg = host.to_string();
        let mut cmd = Command::new("npm");
        cmd.args([
            "run",
            "dev",
            "--",
            "--port",
            &port_arg,
            "--hostname",
            &host_arg,
        ]);
        cmd
    } else {
        bail!("bun or npm is required to run docs hub dev server");
    };

    let mut child = cmd
        .current_dir(hub_root)
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .context("failed to start docs hub dev server")?;

    if !no_open {
        let url = format!("http://{}:{}", host, port);
        wait_for_port(host, port, std::time::Duration::from_secs(10));
        open_in_browser(&url);
    }

    let status = child.wait().context("failed to wait on docs hub")?;
    if !status.success() {
        bail!("docs hub dev server exited with error");
    }
    Ok(())
}

fn start_docs_hub_daemon(hub_root: &Path, host: &str, port: u16) -> Result<()> {
    let mut cmd = if which("bun").is_ok() {
        let port_arg = port.to_string();
        let host_arg = host.to_string();
        let mut cmd = Command::new("bun");
        cmd.args([
            "run",
            "dev",
            "--",
            "--port",
            &port_arg,
            "--hostname",
            &host_arg,
        ]);
        cmd
    } else if which("npm").is_ok() {
        let port_arg = port.to_string();
        let host_arg = host.to_string();
        let mut cmd = Command::new("npm");
        cmd.args([
            "run",
            "dev",
            "--",
            "--port",
            &port_arg,
            "--hostname",
            &host_arg,
        ]);
        cmd
    } else {
        bail!("bun or npm is required to run docs hub dev server");
    };

    let child = cmd
        .current_dir(hub_root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("failed to start docs hub daemon")?;
    persist_docs_hub_pid(child.id())?;
    Ok(())
}

fn open_in_browser(url: &str) {
    let _ = Command::new("open").arg(url).status();
}

struct DirGuard {
    previous: PathBuf,
}

impl DirGuard {
    fn new(path: &Path) -> Result<Self> {
        let previous = std::env::current_dir().context("failed to read current directory")?;
        std::env::set_current_dir(path)
            .with_context(|| format!("failed to switch to {}", path.display()))?;
        Ok(Self { previous })
    }
}

impl Drop for DirGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.previous);
    }
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

fn attach_pages_domain(hub_root: &Path, project: &str, domain: &str) -> Result<()> {
    println!("Attaching custom domain {domain} to {project}...");
    let mut cmd = if which("bun").is_ok() {
        let mut cmd = Command::new("bun");
        cmd.args(["x", "wrangler", "pages", "domain", "add", project, domain]);
        cmd
    } else if which("npx").is_ok() {
        let mut cmd = Command::new("npx");
        cmd.args(["wrangler", "pages", "domain", "add", project, domain]);
        cmd
    } else if which("npm").is_ok() {
        let mut cmd = Command::new("npm");
        cmd.args([
            "exec", "wrangler", "--", "pages", "domain", "add", project, domain,
        ]);
        cmd
    } else {
        bail!("bun, npx, or npm is required to run wrangler");
    };
    let status = cmd
        .current_dir(hub_root)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .context("failed to run wrangler pages domain add")?;
    if !status.success() {
        bail!("wrangler pages domain add failed");
    }
    Ok(())
}

fn prompt_line(message: &str, default: Option<&str>) -> Result<String> {
    if let Some(default) = default {
        print!("{message} [{default}]: ");
    } else {
        print!("{message}: ");
    }
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(default.unwrap_or("").to_string());
    }
    Ok(trimmed.to_string())
}

fn prompt_yes_no(message: &str, default_yes: bool) -> Result<bool> {
    let prompt = if default_yes { "[Y/n]" } else { "[y/N]" };
    print!("{message} {prompt}: ");
    io::stdout().flush()?;
    if io::stdin().is_terminal() {
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let answer = input.trim().to_ascii_lowercase();
        if answer.is_empty() {
            return Ok(default_yes);
        }
        return Ok(answer == "y" || answer == "yes");
    }
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_ascii_lowercase();
    if answer.is_empty() {
        return Ok(default_yes);
    }
    Ok(answer == "y" || answer == "yes")
}

fn wait_for_port(host: &str, port: u16, timeout: std::time::Duration) {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if std::net::TcpStream::connect((host, port)).is_ok() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

fn docs_hub_healthy(host: &str, port: u16) -> bool {
    std::net::TcpStream::connect((host, port)).is_ok()
}

fn docs_hub_pid_path() -> PathBuf {
    config::global_state_dir().join("docs-hub.pid")
}

fn load_docs_hub_pid() -> Result<Option<u32>> {
    let path = docs_hub_pid_path();
    if !path.exists() {
        return Ok(None);
    }
    let contents =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let pid: u32 = contents.trim().parse().ok().unwrap_or(0);
    if pid == 0 { Ok(None) } else { Ok(Some(pid)) }
}

fn persist_docs_hub_pid(pid: u32) -> Result<()> {
    let path = docs_hub_pid_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(&path, pid.to_string())
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn remove_docs_hub_pid() -> Result<()> {
    let path = docs_hub_pid_path();
    if path.exists() {
        fs::remove_file(path).ok();
    }
    Ok(())
}

fn process_alive(pid: u32) -> Result<bool> {
    #[cfg(unix)]
    {
        let status = Command::new("kill").arg("-0").arg(pid.to_string()).status();
        return Ok(status.map(|s| s.success()).unwrap_or(false));
    }

    #[cfg(windows)]
    {
        let output = Command::new("tasklist")
            .output()
            .context("failed to invoke tasklist")?;
        if !output.status.success() {
            return Ok(false);
        }
        let needle = pid.to_string();
        let body = String::from_utf8_lossy(&output.stdout);
        Ok(body.lines().any(|line| line.contains(&needle)))
    }
}

fn terminate_process(pid: u32) -> Result<()> {
    #[cfg(unix)]
    {
        Command::new("kill")
            .arg(pid.to_string())
            .status()
            .context("failed to invoke kill")?;
        return Ok(());
    }

    #[cfg(windows)]
    {
        Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .status()
            .context("failed to invoke taskkill")?;
        Ok(())
    }
}

fn open_project_docs(project_root: &Path) -> Result<()> {
    let code_root = config::expand_path("~/code");
    let org_root = config::expand_path("~/org");
    let Some(project) = project_docs_for_root(project_root, &code_root, &org_root, false) else {
        bail!("Unable to resolve docs for {}", project_root.display());
    };

    let hub_opts = DocsHubOpts {
        host: "127.0.0.1".to_string(),
        port: 4410,
        hub_root: "~/.config/flow/docs-hub".to_string(),
        template_root: DEFAULT_DOCS_TEMPLATE_ROOT.to_string(),
        code_root: "~/code".to_string(),
        org_root: "~/org".to_string(),
        no_ai: true,
        no_open: true,
        sync_only: false,
    };
    ensure_docs_hub_daemon_with_focus(&hub_opts, Some(project_root))?;

    if !(project_root.starts_with(&code_root) || project_root.starts_with(&org_root)) {
        println!(
            "Docs hub only indexes ~/code and ~/org; {} may not be available.",
            project_root.display()
        );
    }

    let url = format!(
        "http://{}:{}/{}",
        hub_opts.host, hub_opts.port, project.slug
    );
    println!(
        "Docs hub open: project={} slug={}",
        project_root.display(),
        project.slug
    );
    open_in_browser(&url);
    println!("Opened {url}");
    Ok(())
}

fn deploy_docs_hub(project_root: &Path, opts: DocsDeployOpts) -> Result<()> {
    let code_root = config::expand_path("~/code");
    let org_root = config::expand_path("~/org");
    let Some(project) = project_docs_for_root(project_root, &code_root, &org_root, false) else {
        bail!("Unable to resolve docs for {}", project_root.display());
    };

    let default_project = if !project.slug.is_empty() {
        project.slug.clone()
    } else {
        slugify_token(&project.name)
    };
    let project_name = opts.project.as_deref().unwrap_or(&default_project).trim();
    let project_name = if project_name.is_empty() {
        default_project.clone()
    } else {
        slugify_token(project_name)
    };

    let domain = if let Some(domain) = opts.domain.as_deref() {
        let trimmed = domain.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    } else if opts.yes {
        None
    } else {
        let input = prompt_line("Custom domain (leave blank to skip)", None)?;
        let trimmed = input.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    };

    if !opts.yes {
        println!("Docs deploy:");
        println!("  Project: {}", project_name);
        println!("  Source: {}", project_root.display());
        if let Some(domain) = &domain {
            println!("  Domain: {}", domain);
        } else {
            println!("  Domain: (none)");
        }
        if !prompt_yes_no("Proceed with deploy?", true)? {
            println!("Canceled.");
            return Ok(());
        }
    }

    let hub_root = config::expand_path("~/.config/flow/docs-hub");
    let template_root = config::expand_path(DEFAULT_DOCS_TEMPLATE_ROOT);
    ensure_docs_hub(&hub_root, &template_root)?;
    println!(
        "Docs hub: syncing focused project {} ({})",
        project.name, project.slug
    );
    sync_docs_hub_content_focus(&hub_root, &project)?;
    ensure_docs_hub_deps(&hub_root)?;

    let _guard = DirGuard::new(&hub_root)?;
    unsafe {
        std::env::set_var("FLOW_DOCS_PROJECT", &project_name);
    }
    deploy::run(DeployCommand { action: None })?;

    if let Some(domain) = &domain {
        attach_pages_domain(&hub_root, &project_name, domain)?;
    }

    println!("Docs deploy complete.");
    Ok(())
}

#[derive(Debug, Clone)]
struct ProjectDocs {
    name: String,
    slug: String,
    slug_base: String,
    slug_path: String,
    root: PathBuf,
    docs_dir: Option<PathBuf>,
    ai_docs_dir: Option<PathBuf>,
    ai_web_dir: Option<PathBuf>,
}

fn collect_projects(
    code_root: &Path,
    org_root: &Path,
    include_ai: bool,
) -> Result<Vec<ProjectDocs>> {
    let mut projects = Vec::new();
    collect_projects_from_root(&mut projects, code_root, "code", include_ai)?;
    collect_projects_from_root(&mut projects, org_root, "org", include_ai)?;
    resolve_project_slugs(&mut projects);
    projects.sort_by(|a, b| a.slug.cmp(&b.slug));
    Ok(projects)
}

fn collect_projects_from_root(
    projects: &mut Vec<ProjectDocs>,
    root: &Path,
    scope: &str,
    include_ai: bool,
) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }

    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let name = dir.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if should_skip_dir(name) {
            continue;
        }

        let docs_dir = dir.join(PROJECT_DOCS_DIR);
        let ai_docs_dir = dir.join(".ai").join("docs");
        let ai_web_dir = dir.join(".ai").join("web");
        let has_docs = docs_dir.is_dir();
        let has_ai = include_ai && ai_docs_dir.is_dir();
        let has_web = include_ai && ai_web_dir.is_dir();

        if has_docs || has_ai || has_web {
            let path_slug = slug_for_path(&dir, root, Some(scope));
            let name = project_name_from_flow_toml(&dir).unwrap_or_else(|| {
                dir.file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(&path_slug)
                    .to_string()
            });
            let slug_base = slugify_project_name(&name, &path_slug);
            projects.push(ProjectDocs {
                name,
                slug: slug_base.clone(),
                slug_base,
                slug_path: path_slug,
                root: dir.clone(),
                docs_dir: if has_docs { Some(docs_dir) } else { None },
                ai_docs_dir: if has_ai { Some(ai_docs_dir) } else { None },
                ai_web_dir: if has_web { Some(ai_web_dir) } else { None },
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

fn project_name_from_flow_toml(project_root: &Path) -> Option<String> {
    let flow_path = project_root.join("flow.toml");
    if !flow_path.exists() {
        return None;
    }
    let cfg = config::load(&flow_path).ok()?;
    cfg.project_name
}

fn slugify_project_name(name: &str, fallback: &str) -> String {
    let slug = slugify_token(name);
    if slug.is_empty() {
        slugify_token(fallback)
    } else {
        slug
    }
}

fn slugify_token(input: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in input.chars() {
        let ch = ch.to_ascii_lowercase();
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            last_dash = false;
        } else if matches!(ch, '-' | '_' | ' ' | '.' | '/' | '\\') {
            if !last_dash {
                out.push('-');
                last_dash = true;
            }
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    trimmed
}

fn resolve_project_slugs(projects: &mut [ProjectDocs]) {
    let mut counts = std::collections::HashMap::new();
    for project in projects.iter() {
        *counts.entry(project.slug_base.clone()).or_insert(0usize) += 1;
    }
    let mut used = std::collections::HashSet::new();
    for project in projects.iter_mut() {
        let mut slug = if project.slug_base.is_empty() {
            project.slug_path.clone()
        } else if counts.get(&project.slug_base).copied().unwrap_or(0) > 1 {
            project.slug_path.clone()
        } else {
            project.slug_base.clone()
        };
        if slug.is_empty() {
            slug = project.slug_path.clone();
        }
        let mut candidate = slug.clone();
        let mut counter = 2usize;
        while used.contains(&candidate) {
            candidate = format!("{}-{}", slug, counter);
            counter += 1;
        }
        used.insert(candidate.clone());
        project.slug = candidate;
    }
}

fn project_slug_candidates(
    project_root: &Path,
    code_root: &Path,
    org_root: &Path,
) -> (String, String) {
    let scope = if project_root.starts_with(org_root) {
        "org"
    } else if project_root.starts_with(code_root) {
        "code"
    } else {
        "project"
    };
    let path_slug = if scope == "project" {
        project_root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("project")
            .to_string()
    } else {
        slug_for_path(
            project_root,
            if scope == "org" { org_root } else { code_root },
            Some(scope),
        )
    };
    let name = project_name_from_flow_toml(project_root).unwrap_or_else(|| {
        project_root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&path_slug)
            .to_string()
    });
    let slug_base = slugify_project_name(&name, &path_slug);
    (slug_base, path_slug)
}

fn focus_project_root_from_env() -> Option<PathBuf> {
    let raw = std::env::var("FLOW_DOCS_FOCUS").ok()?;
    let value = raw.trim();
    if value.is_empty() {
        return None;
    }
    let lower = value.to_ascii_lowercase();
    let root = if matches!(lower.as_str(), "1" | "true" | "yes") {
        resolve_project_root_from_cwd()
    } else {
        let expanded = config::expand_path(value);
        if expanded.is_file() && expanded.file_name().and_then(|s| s.to_str()) == Some("flow.toml")
        {
            expanded.parent().map(|p| p.to_path_buf())
        } else if expanded.is_dir() {
            Some(expanded)
        } else {
            None
        }
    }?;
    Some(root)
}

fn resolve_project_root_from_cwd() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    if cwd.join("flow.toml").exists() {
        return Some(cwd);
    }
    let flow_path = find_flow_toml(&cwd)?;
    flow_path.parent().map(|p| p.to_path_buf())
}

fn find_flow_toml(start: &Path) -> Option<PathBuf> {
    let mut current = start.to_path_buf();
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

fn project_docs_for_root(
    project_root: &Path,
    code_root: &Path,
    org_root: &Path,
    include_ai: bool,
) -> Option<ProjectDocs> {
    if !project_root.exists() {
        return None;
    }
    let docs_dir = project_root.join(PROJECT_DOCS_DIR);
    let ai_docs_dir = project_root.join(".ai").join("docs");
    let ai_web_dir = project_root.join(".ai").join("web");
    let has_docs = docs_dir.is_dir();
    let has_ai = include_ai && ai_docs_dir.is_dir();
    let has_web = include_ai && ai_web_dir.is_dir();

    let name = project_name_from_flow_toml(project_root).unwrap_or_else(|| {
        project_root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("project")
            .to_string()
    });
    let (slug_base, slug_path) = project_slug_candidates(project_root, code_root, org_root);
    let mut project = ProjectDocs {
        name,
        slug: slug_base.clone(),
        slug_base,
        slug_path,
        root: project_root.to_path_buf(),
        docs_dir: if has_docs { Some(docs_dir) } else { None },
        ai_docs_dir: if has_ai { Some(ai_docs_dir) } else { None },
        ai_web_dir: if has_web { Some(ai_web_dir) } else { None },
    };
    if project.slug.is_empty() {
        project.slug = project.slug_path.clone();
    }
    let mut projects = vec![project];
    resolve_project_slugs(&mut projects);
    projects.into_iter().next()
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
    let projects_root = content_root.clone();
    clear_docs_hub_focus_marker(hub_root).ok();

    if content_root.exists() {
        fs::remove_dir_all(&content_root)
            .with_context(|| format!("failed to remove {}", content_root.display()))?;
    }
    fs::create_dir_all(&projects_root)
        .with_context(|| format!("failed to create {}", projects_root.display()))?;

    println!(
        "Docs hub: writing {} project(s) to {}",
        projects.len(),
        projects_root.display()
    );
    for project in projects {
        let project_root = projects_root.join(&project.slug);
        fs::create_dir_all(&project_root)
            .with_context(|| format!("failed to create {}", project_root.display()))?;

        if let Some(docs_dir) = &project.docs_dir {
            copy_docs_dir_with_frontmatter(docs_dir, &project_root, true)?;
        }
        if let Some(ai_docs_dir) = &project.ai_docs_dir {
            copy_docs_dir_with_frontmatter(ai_docs_dir, &project_root, false)?;
        }
        if let Some(ai_web_dir) = &project.ai_web_dir {
            copy_docs_dir_with_frontmatter(ai_web_dir, &project_root, false)?;
        }

        let index_path = project_root.join("index.mdx");
        if let Some(content) = project_readme_content(&project.root, &project.name)? {
            let index_md = project_root.join("index.md");
            if index_md.exists() {
                fs::remove_file(&index_md).ok();
            }
            fs::write(&index_path, content)
                .with_context(|| format!("failed to write {}", index_path.display()))?;
        } else if !index_path.exists() {
            let mut lines = Vec::new();
            lines.push("---".to_string());
            lines.push(format!("title: {}", quote_yaml_string(&project.name)));
            lines.push("---".to_string());
            lines.push(String::new());
            fs::write(&index_path, lines.join("\n"))
                .with_context(|| format!("failed to write {}", index_path.display()))?;
        }
    }

    let root_index = content_root.join("index.mdx");
    fs::write(&root_index, render_root_index(projects))
        .with_context(|| format!("failed to write {}", root_index.display()))?;

    Ok(())
}

fn sync_docs_hub_content_focus(hub_root: &Path, project: &ProjectDocs) -> Result<()> {
    let content_root = hub_root.join(HUB_CONTENT_ROOT);
    let projects_root = content_root.clone();

    if content_root.exists() {
        fs::remove_dir_all(&content_root)
            .with_context(|| format!("failed to remove {}", content_root.display()))?;
    }
    fs::create_dir_all(&projects_root)
        .with_context(|| format!("failed to create {}", projects_root.display()))?;

    let project_root = projects_root.join(&project.slug);
    fs::create_dir_all(&project_root)
        .with_context(|| format!("failed to create {}", project_root.display()))?;

    if let Some(docs_dir) = &project.docs_dir {
        copy_docs_dir_with_frontmatter(docs_dir, &project_root, true)?;
    }
    if let Some(ai_docs_dir) = &project.ai_docs_dir {
        copy_docs_dir_with_frontmatter(ai_docs_dir, &project_root, false)?;
    }
    if let Some(ai_web_dir) = &project.ai_web_dir {
        copy_docs_dir_with_frontmatter(ai_web_dir, &project_root, false)?;
    }

    let index_path = project_root.join("index.mdx");
    if let Some(content) = project_readme_content(&project.root, &project.name)? {
        let index_md = project_root.join("index.md");
        if index_md.exists() {
            fs::remove_file(&index_md).ok();
        }
        fs::write(&index_path, content)
            .with_context(|| format!("failed to write {}", index_path.display()))?;
    } else if !index_path.exists() {
        let mut lines = Vec::new();
        lines.push("---".to_string());
        lines.push(format!("title: {}", quote_yaml_string(&project.name)));
        lines.push("---".to_string());
        lines.push(String::new());
        fs::write(&index_path, lines.join("\n"))
            .with_context(|| format!("failed to write {}", index_path.display()))?;
    }

    let root_index = content_root.join("index.mdx");
    fs::write(&root_index, render_root_index(&[project.clone()]))
        .with_context(|| format!("failed to write {}", root_index.display()))?;
    write_docs_hub_focus_marker(hub_root, &project.slug)?;
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
        lines
            .push("Add `docs/` or `.ai/docs` to a project and run `f docs hub` again.".to_string());
        lines.push(String::new());
        return lines.join("\n");
    }
    lines.push("Projects:".to_string());
    lines.push(String::new());
    for project in projects {
        lines.push(format!("- [{}](./{})", project.name, project.slug));
    }
    lines.push(String::new());
    lines.join("\n")
}

fn read_docs_hub_focus_marker(hub_root: &Path) -> Option<String> {
    let path = hub_root.join(DOCS_HUB_FOCUS_FILE);
    let value = fs::read_to_string(path).ok()?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

fn write_docs_hub_focus_marker(hub_root: &Path, slug: &str) -> Result<()> {
    let path = hub_root.join(DOCS_HUB_FOCUS_FILE);
    fs::write(&path, slug).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn clear_docs_hub_focus_marker(hub_root: &Path) -> Result<()> {
    let path = hub_root.join(DOCS_HUB_FOCUS_FILE);
    if path.exists() {
        fs::remove_file(&path).ok();
    }
    Ok(())
}

fn project_readme_content(project_root: &Path, title: &str) -> Result<Option<String>> {
    let readme_path = find_project_readme(project_root);
    let Some(readme_path) = readme_path else {
        return Ok(None);
    };
    let content = fs::read_to_string(&readme_path)
        .with_context(|| format!("failed to read {}", readme_path.display()))?;
    let sanitized = sanitize_markdown_content(&content);
    let stripped = strip_frontmatter(&sanitized);
    let updated = ensure_frontmatter_title(&stripped, title);
    Ok(Some(updated))
}

fn find_project_readme(project_root: &Path) -> Option<PathBuf> {
    let candidates = ["README.mdx", "README.md", "readme.mdx", "readme.md"];
    for candidate in candidates {
        let path = project_root.join(candidate);
        if path.exists() {
            return Some(path);
        }
    }
    None
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

fn merge_docs_scaffold(from: &Path, to: &Path) -> Result<()> {
    copy_dir_filtered_missing(from, to, true)
}

fn copy_dir_filtered_missing(from: &Path, to: &Path, allow_assets: bool) -> Result<()> {
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
            copy_dir_filtered_missing(&path, &dest, allow_assets)?;
        } else if file_type.is_file() {
            if dest.exists() {
                continue;
            }
            if should_copy_doc_file(&path, allow_assets) {
                fs::copy(&path, &dest)
                    .with_context(|| format!("failed to copy {}", path.display()))?;
            }
        }
    }
    Ok(())
}

fn copy_docs_dir_with_frontmatter(from: &Path, to: &Path, overwrite: bool) -> Result<()> {
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
            copy_docs_dir_with_frontmatter(&path, &dest, overwrite)?;
        } else if file_type.is_file() {
            if !overwrite && dest.exists() {
                continue;
            }
            let ext = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");
            if ext == "toml" {
                let dest = dest.with_extension("mdx");
                if !overwrite && dest.exists() {
                    continue;
                }
                let content = fs::read_to_string(&path)
                    .with_context(|| format!("failed to read {}", path.display()))?;
                let body = format!("```toml\n{}\n```", content.trim_end_matches('\n'));
                let title = title_from_filename(&path);
                let updated = ensure_frontmatter_title(&body, &title);
                fs::write(&dest, updated.as_bytes())
                    .with_context(|| format!("failed to write {}", dest.display()))?;
                continue;
            }
            if !matches!(ext, "md" | "mdx") {
                continue;
            }
            let content = fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            let sanitized = sanitize_markdown_content(&content);
            let stripped = strip_frontmatter(&sanitized);
            let title = derive_title(&stripped, &path);
            let updated = ensure_frontmatter_title(&stripped, &title);
            fs::write(&dest, updated.as_bytes())
                .with_context(|| format!("failed to write {}", dest.display()))?;
        }
    }
    Ok(())
}

fn derive_title(content: &str, path: &Path) -> String {
    if let Some(title) = extract_title_from_frontmatter(content) {
        return sanitize_title(&title, path);
    }
    if let Some(title) = first_heading(content) {
        return sanitize_title(&title, path);
    }
    title_from_filename(path)
}

fn extract_title_from_frontmatter(content: &str) -> Option<String> {
    let Some((frontmatter, _)) = split_frontmatter(content) else {
        return None;
    };
    for line in frontmatter.lines() {
        let trimmed = line.trim();
        if let Some(value) = trimmed.strip_prefix("title:") {
            let title = value.trim().trim_matches('"').trim_matches('\'');
            if !title.is_empty() {
                return Some(title.to_string());
            }
        }
    }
    None
}

fn first_heading(content: &str) -> Option<String> {
    let rest = split_frontmatter(content)
        .map(|(_, rest)| rest)
        .unwrap_or_else(|| content.to_string());
    for line in rest.lines() {
        let trimmed = line.trim_start();
        if let Some(title) = trimmed.strip_prefix("# ") {
            let title = title.trim();
            if !title.is_empty() {
                return Some(title.to_string());
            }
        }
    }
    None
}

fn title_from_filename(path: &Path) -> String {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("Doc");
    let mut title = stem.replace('-', " ").replace('_', " ");
    if let Some(first) = title.get(0..1) {
        title.replace_range(0..1, &first.to_uppercase());
    }
    title
}

fn strip_leading_heading(content: &str, title: &str) -> String {
    let mut lines: Vec<&str> = content.lines().collect();
    let mut idx = 0usize;
    while idx < lines.len() && lines[idx].trim().is_empty() {
        idx += 1;
    }
    if idx < lines.len() {
        if let Some(heading) = lines[idx].trim().strip_prefix("# ") {
            if normalize_title(heading) == normalize_title(title) {
                lines.remove(idx);
                while idx < lines.len() && lines[idx].trim().is_empty() {
                    lines.remove(idx);
                }
            }
        }
    }
    let mut out = lines.join("\n");
    if content.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn normalize_title(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn ensure_frontmatter_title(content: &str, title: &str) -> String {
    let ends_with_newline = content.ends_with('\n');
    let raw_title = title;
    let title = quote_yaml_string(title);
    if let Some((frontmatter, rest)) = split_frontmatter(content) {
        let rest = strip_leading_heading(&rest, raw_title);
        let cleaned = frontmatter
            .lines()
            .filter(|line| !line.trim_start().starts_with("title:"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut updated = String::new();
        updated.push_str("---\n");
        updated.push_str(&format!("title: {}\n", title));
        if !cleaned.trim().is_empty() {
            updated.push_str(&cleaned);
            if !cleaned.ends_with('\n') {
                updated.push('\n');
            }
        }
        updated.push_str("---\n");
        if !rest.is_empty() {
            updated.push_str(rest.trim_start_matches('\n'));
            if ends_with_newline && !updated.ends_with('\n') {
                updated.push('\n');
            }
        }
        return updated;
    }

    let mut updated = String::new();
    updated.push_str("---\n");
    updated.push_str(&format!("title: {}\n", title));
    updated.push_str("---\n");
    if !content.is_empty() {
        let rest = strip_leading_heading(content, raw_title);
        updated.push_str(rest.trim_start_matches('\n'));
        if ends_with_newline && !updated.ends_with('\n') {
            updated.push('\n');
        }
    }
    updated
}

fn split_frontmatter(content: &str) -> Option<(String, String)> {
    let mut lines = content.lines();
    let first = lines.next()?;
    if first.trim() != "---" {
        return None;
    }

    let mut frontmatter_lines = Vec::new();
    let mut in_frontmatter = true;
    let mut rest_lines = Vec::new();
    for line in lines {
        if in_frontmatter && line.trim() == "---" {
            in_frontmatter = false;
            continue;
        }
        if in_frontmatter {
            frontmatter_lines.push(line);
        } else {
            rest_lines.push(line);
        }
    }
    if in_frontmatter {
        return None;
    }

    let frontmatter = frontmatter_lines.join("\n");
    let rest = rest_lines.join("\n");
    Some((frontmatter, rest))
}

fn strip_frontmatter(content: &str) -> String {
    if let Some((_, rest)) = split_frontmatter(content) {
        let mut out = rest;
        if content.ends_with('\n') && !out.ends_with('\n') {
            out.push('\n');
        }
        return out;
    }
    content.to_string()
}

fn sanitize_title(title: &str, path: &Path) -> String {
    let mut out = String::new();
    let mut chars = title.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '`' {
            continue;
        }
        if ch == '[' {
            let mut text = String::new();
            while let Some(c) = chars.next() {
                if c == ']' {
                    break;
                }
                text.push(c);
            }
            if matches!(chars.peek(), Some('(')) {
                chars.next();
                let mut depth = 1;
                while let Some(c) = chars.next() {
                    if c == '(' {
                        depth += 1;
                    } else if c == ')' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                }
            }
            out.push_str(&text);
            continue;
        }
        out.push(ch);
    }
    let trimmed = out.trim();
    if trimmed.is_empty() {
        return title_from_filename(path);
    }
    trimmed.to_string()
}

fn quote_yaml_string(value: &str) -> String {
    let mut out = String::new();
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn is_mdx_declaration_line(trimmed: &str) -> bool {
    if trimmed.starts_with("import ") {
        return true;
    }
    if trimmed.starts_with("export ") {
        let rest = trimmed.trim_start_matches("export ").trim_start();
        return rest.starts_with("const ")
            || rest.starts_with("default")
            || rest.starts_with("function ")
            || rest.starts_with("type ")
            || rest.starts_with("interface ")
            || rest.starts_with("{");
    }
    false
}

fn sanitize_markdown_content(content: &str) -> String {
    let mut out = Vec::new();
    let mut in_code = false;
    let mut fence = String::new();
    let mut in_frontmatter = false;
    let mut frontmatter_checked = false;

    for line in content.lines() {
        let trimmed = line.trim_start();
        if !frontmatter_checked {
            frontmatter_checked = true;
            if trimmed == "---" {
                in_frontmatter = true;
                out.push(line.to_string());
                continue;
            }
        }

        if in_frontmatter {
            out.push(line.to_string());
            if trimmed == "---" {
                in_frontmatter = false;
            }
            continue;
        }

        if !in_code && is_mdx_declaration_line(trimmed) {
            continue;
        }

        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            let marker = trimmed.chars().take(3).collect::<String>();
            if !in_code {
                in_code = true;
                fence = marker.clone();
                out.push(normalize_code_fence_line(line));
                continue;
            }
            if trimmed.starts_with(&fence) {
                in_code = false;
                fence.clear();
                out.push(line.to_string());
                continue;
            }
        }

        if in_code {
            out.push(line.to_string());
            continue;
        }

        let rewritten = rewrite_markdown_images(line);
        if contains_html_tag(&rewritten) {
            out.push(escape_html_line(&rewritten));
            continue;
        }

        out.push(rewritten);
    }

    let mut joined = out.join("\n");
    if content.ends_with('\n') {
        joined.push('\n');
    }
    joined
}

fn rewrite_markdown_images(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(start) = rest.find("![") {
        out.push_str(&rest[..start]);
        let after_start = &rest[start + 2..];
        let Some(end_bracket) = after_start.find(']') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let alt = &after_start[..end_bracket];
        let after_bracket = &after_start[end_bracket + 1..];
        let after_bracket_trim = after_bracket.trim_start();
        if !after_bracket_trim.starts_with('(') {
            out.push_str(&rest[start..start + 2 + end_bracket + 1]);
            rest = &after_start[end_bracket + 1..];
            continue;
        }
        let paren_offset = after_bracket.len() - after_bracket_trim.len();
        let after_paren = &after_bracket[paren_offset + 1..];
        let Some(end_paren) = after_paren.find(')') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let inner = &after_paren[..end_paren];
        let dest = extract_markdown_dest(inner);
        if is_remote_image_dest(dest) {
            out.push_str("![");
            out.push_str(alt);
            out.push_str("](");
            out.push_str(inner);
            out.push(')');
        } else {
            let label = if alt.trim().is_empty() { "image" } else { alt };
            out.push('[');
            out.push_str(label);
            out.push_str("](");
            out.push_str(inner);
            out.push(')');
        }
        rest = &after_paren[end_paren + 1..];
    }
    out.push_str(rest);
    out
}

fn extract_markdown_dest(inner: &str) -> &str {
    let trimmed = inner.trim_start();
    if let Some(rest) = trimmed.strip_prefix('<') {
        if let Some(end) = rest.find('>') {
            return &rest[..end];
        }
    }
    let mut end = trimmed.len();
    for (idx, ch) in trimmed.char_indices() {
        if ch.is_whitespace() {
            end = idx;
            break;
        }
    }
    &trimmed[..end]
}

fn is_remote_image_dest(dest: &str) -> bool {
    let lower = dest.to_ascii_lowercase();
    lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("data:")
        || lower.starts_with("mailto:")
}

fn normalize_code_fence_line(line: &str) -> String {
    let idx = line
        .char_indices()
        .find(|(_, ch)| !ch.is_whitespace())
        .map(|(i, _)| i)
        .unwrap_or(0);
    let (prefix, trimmed) = line.split_at(idx);
    let fence = if trimmed.starts_with("```") {
        "```"
    } else if trimmed.starts_with("~~~") {
        "~~~"
    } else {
        return line.to_string();
    };
    let rest = &trimmed[fence.len()..];
    let rest_trim_start = rest
        .char_indices()
        .find(|(_, ch)| !ch.is_whitespace())
        .map(|(i, _)| i)
        .unwrap_or(rest.len());
    let rest_trim = &rest[rest_trim_start..];
    if rest_trim.is_empty() {
        return line.to_string();
    }
    let (lang_token, _) = split_lang_token(rest_trim);
    let normalized = normalize_code_lang(lang_token);
    let Some(normalized) = normalized else {
        return line.to_string();
    };
    let after_lang = &rest[rest_trim_start + lang_token.len()..];
    let before_lang = &rest[..rest_trim_start];
    format!("{prefix}{fence}{before_lang}{normalized}{after_lang}")
}

fn split_lang_token(rest_trim: &str) -> (&str, &str) {
    let mut end = rest_trim.len();
    for (idx, ch) in rest_trim.char_indices() {
        if ch.is_whitespace() || matches!(ch, '{' | '[' | '(') {
            end = idx;
            break;
        }
    }
    (&rest_trim[..end], &rest_trim[end..])
}

fn normalize_code_lang(lang: &str) -> Option<&'static str> {
    let lower = lang.to_ascii_lowercase();
    if lower.is_empty() {
        return None;
    }
    if matches!(
        lower.as_str(),
        "text"
            | "txt"
            | "plaintext"
            | "bash"
            | "sh"
            | "zsh"
            | "fish"
            | "shell"
            | "shellscript"
            | "console"
            | "json"
            | "yaml"
            | "yml"
            | "toml"
            | "ini"
            | "md"
            | "markdown"
            | "mdx"
            | "js"
            | "jsx"
            | "ts"
            | "tsx"
            | "py"
            | "python"
            | "rs"
            | "rust"
            | "go"
            | "c"
            | "cpp"
            | "cxx"
            | "java"
            | "kotlin"
            | "swift"
            | "rb"
            | "ruby"
            | "php"
            | "html"
            | "css"
            | "scss"
            | "less"
            | "sql"
            | "graphql"
            | "graphqls"
            | "dockerfile"
            | "make"
            | "makefile"
    ) {
        return None;
    }
    Some("text")
}

fn contains_html_tag(line: &str) -> bool {
    let bytes = line.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == b'<' {
            if i + 1 >= bytes.len() {
                continue;
            }
            let next = bytes[i + 1] as char;
            if next.is_ascii_alphabetic() || matches!(next, '/' | '!') {
                if line[i + 1..].contains('>') {
                    return true;
                }
            }
        }
    }
    false
}

fn escape_html_line(line: &str) -> String {
    line.replace('<', "&lt;").replace('>', "&gt;")
}

fn ensure_docs_hub_config(hub_root: &Path) -> Result<()> {
    let ts_path = hub_root.join("source.config.ts");
    let mjs_path = hub_root.join(".source").join("source.config.mjs");
    if ts_path.exists() {
        rewrite_source_config(&ts_path, true)?;
    }
    if mjs_path.exists() {
        rewrite_source_config(&mjs_path, false)?;
    }
    Ok(())
}

fn ensure_docs_hub_layout(hub_root: &Path) -> Result<()> {
    let page_path = hub_root
        .join("app")
        .join("(docs)")
        .join("[[...slug]]")
        .join("page.tsx");
    if !page_path.exists() {
        return Ok(());
    }
    fs::write(&page_path, DOCS_HUB_PAGE_TEMPLATE.as_bytes())
        .with_context(|| format!("failed to write {}", page_path.display()))?;
    Ok(())
}

const DOCS_HUB_PAGE_TEMPLATE: &str = r#"import { source } from "@/lib/source"
import { DocsLayout } from "fumadocs-ui/layouts/docs"
import { DocsPage, DocsBody, DocsDescription, DocsTitle } from "fumadocs-ui/page"
import { notFound } from "next/navigation"
import { useMDXComponents } from "@/mdx-components"
import type { Metadata } from "next"

type TreeNode = {
  name?: string
  url?: string
  path?: string
  slug?: string
  children?: TreeNode[]
}

function pickProjectTree(tree: TreeNode, slug?: string) {
  if (!slug || !tree || !Array.isArray(tree.children)) return tree
  const target = slug.toLowerCase()
  const child = tree.children.find((node) => {
    const url = String(node?.url ?? node?.path ?? "")
    if (url === `/${target}` || url === `${target}`) return true
    if (node?.slug && String(node.slug).toLowerCase() === target) return true
    if (node?.name && String(node.name).toLowerCase() === target) return true
    return false
  })
  if (!child) return tree
  if (Array.isArray(child.children) && child.children.length > 0) {
    return { ...tree, children: child.children }
  }
  return { ...tree, children: [child] }
}

function navTitleForSlug(slug?: string) {
  if (!slug) return "Docs"
  const root = source.getPage([slug])
  return root?.data?.title ?? slug
}

export default async function Page(props: {
  params: Promise<{ slug?: string[] }>
}) {
  const params = await props.params
  const page = source.getPage(params.slug)
  if (!page) notFound()

  const rootSlug = params.slug?.[0]
  const tree = pickProjectTree(source.pageTree as TreeNode, rootSlug)
  const navTitle = navTitleForSlug(rootSlug)
  const MDX = page.data.body
  const mdxComponents = useMDXComponents()

  const navUrl = rootSlug ? `/${rootSlug}` : "/"

  return (
    <DocsLayout
      tree={tree}
      nav={{ title: navTitle, url: navUrl }}
      sidebar={{ defaultOpenLevel: 1 }}
    >
      <DocsPage toc={page.data.toc} full={page.data.full}>
        <DocsTitle>{page.data.title}</DocsTitle>
        <DocsDescription>{page.data.description}</DocsDescription>
        <DocsBody>
          <MDX components={mdxComponents} />
        </DocsBody>
      </DocsPage>
    </DocsLayout>
  )
}

export const dynamicParams = false

export async function generateStaticParams() {
  return source.generateParams()
}

export async function generateMetadata(props: {
  params: Promise<{ slug?: string[] }>
}): Promise<Metadata> {
  const params = await props.params
  const page = source.getPage(params.slug)
  if (!page) notFound()

  return {
    title: page.data.title,
    description: page.data.description,
  }
}
"#;

fn rewrite_source_config(path: &Path, is_ts: bool) -> Result<()> {
    let contents = if is_ts {
        r#"import { defineDocs, defineConfig } from "fumadocs-mdx/config"

export const docs = defineDocs({
  dir: "content/docs",
})

export default defineConfig({
  mdxOptions: {
    remarkImageOptions: {
      onError: "hide",
      external: { timeout: 1500 },
      useImport: false,
    },
  },
})
"#
    } else {
        r#"import { defineDocs, defineConfig } from "fumadocs-mdx/config"

export const docs = defineDocs({
  dir: "content/docs",
})

export default defineConfig({
  mdxOptions: {
    remarkImageOptions: {
      onError: "ignore",
      external: false,
    },
  },
})
"#
    };
    fs::write(path, contents.as_bytes())
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn docs_hub_needs_reset(hub_root: &Path) -> Result<bool> {
    let server_path = hub_root.join(".source").join("server.ts");
    if !server_path.exists() {
        return Ok(false);
    }
    let contents = fs::read_to_string(&server_path)
        .with_context(|| format!("failed to read {}", server_path.display()))?;
    Ok(contents.contains("content/docs/projects/"))
}

fn remove_docs_hub_cache(hub_root: &Path) -> Result<()> {
    let source_root = hub_root.join(".source");
    if source_root.exists() {
        fs::remove_dir_all(&source_root)
            .with_context(|| format!("failed to remove {}", source_root.display()))?;
    }
    let next_root = hub_root.join(".next");
    if next_root.exists() {
        fs::remove_dir_all(&next_root)
            .with_context(|| format!("failed to remove {}", next_root.display()))?;
    }
    Ok(())
}

fn kill_docs_hub_by_port(port: u16) -> Result<()> {
    #[cfg(unix)]
    {
        let port_arg = format!("tcp:{port}");
        let output = Command::new("lsof").args(["-ti", &port_arg]).output();
        let Ok(output) = output else {
            return Ok(());
        };
        if !output.status.success() {
            return Ok(());
        }
        let pids = String::from_utf8_lossy(&output.stdout);
        for pid in pids.lines().map(str::trim).filter(|line| !line.is_empty()) {
            let _ = Command::new("kill").arg(pid).status();
        }
    }
    Ok(())
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
            fs::copy(&path, &dest).with_context(|| format!("failed to copy {}", path.display()))?;
        }
    }
    Ok(())
}

fn sync_docs_hub_template_file(
    hub_root: &Path,
    template_root: &Path,
    rel_path: &str,
    overwrite: bool,
) -> Result<()> {
    let src = template_root.join(rel_path);
    if !src.exists() {
        return Ok(());
    }
    let dest = hub_root.join(rel_path);
    if !overwrite && dest.exists() {
        return Ok(());
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::copy(&src, &dest).with_context(|| format!("failed to copy {}", src.display()))?;
    Ok(())
}

fn ensure_docs_hub_flow_toml(hub_root: &Path, template_root: &Path) -> Result<()> {
    let src = template_root.join("flow.toml");
    if !src.exists() {
        return Ok(());
    }
    let dest = hub_root.join("flow.toml");
    if !dest.exists() {
        fs::copy(&src, &dest).with_context(|| format!("failed to copy {}", src.display()))?;
        return Ok(());
    }
    let dest_contents =
        fs::read_to_string(&dest).with_context(|| format!("failed to read {}", dest.display()))?;
    if dest_contents.contains("[cloudflare]") {
        return Ok(());
    }
    let src_contents =
        fs::read_to_string(&src).with_context(|| format!("failed to read {}", src.display()))?;
    let Some(idx) = src_contents.find("[cloudflare]") else {
        return Ok(());
    };
    let mut updated = dest_contents;
    if !updated.ends_with('\n') {
        updated.push('\n');
    }
    if !updated.trim().is_empty() {
        updated.push('\n');
    }
    updated.push_str(src_contents[idx..].trim_start());
    updated.push('\n');
    fs::write(&dest, updated).with_context(|| format!("failed to write {}", dest.display()))?;
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
