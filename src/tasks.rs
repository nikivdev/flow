use std::{
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};

use crate::{
    cli::{TaskRunOpts, TasksOpts},
    config::{self, Config, TaskConfig},
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
    let Some(task) = cfg.tasks.iter().find(|task| task.name == opts.name) else {
        bail!(
            "task '{}' not found in {}",
            opts.name,
            config_path.display()
        );
    };

    execute_task(task, config_path.parent().unwrap_or(Path::new(".")))
}

fn load_project_config(path: PathBuf) -> Result<(PathBuf, Config)> {
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

fn execute_task(task: &TaskConfig, workdir: &Path) -> Result<()> {
    let command = task.command.trim();
    if command.is_empty() {
        bail!("task '{}' has an empty command", task.name);
    }

    println!("Running task '{}': {}", task.name, command);
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

fn format_task_lines(tasks: &[TaskConfig]) -> Vec<String> {
    let mut lines = Vec::new();
    for (idx, task) in tasks.iter().enumerate() {
        lines.push(format!("{:>2}. {} – {}", idx + 1, task.name, task.command));
        if let Some(desc) = &task.description {
            lines.push(format!("    {desc}"));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn formats_task_lines_with_descriptions() {
        let tasks = vec![
            TaskConfig {
                name: "lint".to_string(),
                command: "golangci-lint run".to_string(),
                description: Some("Run lint checks".to_string()),
            },
            TaskConfig {
                name: "test".to_string(),
                command: "gotestsum ./...".to_string(),
                description: None,
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
            description: None,
        };
        let err = execute_task(&task, Path::new(".")).unwrap_err();
        assert!(
            err.to_string().contains("empty command"),
            "unexpected error: {err:?}"
        );
    }
}
