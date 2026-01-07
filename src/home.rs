use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::cli::{HomeOpts, ReposCloneOpts};
use crate::repos;

const KAR_REPO_URL: &str = "https://github.com/nikivdev/kar";
const DEFAULT_REPOS_ROOT: &str = "~/repos";

#[derive(Debug, Clone)]
struct RepoInput {
    owner: String,
    repo: String,
    clone_url: String,
    scheme: RepoScheme,
}

#[derive(Debug, Clone, Copy)]
enum RepoScheme {
    Https,
    Ssh,
}

#[derive(Debug, Default, Deserialize)]
struct HomeConfigFile {
    #[serde(default)]
    home: Option<HomeConfigSection>,
    #[serde(default)]
    internal_repo: Option<String>,
    #[serde(default)]
    internal_repo_url: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct HomeConfigSection {
    #[serde(default)]
    internal_repo: Option<String>,
    #[serde(default)]
    internal_repo_url: Option<String>,
}

pub fn run(opts: HomeOpts) -> Result<()> {
    let home = dirs::home_dir().context("Could not find home directory")?;
    let config_dir = home.join("config");
    let repo = parse_repo_input(&opts.repo)?;
    let flow_bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("f"));

    ensure_repo(&config_dir, Some(&repo.clone_url), "config")?;

    let internal_url = if let Some(internal) = opts.internal.as_deref() {
        Some(internal.to_string())
    } else {
        read_internal_repo(&config_dir)?.or_else(|| derive_internal_repo(&repo))
    };

    let internal_dir = config_dir.join("i");
    if internal_dir.exists() {
        ensure_repo(&internal_dir, internal_url.as_deref(), "config/i")?;
    } else if let Some(url) = internal_url.as_deref() {
        ensure_repo(&internal_dir, Some(url), "config/i")?;
    } else {
        println!(
            "No internal repo configured; skipping {} (use --internal or add home.toml)",
            internal_dir.display()
        );
    }

    apply_config(&config_dir)?;
    ensure_kar_repo(&flow_bin)?;

    Ok(())
}

fn apply_config(config_dir: &Path) -> Result<()> {
    let sync_script = config_dir.join("sync").join("src").join("main.ts");
    if sync_script.exists() {
        if which::which("bun").is_ok() {
            run_command(
                "bun",
                &[sync_script.to_string_lossy().as_ref(), "link"],
                Some(config_dir),
            )?;
            return Ok(());
        }
    }

    if which::which("sync").is_ok() {
        run_command("sync", &["link"], Some(config_dir))?;
        return Ok(());
    }

    let fallback = config_dir.join("sh").join("check-config-setup.sh");
    if fallback.exists() {
        println!("sync not available; falling back to {}", fallback.display());
        run_command(
            fallback.to_string_lossy().as_ref(),
            &[],
            Some(config_dir),
        )?;
        let internal_fallback = config_dir.join("sh").join("ensure-i-dotfiles.sh");
        if internal_fallback.exists() {
            run_command(
                internal_fallback.to_string_lossy().as_ref(),
                &[],
                Some(config_dir),
            )?;
        }
        return Ok(());
    }

    bail!(
        "sync tool not available; install bun or build the sync CLI in {}",
        config_dir.display()
    )
}

fn ensure_kar_repo(flow_bin: &Path) -> Result<()> {
    let opts = ReposCloneOpts {
        url: KAR_REPO_URL.to_string(),
        root: DEFAULT_REPOS_ROOT.to_string(),
        full: false,
        no_upstream: false,
        upstream_url: None,
    };
    let repo_path = repos::clone_repo(opts)?;
    update_repo(&repo_path)?;

    let flow_toml = repo_path.join("flow.toml");
    if !flow_toml.exists() {
        println!(
            "No flow.toml found in {}; skipping f deploy",
            repo_path.display()
        );
        return Ok(());
    }

    println!("Deploying kar from {}", repo_path.display());
    run_command(flow_bin.to_string_lossy().as_ref(), &["deploy"], Some(&repo_path))?;
    Ok(())
}

fn ensure_repo(dest: &Path, repo_url: Option<&str>, label: &str) -> Result<()> {
    if dest.exists() {
        if !dest.join(".git").exists() {
            bail!(
                "{} exists but is not a git repo: {}",
                label,
                dest.display()
            );
        }

        if let Some(expected) = repo_url {
            if let Ok(actual) = git_capture(dest, &["remote", "get-url", "origin"]) {
                if !urls_match(expected, actual.trim()) {
                    bail!(
                        "{} origin mismatch: expected {}, got {}",
                        label,
                        expected,
                        actual.trim()
                    );
                }
            }
        }

        update_repo(dest)?;
        return Ok(());
    }

    let repo_url = repo_url.ok_or_else(|| anyhow::anyhow!("{} repo URL required", label))?;
    clone_repo(repo_url, dest)?;
    Ok(())
}

fn update_repo(dest: &Path) -> Result<()> {
    run_command("git", &["fetch", "--prune", "origin"], Some(dest))?;
    let branch = default_branch(dest)?;
    run_command(
        "git",
        &["checkout", "-B", &branch, &format!("origin/{}", branch)],
        Some(dest),
    )?;
    run_command(
        "git",
        &["reset", "--hard", &format!("origin/{}", branch)],
        Some(dest),
    )?;
    println!("Updated {}", dest.display());
    Ok(())
}

fn clone_repo(repo_url: &str, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
    }

    run_command(
        "git",
        &["clone", repo_url, dest.to_string_lossy().as_ref()],
        None,
    )?;
    println!("Cloned {}", dest.display());
    Ok(())
}

fn default_branch(dest: &Path) -> Result<String> {
    if let Ok(head) = git_capture(dest, &["symbolic-ref", "refs/remotes/origin/HEAD"]) {
        if let Some(branch) = head.trim().rsplit('/').next() {
            if !branch.is_empty() {
                return Ok(branch.to_string());
            }
        }
    }

    if git_ref_exists(dest, "refs/remotes/origin/main")? {
        return Ok("main".to_string());
    }
    if git_ref_exists(dest, "refs/remotes/origin/master")? {
        return Ok("master".to_string());
    }

    Ok("main".to_string())
}

fn git_ref_exists(dest: &Path, reference: &str) -> Result<bool> {
    let status = Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", reference])
        .current_dir(dest)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("failed to run git")?;
    Ok(status.success())
}

fn git_capture(dest: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dest)
        .stdin(Stdio::null())
        .output()
        .context("failed to run git")?;
    if !output.status.success() {
        bail!("git {} failed", args.join(" "));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn run_command(cmd: &str, args: &[&str], cwd: Option<&Path>) -> Result<()> {
    let mut command = Command::new(cmd);
    command.args(args);
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    let status = command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("failed to run {}", cmd))?;
    if !status.success() {
        bail!("{} failed with status {}", cmd, status);
    }
    Ok(())
}

fn read_internal_repo(config_dir: &Path) -> Result<Option<String>> {
    let candidates = [config_dir.join("home.toml"), config_dir.join(".home.toml")];
    for path in candidates {
        if !path.exists() {
            continue;
        }
        let raw =
            fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
        let parsed: HomeConfigFile = toml::from_str(&raw)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        let from_section = parsed
            .home
            .as_ref()
            .and_then(|h| h.internal_repo.clone().or(h.internal_repo_url.clone()));
        let flat = parsed.internal_repo.or(parsed.internal_repo_url);
        if from_section.is_some() {
            return Ok(from_section);
        }
        if flat.is_some() {
            return Ok(flat);
        }
    }
    Ok(None)
}

fn derive_internal_repo(repo: &RepoInput) -> Option<String> {
    let suffix = format!("{}-i", repo.repo);
    match repo.scheme {
        RepoScheme::Https => Some(format!("https://github.com/{}/{}.git", repo.owner, suffix)),
        RepoScheme::Ssh => Some(format!("git@github.com:{}/{}.git", repo.owner, suffix)),
    }
}

fn parse_repo_input(input: &str) -> Result<RepoInput> {
    let trimmed = input.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        bail!("repo URL is required");
    }

    if let Some(rest) = trimmed.strip_prefix("git@github.com:") {
        return parse_owner_repo(rest, RepoScheme::Ssh);
    }

    if let Some(rest) = trimmed.strip_prefix("ssh://git@github.com/") {
        return parse_owner_repo(rest, RepoScheme::Ssh);
    }

    if let Some(rest) = trimmed.strip_prefix("https://github.com/") {
        return parse_owner_repo(rest, RepoScheme::Https);
    }

    if let Some(rest) = trimmed.strip_prefix("http://github.com/") {
        return parse_owner_repo(rest, RepoScheme::Https);
    }

    if let Some(rest) = trimmed.strip_prefix("github.com/") {
        return parse_owner_repo(rest, RepoScheme::Https);
    }

    if trimmed.contains('/') {
        return parse_owner_repo(trimmed, RepoScheme::Https);
    }

    bail!("unable to parse GitHub repo from: {}", input)
}

fn parse_owner_repo(raw: &str, scheme: RepoScheme) -> Result<RepoInput> {
    let cleaned = raw.trim().trim_end_matches(".git").trim_end_matches('/');
    let mut parts = cleaned.splitn(2, '/');
    let owner = parts.next().unwrap_or("").trim();
    let repo = parts.next().unwrap_or("").trim();
    if owner.is_empty() || repo.is_empty() {
        bail!("unable to parse GitHub repo from: {}", raw);
    }

    let clone_url = match scheme {
        RepoScheme::Https => format!("https://github.com/{}/{}.git", owner, repo),
        RepoScheme::Ssh => format!("git@github.com:{}/{}.git", owner, repo),
    };

    Ok(RepoInput {
        owner: owner.to_string(),
        repo: repo.to_string(),
        clone_url,
        scheme,
    })
}

fn urls_match(a: &str, b: &str) -> bool {
    normalize_repo_url(a) == normalize_repo_url(b)
}

fn normalize_repo_url(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/').trim_end_matches(".git");
    if let Some(rest) = trimmed.strip_prefix("git@github.com:") {
        return format!("github.com/{}", rest);
    }
    if let Some(rest) = trimmed.strip_prefix("ssh://git@github.com/") {
        return format!("github.com/{}", rest);
    }
    if let Some(rest) = trimmed.strip_prefix("https://github.com/") {
        return format!("github.com/{}", rest);
    }
    if let Some(rest) = trimmed.strip_prefix("http://github.com/") {
        return format!("github.com/{}", rest);
    }
    if let Some(rest) = trimmed.strip_prefix("github.com/") {
        return format!("github.com/{}", rest);
    }
    trimmed.to_string()
}
