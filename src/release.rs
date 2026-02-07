use anyhow::{Result, bail};

use crate::{
    cli::{GhReleaseCommand, ReleaseAction, ReleaseCommand, ReleaseOpts},
    config::Config,
    gh_release, registry, release_signing,
    tasks::{self, find_task},
};
use std::path::Path;

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
    if let Some(action) = cmd.action.clone() {
        match action {
            ReleaseAction::Github(cmd) => return gh_release::run(cmd),
            ReleaseAction::Signing(cmd) => return release_signing::run(cmd),
            _ => {}
        }
    }

    let (config_path, cfg) = tasks::load_project_config(cmd.config.clone())?;

    match cmd.action {
        Some(ReleaseAction::Github(cmd)) => gh_release::run(cmd),
        Some(ReleaseAction::Registry(opts)) => registry::publish(&config_path, &cfg, opts),
        Some(ReleaseAction::Task(opts)) => run_task(ReleaseOpts {
            config: config_path,
            args: opts.args,
        }),
        Some(ReleaseAction::Signing(cmd)) => release_signing::run(cmd),
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
    let provider = cfg
        .release
        .as_ref()
        .and_then(|release| release.default.as_deref())
        .or_else(|| {
            cfg.release
                .as_ref()
                .and_then(|release| release.registry.as_ref())
                .map(|_| "registry")
        })
        .unwrap_or("task");

    match provider {
        "registry" => {
            registry::publish(config_path, cfg, crate::cli::RegistryReleaseOpts::default())
        }
        "task" | "release" => run_task(ReleaseOpts {
            config: config_path.to_path_buf(),
            args: Vec::new(),
        }),
        "github" | "gh" => gh_release::run(GhReleaseCommand { action: None }),
        other => bail!(
            "Unknown release provider '{}'. Expected registry, task, or github.",
            other
        ),
    }
}
