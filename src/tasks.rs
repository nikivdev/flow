use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::{
    cli::TasksOpts,
    config::{self, TaskConfig},
};

pub fn list(opts: TasksOpts) -> Result<()> {
    let config_path = resolve_path(opts.config)?;
    let cfg = config::load(&config_path).with_context(|| {
        format!(
            "failed to load flow tasks configuration at {}",
            config_path.display()
        )
    })?;

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

fn resolve_path(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
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
}
