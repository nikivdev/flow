//! GitHub release management.
//!
//! Provides functionality to:
//! - Create GitHub releases with version tags
//! - Upload release assets (binaries, tarballs)
//! - List and manage existing releases

use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::cli::{GhReleaseAction, GhReleaseCommand, GhReleaseCreateOpts};

/// Run the release command.
pub fn run(cmd: GhReleaseCommand) -> Result<()> {
    // Check if gh CLI is available
    if Command::new("gh").arg("--version").output().is_err() {
        bail!("GitHub CLI (gh) is not installed. Install from: https://cli.github.com");
    }

    // Check if authenticated
    let auth_status = Command::new("gh")
        .args(["auth", "status"])
        .output()
        .context("failed to check gh auth status")?;

    if !auth_status.status.success() {
        println!("Not authenticated with GitHub.");
        println!("Run: gh auth login");
        bail!("GitHub authentication required");
    }

    // Check if in a git repo
    if !Path::new(".git").exists() {
        bail!("Not in a git repository. Run this command from a git repo root.");
    }

    match cmd.action {
        Some(GhReleaseAction::Create(opts)) => create_release(opts),
        Some(GhReleaseAction::List { limit }) => list_releases(limit),
        Some(GhReleaseAction::Delete { tag, yes }) => delete_release(&tag, yes),
        Some(GhReleaseAction::Download { tag, output }) => {
            download_release(tag.as_deref(), &output)
        }
        None => list_releases(10), // Default action
    }
}

/// Create a new GitHub release.
fn create_release(opts: GhReleaseCreateOpts) -> Result<()> {
    // Determine the tag
    let tag = match opts.tag {
        Some(t) => t,
        None => detect_version()?,
    };

    // Ensure tag has 'v' prefix for consistency
    let tag = if tag.starts_with('v') {
        tag
    } else {
        format!("v{}", tag)
    };

    println!("Creating release {}...", tag);

    // Check if tag already exists
    let tag_exists = Command::new("gh")
        .args(["release", "view", &tag])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if tag_exists {
        bail!(
            "Release {} already exists. Use a different version or delete the existing release.",
            tag
        );
    }

    // Validate assets exist
    for asset in &opts.asset {
        if !Path::new(asset).exists() {
            bail!("Asset file not found: {}", asset);
        }
    }

    // Confirmation
    if !opts.yes {
        println!();
        println!("Release details:");
        println!("  Tag: {}", tag);
        if let Some(ref title) = opts.title {
            println!("  Title: {}", title);
        }
        if opts.draft {
            println!("  Type: Draft");
        }
        if opts.prerelease {
            println!("  Type: Pre-release");
        }
        if !opts.asset.is_empty() {
            println!("  Assets: {}", opts.asset.join(", "));
        }
        println!();

        print!("Create release? [Y/n]: ");
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let input = input.trim().to_lowercase();
        if input == "n" || input == "no" {
            println!("Aborted.");
            return Ok(());
        }
    }

    // Build the gh release create command
    let mut args = vec!["release", "create", &tag];

    let title_str;
    if let Some(ref title) = opts.title {
        args.push("--title");
        title_str = title.clone();
        args.push(&title_str);
    }

    let notes_str;
    if let Some(ref notes) = opts.notes {
        args.push("--notes");
        notes_str = notes.clone();
        args.push(&notes_str);
    } else if let Some(ref notes_file) = opts.notes_file {
        args.push("--notes-file");
        args.push(notes_file);
    } else if opts.generate_notes {
        args.push("--generate-notes");
    }

    if opts.draft {
        args.push("--draft");
    }

    if opts.prerelease {
        args.push("--prerelease");
    }

    let target_str;
    if let Some(ref target) = opts.target {
        args.push("--target");
        target_str = target.clone();
        args.push(&target_str);
    }

    // Add assets
    for asset in &opts.asset {
        args.push(asset);
    }

    println!("Running: gh {}", args.join(" "));

    let status = Command::new("gh")
        .args(&args)
        .status()
        .context("failed to create release")?;

    if !status.success() {
        bail!("Failed to create release");
    }

    println!();
    println!("Release {} created successfully!", tag);

    // Show the release URL
    let url_output = Command::new("gh")
        .args(["release", "view", &tag, "--json", "url", "-q", ".url"])
        .output();

    if let Ok(output) = url_output {
        if output.status.success() {
            let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !url.is_empty() {
                println!("View at: {}", url);
            }
        }
    }

    Ok(())
}

/// List recent releases.
fn list_releases(limit: usize) -> Result<()> {
    let output = Command::new("gh")
        .args(["release", "list", "--limit", &limit.to_string()])
        .output()
        .context("failed to list releases")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("no releases found") {
            println!("No releases found.");
            return Ok(());
        }
        bail!("Failed to list releases: {}", stderr);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim().is_empty() {
        println!("No releases found.");
    } else {
        println!("{}", stdout);
    }

    Ok(())
}

/// Delete a release.
fn delete_release(tag: &str, yes: bool) -> Result<()> {
    // Check if release exists
    let exists = Command::new("gh")
        .args(["release", "view", tag])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if !exists {
        bail!("Release {} not found", tag);
    }

    if !yes {
        print!("Delete release {}? [y/N]: ", tag);
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let input = input.trim().to_lowercase();
        if input != "y" && input != "yes" {
            println!("Aborted.");
            return Ok(());
        }
    }

    let status = Command::new("gh")
        .args(["release", "delete", tag, "--yes"])
        .status()
        .context("failed to delete release")?;

    if !status.success() {
        bail!("Failed to delete release");
    }

    println!("Release {} deleted.", tag);

    // Optionally delete the tag too
    print!("Also delete the git tag {}? [y/N]: ", tag);
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim().to_lowercase();
    if input == "y" || input == "yes" {
        Command::new("git").args(["tag", "-d", tag]).status().ok();
        Command::new("git")
            .args(["push", "origin", &format!(":refs/tags/{}", tag)])
            .status()
            .ok();
        println!("Tag {} deleted.", tag);
    }

    Ok(())
}

/// Download release assets.
fn download_release(tag: Option<&str>, output: &str) -> Result<()> {
    let mut args = vec!["release", "download"];

    if let Some(t) = tag {
        args.push(t);
    }

    args.push("--dir");
    args.push(output);

    // Create output directory if needed
    if output != "." {
        fs::create_dir_all(output).context("failed to create output directory")?;
    }

    println!("Downloading release assets to {}...", output);

    let status = Command::new("gh")
        .args(&args)
        .status()
        .context("failed to download release")?;

    if !status.success() {
        bail!("Failed to download release assets");
    }

    println!("Download complete.");
    Ok(())
}

/// Detect version from Cargo.toml or package.json.
fn detect_version() -> Result<String> {
    // Try Cargo.toml first
    if Path::new("Cargo.toml").exists() {
        let content = fs::read_to_string("Cargo.toml")?;
        for line in content.lines() {
            if line.starts_with("version") {
                if let Some(version) = line.split('=').nth(1) {
                    let version = version.trim().trim_matches('"').trim_matches('\'');
                    return Ok(version.to_string());
                }
            }
        }
    }

    // Try package.json
    if Path::new("package.json").exists() {
        let content = fs::read_to_string("package.json")?;
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(version) = json.get("version").and_then(|v| v.as_str()) {
                return Ok(version.to_string());
            }
        }
    }

    // Try pyproject.toml
    if Path::new("pyproject.toml").exists() {
        let content = fs::read_to_string("pyproject.toml")?;
        for line in content.lines() {
            if line.starts_with("version") {
                if let Some(version) = line.split('=').nth(1) {
                    let version = version.trim().trim_matches('"').trim_matches('\'');
                    return Ok(version.to_string());
                }
            }
        }
    }

    // Try to get from git tags
    let output = Command::new("git")
        .args(["describe", "--tags", "--abbrev=0"])
        .output();

    if let Ok(output) = output {
        if output.status.success() {
            let tag = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !tag.is_empty() {
                // Increment patch version
                let version = tag.strip_prefix('v').unwrap_or(&tag);
                let parts: Vec<&str> = version.split('.').collect();
                if parts.len() >= 3 {
                    if let Ok(patch) = parts[2].parse::<u32>() {
                        return Ok(format!("{}.{}.{}", parts[0], parts[1], patch + 1));
                    }
                }
                return Ok(version.to_string());
            }
        }
    }

    bail!(
        "Could not detect version. Please specify with: f release create <tag>\n\
         Or add version to Cargo.toml, package.json, or pyproject.toml"
    )
}
