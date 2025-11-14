use std::{
    io::Write,
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail};

use crate::{cli::TasksOpts, config::TaskConfig, tasks::load_project_config};

pub fn run(opts: TasksOpts) -> Result<()> {
    let (config_path, cfg) = load_project_config(opts.config)?;
    let config_arg = config_path.display().to_string();

    let mut entries = builtin_entries(&config_arg);
    for task in &cfg.tasks {
        entries.push(PaletteEntry::from_task(task, &config_arg));
    }

    if entries.is_empty() {
        println!("No commands or tasks available. Add entries to flow.toml.");
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
        let display = format!("[task] {} – {}", task.name, summary);
        let exec = vec![
            "run".into(),
            "--config".into(),
            config_arg.to_string(),
            task.name.clone(),
        ];

        Self { display, exec }
    }
}

fn builtin_entries(config_arg: &str) -> Vec<PaletteEntry> {
    vec![
        PaletteEntry::new("[cmd] daemon – start HTTP daemon", vec!["daemon".into()]),
        PaletteEntry::new("[cmd] screen – preview frames", vec!["screen".into()]),
        PaletteEntry::new(
            "[cmd] servers – inspect managed servers",
            vec!["servers".into()],
        ),
        PaletteEntry::new(
            "[cmd] tasks – list project tasks",
            vec!["tasks".into(), "--config".into(), config_arg.to_string()],
        ),
        PaletteEntry::new(
            "[cmd] setup – emit shell aliases",
            vec!["setup".into(), "--config".into(), config_arg.to_string()],
        ),
    ]
}
