use std::net::IpAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::{
    cli::{
        DomainsAction, DomainsAddOpts, DomainsCommand, DomainsEngineArg, DomainsRmOpts, KillOpts,
        LifecycleRunOpts, TaskRunOpts,
    },
    config::{self, Config, LifecycleDomainsConfig},
    domains, processes, tasks,
};

pub fn run_up(opts: LifecycleRunOpts) -> Result<()> {
    let project = resolve_project_config(&opts.config)?;
    let lifecycle = project.config.lifecycle.clone().unwrap_or_default();

    if let Some(domains_cfg) = lifecycle.domains.as_ref() {
        ensure_domains_up(domains_cfg)?;
    }

    let ran_task = match lifecycle.up_task.as_deref() {
        Some(task) => run_required_task(&project.flow_path, task, opts.args)?,
        None => run_optional_task_chain(&project.flow_path, &["up", "dev"], opts.args)?,
    };

    if !ran_task {
        bail!(
            "No lifecycle up task found. Define task 'up' or 'dev', or set [lifecycle].up_task in {}",
            project.flow_path.display()
        );
    }

    Ok(())
}

pub fn run_down(opts: LifecycleRunOpts) -> Result<()> {
    let project = resolve_project_config(&opts.config)?;
    let lifecycle = project.config.lifecycle.clone().unwrap_or_default();

    let mut task_ran = match lifecycle.down_task.as_deref() {
        Some(task) => run_required_task(&project.flow_path, task, opts.args.clone())?,
        None => run_optional_task_chain(&project.flow_path, &["down"], opts.args.clone())?,
    };

    if !task_ran && lifecycle.down_task.is_none() {
        processes::kill_processes(KillOpts {
            config: project.flow_path.clone(),
            task: None,
            pid: None,
            all: true,
            force: false,
            timeout: 5,
        })?;
        task_ran = true;
    }

    let mut domain_action_ran = false;
    if let Some(domains_cfg) = lifecycle.domains.as_ref() {
        domain_action_ran = run_domains_down(domains_cfg)?;
    }

    if !task_ran && !domain_action_ran {
        bail!(
            "No lifecycle down action found. Define task 'down', set [lifecycle].down_task, or enable [lifecycle.domains] cleanup in {}",
            project.flow_path.display()
        );
    }

    Ok(())
}

fn run_required_task(config_path: &Path, task_name: &str, args: Vec<String>) -> Result<bool> {
    match run_task(config_path, task_name, args) {
        Ok(()) => Ok(true),
        Err(err) if is_task_not_found(&err) => {
            bail!("lifecycle task '{}' not found", task_name);
        }
        Err(err) => Err(err),
    }
}

fn run_optional_task_chain(
    config_path: &Path,
    candidates: &[&str],
    args: Vec<String>,
) -> Result<bool> {
    for name in candidates {
        match run_task(config_path, name, args.clone()) {
            Ok(()) => return Ok(true),
            Err(err) if is_task_not_found(&err) => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(false)
}

fn run_task(config_path: &Path, task_name: &str, args: Vec<String>) -> Result<()> {
    tasks::run(TaskRunOpts {
        config: config_path.to_path_buf(),
        delegate_to_hub: false,
        hub_host: IpAddr::from([127, 0, 0, 1]),
        hub_port: 9050,
        name: task_name.to_string(),
        args,
    })
}

fn ensure_domains_up(cfg: &LifecycleDomainsConfig) -> Result<()> {
    let host = cfg
        .host
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| anyhow::anyhow!("lifecycle.domains.host is required"))?;
    let target = cfg
        .target
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| anyhow::anyhow!("lifecycle.domains.target is required"))?;
    let engine = parse_domains_engine(cfg.engine.as_deref())?;

    domains::run(DomainsCommand {
        engine,
        action: Some(DomainsAction::Add(DomainsAddOpts {
            host: host.to_string(),
            target: target.to_string(),
            replace: true,
        })),
    })?;

    domains::run(DomainsCommand {
        engine,
        action: Some(DomainsAction::Up),
    })?;

    Ok(())
}

fn run_domains_down(cfg: &LifecycleDomainsConfig) -> Result<bool> {
    let mut changed = false;
    let engine = parse_domains_engine(cfg.engine.as_deref())?;

    if cfg.remove_on_down.unwrap_or(false) {
        let host = cfg
            .host
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("lifecycle.domains.host is required when remove_on_down=true")
            })?;
        domains::run(DomainsCommand {
            engine,
            action: Some(DomainsAction::Rm(DomainsRmOpts {
                host: host.to_string(),
            })),
        })?;
        changed = true;
    }

    if cfg.stop_proxy_on_down.unwrap_or(false) {
        domains::run(DomainsCommand {
            engine,
            action: Some(DomainsAction::Down),
        })?;
        changed = true;
    }

    Ok(changed)
}

fn parse_domains_engine(raw: Option<&str>) -> Result<Option<DomainsEngineArg>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let engine = match raw.trim().to_ascii_lowercase().as_str() {
        "docker" => DomainsEngineArg::Docker,
        "native" => DomainsEngineArg::Native,
        other => bail!(
            "invalid lifecycle.domains.engine '{}': expected 'docker' or 'native'",
            other
        ),
    };
    Ok(Some(engine))
}

fn resolve_project_config(config_arg: &Path) -> Result<ProjectConfig> {
    let cwd = std::env::current_dir().context("Failed to read current directory")?;
    let flow_path = resolve_flow_path(config_arg, &cwd)?;
    let cfg = config::load(&flow_path)
        .with_context(|| format!("Failed to load {}", flow_path.display()))?;
    Ok(ProjectConfig {
        flow_path,
        config: cfg,
    })
}

fn resolve_flow_path(config_arg: &Path, cwd: &Path) -> Result<PathBuf> {
    if config_arg.is_absolute() {
        if config_arg.exists() {
            return Ok(config_arg.to_path_buf());
        }
        bail!("config path not found: {}", config_arg.display());
    }

    let direct = cwd.join(config_arg);
    if direct.exists() {
        return Ok(direct);
    }

    if config_arg == Path::new("flow.toml") {
        if let Some(found) = find_flow_toml_upwards(cwd) {
            return Ok(found);
        }
    }

    bail!("config path not found: {}", direct.display());
}

fn find_flow_toml_upwards(start: &Path) -> Option<PathBuf> {
    let mut cur = start.to_path_buf();
    loop {
        let cand = cur.join("flow.toml");
        if cand.exists() {
            return Some(cand);
        }
        if !cur.pop() {
            break;
        }
    }
    None
}

fn is_task_not_found(err: &anyhow::Error) -> bool {
    let msg = err.to_string().to_ascii_lowercase();
    msg.contains("task '") && msg.contains("not found")
}

struct ProjectConfig {
    flow_path: PathBuf,
    config: Config,
}
