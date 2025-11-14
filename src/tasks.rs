use std::{
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};

use crate::{
    cli::{TaskRunOpts, TasksOpts},
    config::{self, Config, DependencySpec, TaskConfig},
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
    let dependency_commands = resolve_task_dependencies(task, &cfg)?;
    ensure_dependencies_available(&dependency_commands)?;
    execute_task(task, config_path.parent().unwrap_or(Path::new(".")))
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

fn resolve_task_dependencies(task: &TaskConfig, cfg: &Config) -> Result<Vec<String>> {
    if task.dependencies.is_empty() {
        return Ok(Vec::new());
    }

    if cfg.dependencies.is_empty() {
        bail!(
            "task '{}' declares dependencies but no [dependencies] table is defined",
            task.name
        );
    }

    let mut missing = Vec::new();
    let mut commands = Vec::new();
    for dep_name in &task.dependencies {
        if let Some(spec) = cfg.dependencies.get(dep_name) {
            spec.extend_commands(&mut commands);
        } else {
            missing.push(dep_name.as_str());
        }
    }

    if !missing.is_empty() {
        bail!(
            "task '{}' references unknown dependencies: {}",
            task.name,
            missing.join(", ")
        );
    }

    Ok(commands)
}

fn ensure_dependencies_available(commands: &[String]) -> Result<()> {
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
                dependencies: Vec::new(),
                description: Some("Run lint checks".to_string()),
            },
            TaskConfig {
                name: "test".to_string(),
                command: "gotestsum ./...".to_string(),
                dependencies: Vec::new(),
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
            dependencies: Vec::new(),
            description: None,
        };
        let err = execute_task(&task, Path::new(".")).unwrap_err();
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
            dependencies: vec!["fast".into(), "toolkit".into()],
            description: None,
        };

        let commands = resolve_task_dependencies(&task, &cfg).expect("dependencies should resolve");
        assert_eq!(
            commands,
            vec!["fast".to_string(), "rg".to_string(), "fd".to_string()]
        );
    }

    #[test]
    fn errors_on_missing_dependencies() {
        let cfg = Config::default();
        let task = TaskConfig {
            name: "ci".into(),
            command: "ci".into(),
            dependencies: vec!["unknown".into()],
            description: None,
        };

        let err = resolve_task_dependencies(&task, &cfg).unwrap_err();
        assert!(
            err.to_string()
                .contains("declares dependencies but no [dependencies] table"),
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
            dependencies: vec!["unknown".into()],
            description: None,
        };

        let err = resolve_task_dependencies(&task, &cfg).unwrap_err();
        assert!(
            err.to_string().contains("references unknown dependencies"),
            "unexpected error: {err:?}"
        );
    }
}
