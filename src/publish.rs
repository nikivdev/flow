//! Publish projects to GitHub.

use std::io::{self, Write};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::cli::PublishOpts;

/// Run the publish command.
pub fn run(opts: PublishOpts) -> Result<()> {
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

    // Get current directory name as default repo name
    let cwd = std::env::current_dir()?;
    let folder_name = cwd
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("repo")
        .to_string();

    // Check if already a git repo
    let is_git_repo = cwd.join(".git").exists();

    // Get GitHub username
    let gh_user = Command::new("gh")
        .args(["api", "user", "-q", ".login"])
        .output()
        .context("failed to get GitHub username")?;

    let username = String::from_utf8_lossy(&gh_user.stdout).trim().to_string();
    if username.is_empty() {
        bail!("Could not determine GitHub username");
    }

    // Determine repo name
    let repo_name = if let Some(name) = opts.name {
        name
    } else if opts.yes {
        folder_name.clone()
    } else {
        print!("Repository name [{}]: ", folder_name);
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let input = input.trim();
        if input.is_empty() {
            folder_name.clone()
        } else {
            input.to_string()
        }
    };

    // Determine visibility
    let is_public = if opts.public {
        true
    } else if opts.private {
        false
    } else if opts.yes {
        false // Default to private if -y is passed
    } else {
        print!("Visibility (public/private) [private]: ");
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let input = input.trim().to_lowercase();
        input == "public" || input == "pub" || input == "p"
    };

    let visibility = if is_public { "public" } else { "private" };
    let full_name = format!("{}/{}", username, repo_name);

    // Check if repo already exists
    let repo_check = Command::new("gh")
        .args(["repo", "view", &full_name, "--json", "visibility", "-q", ".visibility"])
        .output();

    if let Ok(output) = repo_check {
        if output.status.success() {
            let current_visibility = String::from_utf8_lossy(&output.stdout).trim().to_lowercase();
            println!("Repository {} already exists ({}).", full_name, current_visibility);

            // Check if visibility needs to change
            let target_visibility = if is_public { "public" } else { "private" };
            if current_visibility != target_visibility {
                println!("Updating visibility to {}...", target_visibility);
                let visibility_flag = format!("--visibility={}", target_visibility);
                let update_result = Command::new("gh")
                    .args(["repo", "edit", &full_name, &visibility_flag])
                    .status()
                    .context("failed to update repository visibility")?;

                if update_result.success() {
                    println!("✓ Updated to {}", target_visibility);
                } else {
                    println!("Warning: Could not update visibility");
                }
            }

            // Check if origin remote exists
            let origin_check = Command::new("git")
                .args(["remote", "get-url", "origin"])
                .output();

            if origin_check.map(|o| o.status.success()).unwrap_or(false) {
                println!("\n✓ https://github.com/{}", full_name);
                return Ok(());
            }

            // Add origin and push
            println!("Adding origin remote...");
            let remote_url = format!("git@github.com:{}.git", full_name);
            Command::new("git")
                .args(["remote", "add", "origin", &remote_url])
                .status()
                .context("failed to add origin remote")?;

            println!("Pushing to {}...", full_name);
            push_to_origin()?;

            println!("\n✓ Published to https://github.com/{}", full_name);
            return Ok(());
        }
    }

    // Show confirmation
    if !opts.yes {
        println!();
        println!("Create repository:");
        println!("  Name: {}", full_name);
        println!("  Visibility: {}", visibility);
        if let Some(ref desc) = opts.description {
            println!("  Description: {}", desc);
        }
        println!();

        print!("Proceed? [Y/n]: ");
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let input = input.trim().to_lowercase();
        if input == "n" || input == "no" {
            println!("Aborted.");
            return Ok(());
        }
    }

    // Initialize git if needed
    if !is_git_repo {
        println!("Initializing git repository...");
        Command::new("git")
            .args(["init"])
            .status()
            .context("failed to initialize git")?;

        // Create initial commit if no commits exist
        let has_commits = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);

        if !has_commits {
            // Stage all files
            Command::new("git")
                .args(["add", "."])
                .status()
                .context("failed to stage files")?;

            Command::new("git")
                .args(["commit", "-m", "Initial commit"])
                .status()
                .context("failed to create initial commit")?;
        }
    }

    // Create the repository
    println!("Creating repository on GitHub...");

    let mut args = vec![
        "repo".to_string(),
        "create".to_string(),
        repo_name.clone(),
        format!("--{}", visibility),
        "--source=.".to_string(),
        "--push".to_string(),
    ];

    if let Some(desc) = opts.description {
        args.push("--description".to_string());
        args.push(desc);
    }

    let create_result = Command::new("gh")
        .args(&args)
        .status()
        .context("failed to create repository")?;

    if !create_result.success() {
        bail!("Failed to create repository");
    }

    println!();
    println!("✓ Published to https://github.com/{}", full_name);

    Ok(())
}

fn push_to_origin() -> Result<()> {
    // Get current branch
    let branch = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .context("failed to get current branch")?;

    let branch = String::from_utf8_lossy(&branch.stdout).trim().to_string();
    let branch = if branch.is_empty() || branch == "HEAD" {
        "main".to_string()
    } else {
        branch
    };

    Command::new("git")
        .args(["push", "-u", "origin", &branch])
        .status()
        .context("failed to push to origin")?;

    Ok(())
}
