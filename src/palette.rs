use std::{
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail};

use crate::{
    cli::TasksOpts,
    config::{self, TaskConfig},
};

pub fn run(opts: TasksOpts) -> Result<()> {
    let entries = build_entries(Some(opts))?;
    present(entries)
}

/// Show global commands/tasks only (no project flow.toml required).
pub fn run_global() -> Result<()> {
    let entries = build_entries(None)?;
    present(entries)
}

fn run_fzf<'a>(entries: &'a [PaletteEntry]) -> Result<Option<&'a PaletteEntry>> {
    let mut child = Command::new("fzf")
        .arg("--prompt")
        .arg("f> ")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("failed to spawn fzf")?;

    {
        let stdin = child.stdin.as_mut().context("failed to open fzf stdin")?;
        for entry in entries {
            writeln!(stdin, "{}", entry.display)?;
        }
    }

    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Ok(None);
    }
    let selection = String::from_utf8(output.stdout)
        .context("fzf output was not valid UTF-8")?
        .trim()
        .to_string();
    if selection.is_empty() {
        return Ok(None);
    }

    let entry = entries.iter().find(|entry| entry.display == selection);
    Ok(entry)
}

fn run_entry(entry: &PaletteEntry) -> Result<()> {
    let exe = std::env::current_exe().context("failed to resolve current executable")?;
    let status = Command::new(exe)
        .args(&entry.exec)
        .status()
        .with_context(|| format!("failed to run {}", entry.display))?;

    if status.success() {
        Ok(())
    } else {
        bail!(
            "{} exited with status {}",
            entry.display,
            status.code().unwrap_or(-1)
        );
    }
}

fn present(entries: Vec<PaletteEntry>) -> Result<()> {
    if entries.is_empty() {
        println!("No commands or tasks available. Add entries to flow.toml or global config.");
        return Ok(());
    }

    if which::which("fzf").is_err() {
        println!("fzf not found on PATH – install it to use fuzzy selection.");
        println!("Available commands:");
        for entry in &entries {
            println!("  {}", entry.display);
        }
        return Ok(());
    }

    if let Some(selected) = run_fzf(&entries)? {
        run_entry(selected)?;
    }

    Ok(())
}

struct PaletteEntry {
    display: String,
    exec: Vec<String>,
}

impl PaletteEntry {
    fn new(display: &str, exec: Vec<String>) -> Self {
        Self {
            display: display.to_string(),
            exec,
        }
    }

    fn from_task(task: &TaskConfig, config_arg: &str) -> Self {
        let summary = task
            .description
            .as_deref()
            .unwrap_or_else(|| task.command.as_str());
        let display = format!("[task] {} – {}", task.name, truncate(summary, 96));
        let exec = vec![
            "run".into(),
            "--config".into(),
            config_arg.to_string(),
            task.name.clone(),
        ];

        Self { display, exec }
    }
}

fn build_entries(project_opts: Option<TasksOpts>) -> Result<Vec<PaletteEntry>> {
    let mut entries = Vec::new();
    let global_cfg = load_if_exists(config::default_config_path())?;
    let mut has_project = false;

    if let Some(opts) = project_opts {
        if let Some((config_path, cfg)) = load_project_if_exists(opts)? {
            has_project = true;
            let config_arg = config_path.display().to_string();
            for task in &cfg.tasks {
                entries.push(PaletteEntry::from_task(task, &config_arg));
            }
        }
    }

    if has_project {
        return Ok(entries);
    }

    entries.extend(builtin_entries());

    if let Some((global_path, cfg)) = global_cfg {
        let arg = global_path.display().to_string();
        for task in &cfg.tasks {
            entries.push(PaletteEntry::from_task(task, &arg));
        }
    }

    Ok(entries)
}

fn builtin_entries() -> Vec<PaletteEntry> {
    let entries = vec![
        PaletteEntry::new("[cmd] hub – ensure daemon is running", vec!["hub".into()]),
        PaletteEntry::new("[cmd] search – global commands/tasks", vec!["search".into()]),
        PaletteEntry::new("[cmd] init – scaffold flow.toml", vec!["init".into()]),
    ];

    entries
}

fn load_if_exists(path: PathBuf) -> Result<Option<(PathBuf, config::Config)>> {
    if path.exists() {
        let cfg = config::load(&path)?;
        Ok(Some((path, cfg)))
    } else {
        Ok(None)
    }
}

fn load_project_if_exists(opts: TasksOpts) -> Result<Option<(PathBuf, config::Config)>> {
    let config_path = resolve_path(opts.config)?;
    if !config_path.exists() {
        return Ok(None);
    }
    let cfg = config::load(&config_path)?;
    Ok(Some((config_path, cfg)))
}

fn resolve_path(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn truncate(input: &str, max: usize) -> String {
    let mut out = String::new();
    for ch in input.chars() {
        if out.chars().count() + 1 >= max {
            break;
        }
        out.push(ch);
    }
    if out.len() < input.len() {
        out.push('…');
    }
    out
}
