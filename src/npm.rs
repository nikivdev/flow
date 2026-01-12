//! Publish binaries to npm registry.
//!
//! Creates a single npm package that bundles vendor binaries.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::cli::NpmCommand;

/// Platform target for npm packages
#[derive(Debug, Clone)]
pub struct Platform {
    pub name: &'static str,
    pub os: &'static str,
    pub cpu: &'static str,
    pub rust_target: &'static str,
}

/// All supported platforms
pub const PLATFORMS: &[Platform] = &[
    Platform {
        name: "darwin-arm64",
        os: "darwin",
        cpu: "arm64",
        rust_target: "aarch64-apple-darwin",
    },
    Platform {
        name: "darwin-x64",
        os: "darwin",
        cpu: "x64",
        rust_target: "x86_64-apple-darwin",
    },
    Platform {
        name: "linux-arm64",
        os: "linux",
        cpu: "arm64",
        rust_target: "aarch64-unknown-linux-gnu",
    },
    Platform {
        name: "linux-x64",
        os: "linux",
        cpu: "x64",
        rust_target: "x86_64-unknown-linux-gnu",
    },
];

#[derive(Debug, Serialize, Deserialize)]
struct PackageJson {
    name: String,
    version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    license: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    homepage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository: Option<Repository>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bin: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scripts: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    files: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    engines: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    keywords: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Repository {
    #[serde(rename = "type")]
    repo_type: String,
    url: String,
}

#[derive(Debug, Clone)]
struct ProjectMeta {
    name: String,
    version: String,
    bin_names: Vec<String>,
    description: Option<String>,
    license: Option<String>,
    repository: Option<String>,
    homepage: Option<String>,
}

#[derive(Debug, Clone)]
struct BinEntry {
    name: String,
    target: String,
}

/// Run the npm command.
pub fn run(cmd: NpmCommand) -> Result<()> {
    match cmd.action {
        Some(crate::cli::NpmAction::Publish(opts)) => publish(opts),
        Some(crate::cli::NpmAction::Init(opts)) => init(opts),
        None => {
            println!("Usage: f publish npm <init|publish>");
            println!();
            println!("Commands:");
            println!("  init     Initialize npm package structure");
            println!("  publish  Build and publish to npm");
            Ok(())
        }
    }
}

/// Initialize npm package structure for a project.
pub(crate) fn init(opts: crate::cli::NpmInitOpts) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let project_root = opts.path.map(PathBuf::from).unwrap_or(cwd);

    // Detect project type and get metadata
    let meta = detect_project(&project_root)?;
    let detected_name = project_root
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.to_string())
        .unwrap_or_else(|| strip_scope(&meta.name));
    let pkg_name = opts.name.unwrap_or(detected_name);

    let scope = opts.scope.map(|value| {
        if value.starts_with('@') {
            value
        } else {
            format!("@{}", value)
        }
    });
    let full_name = if let Some(scope) = scope.as_deref() {
        format!("{}/{}", scope, pkg_name)
    } else {
        pkg_name.clone()
    };

    println!("Initializing npm packages for {}...", full_name);

    let npm_dir = project_root.join("npm");
    fs::create_dir_all(&npm_dir)?;

    let bin_entries = build_bin_entries(&meta);

    // Create main package
    create_main_package(&npm_dir, &project_root, &full_name, &meta, &bin_entries)?;
    create_bin_wrappers(&npm_dir.join(&pkg_name), &bin_entries)?;
    copy_optional_docs(&project_root, &npm_dir.join(&pkg_name))?;

    println!("✓ Created npm package structure in {}/", npm_dir.display());
    println!();
    println!("Next steps:");
    println!("  1. Build binaries for all platforms");
    println!("  2. Copy binaries to npm/<package>/vendor/<target>/flow/");
    println!("  3. Run: f publish npm publish");

    Ok(())
}

/// Build and publish to npm.
pub(crate) fn publish(opts: crate::cli::NpmPublishOpts) -> Result<()> {
    publish_with_name(opts, None)
}

/// Build and publish to npm, optionally overriding the package name.
pub(crate) fn publish_with_name(
    opts: crate::cli::NpmPublishOpts,
    package_name: Option<String>,
) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let project_root = opts.path.clone().map(PathBuf::from).unwrap_or(cwd);

    // Check npm is available
    if Command::new("npm").arg("--version").output().is_err() {
        bail!("npm is not installed");
    }

    let npm_dir = project_root.join("npm");
    if !npm_dir.exists() {
        bail!("npm directory not found. Run 'f publish npm init' first.");
    }

    // Get version
    let version = if let Some(ref v) = opts.version {
        v.clone()
    } else {
        detect_version(&project_root)?
    };

    println!("Publishing version {}...", version);

    // Build if requested
    if opts.build {
        build_all_platforms(&project_root, &opts)?;
    }

    // Update version (and name when provided)
    update_package_metadata(&npm_dir, &version, package_name.as_deref())?;

    let pkg_dir = find_main_package_dir(&npm_dir)?;
    copy_optional_docs(&project_root, &pkg_dir)?;
    println!("Publishing {}...", pkg_dir.display());
    if !opts.dry_run {
        let access = opts.access.as_deref().unwrap_or("public");
        let tag = resolve_publish_tag(&version, opts.tag.as_deref());
        let mut command = Command::new("npm");
        command
            .args(["publish", "--access", access, "--tag", &tag])
            .current_dir(&pkg_dir);

        let _npmrc = inject_npm_token(&mut command)?;
        let status = command.status().context("failed to run npm publish")?;

        if !status.success() {
            bail!("Failed to publish npm package");
        }
    } else {
        println!("  (dry run)");
    }

    println!();
    if opts.dry_run {
        println!("Dry run complete. Would publish package.");
    } else {
        println!("✓ Published package to npm");
    }

    Ok(())
}

fn resolve_publish_tag(version: &str, tag: Option<&str>) -> String {
    if let Some(tag) = tag {
        return tag.to_string();
    }
    if version.contains('-') {
        return "latest".to_string();
    }
    "latest".to_string()
}

fn inject_npm_token(command: &mut Command) -> Result<Option<tempfile::NamedTempFile>> {
    let token = match std::env::var("NODE_AUTH_TOKEN") {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    if token.trim().is_empty() {
        return Ok(None);
    }

    let mut npmrc = tempfile::NamedTempFile::new().context("failed to create temp npmrc")?;
    writeln!(npmrc, "//registry.npmjs.org/:_authToken={}", token)
        .context("failed to write npmrc")?;
    command.env("NPM_CONFIG_USERCONFIG", npmrc.path());
    Ok(Some(npmrc))
}

fn detect_project(root: &Path) -> Result<ProjectMeta> {
    // Try Cargo.toml (Rust)
    let cargo_toml = root.join("Cargo.toml");
    if cargo_toml.exists() {
        let content = fs::read_to_string(&cargo_toml)?;
        let toml: toml::Value = toml::from_str(&content)?;
        let package = toml.get("package");

        let name = package
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("unknown")
            .to_string();

        let version = package
            .and_then(|p| p.get("version"))
            .and_then(|v| v.as_str())
            .unwrap_or("0.1.0")
            .to_string();

        let description = package
            .and_then(|p| p.get("description"))
            .and_then(|v| v.as_str())
            .map(|v| v.to_string());
        let license = package
            .and_then(|p| p.get("license"))
            .and_then(|v| v.as_str())
            .map(|v| v.to_string());
        let repository = package
            .and_then(|p| p.get("repository"))
            .and_then(|v| v.as_str())
            .map(|v| v.to_string());
        let homepage = package
            .and_then(|p| p.get("homepage"))
            .and_then(|v| v.as_str())
            .map(|v| v.to_string());

        let mut bin_names = Vec::new();
        if let Some(bins) = toml.get("bin").and_then(|b| b.as_array()) {
            for bin in bins {
                if let Some(bin_name) = bin.get("name").and_then(|n| n.as_str()) {
                    bin_names.push(bin_name.to_string());
                }
            }
        }
        if bin_names.is_empty() {
            bin_names.push(name.clone());
        }

        return Ok(ProjectMeta {
            name,
            version,
            bin_names,
            description,
            license,
            repository,
            homepage,
        });
    }

    // Try package.json (Node/Bun)
    let package_json = root.join("package.json");
    if package_json.exists() {
        let content = fs::read_to_string(&package_json)?;
        let json: serde_json::Value = serde_json::from_str(&content)?;

        let name = json
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("unknown")
            .to_string();

        let version = json
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("0.1.0")
            .to_string();

        let description = json
            .get("description")
            .and_then(|v| v.as_str())
            .map(|v| v.to_string());
        let license = json
            .get("license")
            .and_then(|v| v.as_str())
            .map(|v| v.to_string());
        let homepage = json
            .get("homepage")
            .and_then(|v| v.as_str())
            .map(|v| v.to_string());
        let repository = match json.get("repository") {
            Some(value) if value.is_string() => value.as_str().map(|v| v.to_string()),
            Some(value) if value.is_object() => value
                .get("url")
                .and_then(|v| v.as_str())
                .map(|v| v.to_string()),
            _ => None,
        };

        let mut bin_names = Vec::new();
        match json.get("bin") {
            Some(bin) if bin.is_string() => {
                bin_names.push(name.clone());
            }
            Some(bin) if bin.is_object() => {
                if let Some(obj) = bin.as_object() {
                    bin_names.extend(obj.keys().cloned());
                }
            }
            _ => {}
        }
        if bin_names.is_empty() {
            bin_names.push(strip_scope(&name));
        }

        return Ok(ProjectMeta {
            name,
            version,
            bin_names,
            description,
            license,
            repository,
            homepage,
        });
    }

    // Try go.mod (Go)
    let go_mod = root.join("go.mod");
    if go_mod.exists() {
        let content = fs::read_to_string(&go_mod)?;
        let name = content
            .lines()
            .find(|l| l.starts_with("module "))
            .map(|l| l.trim_start_matches("module ").trim())
            .and_then(|m| m.rsplit('/').next())
            .unwrap_or("unknown")
            .to_string();

        return Ok(ProjectMeta {
            name: name.clone(),
            version: "0.1.0".to_string(),
            bin_names: vec![name],
            description: None,
            license: None,
            repository: None,
            homepage: None,
        });
    }

    bail!("Could not detect project type. Supported: Rust (Cargo.toml), Node (package.json), Go (go.mod)");
}

fn detect_version(root: &Path) -> Result<String> {
    Ok(detect_project(root)?.version)
}

fn create_main_package(
    npm_dir: &Path,
    project_root: &Path,
    full_name: &str,
    meta: &ProjectMeta,
    bin_entries: &[BinEntry],
) -> Result<()> {
    let pkg_name = full_name.split('/').last().unwrap_or(full_name);
    let pkg_dir = npm_dir.join(pkg_name);
    fs::create_dir_all(&pkg_dir)?;

    let mut bin = HashMap::new();
    for entry in bin_entries {
        bin.insert(entry.name.to_string(), format!("bin/{}.js", entry.name));
    }

    let mut engines = HashMap::new();
    engines.insert("node".to_string(), ">=16".to_string());

    let repo_url = meta
        .repository
        .clone()
        .or_else(|| detect_repository_url(project_root));

    let pkg = PackageJson {
        name: full_name.to_string(),
        version: meta.version.to_string(),
        description: meta
            .description
            .clone()
            .or_else(|| Some(format!("{} CLI", pkg_name))),
        license: meta.license.clone().or_else(|| Some("MIT".to_string())),
        homepage: meta.homepage.clone(),
        repository: repo_url.map(|url| Repository {
            repo_type: "git".to_string(),
            url,
        }),
        bin: Some(bin),
        scripts: None,
        files: Some(vec![
            "bin".to_string(),
            "vendor".to_string(),
            "README.md".to_string(),
            "LICENSE".to_string(),
        ]),
        engines: Some(engines),
        keywords: Some(vec![
            "cli".to_string(),
            "workflow".to_string(),
            "developer-tools".to_string(),
        ]),
    };

    let json = serde_json::to_string_pretty(&pkg)?;
    fs::write(pkg_dir.join("package.json"), json)?;

    Ok(())
}

fn update_package_metadata(npm_dir: &Path, version: &str, name: Option<&str>) -> Result<()> {
    let pkg_dir = find_main_package_dir(npm_dir)?;
    let pkg_json = pkg_dir.join("package.json");
    let content = fs::read_to_string(&pkg_json)?;
    let mut pkg: serde_json::Value = serde_json::from_str(&content)?;

    if let Some(obj) = pkg.as_object_mut() {
        obj.insert("version".to_string(), serde_json::json!(version));
        if let Some(name) = name {
            obj.insert("name".to_string(), serde_json::json!(name));
        }
    }

    let updated = serde_json::to_string_pretty(&pkg)?;
    fs::write(&pkg_json, updated)?;
    Ok(())
}

fn build_all_platforms(root: &Path, opts: &crate::cli::NpmPublishOpts) -> Result<()> {
    let cargo_toml = root.join("Cargo.toml");
    if cargo_toml.exists() {
        return build_rust_platforms(root, opts);
    }

    let package_json = root.join("package.json");
    if package_json.exists() {
        return build_bun_platforms(root, opts);
    }

    bail!("Don't know how to build this project type");
}

fn build_rust_platforms(root: &Path, opts: &crate::cli::NpmPublishOpts) -> Result<()> {
    let npm_dir = root.join("npm");
    let meta = detect_project(root)?;
    let bin_entries = build_bin_entries(&meta);
    let bin_targets = unique_bin_targets(&bin_entries);
    let pkg_dir = find_main_package_dir(&npm_dir)?;

    let platforms = select_platforms(opts)?;
    for platform in platforms {
        println!("Building for {}...", platform.rust_target);

        let status = Command::new("cargo")
            .args(["build", "--release", "--target", platform.rust_target])
            .current_dir(root)
            .status()
            .context("failed to run cargo build")?;

        if !status.success() {
            println!("  Warning: build failed for {}", platform.rust_target);
            continue;
        }

        let vendor_dir = pkg_dir
            .join("vendor")
            .join(platform.rust_target)
            .join("flow");
        fs::create_dir_all(&vendor_dir)?;

        for bin_name in &bin_targets {
            let src = root
                .join("target")
                .join(platform.rust_target)
                .join("release")
                .join(bin_name);

            if !src.exists() {
                println!("  Warning: missing binary {}", src.display());
                continue;
            }

            let dst = vendor_dir.join(bin_name);
            fs::copy(&src, &dst)?;

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = fs::metadata(&dst)?.permissions();
                perms.set_mode(0o755);
                fs::set_permissions(&dst, perms)?;
            }

            println!("  ✓ Copied to {}", dst.display());
        }
    }

    Ok(())
}

fn select_platforms(opts: &crate::cli::NpmPublishOpts) -> Result<Vec<&'static Platform>> {
    if should_build_all(opts) {
        return Ok(PLATFORMS.iter().collect());
    }

    let (os, cpu) = current_platform();
    let matches: Vec<&Platform> = PLATFORMS
        .iter()
        .filter(|platform| platform.os == os && platform.cpu == cpu)
        .collect();
    if matches.is_empty() {
        bail!(
            "No npm build target for {}/{}. Use --all-targets to build all.",
            os,
            cpu
        );
    }
    Ok(matches)
}

fn should_build_all(opts: &crate::cli::NpmPublishOpts) -> bool {
    if opts.all_targets {
        return true;
    }
    match std::env::var("FLOW_NPM_BUILD_ALL") {
        Ok(value) => matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"),
        Err(_) => false,
    }
}

fn current_platform() -> (String, String) {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let cpu = match std::env::consts::ARCH {
        "aarch64" | "arm64" => "arm64",
        "x86_64" | "x64" => "x64",
        other => other,
    };
    (os.to_string(), cpu.to_string())
}

fn build_bun_platforms(root: &Path, _opts: &crate::cli::NpmPublishOpts) -> Result<()> {
    println!("Building with bun...");

    let status = Command::new("bun")
        .args(["run", "build"])
        .current_dir(root)
        .status()
        .context("failed to run bun build")?;

    if !status.success() {
        bail!("bun build failed");
    }

    println!("  ✓ Build complete");
    println!("  Note: Copy binaries from dist/ to npm/<package>/vendor/<target>/flow/ manually");

    Ok(())
}

fn strip_scope(name: &str) -> String {
    name.strip_prefix('@')
        .and_then(|value| value.split_once('/').map(|(_, name)| name))
        .unwrap_or(name)
        .to_string()
}

fn build_bin_entries(meta: &ProjectMeta) -> Vec<BinEntry> {
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    for name in &meta.bin_names {
        if seen.insert(name.to_string()) {
            entries.push(BinEntry {
                name: name.to_string(),
                target: name.to_string(),
            });
        }
    }

    let has_f = entries.iter().any(|entry| entry.name == "f");
    let has_flow = entries.iter().any(|entry| entry.name == "flow");
    if has_f && !has_flow {
        entries.push(BinEntry {
            name: "flow".to_string(),
            target: "f".to_string(),
        });
    }

    entries
}

fn detect_repository_url(project_root: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(project_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if raw.is_empty() {
        return None;
    }

    if raw.starts_with("git@") {
        let path = raw
            .split_once(':')
            .map(|(_, p)| p)
            .unwrap_or(raw.as_str());
        let https = format!("https://github.com/{}", path.trim_end_matches(".git"));
        return Some(https);
    }

    if raw.starts_with("https://") {
        return Some(raw.trim_end_matches(".git").to_string());
    }

    if raw.starts_with("ssh://") {
        let trimmed = raw.trim_start_matches("ssh://");
        if let Some((host, path)) = trimmed.split_once('/') {
            let host = host.trim_start_matches("git@");
            let https = format!(
                "https://{}/{}",
                host,
                path.trim_end_matches(".git")
            );
            return Some(https);
        }
    }

    None
}

fn unique_bin_targets(entries: &[BinEntry]) -> Vec<String> {
    let mut targets = Vec::new();
    let mut seen = HashSet::new();
    for entry in entries {
        if seen.insert(entry.target.clone()) {
            targets.push(entry.target.clone());
        }
    }
    targets
}

fn find_main_package_dir(npm_dir: &Path) -> Result<PathBuf> {
    if !npm_dir.exists() {
        bail!("npm directory not found: {}", npm_dir.display());
    }

    let mut candidates = Vec::new();
    for entry in fs::read_dir(npm_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.contains("-darwin-") || name.contains("-linux-") || name.contains("-win32-") {
            continue;
        }
        if path.join("package.json").exists() {
            candidates.push(path);
        }
    }

    match candidates.len() {
        0 => bail!("No npm package found in {}", npm_dir.display()),
        1 => Ok(candidates.remove(0)),
        _ => bail!("Multiple npm packages found in {}", npm_dir.display()),
    }
}

fn create_bin_wrappers(pkg_dir: &Path, bin_entries: &[BinEntry]) -> Result<()> {
    let bin_dir = pkg_dir.join("bin");
    fs::create_dir_all(&bin_dir)?;

    for entry in bin_entries {
        let script = bin_wrapper_script(&entry.target);
        let path = bin_dir.join(format!("{}.js", entry.name));
        fs::write(&path, script)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&path)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&path, perms)?;
        }
    }

    Ok(())
}

fn bin_wrapper_script(target_bin: &str) -> String {
    format!(
        r#"#!/usr/bin/env node
const {{ spawn }} = require("node:child_process");
const {{ existsSync }} = require("node:fs");
const path = require("node:path");

const BIN_NAME = "{target_bin}";

function resolveTarget() {{
  const {{ platform, arch }} = process;
  if (platform === "darwin" && arch === "arm64") return "aarch64-apple-darwin";
  if (platform === "darwin" && arch === "x64") return "x86_64-apple-darwin";
  if (platform === "linux" && arch === "arm64") return "aarch64-unknown-linux-gnu";
  if (platform === "linux" && arch === "x64") return "x86_64-unknown-linux-gnu";
  return null;
}}

function getUpdatedPath(newDirs) {{
  const pathSep = process.platform === "win32" ? ";" : ":";
  const existingPath = process.env.PATH || "";
  return [...newDirs, ...existingPath.split(pathSep).filter(Boolean)].join(pathSep);
}}

function detectPackageManager() {{
  const userAgent = process.env.npm_config_user_agent || "";
  if (/\\bbun\\//.test(userAgent)) return "bun";

  const execPath = process.env.npm_execpath || "";
  if (execPath.includes("bun")) return "bun";

  if (__dirname.includes(".bun/install/global") || __dirname.includes(".bun\\\\install\\\\global")) {{
    return "bun";
  }}

  return userAgent ? "npm" : null;
}}

const target = resolveTarget();
if (!target) {{
  console.error(`Unsupported platform: ${{process.platform}} (${{process.arch}})`);
  process.exit(1);
}}

const vendorRoot = path.join(__dirname, "..", "vendor");
const archRoot = path.join(vendorRoot, target);
const binDir = path.join(archRoot, "flow");
const binName = process.platform === "win32" ? `${{BIN_NAME}}.exe` : BIN_NAME;
const binaryPath = path.join(binDir, binName);

if (!existsSync(binaryPath)) {{
  console.error(`Missing binary: ${{binaryPath}}`);
  console.error("Try reinstalling the package or rebuilding the npm vendor artifacts.");
  process.exit(1);
}}

const extraDirs = [];
const pathDir = path.join(archRoot, "path");
if (existsSync(pathDir)) {{
  extraDirs.push(pathDir);
}}
extraDirs.push(binDir);

const env = {{ ...process.env, PATH: getUpdatedPath(extraDirs) }};
const manager = detectPackageManager();
if (manager === "bun") {{
  env.FLOW_MANAGED_BY_BUN = "1";
}} else if (manager) {{
  env.FLOW_MANAGED_BY_NPM = "1";
}}

const child = spawn(binaryPath, process.argv.slice(2), {{
  stdio: "inherit",
  env,
}});

child.on("error", (err) => {{
  console.error(err);
  process.exit(1);
}});

const forwardSignal = (signal) => {{
  if (child.killed) return;
  try {{
    child.kill(signal);
  }} catch {{}}
}};

["SIGINT", "SIGTERM", "SIGHUP"].forEach((sig) => {{
  process.on(sig, () => forwardSignal(sig));
}});

child.on("exit", (code, signal) => {{
  if (signal) {{
    process.kill(process.pid, signal);
  }} else {{
    process.exit(code ?? 1);
  }}
}});
"#,
        target_bin = target_bin
    )
}

fn copy_optional_docs(project_root: &Path, pkg_dir: &Path) -> Result<()> {
    let candidates = [
        ("README.md", "README.md"),
        ("readme.md", "README.md"),
        ("LICENSE", "LICENSE"),
        ("license", "LICENSE"),
    ];

    for (src_name, dst_name) in candidates {
        let src = project_root.join(src_name);
        if !src.exists() {
            continue;
        }
        let dst = pkg_dir.join(dst_name);
        if dst.exists() {
            continue;
        }
        fs::copy(&src, &dst)?;
    }
    Ok(())
}
