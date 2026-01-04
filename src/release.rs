use anyhow::{bail, Result};

use crate::{
    cli::ReleaseOpts,
    tasks::{self, find_task},
};

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

pub fn run(opts: ReleaseOpts) -> Result<()> {
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
