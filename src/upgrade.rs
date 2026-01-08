//! Self-upgrade functionality for flow.
//!
//! Similar to Deno's upgrade system:
//! - Fetches latest version from GitHub releases
//! - Downloads and replaces the current binary
//! - Background version checking with caching

use std::env;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use serde::Deserialize;

use crate::cli::UpgradeOpts;

const GITHUB_OWNER: &str = "nikivdev";
const GITHUB_REPO: &str = "flow";
const UPGRADE_CHECK_INTERVAL_HOURS: u64 = 24;

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    assets: Vec<GitHubAsset>,
    html_url: String,
}

#[derive(Debug, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

/// Version check cache stored in ~/.cache/flow/upgrade_check.txt
#[derive(Debug)]
struct VersionCache {
    last_checked: u64,
    latest_version: String,
    current_version: String,
}

impl VersionCache {
    fn cache_path() -> PathBuf {
        dirs::cache_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("flow")
            .join("upgrade_check.txt")
    }

    fn load() -> Option<Self> {
        let path = Self::cache_path();
        let content = fs::read_to_string(&path).ok()?;
        let parts: Vec<&str> = content.trim().split('!').collect();
        if parts.len() >= 3 {
            Some(Self {
                last_checked: parts[0].parse().ok()?,
                latest_version: parts[1].to_string(),
                current_version: parts[2].to_string(),
            })
        } else {
            None
        }
    }

    fn save(&self) -> Result<()> {
        let path = Self::cache_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = format!(
            "{}!{}!{}",
            self.last_checked, self.latest_version, self.current_version
        );
        fs::write(&path, content)?;
        Ok(())
    }

    fn now_timestamp() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    fn should_check(&self) -> bool {
        let now = Self::now_timestamp();
        let elapsed_hours = (now.saturating_sub(self.last_checked)) / 3600;
        elapsed_hours >= UPGRADE_CHECK_INTERVAL_HOURS
    }
}

/// Get current version from Cargo.toml embedded at compile time.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Detect the current platform (os, arch).
fn detect_platform() -> Result<(String, String)> {
    let os = if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        bail!("Unsupported operating system");
    };

    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else if cfg!(target_arch = "x86_64") {
        "amd64"
    } else {
        bail!("Unsupported architecture");
    };

    Ok((os.to_string(), arch.to_string()))
}

/// Fetch the latest release info from GitHub.
fn fetch_latest_release(client: &Client) -> Result<GitHubRelease> {
    let url = format!(
        "https://api.github.com/repos/{}/{}/releases/latest",
        GITHUB_OWNER, GITHUB_REPO
    );

    let response = client
        .get(&url)
        .header("User-Agent", format!("flow/{}", current_version()))
        .header("Accept", "application/vnd.github.v3+json")
        .timeout(Duration::from_secs(30))
        .send()
        .context("Failed to fetch release info from GitHub")?;

    if !response.status().is_success() {
        bail!(
            "GitHub API returned status {}: {}",
            response.status(),
            response.text().unwrap_or_default()
        );
    }

    response
        .json::<GitHubRelease>()
        .context("Failed to parse GitHub release response")
}

/// Parse version string, stripping 'v' prefix if present.
fn parse_version(version: &str) -> &str {
    version.strip_prefix('v').unwrap_or(version)
}

/// Compare two semver-like versions. Returns true if `latest` is newer than `current`.
fn is_newer_version(current: &str, latest: &str) -> bool {
    let current = parse_version(current);
    let latest = parse_version(latest);

    let parse_parts = |v: &str| -> Vec<u32> {
        v.split(|c: char| c == '.' || c == '-')
            .filter_map(|s| s.parse().ok())
            .collect()
    };

    let current_parts = parse_parts(current);
    let latest_parts = parse_parts(latest);

    for (c, l) in current_parts.iter().zip(latest_parts.iter()) {
        if l > c {
            return true;
        }
        if l < c {
            return false;
        }
    }

    latest_parts.len() > current_parts.len()
}

/// Download a file with progress indication.
fn download_with_progress(client: &Client, url: &str, dest: &Path) -> Result<()> {
    let response = client
        .get(url)
        .header("User-Agent", format!("flow/{}", current_version()))
        .timeout(Duration::from_secs(300))
        .send()
        .context("Failed to start download")?;

    if !response.status().is_success() {
        bail!("Download failed with status {}", response.status());
    }

    let total_size = response.content_length();
    let mut file = File::create(dest).context("Failed to create temp file")?;

    let bytes = response.bytes().context("Failed to read response")?;

    if let Some(total) = total_size {
        println!("Downloading {} bytes...", total);
    }

    file.write_all(&bytes)?;
    Ok(())
}

/// Extract tarball and find the binary.
fn extract_binary(tarball: &Path, binary_name: &str) -> Result<PathBuf> {
    let temp_dir = tempfile::tempdir().context("Failed to create temp directory")?;
    let temp_path = temp_dir.path();

    // Extract tarball
    let status = Command::new("tar")
        .args([
            "-xzf",
            tarball.to_str().unwrap(),
            "-C",
            temp_path.to_str().unwrap(),
        ])
        .status()
        .context("Failed to run tar")?;

    if !status.success() {
        bail!("Failed to extract tarball");
    }

    // Find the binary (might be in a subdirectory)
    let find_binary = |dir: &Path| -> Option<PathBuf> {
        if dir.join(binary_name).exists() {
            return Some(dir.join(binary_name));
        }
        // Check one level deep
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let bin_path = path.join(binary_name);
                    if bin_path.exists() {
                        return Some(bin_path);
                    }
                }
            }
        }
        None
    };

    let binary_path = find_binary(temp_path)
        .ok_or_else(|| anyhow::anyhow!("Binary '{}' not found in tarball", binary_name))?;

    // Copy to a persistent temp location
    let dest = env::temp_dir().join(format!("flow_upgrade_{}", binary_name));
    fs::copy(&binary_path, &dest).context("Failed to copy binary")?;

    Ok(dest)
}

/// Validate the new binary by running --version.
fn validate_binary(path: &Path) -> Result<String> {
    let output = Command::new(path)
        .arg("--version")
        .output()
        .context("Failed to validate new binary")?;

    if !output.status.success() {
        bail!("New binary validation failed");
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Replace the current executable with the new one.
fn replace_executable(new_exe: &Path, current_exe: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        // On Unix, we can delete the running executable and replace it
        fs::remove_file(current_exe).context("Failed to remove current executable")?;
        fs::copy(new_exe, current_exe).context("Failed to copy new executable")?;

        // Set executable permissions
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(current_exe)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(current_exe, perms)?;
    }

    #[cfg(windows)]
    {
        // On Windows, rename the old executable first
        let old_exe = current_exe.with_extension("old.exe");
        if old_exe.exists() {
            fs::remove_file(&old_exe).ok();
        }
        fs::rename(current_exe, &old_exe).context("Failed to rename current executable")?;
        fs::copy(new_exe, current_exe).context("Failed to copy new executable")?;
    }

    Ok(())
}

/// Get the path to the current executable.
fn current_exe_path() -> Result<PathBuf> {
    env::current_exe().context("Failed to get current executable path")
}

/// Check write permissions for the executable path.
fn check_write_permission(path: &Path) -> Result<()> {
    let parent = path.parent().unwrap_or(path);

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let metadata = fs::metadata(path).or_else(|_| fs::metadata(parent))?;
        let uid = unsafe { libc::getuid() };

        if metadata.uid() == 0 && uid != 0 {
            bail!(
                "You don't have write permission to {} because it's owned by root.\n\
                 Consider updating flow through your package manager if installed from it.\n\
                 Otherwise run `f upgrade` as root.",
                path.display()
            );
        }
    }

    // Try to check if we can write
    if path.exists() {
        let metadata = fs::metadata(path)?;
        if metadata.permissions().readonly() {
            bail!("You do not have write permission to {}", path.display());
        }
    } else if !parent.exists() || fs::metadata(parent)?.permissions().readonly() {
        bail!("You do not have write permission to {}", parent.display());
    }

    Ok(())
}

/// Run the upgrade command.
pub fn run(opts: UpgradeOpts) -> Result<()> {
    let current = current_version();
    let current_exe = current_exe_path()?;

    println!("Current version: {}", current);

    // Check write permissions early
    let output_path = opts
        .output
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| current_exe.clone());
    check_write_permission(&output_path)?;

    let client = Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .context("Failed to create HTTP client")?;

    // Fetch latest release
    println!("Checking for updates...");
    let release = fetch_latest_release(&client)?;
    let latest = parse_version(&release.tag_name);

    println!("Latest version: {}", latest);

    // Check if upgrade is needed
    if !opts.force && !is_newer_version(current, latest) {
        println!("Already on the latest version.");
        return Ok(());
    }

    if opts.force {
        println!("Forcing upgrade...");
    }

    // Detect platform and find the right asset
    let (os, arch) = detect_platform()?;
    let asset_name = format!("flow_{}_{}_{}.", release.tag_name, os, arch);

    let asset = release
        .assets
        .iter()
        .find(|a| a.name.starts_with(&asset_name) || a.name.contains(&format!("{}_{}", os, arch)))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "No release asset found for {}/{}. Available: {:?}",
                os,
                arch,
                release.assets.iter().map(|a| &a.name).collect::<Vec<_>>()
            )
        })?;

    println!("Downloading {}...", asset.name);

    // Dry run mode
    if opts.dry_run {
        println!("\n[dry-run] Would download: {}", asset.browser_download_url);
        println!("[dry-run] Would install to: {}", output_path.display());
        return Ok(());
    }

    // Download the release
    let temp_tarball = env::temp_dir().join("flow_upgrade.tar.gz");
    download_with_progress(&client, &asset.browser_download_url, &temp_tarball)?;

    // Extract and find the binary
    println!("Extracting...");
    let binary_name = if cfg!(windows) { "f.exe" } else { "f" };
    let new_exe = extract_binary(&temp_tarball, binary_name)?;

    // Validate the new binary
    println!("Validating...");
    let new_version = validate_binary(&new_exe)?;
    println!("New binary version: {}", new_version);

    // Replace the executable
    println!("Installing...");
    replace_executable(&new_exe, &output_path)?;

    // Cleanup
    fs::remove_file(&temp_tarball).ok();
    fs::remove_file(&new_exe).ok();

    // Update cache
    let cache = VersionCache {
        last_checked: VersionCache::now_timestamp(),
        latest_version: latest.to_string(),
        current_version: latest.to_string(),
    };
    cache.save().ok();

    println!();
    println!("Successfully upgraded to flow {}", latest);
    println!();
    println!("Release notes: {}", release.html_url);

    Ok(())
}

/// Check for upgrades in the background (non-blocking).
/// Returns Some((latest_version)) if an upgrade is available.
pub fn check_for_upgrade_prompt() -> Option<String> {
    // Check if disabled via environment variable
    if env::var("FLOW_NO_UPDATE_CHECK").is_ok() {
        return None;
    }

    // Check cache first
    let current = current_version();

    if let Some(cache) = VersionCache::load() {
        // If current version changed, user already upgraded
        if cache.current_version != current {
            return None;
        }

        // If we've checked recently, use cached result
        if !cache.should_check() {
            if is_newer_version(current, &cache.latest_version) {
                return Some(cache.latest_version);
            }
            return None;
        }
    }

    // Perform check (with short timeout for background use)
    let client = Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .ok()?;

    let release = fetch_latest_release(&client).ok()?;
    let latest = parse_version(&release.tag_name).to_string();

    // Update cache
    let cache = VersionCache {
        last_checked: VersionCache::now_timestamp(),
        latest_version: latest.clone(),
        current_version: current.to_string(),
    };
    cache.save().ok();

    if is_newer_version(current, &latest) {
        Some(latest)
    } else {
        None
    }
}

/// Print upgrade prompt if a new version is available.
/// Call this at the end of command execution.
pub fn maybe_print_upgrade_prompt() {
    // Only show on TTY
    if !atty::is(atty::Stream::Stderr) {
        return;
    }

    if let Some(latest) = check_for_upgrade_prompt() {
        eprintln!();
        eprintln!(
            "A new version of flow is available: {} -> {}",
            current_version(),
            latest
        );
        eprintln!("Run `f upgrade` to install it.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_newer_version() {
        assert!(is_newer_version("0.1.0", "0.2.0"));
        assert!(is_newer_version("0.1.0", "1.0.0"));
        assert!(is_newer_version("1.0.0", "1.0.1"));
        assert!(is_newer_version("1.0.0", "1.1.0"));
        assert!(!is_newer_version("0.2.0", "0.1.0"));
        assert!(!is_newer_version("1.0.0", "1.0.0"));
        assert!(is_newer_version("v0.1.0", "v0.2.0"));
    }

    #[test]
    fn test_parse_version() {
        assert_eq!(parse_version("v1.0.0"), "1.0.0");
        assert_eq!(parse_version("1.0.0"), "1.0.0");
    }
}
