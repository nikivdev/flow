use anyhow::{Result, bail};
use chrono::{Datelike, Local};
use reqwest::blocking::Client;
use serde_json::Value;

use crate::{
    cli::{GhReleaseCommand, NpmReleaseOpts, ReleaseAction, ReleaseCommand, ReleaseOpts},
    config::{Config, NpmReleaseConfig},
    env,
    gh_release,
    registry,
    npm,
    tasks::{self, find_task},
};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn available_tasks(cfg: &crate::config::Config) -> String {
    let mut names: Vec<_> = cfg.tasks.iter().map(|task| task.name.clone()).collect();
    names.sort();
    names.join(", ")
}

fn resolve_release_task(cfg: &crate::config::Config) -> Result<String> {
    if let Some(name) = cfg.flow.release_task.as_deref() {
        if find_task(cfg, name).is_some() {
            return Ok(name.to_string());
        }
        bail!(
            "release_task '{}' not found. Available tasks: {}",
            name,
            available_tasks(cfg)
        );
    }

    for fallback in ["release", "release-build"] {
        if find_task(cfg, fallback).is_some() {
            return Ok(fallback.to_string());
        }
    }

    if let Some(name) = cfg.flow.primary_task.as_deref() {
        if find_task(cfg, name).is_some() {
            return Ok(name.to_string());
        }
    }

    bail!(
        "no release task found. Configure flow.release_task or add a 'release' task. Available tasks: {}",
        available_tasks(cfg)
    );
}

pub fn run(cmd: ReleaseCommand) -> Result<()> {
    if let Some(ReleaseAction::Github(cmd)) = cmd.action.clone() {
        return gh_release::run(cmd);
    }

    let (config_path, cfg) = tasks::load_project_config(cmd.config.clone())?;

    match cmd.action {
        Some(ReleaseAction::Github(cmd)) => gh_release::run(cmd),
        Some(ReleaseAction::Npm(opts)) => run_npm_release(&config_path, &cfg, opts),
        Some(ReleaseAction::Registry(opts)) => {
            registry::publish(&config_path, &cfg, opts)
        }
        Some(ReleaseAction::Task(opts)) => run_task(ReleaseOpts {
            config: config_path,
            args: opts.args,
        }),
        None => run_default(&config_path, &cfg),
    }
}

pub fn run_task(opts: ReleaseOpts) -> Result<()> {
    let (config_path, cfg) = tasks::load_project_config(opts.config)?;
    let task_name = resolve_release_task(&cfg)?;

    tasks::run(crate::cli::TaskRunOpts {
        config: config_path,
        delegate_to_hub: false,
        hub_host: std::net::IpAddr::from([127, 0, 0, 1]),
        hub_port: 9050,
        name: task_name,
        args: opts.args,
    })
}

fn run_default(config_path: &Path, cfg: &Config) -> Result<()> {
    let npm_configured = cfg
        .release
        .as_ref()
        .and_then(|release| release.npm.as_ref())
        .is_some();
    let provider = cfg
        .release
        .as_ref()
        .and_then(|release| release.default.as_deref())
        .or_else(|| if npm_configured { Some("npm") } else { None })
        .unwrap_or("task");

    match provider {
        "npm" => run_npm_release(config_path, cfg, NpmReleaseOpts::default()),
        "registry" | "myflow" => registry::publish(
            config_path,
            cfg,
            crate::cli::RegistryReleaseOpts::default(),
        ),
        "task" | "release" => run_task(ReleaseOpts {
            config: config_path.to_path_buf(),
            args: Vec::new(),
        }),
        "github" | "gh" => gh_release::run(GhReleaseCommand { action: None }),
        other => bail!(
            "Unknown release provider '{}'. Expected npm, task, or github.",
            other
        ),
    }
}

fn run_npm_release(config_path: &Path, cfg: &Config, opts: NpmReleaseOpts) -> Result<()> {
    let project_root = config_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    let npm_config = cfg
        .release
        .as_ref()
        .and_then(|release| release.npm.as_ref());
    let (scope, package) = resolve_npm_package(cfg, npm_config, &project_root);
    let full_name = match (scope.as_deref(), package.as_deref()) {
        (Some(scope), Some(package)) => Some(format!("{}/{}", scope, package)),
        (None, Some(package)) => Some(package.to_string()),
        _ => None,
    };

    let npm_dir = project_root.join("npm");
    if !npm_dir.exists() {
        println!("npm/ not found; initializing npm package layout...");
        npm::init(crate::cli::NpmInitOpts {
            path: Some(project_root.to_string_lossy().to_string()),
            scope,
            name: package,
        })?;
    }

    ensure_npm_token()?;

    let resolved_version = resolve_release_version(cfg, opts.version.clone(), full_name.as_deref());

    npm::publish_with_name(
        crate::cli::NpmPublishOpts {
            path: Some(project_root.to_string_lossy().to_string()),
            version: resolved_version,
            access: npm_config.and_then(|cfg| cfg.access.clone()),
            tag: opts
                .tag
                .clone()
                .or_else(|| npm_config.and_then(|cfg| cfg.tag.clone())),
            build: !opts.no_build,
            all_targets: opts.all_targets,
            dry_run: opts.dry_run,
        },
        full_name,
    )
}

fn resolve_release_version(
    cfg: &Config,
    version: Option<String>,
    package_name: Option<&str>,
) -> Option<String> {
    if version.is_some() {
        return version;
    }
    let versioning = cfg
        .release
        .as_ref()
        .and_then(|release| release.versioning.as_deref());
    match versioning {
        Some("calver") | Some("calendar") | Some("date") => {
            Some(calver_version(cfg, package_name))
        }
        _ => None,
    }
}

fn calver_version(cfg: &Config, package_name: Option<&str>) -> String {
    let now = Local::now();
    let mut version = format!("{}.{}.{}", now.year(), now.month(), now.day());
    let suffix = cfg
        .release
        .as_ref()
        .and_then(|release| release.calver_suffix.clone())
        .or_else(|| std::env::var("FLOW_CALVER_SUFFIX").ok());
    if let Some(suffix) = suffix {
        let trimmed = suffix.trim();
        if !trimmed.is_empty() {
            version = format!("{}-{}", version, trimmed);
        }
        return version;
    }
    if let Some(pkg) = package_name {
        match next_calver_suffix(pkg, &version) {
            Ok(Some(next)) => return format!("{}-{}", version, next),
            Ok(None) => {}
            Err(err) => {
                println!("WARN failed to check npm for existing versions: {}", err);
            }
        }
    }
    version
}

fn next_calver_suffix(package_name: &str, base: &str) -> Result<Option<u64>> {
    let versions = fetch_npm_versions(package_name)?;
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
    Ok(max_suffix.map(|value| value + 1))
}

fn fetch_npm_versions(package_name: &str) -> Result<Vec<String>> {
    let encoded = encode_npm_package(package_name);
    let url = format!("https://registry.npmjs.org/{}", encoded);
    let client = Client::builder().timeout(Duration::from_secs(10)).build()?;
    let resp = client.get(url).send()?;
    if resp.status().as_u16() == 404 {
        return Ok(Vec::new());
    }
    if !resp.status().is_success() {
        bail!("npm registry returned {}", resp.status());
    }
    let json: Value = resp.json()?;
    let versions = json
        .get("versions")
        .and_then(|value| value.as_object())
        .map(|map| map.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    let mut unique = HashSet::new();
    let mut out = Vec::new();
    for version in versions {
        if unique.insert(version.clone()) {
            out.push(version);
        }
    }
    Ok(out)
}

fn encode_npm_package(package_name: &str) -> String {
    package_name.replace('@', "%40").replace('/', "%2f")
}

fn resolve_npm_package(
    cfg: &Config,
    npm_cfg: Option<&NpmReleaseConfig>,
    project_root: &Path,
) -> (Option<String>, Option<String>) {
    let mut scope = npm_cfg.and_then(|cfg| cfg.scope.clone());
    let mut package = npm_cfg.and_then(|cfg| cfg.package.clone());

    if let Some(pkg) = package.clone() {
        if let Some((pkg_scope, name)) = pkg.split_once('/') {
            if pkg_scope.starts_with('@') {
                scope = Some(pkg_scope.to_string());
                package = Some(name.to_string());
            }
        }
    }

    if scope.is_none() || package.is_none() {
        if let Some((owner, repo)) = infer_repo_owner_repo(project_root) {
            if scope.is_none() {
                scope = Some(format!("@{}", owner));
            }
            if package.is_none() {
                package = Some(repo);
            }
        }
    }

    if package.is_none() {
        package = cfg
            .project_name
            .clone()
            .or_else(|| project_root.file_name().and_then(|n| n.to_str()).map(|n| n.to_string()));
    }

    (scope, package)
}

fn infer_repo_owner_repo(project_root: &Path) -> Option<(String, String)> {
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

    let path = if raw.starts_with("git@") {
        raw.split_once(':')
            .map(|(_, p)| p)
            .unwrap_or(raw.as_str())
            .to_string()
    } else if raw.starts_with("https://") || raw.starts_with("http://") {
        raw.split("://")
            .nth(1)
            .and_then(|rest| rest.split_once('/'))
            .map(|(_, p)| p.to_string())
            .unwrap_or(raw)
    } else if raw.starts_with("ssh://") {
        let trimmed = raw.trim_start_matches("ssh://");
        trimmed
            .split_once('/')
            .map(|(_, p)| p.to_string())
            .unwrap_or(trimmed.to_string())
    } else {
        raw
    };

    let trimmed = path.trim_end_matches(".git");
    let mut parts = trimmed.split('/');
    let owner = parts.next()?.to_string();
    let repo = parts.next()?.to_string();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner, repo))
}

fn ensure_npm_token() -> Result<()> {
    if std::env::var("NODE_AUTH_TOKEN").is_ok() {
        return Ok(());
    }

    let vars = env::fetch_personal_env_vars(&["NODE_AUTH_TOKEN".to_string()])?;
    if let Some(token) = vars.get("NODE_AUTH_TOKEN") {
        // Rust 2024 marks env mutation as unsafe; keep scope minimal.
        unsafe {
            std::env::set_var("NODE_AUTH_TOKEN", token);
        }
        return Ok(());
    }

    bail!("NODE_AUTH_TOKEN not set. Run `f env new` and choose the npm token.");
}
