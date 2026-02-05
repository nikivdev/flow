//! Publish projects to gitedit.dev or GitHub.

use std::collections::HashSet;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use crossterm::event::{self, Event as CEvent, KeyCode};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use reqwest::blocking::Client;
use serde::Serialize;

use crate::cli::{PublishAction, PublishCommand, PublishOpts};
use crate::config;
use crate::vcs;

fn parse_github_repo(url: &str) -> Result<(String, String, String)> {
    let trimmed = url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        bail!("GitHub URL is empty");
    }

    if let Some(rest) = trimmed.strip_prefix("git@github.com:") {
        let rest = rest.trim_end_matches(".git");
        let Some((owner, repo)) = rest.split_once('/') else {
            bail!("Invalid GitHub SSH URL: {}", url);
        };
        return Ok((
            owner.to_string(),
            repo.to_string(),
            format!("git@github.com:{}/{}.git", owner, repo),
        ));
    }

    if let Some(rest) = trimmed.strip_prefix("https://github.com/") {
        let rest = rest.trim_end_matches(".git");
        let Some((owner, repo)) = rest.split_once('/') else {
            bail!("Invalid GitHub HTTPS URL: {}", url);
        };
        return Ok((
            owner.to_string(),
            repo.to_string(),
            format!("git@github.com:{}/{}.git", owner, repo),
        ));
    }

    bail!(
        "Unsupported GitHub URL (expected https://github.com/... or git@github.com:...): {}",
        url
    );
}

/// Run the publish command.
pub fn run(cmd: PublishCommand) -> Result<()> {
    match cmd.action {
        Some(PublishAction::Gitedit(opts)) => run_gitedit(opts),
        Some(PublishAction::Github(opts)) => run_github(opts),
        None => run_fuzzy_select(),
    }
}

/// Show fuzzy picker for publish targets.
fn run_fuzzy_select() -> Result<()> {
    let options = vec![
        ("gitedit", "Publish to gitedit.dev"),
        ("github", "Publish to GitHub"),
    ];

    let input = options
        .iter()
        .map(|(cmd, desc)| format!("{}\t{}", cmd, desc))
        .collect::<Vec<_>>()
        .join("\n");

    let output = Command::new("fzf")
        .args([
            "--height=10",
            "--reverse",
            "--delimiter=\t",
            "--with-nth=1,2",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn fzf")?;

    output.stdin.as_ref().unwrap().write_all(input.as_bytes())?;

    let result = output.wait_with_output()?;
    if !result.status.success() {
        return Ok(()); // User cancelled
    }

    let selected = String::from_utf8_lossy(&result.stdout)
        .trim()
        .split('\t')
        .next()
        .unwrap_or("")
        .to_string();

    match selected.as_str() {
        "gitedit" => run_gitedit(PublishOpts::default()),
        "github" => run_github(PublishOpts::default()),
        _ => Ok(()),
    }
}

/// Run the GitHub publish flow.
pub fn run_github(opts: PublishOpts) -> Result<()> {
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

    // Get GitHub username (fallback owner)
    let gh_user = Command::new("gh")
        .args(["api", "user", "-q", ".login"])
        .output()
        .context("failed to get GitHub username")?;

    let username = String::from_utf8_lossy(&gh_user.stdout).trim().to_string();
    if username.is_empty() {
        bail!("Could not determine GitHub username");
    }
    let mut owner = opts.owner.clone().unwrap_or_else(|| username.clone());
    let mut repo_name_from_url: Option<String> = None;
    let mut remote_from_url: Option<String> = None;
    if let Some(url) = opts.url.as_ref() {
        let (parsed_owner, parsed_name, parsed_remote) = parse_github_repo(url)?;
        owner = parsed_owner;
        repo_name_from_url = Some(parsed_name);
        remote_from_url = Some(parsed_remote);
    }

    // Determine repo name
    let repo_name = if let Some(name) = opts.name {
        name
    } else if let Some(name) = repo_name_from_url.clone() {
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
        prompt_public_choice()?
    };

    let visibility = if is_public { "public" } else { "private" };
    let full_name = format!("{}/{}", owner, repo_name);
    let desired_remote =
        remote_from_url.unwrap_or_else(|| format!("git@github.com:{}.git", full_name));
    let set_origin = opts.set_origin || opts.url.is_some();

    // Check if repo already exists
    let repo_check = Command::new("gh")
        .args([
            "repo",
            "view",
            &full_name,
            "--json",
            "visibility",
            "-q",
            ".visibility",
        ])
        .output();

    if let Ok(output) = repo_check {
        if output.status.success() {
            let current_visibility = String::from_utf8_lossy(&output.stdout)
                .trim()
                .to_lowercase();
            println!(
                "Repository {} already exists ({}).",
                full_name, current_visibility
            );

            // Check if visibility needs to change
            let target_visibility = if is_public { "public" } else { "private" };
            if current_visibility != target_visibility {
                println!("Updating visibility to {}...", target_visibility);
                let visibility_flag = format!("--visibility={}", target_visibility);
                let update_result = Command::new("gh")
                    .args([
                        "repo",
                        "edit",
                        &full_name,
                        &visibility_flag,
                        "--accept-visibility-change-consequences",
                    ])
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

            if let Ok(output) = origin_check {
                if output.status.success() {
                    let current_origin = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if set_origin && current_origin != desired_remote {
                        println!("Updating origin remote...");
                        Command::new("git")
                            .args(["remote", "set-url", "origin", &desired_remote])
                            .status()
                            .context("failed to update origin remote")?;
                    }

                    let should_push = if opts.yes {
                        true
                    } else {
                        prompt_push_choice()?
                    };

                    if should_push {
                        println!("Pushing to {}...", full_name);
                        push_to_origin()?;
                    }

                    println!("\n✓ https://github.com/{}", full_name);
                    return Ok(());
                }
            }

            // Add origin and push
            println!("Adding origin remote...");
            let remote_url = desired_remote.clone();
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

const MAX_GITEDIT_FILE_BYTES: u64 = 512 * 1024;
const MAX_GITEDIT_TOTAL_BYTES: u64 = 8 * 1024 * 1024;
const MAX_GITEDIT_FILES: usize = 4000;

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct RepoSnapshot {
    repo: RepoMeta,
    tree: Vec<RepoTreeEntry>,
    files: Vec<RepoFileEntry>,
    readme: Option<RepoReadme>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct RepoMeta {
    description: Option<String>,
    default_branch: String,
    language: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct RepoTreeEntry {
    path: String,
    #[serde(rename = "type")]
    entry_type: String,
    sha: String,
    size: Option<u64>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct RepoFileEntry {
    path: String,
    content: String,
    size: u64,
    is_binary: bool,
    encoding: String,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct RepoReadme {
    path: String,
    content: String,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct GiteditSyncPayload {
    owner: String,
    repo: String,
    commit_sha: String,
    branch: Option<String>,
    #[serde(rename = "ref")]
    ref_name: Option<String>,
    event: String,
    source: String,
    commit_message: Option<String>,
    author_name: Option<String>,
    author_email: Option<String>,
    session_hash: Option<String>,
    repo_snapshot: Option<RepoSnapshot>,
}

fn run_gitedit(opts: PublishOpts) -> Result<()> {
    let repo_root = git_root()?;
    ensure_git_repo(&repo_root)?;

    let folder_name = repo_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("repo")
        .to_string();

    let repo_name = resolve_repo_name(&opts, &folder_name)?;
    let (owner, repo_override) = gitedit_repo_override(&repo_root);
    let repo_name = repo_override.unwrap_or(repo_name);
    let owner = resolve_gitedit_owner(&opts, owner, &repo_root)?;
    let full_name = format!("{}/{}", owner, repo_name);

    if !opts.yes {
        println!();
        println!("Publish to gitedit.dev:");
        println!("  Repo: {}", full_name);
        if let Some(ref desc) = opts.description {
            println!("  Description: {}", desc);
        }
        println!();

        print!("Proceed? [Y/n]: ");
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let input = input.trim().to_ascii_lowercase();
        if input == "n" || input == "no" {
            println!("Aborted.");
            return Ok(());
        }
    }

    let commit_sha = git_capture_in(&repo_root, &["rev-parse", "HEAD"])
        .context("failed to read git HEAD")?
        .trim()
        .to_string();
    let branch = git_capture_in(&repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| value != "HEAD");
    let ref_name = branch.as_ref().map(|name| format!("refs/heads/{}", name));
    let default_branch = branch.clone().unwrap_or_else(|| "main".to_string());

    let commit_message = git_capture_in(&repo_root, &["log", "-1", "--format=%B"])
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let author_name = git_capture_in(&repo_root, &["log", "-1", "--format=%an"])
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let author_email = git_capture_in(&repo_root, &["log", "-1", "--format=%ae"])
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    let snapshot = build_repo_snapshot(&repo_root, &default_branch, opts.description.clone())?;
    let payload = GiteditSyncPayload {
        owner: owner.clone(),
        repo: repo_name.clone(),
        commit_sha,
        branch,
        ref_name,
        event: "commit".to_string(),
        source: "flow-cli".to_string(),
        commit_message,
        author_name,
        author_email,
        session_hash: None,
        repo_snapshot: Some(snapshot),
    };

    let base_url = gitedit_api_url(&repo_root);
    let api_url = format!("{}/api/mirrors/sync", base_url.trim_end_matches('/'));
    let view_url = format!("{}/{}/{}", base_url.trim_end_matches('/'), owner, repo_name);
    let token = gitedit_token(&repo_root);

    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to build HTTP client")?;
    let mut request = client.post(&api_url).json(&payload);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request.send().context("failed to publish to gitedit")?;
    if !response.status().is_success() {
        bail!("gitedit publish failed: HTTP {}", response.status());
    }

    println!();
    println!("✓ Published to {}", view_url);
    Ok(())
}

fn resolve_repo_name(opts: &PublishOpts, fallback: &str) -> Result<String> {
    let name = if let Some(name) = opts.name.clone() {
        name
    } else if opts.yes {
        fallback.to_string()
    } else {
        print!("Repository name [{}]: ", fallback);
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let input = input.trim();
        if input.is_empty() {
            fallback.to_string()
        } else {
            input.to_string()
        }
    };
    Ok(name)
}

fn resolve_gitedit_owner(
    opts: &PublishOpts,
    override_owner: Option<String>,
    _repo_root: &Path,
) -> Result<String> {
    if let Some(owner) = opts.owner.clone() {
        return Ok(owner);
    }
    if let Some(owner) = override_owner {
        return Ok(owner);
    }
    if let Ok(owner) = std::env::var("GITEDIT_OWNER") {
        let owner = owner.trim();
        if !owner.is_empty() {
            return Ok(owner.to_string());
        }
    }
    if let Ok(owner) = std::env::var("USER") {
        let slug = sanitize_slug(&owner);
        if !slug.is_empty() {
            if opts.yes {
                return Ok(slug);
            }
            print!("gitedit owner [{}]: ", slug);
            io::stdout().flush()?;
            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            let input = input.trim();
            if input.is_empty() {
                return Ok(slug);
            }
            return Ok(input.to_string());
        }
    }
    if opts.yes {
        bail!("gitedit owner not set (use --owner or GITEDIT_OWNER)");
    }
    print!("gitedit owner: ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim();
    if input.is_empty() {
        bail!("gitedit owner is required");
    }
    Ok(input.to_string())
}

fn gitedit_repo_override(repo_root: &Path) -> (Option<String>, Option<String>) {
    let flow_path = find_flow_toml(repo_root);
    let Some(flow_path) = flow_path else {
        return (None, None);
    };
    let cfg = match config::load(&flow_path) {
        Ok(cfg) => cfg,
        Err(_) => return (None, None),
    };
    let raw = match cfg.options.gitedit_repo_full_name {
        Some(value) => value,
        None => return (None, None),
    };
    let mut parts = raw.split('/');
    let owner = parts.next().map(|value| value.to_string());
    let repo = parts.next().map(|value| value.to_string());
    (owner, repo)
}

fn gitedit_api_url(repo_root: &Path) -> String {
    let flow_path = find_flow_toml(repo_root);
    if let Some(flow_path) = flow_path {
        if let Ok(cfg) = config::load(&flow_path) {
            if let Some(url) = cfg.options.gitedit_url {
                let trimmed = url.trim().to_string();
                if !trimmed.is_empty() {
                    return trimmed;
                }
            }
        }
    }
    "https://gitedit.dev".to_string()
}

fn gitedit_token(repo_root: &Path) -> Option<String> {
    for key in [
        "GITEDIT_PUBLISH_TOKEN",
        "GITEDIT_TOKEN",
        "FLOW_GITEDIT_TOKEN",
    ] {
        if let Ok(value) = std::env::var(key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    let flow_path = find_flow_toml(repo_root)?;
    let cfg = config::load(&flow_path).ok()?;
    cfg.options.gitedit_token
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

fn git_root() -> Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("failed to locate git root")?;
    if !output.status.success() {
        bail!("not inside a git repository");
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(PathBuf::from(path))
}

fn ensure_git_repo(repo_root: &Path) -> Result<()> {
    let _ = vcs::ensure_jj_repo_in(repo_root)?;
    let git_dir = repo_root.join(".git");
    if !git_dir.exists() {
        Command::new("git")
            .args(["init"])
            .current_dir(repo_root)
            .status()
            .context("failed to initialize git")?;
    }

    let has_commits = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_root)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if !has_commits {
        Command::new("git")
            .args(["add", "."])
            .current_dir(repo_root)
            .status()
            .context("failed to stage files")?;
        Command::new("git")
            .args(["commit", "-m", "Initial commit"])
            .current_dir(repo_root)
            .status()
            .context("failed to create initial commit")?;
    }

    Ok(())
}

fn git_capture_in(repo_root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;
    if !output.status.success() {
        bail!("git {} failed", args.join(" "));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn build_repo_snapshot(
    repo_root: &Path,
    default_branch: &str,
    description: Option<String>,
) -> Result<RepoSnapshot> {
    let tree_output = git_capture_in(repo_root, &["ls-tree", "-r", "-t", "-l", "HEAD"])?;
    let mut tree = Vec::new();
    let mut files = Vec::new();
    let mut seen_paths = HashSet::new();
    let mut total_bytes: u64 = 0;
    let mut skipped_files: usize = 0;

    let mut readme_path: Option<String> = None;

    for line in tree_output.lines() {
        let Some((left, path)) = line.split_once('\t') else {
            continue;
        };
        let mut parts = left.split_whitespace();
        let _mode = parts.next();
        let entry_type = match parts.next() {
            Some(value) => value,
            None => continue,
        };
        let sha = match parts.next() {
            Some(value) => value.to_string(),
            None => continue,
        };
        let size = parts.next().and_then(|value| value.parse::<u64>().ok());
        let path = path.trim().to_string();
        if path.is_empty() {
            continue;
        }

        if !seen_paths.insert(path.clone()) {
            continue;
        }

        tree.push(RepoTreeEntry {
            path: path.clone(),
            entry_type: entry_type.to_string(),
            sha: sha.clone(),
            size,
        });

        if entry_type == "blob" {
            if files.len() >= MAX_GITEDIT_FILES {
                skipped_files += 1;
                continue;
            }

            let size_value = size.unwrap_or(0);
            let (content, is_binary, encoding, included_bytes) =
                read_blob_content(repo_root, &sha, size_value)?;
            if !content.is_empty() {
                total_bytes = total_bytes.saturating_add(included_bytes);
            }
            if total_bytes > MAX_GITEDIT_TOTAL_BYTES {
                files.push(RepoFileEntry {
                    path: path.clone(),
                    content: String::new(),
                    size: size_value,
                    is_binary: true,
                    encoding: "binary".to_string(),
                });
                skipped_files += 1;
                continue;
            }

            files.push(RepoFileEntry {
                path: path.clone(),
                content,
                size: size_value,
                is_binary,
                encoding,
            });

            if readme_path.is_none() && is_readme_path(&path) {
                readme_path = Some(path);
            }
        }
    }

    let readme = readme_path.and_then(|path| {
        files
            .iter()
            .find(|entry| entry.path == path && !entry.is_binary)
            .map(|entry| RepoReadme {
                path: entry.path.clone(),
                content: entry.content.clone(),
            })
    });

    if skipped_files > 0 {
        println!(
            "Warning: skipped {} file(s) (size or limit exceeded) for gitedit snapshot.",
            skipped_files
        );
    }

    Ok(RepoSnapshot {
        repo: RepoMeta {
            description,
            default_branch: default_branch.to_string(),
            language: None,
        },
        tree,
        files,
        readme,
    })
}

fn read_blob_content(
    repo_root: &Path,
    sha: &str,
    size: u64,
) -> Result<(String, bool, String, u64)> {
    if size > MAX_GITEDIT_FILE_BYTES {
        return Ok((String::new(), true, "binary".to_string(), 0));
    }
    let output = Command::new("git")
        .args(["cat-file", "-p", sha])
        .current_dir(repo_root)
        .output()
        .context("failed to read git blob")?;
    if !output.status.success() {
        return Ok((String::new(), true, "binary".to_string(), 0));
    }
    if output.stdout.iter().any(|byte| *byte == 0) {
        return Ok((String::new(), true, "binary".to_string(), 0));
    }
    match String::from_utf8(output.stdout) {
        Ok(text) => Ok((text, false, "utf-8".to_string(), size)),
        Err(_) => Ok((String::new(), true, "binary".to_string(), 0)),
    }
}

fn is_readme_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with("readme.md")
        || lower.ends_with("readme.markdown")
        || lower.ends_with("readme.mdx")
}

fn sanitize_slug(value: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if ch == '-' || ch == '_' {
            out.push(ch);
            prev_dash = ch == '-';
        } else if ch.is_whitespace() || ch == '.' || ch == '/' {
            if !prev_dash && !out.is_empty() {
                out.push('-');
                prev_dash = true;
            }
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

fn prompt_public_choice() -> Result<bool> {
    let default_public = false;
    print!("Public? [y/N]: ");
    io::stdout().flush()?;

    if io::stdin().is_terminal() {
        return read_yes_no_key(default_public);
    }

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_ascii_lowercase();
    if answer.is_empty() {
        return Ok(default_public);
    }
    Ok(matches!(
        answer.as_str(),
        "y" | "yes" | "public" | "pub" | "p"
    ))
}

fn prompt_push_choice() -> Result<bool> {
    let default_push = true;
    print!("Push current branch to origin? [Y/n]: ");
    io::stdout().flush()?;

    if io::stdin().is_terminal() {
        return read_yes_no_key(default_push);
    }

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_ascii_lowercase();
    if answer.is_empty() {
        return Ok(default_push);
    }
    Ok(matches!(answer.as_str(), "y" | "yes"))
}

fn read_yes_no_key(default_yes: bool) -> Result<bool> {
    enable_raw_mode().context("failed to enable raw mode")?;
    let mut selection = default_yes;
    let mut echo_char: Option<char> = None;
    loop {
        if let CEvent::Key(key) = event::read()? {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    selection = true;
                    echo_char = Some('y');
                    break;
                }
                KeyCode::Char('n') | KeyCode::Char('N') => {
                    selection = false;
                    echo_char = Some('n');
                    break;
                }
                KeyCode::Enter => {
                    break;
                }
                KeyCode::Esc => {
                    selection = false;
                    break;
                }
                _ => {}
            }
        }
    }

    disable_raw_mode().context("failed to disable raw mode")?;
    if let Some(ch) = echo_char {
        println!("{ch}");
    } else {
        println!();
    }
    Ok(selection)
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

    let status = Command::new("git")
        .args(["push", "-u", "origin", &branch])
        .status()
        .context("failed to push to origin")?;

    if !status.success() {
        bail!("git push failed");
    }

    Ok(())
}
