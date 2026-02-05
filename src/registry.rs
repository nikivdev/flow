use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{Datelike, Local, Utc};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::cli::{
    InstallOpts, RegistryAction, RegistryCommand, RegistryInitOpts, RegistryReleaseOpts,
};
use crate::config::{self, Config, RegistryReleaseConfig};
use crate::env as flow_env;

const DEFAULT_TOKEN_ENV: &str = "FLOW_REGISTRY_TOKEN";
const WORKER_TOKEN_SECRET: &str = "REGISTRY_TOKEN";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryManifest {
    pub name: String,
    pub version: String,
    pub published_at: String,
    #[serde(default)]
    pub bins: Vec<String>,
    #[serde(default)]
    pub default_bin: Option<String>,
    pub targets: BTreeMap<String, RegistryTarget>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryTarget {
    pub binaries: BTreeMap<String, String>,
    #[serde(default)]
    pub sha256: BTreeMap<String, String>,
}

pub fn run(cmd: RegistryCommand) -> Result<()> {
    match cmd.action {
        Some(RegistryAction::Init(opts)) => init(opts),
        None => {
            println!("Registry commands:");
            println!("  init  Create a registry token and configure worker secrets");
            Ok(())
        }
    }
}

pub fn init(opts: RegistryInitOpts) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let flow_path = find_flow_toml(&cwd);
    let (project_root, flow_cfg) = if let Some(flow_path) = flow_path.as_ref() {
        let cfg = config::load(flow_path)?;
        let root = flow_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| cwd.clone());
        (root, Some(cfg))
    } else {
        (cwd.clone(), None)
    };

    let registry_cfg = flow_cfg
        .as_ref()
        .and_then(|cfg| cfg.release.as_ref())
        .and_then(|release| release.registry.as_ref());

    let token_env = opts
        .token_env
        .clone()
        .or_else(|| registry_cfg.and_then(|cfg| cfg.token_env.clone()))
        .unwrap_or_else(|| DEFAULT_TOKEN_ENV.to_string());

    let registry_url = resolve_registry_url(opts.registry.as_deref(), registry_cfg).ok();

    let token = opts.token.unwrap_or_else(generate_registry_token);
    flow_env::set_personal_env_var(&token_env, &token)?;

    if opts.no_worker {
        println!("Skipped worker secret setup (--no-worker).");
    } else {
        let worker_path = resolve_worker_path(opts.worker.as_ref(), &project_root)?
            .context("worker path not found; pass --worker to set secrets")?;
        set_worker_secret(&worker_path, &token)?;
    }

    if let Some(registry_url) = registry_url {
        println!("Registry URL: {}", registry_url);
    }

    if opts.show_token {
        println!("Registry token: {}", token);
    } else {
        let preview = token.chars().take(6).collect::<String>();
        println!("Registry token: {}… (use --show-token to print)", preview);
    }

    println!("Ready to release with `f release`.");
    Ok(())
}

pub fn publish(config_path: &Path, cfg: &Config, opts: RegistryReleaseOpts) -> Result<()> {
    let project_root = config_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    let registry_cfg = cfg
        .release
        .as_ref()
        .and_then(|release| release.registry.as_ref());

    let registry_url = resolve_registry_url(opts.registry.as_deref(), registry_cfg)?;
    let package = resolve_package_name(opts.package.clone(), cfg, registry_cfg, &project_root)?;
    let bins = resolve_bins(&package, opts.bin.clone(), registry_cfg);
    let default_bin = resolve_default_bin(&package, &bins, registry_cfg);
    let version = resolve_registry_version(cfg, opts.version.clone(), &registry_url, &package)?;
    let latest = resolve_latest_flag(opts.latest, opts.no_latest, registry_cfg);

    if !opts.no_build {
        build_binaries(&project_root, &bins)?;
    }

    let target = detect_target_triple()?;
    let mut binaries = BTreeMap::new();
    let mut sha256_map = BTreeMap::new();
    for bin in &bins {
        let path = project_root.join("target").join("release").join(bin);
        if !path.exists() {
            bail!("binary not found: {}", path.display());
        }
        let sha = sha256_file(&path)?;
        let key = format!("packages/{}/{}/{}/{}", package, version, target, bin);
        binaries.insert(bin.clone(), key);
        sha256_map.insert(bin.clone(), sha);
    }

    let mut targets = BTreeMap::new();
    targets.insert(
        target.clone(),
        RegistryTarget {
            binaries,
            sha256: sha256_map,
        },
    );

    let manifest = RegistryManifest {
        name: package.clone(),
        version: version.clone(),
        published_at: Utc::now().to_rfc3339(),
        bins: bins.clone(),
        default_bin,
        targets,
    };

    if opts.dry_run {
        println!(
            "Dry run: would publish {} {} to {} (target {})",
            package, version, registry_url, target
        );
        return Ok(());
    }

    let token_env = registry_cfg
        .and_then(|cfg| cfg.token_env.as_ref())
        .map(|s| s.as_str())
        .unwrap_or(DEFAULT_TOKEN_ENV);
    let token = resolve_registry_token(token_env)?;
    let client = Client::builder().timeout(Duration::from_secs(60)).build()?;

    for bin in &bins {
        let path = project_root.join("target").join("release").join(bin);
        let key = format!("packages/{}/{}/{}/{}", package, version, target, bin);
        let url = format!("{}/{}", registry_url, key);
        let body = fs::read(&path)?;
        let sha = sha256_file(&path)?;
        let response = client
            .put(url)
            .header("Authorization", format!("Bearer {}", token))
            .header("X-Sha256", sha)
            .body(body)
            .send()
            .context("failed to upload binary")?;
        if !response.status().is_success() {
            bail!("registry upload failed for {} ({})", bin, response.status());
        }
    }

    let manifest_url = format!(
        "{}/packages/{}/{}/manifest.json",
        registry_url, package, version
    );
    let mut request = client
        .put(manifest_url)
        .header("Authorization", format!("Bearer {}", token))
        .body(serde_json::to_string_pretty(&manifest)?);
    if latest {
        request = request.query(&[("latest", "1")]);
    }
    let response = request.send().context("failed to upload manifest")?;
    if !response.status().is_success() {
        bail!("registry manifest upload failed ({})", response.status());
    }

    println!("Published {} {} to {}", package, version, registry_url);
    Ok(())
}

pub fn install(opts: InstallOpts) -> Result<()> {
    let name = opts.name.as_deref().unwrap_or("").trim().to_string();
    if name.is_empty() {
        bail!("package name is required for registry install");
    }
    let global_registry = load_global_registry_config();
    let registry_url = resolve_registry_url(opts.registry.as_deref(), global_registry.as_ref())?;
    let client = Client::builder().timeout(Duration::from_secs(60)).build()?;
    let version = opts.version.clone();
    let manifest = fetch_manifest(&client, &registry_url, &name, version.as_deref())?;
    let target = detect_target_triple()?;
    let target_entry = manifest
        .targets
        .get(&target)
        .with_context(|| format!("No binaries for target {}", target))?;
    let bin = resolve_install_bin(&name, &opts.bin, &manifest, target_entry)?;
    let path = target_entry
        .binaries
        .get(&bin)
        .with_context(|| format!("No binary '{}' in manifest", bin))?;
    let download_url = resolve_download_url(&registry_url, path);
    let response = client
        .get(download_url)
        .send()
        .context("failed to download binary")?;
    if !response.status().is_success() {
        bail!("download failed ({})", response.status());
    }
    let bytes = response.bytes().context("failed to read download")?;

    if !opts.no_verify {
        if let Some(expected) = target_entry.sha256.get(&bin) {
            let actual = sha256_bytes(&bytes);
            if expected != &actual {
                bail!("checksum mismatch for {}", bin);
            }
        }
    }

    let bin_dir = opts.bin_dir.clone().unwrap_or_else(default_bin_dir);
    fs::create_dir_all(&bin_dir)
        .with_context(|| format!("failed to create {}", bin_dir.display()))?;
    let dest = bin_dir.join(&bin);
    if dest.exists() && !opts.force {
        bail!(
            "{} already exists (use --force to overwrite)",
            dest.display()
        );
    }

    let mut temp = NamedTempFile::new_in(&bin_dir)
        .with_context(|| format!("failed to create temp file in {}", bin_dir.display()))?;
    temp.write_all(&bytes)?;
    temp.flush()?;
    persist_with_permissions(temp, &dest)?;

    println!("Installed {} to {}", bin, dest.display());
    if !path_in_env(&bin_dir) {
        println!("Add {} to PATH to use it everywhere.", bin_dir.display());
    }
    Ok(())
}

fn resolve_registry_url(
    override_url: Option<&str>,
    cfg: Option<&RegistryReleaseConfig>,
) -> Result<String> {
    let url = override_url
        .map(|s| s.to_string())
        .or_else(|| cfg.and_then(|cfg| cfg.url.clone()))
        .or_else(|| env::var("FLOW_REGISTRY_URL").ok())
        .ok_or_else(|| {
            anyhow::anyhow!("Registry URL not set. Use --registry or set FLOW_REGISTRY_URL.")
        })?;
    Ok(url.trim_end_matches('/').to_string())
}

fn load_global_registry_config() -> Option<RegistryReleaseConfig> {
    let path = config::default_config_path();
    if !path.exists() {
        return None;
    }
    let cfg = config::load(&path).ok()?;
    cfg.release.and_then(|release| release.registry)
}

fn resolve_package_name(
    override_package: Option<String>,
    cfg: &Config,
    registry_cfg: Option<&RegistryReleaseConfig>,
    project_root: &Path,
) -> Result<String> {
    if let Some(value) = override_package {
        return Ok(value);
    }
    if let Some(cfg) = registry_cfg.and_then(|cfg| cfg.package.clone()) {
        return Ok(cfg);
    }
    if let Some(name) = cfg.project_name.clone() {
        return Ok(name);
    }
    let fallback = project_root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("package");
    Ok(fallback.to_string())
}

fn resolve_bins(
    package: &str,
    override_bins: Vec<String>,
    registry_cfg: Option<&RegistryReleaseConfig>,
) -> Vec<String> {
    if !override_bins.is_empty() {
        return override_bins;
    }
    if let Some(bins) = registry_cfg.and_then(|cfg| cfg.bins.clone()) {
        return bins;
    }
    vec![package.to_string()]
}

fn resolve_default_bin(
    package: &str,
    bins: &[String],
    registry_cfg: Option<&RegistryReleaseConfig>,
) -> Option<String> {
    if let Some(default_bin) = registry_cfg.and_then(|cfg| cfg.default_bin.clone()) {
        return Some(default_bin);
    }
    if bins.iter().any(|bin| bin == package) {
        return Some(package.to_string());
    }
    bins.first().cloned()
}

fn resolve_latest_flag(
    latest: bool,
    no_latest: bool,
    registry_cfg: Option<&RegistryReleaseConfig>,
) -> bool {
    if latest {
        return true;
    }
    if no_latest {
        return false;
    }
    registry_cfg.and_then(|cfg| cfg.latest).unwrap_or(true)
}

fn resolve_registry_version(
    cfg: &Config,
    version: Option<String>,
    registry_url: &str,
    package: &str,
) -> Result<String> {
    if let Some(version) = version {
        return Ok(version);
    }
    let versioning = cfg
        .release
        .as_ref()
        .and_then(|release| release.versioning.as_deref());
    match versioning {
        Some("calver") | Some("calendar") | Some("date") => {
            Ok(calver_version(cfg, registry_url, package))
        }
        _ => bail!("Version not provided. Pass --version or set release.versioning."),
    }
}

fn calver_version(cfg: &Config, registry_url: &str, package: &str) -> String {
    let now = Local::now();
    let mut base = format!("{}.{}.{}", now.year(), now.month(), now.day());
    let suffix = cfg
        .release
        .as_ref()
        .and_then(|release| release.calver_suffix.clone())
        .or_else(|| env::var("FLOW_CALVER_SUFFIX").ok());
    if let Some(suffix) = suffix {
        let trimmed = suffix.trim();
        if !trimmed.is_empty() {
            base = format!("{}-{}", base, trimmed);
        }
        return base;
    }

    if let Ok(versions) = fetch_registry_versions(registry_url, package) {
        let mut max_suffix: Option<u64> = None;
        for version in versions {
            if version == base {
                max_suffix = Some(max_suffix.unwrap_or(0).max(0));
                continue;
            }
            if let Some(rest) = version.strip_prefix(&format!("{}-", base)) {
                if let Ok(num) = rest.parse::<u64>() {
                    max_suffix = Some(max_suffix.unwrap_or(0).max(num));
                }
            }
        }
        if let Some(value) = max_suffix {
            return format!("{}-{}", base, value + 1);
        }
    }
    base
}

fn fetch_registry_versions(registry_url: &str, package: &str) -> Result<Vec<String>> {
    let client = Client::builder().timeout(Duration::from_secs(10)).build()?;
    let url = format!("{}/packages/{}/versions.json", registry_url, package);
    let resp = client.get(url).send()?;
    if resp.status().as_u16() == 404 {
        return Ok(Vec::new());
    }
    if !resp.status().is_success() {
        bail!("registry returned {}", resp.status());
    }
    #[derive(Deserialize)]
    struct VersionsResponse {
        versions: Vec<String>,
    }
    let parsed: VersionsResponse = resp.json()?;
    Ok(parsed.versions)
}

fn fetch_manifest(
    client: &Client,
    registry_url: &str,
    name: &str,
    version: Option<&str>,
) -> Result<RegistryManifest> {
    let url = match version {
        Some(version) => format!(
            "{}/packages/{}/{}/manifest.json",
            registry_url, name, version
        ),
        None => format!("{}/packages/{}/latest.json", registry_url, name),
    };
    let resp = client.get(url).send()?;
    if resp.status().as_u16() == 404 {
        bail!("Package '{}' not found in registry", name);
    }
    if !resp.status().is_success() {
        bail!("Registry returned {}", resp.status());
    }
    Ok(resp.json()?)
}

fn resolve_install_bin(
    package: &str,
    override_bin: &Option<String>,
    manifest: &RegistryManifest,
    target: &RegistryTarget,
) -> Result<String> {
    if let Some(bin) = override_bin {
        return Ok(bin.to_string());
    }
    if let Some(bin) = manifest.default_bin.as_ref() {
        return Ok(bin.clone());
    }
    if manifest.bins.len() == 1 {
        return Ok(manifest.bins[0].clone());
    }
    if target.binaries.contains_key(package) {
        return Ok(package.to_string());
    }
    if let Some(first) = target.binaries.keys().next() {
        return Ok(first.clone());
    }
    bail!("No binaries available for this target");
}

fn resolve_download_url(base: &str, path: &str) -> String {
    if path.starts_with("http://") || path.starts_with("https://") {
        return path.to_string();
    }
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

fn resolve_registry_token(token_env: &str) -> Result<String> {
    if let Ok(token) = env::var(token_env) {
        if !token.trim().is_empty() {
            return Ok(token);
        }
    }
    let vars = flow_env::fetch_personal_env_vars(&[token_env.to_string()])?;
    if let Some(token) = vars.get(token_env) {
        return Ok(token.clone());
    }
    bail!(
        "{} not set. Add it with `f env new` or export it in your shell.",
        token_env
    );
}

fn generate_registry_token() -> String {
    let a = uuid::Uuid::new_v4().simple().to_string();
    let b = uuid::Uuid::new_v4().simple().to_string();
    format!("flow_{}{}", a, b)
}

fn resolve_worker_path(explicit: Option<&PathBuf>, project_root: &Path) -> Result<Option<PathBuf>> {
    if let Some(path) = explicit {
        return Ok(Some(path.clone()));
    }

    let candidates = [
        project_root.join("packages").join("worker"),
        project_root.join("worker"),
        project_root.to_path_buf(),
    ];

    for candidate in candidates {
        if has_wrangler_config(&candidate) {
            return Ok(Some(candidate));
        }
    }

    Ok(None)
}

fn has_wrangler_config(path: &Path) -> bool {
    ["wrangler.toml", "wrangler.json", "wrangler.jsonc"]
        .iter()
        .any(|name| path.join(name).exists())
}

fn set_worker_secret(worker_path: &Path, token: &str) -> Result<()> {
    let mut child = Command::new("wrangler")
        .arg("secret")
        .arg("put")
        .arg(WORKER_TOKEN_SECRET)
        .current_dir(worker_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .context("failed to run wrangler secret put")?;

    {
        let stdin = child
            .stdin
            .as_mut()
            .context("failed to open wrangler stdin")?;
        stdin.write_all(token.as_bytes())?;
        stdin.write_all(b"\n")?;
    }

    let status = child.wait()?;
    if !status.success() {
        bail!("wrangler secret put failed");
    }

    println!(
        "✓ Set {} in worker config ({})",
        WORKER_TOKEN_SECRET,
        worker_path.display()
    );
    Ok(())
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

fn build_binaries(project_root: &Path, bins: &[String]) -> Result<()> {
    let mut command = Command::new("cargo");
    command.arg("build").arg("--release");
    for bin in bins {
        command.arg("--bin").arg(bin);
    }
    let status = command
        .current_dir(project_root)
        .status()
        .context("failed to run cargo build")?;
    if !status.success() {
        bail!("cargo build failed");
    }
    Ok(())
}

fn detect_target_triple() -> Result<String> {
    let os = if cfg!(target_os = "macos") {
        "apple-darwin"
    } else if cfg!(target_os = "linux") {
        "unknown-linux-gnu"
    } else if cfg!(target_os = "windows") {
        "pc-windows-msvc"
    } else {
        bail!("Unsupported operating system");
    };

    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else {
        bail!("Unsupported architecture");
    };

    Ok(format!("{}-{}", arch, os))
}

fn sha256_file(path: &Path) -> Result<String> {
    let data = fs::read(path)?;
    Ok(sha256_bytes(&data))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    hex::encode(digest)
}

fn default_bin_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("bin"))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn persist_with_permissions(temp: NamedTempFile, dest: &Path) -> Result<()> {
    temp.persist(dest)
        .map_err(|err| err.error)
        .context("failed to persist binary")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(dest)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(dest, perms)?;
    }
    Ok(())
}

fn path_in_env(bin_dir: &Path) -> bool {
    let path = env::var_os("PATH").unwrap_or_default();
    env::split_paths(&path).any(|entry| entry == bin_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_relative_download_url() {
        let url = resolve_download_url("https://example.com", "packages/foo/bin");
        assert_eq!(url, "https://example.com/packages/foo/bin");
    }

    #[test]
    fn resolves_default_bin_from_manifest() {
        let mut binaries = BTreeMap::new();
        binaries.insert("flow".to_string(), "path".to_string());
        let target = RegistryTarget {
            binaries,
            sha256: BTreeMap::new(),
        };
        let manifest = RegistryManifest {
            name: "flow".to_string(),
            version: "1.0.0".to_string(),
            published_at: "now".to_string(),
            bins: vec!["flow".to_string()],
            default_bin: Some("flow".to_string()),
            targets: BTreeMap::new(),
        };
        let bin = resolve_install_bin("flow", &None, &manifest, &target).unwrap();
        assert_eq!(bin, "flow");
    }
}
