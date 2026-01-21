use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::Deserialize;

use crate::cli::{HomeAction, HomeCommand};
use crate::{config, ssh, ssh_keys};

const DEFAULT_REPOS_ROOT: &str = "~/repos";

#[derive(Debug, Clone)]
struct RepoInput {
    owner: String,
    repo: String,
    clone_url: String,
    scheme: RepoScheme,
}

#[derive(Debug, Clone, Copy, PartialEq)]
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
    #[serde(default)]
    kar_repo: Option<String>,
    #[serde(default)]
    kar_repo_url: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct HomeConfigSection {
    #[serde(default)]
    internal_repo: Option<String>,
    #[serde(default)]
    internal_repo_url: Option<String>,
    #[serde(default)]
    kar_repo: Option<String>,
    #[serde(default)]
    kar_repo_url: Option<String>,
}

pub fn run(opts: HomeCommand) -> Result<()> {
    if let Some(action) = opts.action {
        match action {
            HomeAction::Setup => return setup(),
        }
    }

    ssh::ensure_ssh_env();
    let mode = ssh::ssh_mode();
    if matches!(mode, ssh::SshMode::Force) && !ssh::has_identities() {
        match ssh_keys::ensure_default_identity(24) {
            Ok(()) => {}
            Err(err) => println!(
                "warning: SSH mode is forced but no key is available ({}). Run `f ssh setup` or `f ssh unlock`.",
                err
            ),
        }
    }
    let prefer_ssh = ssh::prefer_ssh();
    let home = dirs::home_dir().context("Could not find home directory")?;
    let config_dir = home.join("config");
    let repo_str = opts
        .repo
        .as_ref()
        .context("Missing repo. Use `f home <repo>` or `f home setup`.")?;
    let repo = coerce_repo_scheme(parse_repo_input(repo_str)?, prefer_ssh);
    let flow_bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("f"));

    ensure_repo(&config_dir, Some(&repo.clone_url), "config", false)?;

    let internal_url = if let Some(internal) = opts.internal.as_deref() {
        Some(coerce_repo_url(internal, prefer_ssh))
    } else {
        read_internal_repo(&config_dir)?
            .map(|url| coerce_repo_url(&url, prefer_ssh))
            .or_else(|| derive_internal_repo(&repo))
    };

    let internal_dir = config_dir.join("i");
    if internal_dir.exists() {
        ensure_repo(&internal_dir, internal_url.as_deref(), "config/i", false)?;
    } else if let Some(url) = internal_url.as_deref() {
        ensure_repo(&internal_dir, Some(url), "config/i", false)?;
    } else {
        println!(
            "No internal repo configured; skipping {} (use --internal or add home.toml)",
            internal_dir.display()
        );
    }

    let archived = archive_existing_configs(&config_dir)?;
    apply_config(&config_dir)?;

    match ssh::ensure_git_ssh_command() {
        Ok(true) => println!("Configured git to use 1Password SSH agent."),
        Ok(false) => {}
        Err(err) => println!("warning: failed to configure git ssh: {}", err),
    }
    if !prefer_ssh {
        match ssh::ensure_git_https_insteadof() {
            Ok(true) => println!("Configured git to use HTTPS when SSH isn't available."),
            Ok(false) => {}
            Err(err) => println!("warning: failed to configure git https rewrites: {}", err),
        }
    }
    if let Some(kar_repo) = resolve_kar_repo(&config_dir)? {
        ensure_kar_repo(&flow_bin, prefer_ssh, &kar_repo)?;
    } else {
        println!("No kar repo configured; skipping kar deploy.");
    }
    validate_setup(&config_dir)?;

    if !archived.is_empty() {
        println!("\nMoved existing config files to ~/flow-archive:");
        for path in archived {
            println!("  {}", path.display());
        }
        println!("Restore any file by moving it back to its original path.");
    }

    Ok(())
}

pub fn setup() -> Result<()> {
    println!("Home setup");
    println!("-----------");

    if !check_git() {
        println!("git not found on PATH. Install Xcode Command Line Tools:");
        println!("  xcode-select --install");
        return Ok(());
    }

    ssh::ensure_ssh_env();

    let ssh_check = check_git_access("git@github.com:github/linguist.git");
    if ssh_check.ok {
        println!("✓ GitHub SSH auth works (git@github.com)");
    } else {
        println!("✗ GitHub SSH auth failed (git@github.com)");
    }

    let https_check = check_git_access("https://github.com/github/linguist.git");
    if https_check.ok {
        println!("✓ GitHub HTTPS works (https://github.com)");
    } else {
        println!("✗ GitHub HTTPS failed (https://github.com)");
    }

    if !ssh_check.ok && https_check.ok {
        match ssh::ensure_git_https_insteadof() {
            Ok(true) => println!("Configured git to use HTTPS when SSH isn't available."),
            Ok(false) => {}
            Err(err) => println!("warning: failed to configure git https rewrites: {}", err),
        }
        println!("If you want SSH instead, add your key to GitHub and run:");
        println!("  f ssh setup");
        println!("  ssh -T git@github.com");
    }

    if !ssh_check.ok && !https_check.ok {
        println!("GitHub connectivity failed. Check your network or proxy settings.");
    }

    if !ssh_check.ok {
        if ssh_check
            .stderr
            .to_lowercase()
            .contains("permission denied (publickey)")
        {
            println!("SSH key is not authorized for GitHub. Add ~/.ssh/id_ed25519.pub to GitHub.");
        } else if ssh_check
            .stderr
            .to_lowercase()
            .contains("host key verification failed")
        {
            println!("Accept GitHub host key first: ssh -T git@github.com");
        }
    }

    println!("Done.");
    Ok(())
}

fn check_git() -> bool {
    Command::new("git")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

struct GitCheck {
    ok: bool,
    stderr: String,
}

fn check_git_access(url: &str) -> GitCheck {
    let output = Command::new("git")
        .args(["ls-remote", "--heads", url])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output();

    match output {
        Ok(out) => GitCheck {
            ok: out.status.success(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        },
        Err(err) => GitCheck {
            ok: false,
            stderr: err.to_string(),
        },
    }
}

fn archive_existing_configs(config_dir: &Path) -> Result<Vec<PathBuf>> {
    let home = dirs::home_dir().context("Could not find home directory")?;
    let archive_root = home.join("flow-archive");
    let mappings = load_link_mappings(config_dir)?;
    let mut moved = Vec::new();

    for (source_rel, dest_rel) in mappings {
        let source = config_dir.join(&source_rel);
        if !source.exists() {
            continue;
        }

        let dest_rel = normalize_dest_rel(&dest_rel)?;
        let dest = home.join(&dest_rel);
        if !dest.exists() {
            continue;
        }

        if is_symlink_to(&dest, &source) {
            continue;
        }

        let mut archive_path = archive_root.join(&dest_rel);
        if archive_path.exists() {
            archive_path =
                archive_path.with_extension(format!("bak-{}", chrono::Utc::now().timestamp()));
        }

        if let Some(parent) = archive_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        fs::rename(&dest, &archive_path).with_context(|| {
            format!(
                "failed to move {} to {}",
                dest.display(),
                archive_path.display()
            )
        })?;
        moved.push(archive_path);
    }

    Ok(moved)
}

fn normalize_dest_rel(dest: &Path) -> Result<PathBuf> {
    let dest_str = dest.to_string_lossy();
    if let Some(stripped) = dest_str.strip_prefix("~/") {
        return Ok(PathBuf::from(stripped));
    }
    if dest.is_absolute() {
        bail!(
            "absolute paths are not supported in sync links: {}",
            dest.display()
        );
    }
    Ok(dest.to_path_buf())
}

fn is_symlink_to(link: &Path, expected: &Path) -> bool {
    let meta = match fs::symlink_metadata(link) {
        Ok(v) => v,
        Err(_) => return false,
    };
    if !meta.file_type().is_symlink() {
        return false;
    }

    let target = match fs::read_link(link) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let resolved = if target.is_absolute() {
        target
    } else {
        link.parent().unwrap_or_else(|| Path::new(".")).join(target)
    };

    let expected = match fs::canonicalize(expected) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let resolved = match fs::canonicalize(resolved) {
        Ok(v) => v,
        Err(_) => return false,
    };

    resolved == expected
}

fn load_link_mappings(config_dir: &Path) -> Result<Vec<(PathBuf, PathBuf)>> {
    let sync_file = config_dir.join("sync").join("src").join("main.ts");
    if sync_file.exists() {
        let raw = fs::read_to_string(&sync_file)
            .with_context(|| format!("failed to read {}", sync_file.display()))?;
        let re =
            Regex::new(r#""([^"]+)"\s*:\s*"([^"]+)""#).context("failed to compile link regex")?;
        let mut links = Vec::new();
        let mut in_links = false;
        for line in raw.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("const LINKS") {
                in_links = true;
                continue;
            }
            if in_links && trimmed.starts_with('}') {
                break;
            }
            if !in_links {
                continue;
            }
            if let Some(caps) = re.captures(trimmed) {
                let src = caps.get(1).map(|m| m.as_str()).unwrap_or("");
                let dst = caps.get(2).map(|m| m.as_str()).unwrap_or("");
                if !src.is_empty() && !dst.is_empty() {
                    links.push((PathBuf::from(src), PathBuf::from(dst)));
                }
            }
        }
        if !links.is_empty() {
            return Ok(links);
        }
    }

    Ok(default_link_mappings())
}

fn default_link_mappings() -> Vec<(PathBuf, PathBuf)> {
    vec![
        ("fish/config.fish", ".config/fish/config.fish"),
        ("fish/fn.fish", ".config/fish/fn.fish"),
        ("i/karabiner/karabiner.edn", ".config/karabiner.edn"),
        ("i/kar", ".config/kar"),
        ("i/git/.gitconfig", ".gitconfig"),
        ("i/ssh/config", ".ssh/config"),
        ("i/ghost/ghost.toml", ".config/ghost/ghost.toml"),
        ("i/flow", ".config/flow"),
    ]
    .into_iter()
    .map(|(src, dst)| (PathBuf::from(src), PathBuf::from(dst)))
    .collect()
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
            ensure_link_targets(config_dir)?;
            return Ok(());
        }
    }

    if which::which("sync").is_ok() {
        run_command("sync", &["link"], Some(config_dir))?;
        ensure_link_targets(config_dir)?;
        return Ok(());
    }

    let fallback = config_dir.join("sh").join("check-config-setup.sh");
    if fallback.exists() {
        println!("sync not available; falling back to {}", fallback.display());
        run_command(fallback.to_string_lossy().as_ref(), &[], Some(config_dir))?;
        let internal_fallback = config_dir.join("sh").join("ensure-i-dotfiles.sh");
        if internal_fallback.exists() {
            run_command(
                internal_fallback.to_string_lossy().as_ref(),
                &[],
                Some(config_dir),
            )?;
        }
        ensure_link_targets(config_dir)?;
        return Ok(());
    }

    println!(
        "sync tool not available; applying symlinks directly from {}",
        config_dir.display()
    );
    ensure_link_targets(config_dir)
}

fn ensure_link_targets(config_dir: &Path) -> Result<()> {
    let home = dirs::home_dir().context("Could not find home directory")?;
    let mappings = load_link_mappings(config_dir)?;

    for (source_rel, dest_rel) in mappings {
        let source = config_dir.join(&source_rel);
        if !source.exists() {
            continue;
        }

        let dest_rel = normalize_dest_rel(&dest_rel)?;
        let dest = home.join(&dest_rel);

        if is_symlink_to(&dest, &source) {
            continue;
        }

        if let Ok(meta) = fs::symlink_metadata(&dest) {
            if meta.file_type().is_dir() {
                fs::remove_dir_all(&dest)
                    .with_context(|| format!("failed to remove {}", dest.display()))?;
            } else {
                fs::remove_file(&dest)
                    .with_context(|| format!("failed to remove {}", dest.display()))?;
            }
        }

        if let Some(parent) = dest.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }
        }

        create_symlink(&source, &dest)?;
    }

    Ok(())
}

fn create_symlink(source: &Path, dest: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(source, dest).with_context(|| {
            format!(
                "failed to symlink {} -> {}",
                dest.display(),
                source.display()
            )
        })?;
        return Ok(());
    }
    #[cfg(not(unix))]
    {
        bail!("symlinks are only supported on unix-like systems");
    }
}

fn ensure_kar_repo(flow_bin: &Path, prefer_ssh: bool, repo_url: &str) -> Result<()> {
    let repo_url = coerce_repo_url(repo_url, prefer_ssh);
    let repo = parse_repo_input(&repo_url)?;
    let root = config::expand_path(DEFAULT_REPOS_ROOT);
    let owner_dir = root.join(&repo.owner);
    let repo_path = owner_dir.join(&repo.repo);

    ensure_repo(&repo_path, Some(&repo.clone_url), "kar", true)?;

    let flow_toml = repo_path.join("flow.toml");
    if !flow_toml.exists() {
        println!(
            "No flow.toml found in {}; skipping f deploy",
            repo_path.display()
        );
        return Ok(());
    }

    println!("Deploying kar from {}", repo_path.display());
    run_command(
        flow_bin.to_string_lossy().as_ref(),
        &["deploy"],
        Some(&repo_path),
    )?;
    Ok(())
}

fn validate_setup(config_dir: &Path) -> Result<()> {
    let home = dirs::home_dir().context("Could not find home directory")?;
    let mappings = load_link_mappings(config_dir)?;
    let mut missing = Vec::new();
    let mut mismatched = Vec::new();

    for (source_rel, dest_rel) in &mappings {
        let source = config_dir.join(source_rel);
        if !source.exists() {
            continue;
        }

        let dest_rel = normalize_dest_rel(dest_rel)?;
        let dest = home.join(&dest_rel);
        if !dest.exists() {
            missing.push(dest);
            continue;
        }

        if !is_symlink_to(&dest, &source) {
            mismatched.push((dest, source));
        }
    }

    let mut critical_missing = Vec::new();
    let kar_config = home.join(".config/kar/config.ts");
    if !kar_config.exists() {
        critical_missing.push(kar_config);
    }
    let karabiner_config = home.join(".config/karabiner.edn");
    if !karabiner_config.exists() {
        critical_missing.push(karabiner_config);
    }

    if missing.is_empty() && mismatched.is_empty() && critical_missing.is_empty() {
        println!("Validation: all expected configs are in place.");
        return Ok(());
    }

    println!("\nValidation warnings:");
    for path in critical_missing {
        println!("  missing critical config: {}", path.display());
    }
    for path in missing {
        println!("  missing link target: {}", path.display());
    }
    for (dest, source) in mismatched {
        println!(
            "  not linked: {} (expected -> {})",
            dest.display(),
            source.display()
        );
    }

    Ok(())
}

fn ensure_repo(
    dest: &Path,
    repo_url: Option<&str>,
    label: &str,
    allow_origin_reset: bool,
) -> Result<()> {
    if dest.exists() {
        if !dest.join(".git").exists() {
            bail!("{} exists but is not a git repo: {}", label, dest.display());
        }

        if let Some(expected) = repo_url {
            if allow_origin_reset {
                ensure_origin_url(dest, expected)?;
            } else if let Ok(actual) = git_capture(dest, &["remote", "get-url", "origin"]) {
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

fn ensure_origin_url(dest: &Path, expected: &str) -> Result<()> {
    match git_capture(dest, &["remote", "get-url", "origin"]) {
        Ok(actual) => {
            let actual = actual.trim();
            let mut needs_reset = !urls_match(expected, actual);
            if !needs_reset {
                if let (Some(expected_scheme), Some(actual_scheme)) =
                    (scheme_for_url(expected), scheme_for_url(actual))
                {
                    if expected_scheme != actual_scheme {
                        needs_reset = true;
                    }
                }
            }
            if needs_reset {
                run_command(
                    "git",
                    &["remote", "set-url", "origin", expected],
                    Some(dest),
                )?;
            }
        }
        Err(_) => {
            run_command("git", &["remote", "add", "origin", expected], Some(dest))?;
        }
    }
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
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let parsed: HomeConfigFile =
            toml::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))?;
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

fn resolve_kar_repo(config_dir: &Path) -> Result<Option<String>> {
    if let Ok(value) = std::env::var("FLOW_HOME_KAR_REPO") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Ok(Some(trimmed.to_string()));
        }
    }
    read_kar_repo(config_dir)
}

fn read_kar_repo(config_dir: &Path) -> Result<Option<String>> {
    let candidates = [config_dir.join("home.toml"), config_dir.join(".home.toml")];
    for path in candidates {
        if !path.exists() {
            continue;
        }
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let parsed: HomeConfigFile =
            toml::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))?;
        let from_section = parsed
            .home
            .as_ref()
            .and_then(|h| h.kar_repo.clone().or(h.kar_repo_url.clone()));
        let flat = parsed.kar_repo.or(parsed.kar_repo_url);
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

fn coerce_repo_scheme(repo: RepoInput, prefer_ssh: bool) -> RepoInput {
    let desired = if prefer_ssh {
        RepoScheme::Ssh
    } else {
        RepoScheme::Https
    };
    if repo.scheme == desired {
        return repo;
    }

    let clone_url = match desired {
        RepoScheme::Https => format!("https://github.com/{}/{}.git", repo.owner, repo.repo),
        RepoScheme::Ssh => format!("git@github.com:{}/{}.git", repo.owner, repo.repo),
    };

    RepoInput {
        owner: repo.owner,
        repo: repo.repo,
        clone_url,
        scheme: desired,
    }
}

fn coerce_repo_url(raw: &str, prefer_ssh: bool) -> String {
    match parse_repo_input(raw) {
        Ok(repo) => coerce_repo_scheme(repo, prefer_ssh).clone_url,
        Err(_) => raw.to_string(),
    }
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

fn scheme_for_url(raw: &str) -> Option<RepoScheme> {
    let trimmed = raw.trim();
    if trimmed.starts_with("git@github.com:") || trimmed.starts_with("ssh://git@github.com/") {
        return Some(RepoScheme::Ssh);
    }
    if trimmed.starts_with("https://github.com/") || trimmed.starts_with("http://github.com/") {
        return Some(RepoScheme::Https);
    }
    None
}
