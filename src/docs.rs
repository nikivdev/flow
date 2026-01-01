//! Auto-generated documentation management.
//!
//! Maintains documentation in `.ai/docs/` that stays in sync with the codebase.

use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::cli::{DocsCommand, DocsAction};

/// Docs directory relative to project root.
const DOCS_DIR: &str = ".ai/docs";

/// Run the docs command.
pub fn run(cmd: DocsCommand) -> Result<()> {
    let project_root = std::env::current_dir()?;
    let docs_dir = project_root.join(DOCS_DIR);

    match cmd.action {
        Some(DocsAction::List) | None => list_docs(&docs_dir),
        Some(DocsAction::Status) => show_status(&project_root, &docs_dir),
        Some(DocsAction::Sync { commits, dry }) => sync_docs(&project_root, &docs_dir, commits, dry),
        Some(DocsAction::Edit { name }) => edit_doc(&docs_dir, &name),
    }
}

/// List all documentation files.
fn list_docs(docs_dir: &Path) -> Result<()> {
    if !docs_dir.exists() {
        println!("No docs directory. Run `f start` to create .ai/docs/");
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
                content.lines()
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
        println!("No docs directory. Run `f start` to create .ai/docs/");
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
        let modified = entry.metadata()
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
        bail!("No docs directory. Run `f start` to create .ai/docs/");
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
