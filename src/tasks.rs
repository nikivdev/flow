use std::{
    net::IpAddr,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use serde_json::json;

use crate::{
    cli::{HubAction, HubCommand, HubOpts, TaskActivateOpts, TaskRunOpts, TasksOpts},
    config::{self, Config, FloxInstallSpec, TaskConfig},
    flox::{self, FloxEnv},
    hub,
};

pub fn list(opts: TasksOpts) -> Result<()> {
    let (config_path, cfg) = load_project_config(opts.config)?;

    if cfg.tasks.is_empty() {
        println!("No tasks defined in {}", config_path.display());
        return Ok(());
    }

    println!("Tasks defined in {}:", config_path.display());
    for line in format_task_lines(&cfg.tasks) {
        println!("{line}");
    }

    Ok(())
}

pub fn run(opts: TaskRunOpts) -> Result<()> {
    let (config_path, cfg) = load_project_config(opts.config)?;
    let Some(task) = find_task(&cfg, &opts.name) else {
        bail!(
            "task '{}' not found in {}",
            opts.name,
            config_path.display()
        );
    };
    let resolved = resolve_task_dependencies(task, &cfg)?;
    let workdir = config_path.parent().unwrap_or(Path::new("."));

    let should_delegate = opts.delegate_to_hub || task.delegate_to_hub;
    if should_delegate {
        match delegate_task_to_hub(task, &resolved, workdir, opts.hub_host, opts.hub_port) {
            Ok(()) => return Ok(()),
            Err(err) => {
                println!(
                    "⚠️  Failed to delegate task '{}' to hub ({}); falling back to local execution.",
                    task.name, err
                );
            }
        }
    }

    let flox_pkgs = collect_flox_packages(&cfg, &resolved.flox);
    if flox_pkgs.is_empty() {
        ensure_command_dependencies_available(&resolved.commands)?;
    } else {
        println!(
            "Skipping host PATH checks; using managed deps [{}]",
            flox_pkgs
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let flox_env = prepare_flox_env(workdir, &flox_pkgs)?;
    execute_task(task, workdir, flox_env.as_ref())
}

pub fn activate(opts: TaskActivateOpts) -> Result<()> {
    let (config_path, cfg) = load_project_config(opts.config)?;
    let workdir = config_path.parent().unwrap_or(Path::new("."));

    let tasks: Vec<&TaskConfig> = cfg
        .tasks
        .iter()
        .filter(|task| task.activate_on_cd_to_root)
        .collect();

    if tasks.is_empty() {
        return Ok(());
    }

    let mut combined = ResolvedDependencies::default();
    for task in &tasks {
        let resolved = resolve_task_dependencies(task, &cfg)?;
        combined.commands.extend(resolved.commands);
        combined.flox.extend(resolved.flox);
    }

    let flox_pkgs = collect_flox_packages(&cfg, &combined.flox);
    if flox_pkgs.is_empty() {
        ensure_command_dependencies_available(&combined.commands)?;
    } else {
        println!(
            "Skipping host PATH checks; using managed deps [{}]",
            flox_pkgs
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let flox_env = prepare_flox_env(workdir, &flox_pkgs)?;

    for task in tasks {
        execute_task(task, workdir, flox_env.as_ref())?;
    }

    Ok(())
}

pub(crate) fn load_project_config(path: PathBuf) -> Result<(PathBuf, Config)> {
    let config_path = resolve_path(path)?;
    let cfg = config::load(&config_path).with_context(|| {
        format!(
            "failed to load flow tasks configuration at {}",
            config_path.display()
        )
    })?;
    Ok((config_path, cfg))
}

fn resolve_path(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn execute_task(task: &TaskConfig, workdir: &Path, flox_env: Option<&FloxEnv>) -> Result<()> {
    let command = task.command.trim();
    if command.is_empty() {
        bail!("task '{}' has an empty command", task.name);
    }

    println!("Running task '{}': {}", task.name, command);
    if let Some(env) = flox_env {
        flox::run_in_env(env, workdir, command)
    } else {
        let status = Command::new("/bin/sh")
            .arg("-c")
            .arg(command)
            .current_dir(workdir)
            .status()
            .with_context(|| format!("failed to spawn command for task '{}'", task.name))?;

        if status.success() {
            Ok(())
        } else {
            bail!(
                "task '{}' exited with status {}",
                task.name,
                status.code().unwrap_or(-1)
            );
        }
    }
}

fn format_task_lines(tasks: &[TaskConfig]) -> Vec<String> {
    let mut lines = Vec::new();
    for (idx, task) in tasks.iter().enumerate() {
        let shortcut_display = if task.shortcuts.is_empty() {
            String::new()
        } else {
            format!(" [{}]", task.shortcuts.join(", "))
        };
        lines.push(format!(
            "{:>2}. {}{} – {}",
            idx + 1,
            task.name,
            shortcut_display,
            task.command
        ));
        if let Some(desc) = &task.description {
            lines.push(format!("    {desc}"));
        }
    }
    lines
}

pub(crate) fn find_task<'a>(cfg: &'a Config, needle: &str) -> Option<&'a TaskConfig> {
    let normalized = needle.trim();
    if normalized.is_empty() {
        return None;
    }

    if let Some(task) = cfg
        .tasks
        .iter()
        .find(|task| task.name.eq_ignore_ascii_case(normalized))
    {
        return Some(task);
    }

    if let Some(task) = cfg.tasks.iter().find(|task| {
        task.shortcuts
            .iter()
            .any(|alias| alias.eq_ignore_ascii_case(normalized))
    }) {
        return Some(task);
    }

    resolve_by_abbreviation(&cfg.tasks, normalized)
}

fn resolve_by_abbreviation<'a>(tasks: &'a [TaskConfig], alias: &str) -> Option<&'a TaskConfig> {
    let alias = alias.trim().to_ascii_lowercase();
    if alias.len() < 2 {
        return None;
    }

    let mut matches = tasks.iter().filter(|task| {
        generate_abbreviation(&task.name)
            .map(|abbr| abbr == alias)
            .unwrap_or(false)
    });

    let first = matches.next()?;
    if matches.next().is_some() {
        None
    } else {
        Some(first)
    }
}

fn generate_abbreviation(name: &str) -> Option<String> {
    let mut abbr = String::new();
    let mut new_segment = true;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            if new_segment {
                abbr.push(ch.to_ascii_lowercase());
                new_segment = false;
            }
        } else {
            new_segment = true;
        }
    }

    if abbr.len() >= 2 { Some(abbr) } else { None }
}

#[derive(Debug, Default)]
struct ResolvedDependencies {
    commands: Vec<String>,
    flox: Vec<(String, FloxInstallSpec)>,
}

fn resolve_task_dependencies(task: &TaskConfig, cfg: &Config) -> Result<ResolvedDependencies> {
    if task.dependencies.is_empty() {
        return Ok(ResolvedDependencies::default());
    }

    let mut missing = Vec::new();
    let mut resolved = ResolvedDependencies::default();
    for dep_name in &task.dependencies {
        if let Some(spec) = cfg.dependencies.get(dep_name) {
            match spec {
                config::DependencySpec::Single(cmd) => resolved.commands.push(cmd.clone()),
                config::DependencySpec::Multiple(cmds) => resolved.commands.extend(cmds.clone()),
                config::DependencySpec::Flox(pkg) => {
                    resolved.flox.push((dep_name.clone(), pkg.clone()));
                }
            }
            continue;
        }

        if let Some(flox) = cfg.flox.as_ref().and_then(|f| f.install.get(dep_name)) {
            resolved.flox.push((dep_name.clone(), flox.clone()));
            continue;
        }

        missing.push(dep_name.as_str());
    }

    if !missing.is_empty() {
        bail!(
            "task '{}' references unknown dependencies: {} (define them under [dependencies] or [flox.install]/[flox.deps])",
            task.name,
            missing.join(", ")
        );
    }

    Ok(resolved)
}

fn ensure_command_dependencies_available(commands: &[String]) -> Result<()> {
    if commands.is_empty() {
        return Ok(());
    }

    println!(
        "Ensuring dependencies are available on PATH: {}",
        commands.join(", ")
    );
    for command in commands {
        which::which(command).with_context(|| dependency_error(command))?;
    }

    Ok(())
}

fn dependency_error(command: &str) -> String {
    let mut msg = format!(
        "dependency '{}' not found in PATH. Install it or adjust the [dependencies] config.",
        command
    );
    if let Some(extra) = dependency_help(command) {
        msg.push('\n');
        msg.push_str(extra);
    }
    msg
}

fn dependency_help(command: &str) -> Option<&'static str> {
    match command {
        "fast" => Some(
            "Get the fast CLI from https://github.com/1focus-ai/fast and ensure it is on PATH.",
        ),
        _ => None,
    }
}

fn prepare_flox_env(
    project_root: &Path,
    deps: &[(String, FloxInstallSpec)],
) -> Result<Option<FloxEnv>> {
    if deps.is_empty() {
        return Ok(None);
    }

    let env = flox::ensure_env(project_root, deps)?;
    let names: Vec<_> = deps.iter().map(|(name, _)| name.as_str()).collect();
    println!(
        "Ensuring flox deps [{}] (manifest: {})",
        names.join(", "),
        env.manifest_path.display()
    );
    Ok(Some(env))
}

fn collect_flox_packages(
    cfg: &Config,
    deps: &[(String, FloxInstallSpec)],
) -> Vec<(String, FloxInstallSpec)> {
    let mut merged = std::collections::BTreeMap::new();
    if let Some(flox) = &cfg.flox {
        for (name, spec) in &flox.install {
            merged.insert(name.clone(), spec.clone());
        }
    }

    for (name, spec) in deps {
        merged.insert(name.clone(), spec.clone());
    }

    merged.into_iter().collect()
}

fn delegate_task_to_hub(
    task: &TaskConfig,
    deps: &ResolvedDependencies,
    workdir: &Path,
    host: IpAddr,
    port: u16,
) -> Result<()> {
    ensure_hub_running(host, port)?;
    let url = format_task_submit_url(host, port);
    let client = Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("failed to construct HTTP client for hub delegation")?;

    let flox_specs: Vec<_> = deps
        .flox
        .iter()
        .map(|(name, spec)| json!({ "name": name, "spec": spec }))
        .collect();

    let payload = json!({
        "task": {
            "name": task.name,
            "command": task.command,
            "dependencies": {
                "commands": deps.commands,
                "flox": flox_specs,
            },
        },
        "cwd": workdir.to_string_lossy(),
        "flow_version": env!("CARGO_PKG_VERSION"),
    });

    let resp = client.post(&url).json(&payload).send().with_context(|| {
        format!(
            "failed to submit task to hub at {}",
            format_addr(host, port)
        )
    })?;

    let status = resp.status();
    if status.is_success() {
        println!(
            "Delegated task '{}' to hub at {}",
            task.name,
            format_addr(host, port)
        );
        Ok(())
    } else {
        let body = resp.text().unwrap_or_default();
        bail!(
            "hub returned {} while delegating task '{}': {}",
            status,
            task.name,
            body
        );
    }
}

fn ensure_hub_running(host: IpAddr, port: u16) -> Result<()> {
    let opts = HubOpts {
        host,
        port,
        config: None,
        no_ui: true,
    };
    let cmd = HubCommand {
        opts,
        action: Some(HubAction::Start),
    };
    hub::run(cmd)
}

fn format_addr(host: IpAddr, port: u16) -> String {
    match host {
        IpAddr::V4(_) => format!("http://{host}:{port}"),
        IpAddr::V6(_) => format!("http://[{host}]:{port}"),
    }
}

fn format_task_submit_url(host: IpAddr, port: u16) -> String {
    match host {
        IpAddr::V4(_) => format!("http://{host}:{port}/tasks/run"),
        IpAddr::V6(_) => format!("http://[{host}]:{port}/tasks/run"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DependencySpec, FloxConfig};
    use std::path::Path;

    #[test]
    fn formats_task_lines_with_descriptions() {
        let tasks = vec![
            TaskConfig {
                name: "lint".to_string(),
                command: "golangci-lint run".to_string(),
                delegate_to_hub: false,
                activate_on_cd_to_root: false,
                dependencies: Vec::new(),
                description: Some("Run lint checks".to_string()),
                shortcuts: Vec::new(),
            },
            TaskConfig {
                name: "test".to_string(),
                command: "gotestsum ./...".to_string(),
                delegate_to_hub: false,
                activate_on_cd_to_root: false,
                dependencies: Vec::new(),
                description: None,
                shortcuts: Vec::new(),
            },
        ];

        let lines = format_task_lines(&tasks);
        assert_eq!(
            lines,
            vec![
                " 1. lint – golangci-lint run".to_string(),
                "    Run lint checks".to_string(),
                " 2. test – gotestsum ./...".to_string(),
            ]
        );
    }

    #[test]
    fn run_rejects_empty_commands() {
        let task = TaskConfig {
            name: "empty".into(),
            command: "".into(),
            delegate_to_hub: false,
            activate_on_cd_to_root: false,
            dependencies: Vec::new(),
            description: None,
            shortcuts: Vec::new(),
        };
        let err = execute_task(&task, Path::new("."), None).unwrap_err();
        assert!(
            err.to_string().contains("empty command"),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn collects_dependency_commands() {
        let mut cfg = Config::default();
        cfg.dependencies
            .insert("fast".into(), DependencySpec::Single("fast".into()));
        cfg.dependencies.insert(
            "toolkit".into(),
            DependencySpec::Multiple(vec!["rg".into(), "fd".into()]),
        );

        let task = TaskConfig {
            name: "ci".into(),
            command: "ci".into(),
            delegate_to_hub: false,
            activate_on_cd_to_root: false,
            dependencies: vec!["fast".into(), "toolkit".into()],
            description: None,
            shortcuts: Vec::new(),
        };

        let resolved = resolve_task_dependencies(&task, &cfg).expect("dependencies should resolve");
        assert_eq!(
            resolved.commands,
            vec!["fast".to_string(), "rg".to_string(), "fd".to_string()]
        );
        assert!(resolved.flox.is_empty());
    }

    #[test]
    fn collects_flox_dependencies_from_dependency_table() {
        let mut cfg = Config::default();
        cfg.dependencies.insert(
            "ripgrep".into(),
            DependencySpec::Flox(FloxInstallSpec {
                pkg_path: "ripgrep".into(),
                pkg_group: None,
                version: None,
                systems: None,
                priority: None,
            }),
        );

        let task = TaskConfig {
            name: "search".into(),
            command: "rg TODO".into(),
            delegate_to_hub: false,
            activate_on_cd_to_root: false,
            dependencies: vec!["ripgrep".into()],
            description: None,
            shortcuts: Vec::new(),
        };

        let resolved = resolve_task_dependencies(&task, &cfg).expect("dependencies should resolve");
        assert!(resolved.commands.is_empty());
        assert_eq!(resolved.flox.len(), 1);
        assert_eq!(resolved.flox[0].0, "ripgrep");
        assert_eq!(resolved.flox[0].1.pkg_path, "ripgrep");
    }

    #[test]
    fn collects_flox_dependencies_from_flox_config() {
        let mut cfg = Config::default();
        let mut install = std::collections::HashMap::new();
        install.insert(
            "node".to_string(),
            FloxInstallSpec {
                pkg_path: "nodejs".into(),
                pkg_group: None,
                version: None,
                systems: None,
                priority: None,
            },
        );
        cfg.flox = Some(FloxConfig { install });

        let task = TaskConfig {
            name: "dev".into(),
            command: "npm start".into(),
            delegate_to_hub: false,
            activate_on_cd_to_root: false,
            dependencies: vec!["node".into()],
            description: None,
            shortcuts: Vec::new(),
        };

        let resolved = resolve_task_dependencies(&task, &cfg).expect("dependencies should resolve");
        assert!(resolved.commands.is_empty());
        assert_eq!(resolved.flox.len(), 1);
        assert_eq!(resolved.flox[0].0, "node");
        assert_eq!(resolved.flox[0].1.pkg_path, "nodejs");
    }

    #[test]
    fn errors_on_missing_dependencies() {
        let cfg = Config::default();
        let task = TaskConfig {
            name: "ci".into(),
            command: "ci".into(),
            delegate_to_hub: false,
            activate_on_cd_to_root: false,
            dependencies: vec!["unknown".into()],
            description: None,
            shortcuts: Vec::new(),
        };

        let err = resolve_task_dependencies(&task, &cfg).unwrap_err();
        assert!(
            err.to_string().contains("references unknown dependencies"),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn errors_when_dependency_not_declared_in_table() {
        let mut cfg = Config::default();
        cfg.dependencies
            .insert("fast".into(), DependencySpec::Single("fast".into()));
        let task = TaskConfig {
            name: "ci".into(),
            command: "ci".into(),
            delegate_to_hub: false,
            activate_on_cd_to_root: false,
            dependencies: vec!["unknown".into()],
            description: None,
            shortcuts: Vec::new(),
        };

        let err = resolve_task_dependencies(&task, &cfg).unwrap_err();
        assert!(
            err.to_string().contains("references unknown dependencies"),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn find_task_matches_shortcuts_and_abbreviations() {
        let mut cfg = Config::default();
        cfg.tasks = vec![
            TaskConfig {
                name: "deploy-cli-release".into(),
                command: "echo deploy".into(),
                delegate_to_hub: false,
                activate_on_cd_to_root: false,
                dependencies: Vec::new(),
                description: None,
                shortcuts: vec!["dcr-alias".into()],
            },
            TaskConfig {
                name: "dev-hub".into(),
                command: "echo dev".into(),
                delegate_to_hub: false,
                activate_on_cd_to_root: false,
                dependencies: Vec::new(),
                description: None,
                shortcuts: Vec::new(),
            },
        ];

        let task = find_task(&cfg, "dcr-alias").expect("shortcut should resolve");
        assert_eq!(task.name, "deploy-cli-release");

        let task = find_task(&cfg, "dcr").expect("abbreviation should resolve");
        assert_eq!(task.name, "deploy-cli-release");

        let task = find_task(&cfg, "dev-hub").expect("exact match should resolve");
        assert_eq!(task.name, "dev-hub");

        let task = find_task(&cfg, "DH").expect("case-insensitive match should resolve");
        assert_eq!(task.name, "dev-hub");
    }

    #[test]
    fn ambiguous_abbreviations_do_not_match() {
        let mut cfg = Config::default();
        cfg.tasks = vec![
            TaskConfig {
                name: "deploy-cli-release".into(),
                command: "echo deploy".into(),
                delegate_to_hub: false,
                activate_on_cd_to_root: false,
                dependencies: Vec::new(),
                description: None,
                shortcuts: Vec::new(),
            },
            TaskConfig {
                name: "deploy-core-runner".into(),
                command: "echo runner".into(),
                delegate_to_hub: false,
                activate_on_cd_to_root: false,
                dependencies: Vec::new(),
                description: None,
                shortcuts: Vec::new(),
            },
        ];

        assert!(
            find_task(&cfg, "dcr").is_none(),
            "abbreviation should be ambiguous"
        );
    }
}
