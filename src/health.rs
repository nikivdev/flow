use std::env;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::cli::HealthOpts;
use crate::doctor;
use crate::setup::add_gitignore_entry;

pub fn run(_opts: HealthOpts) -> Result<()> {
    println!("Running flow health checks...\n");

    ensure_fish_shell()?;
    ensure_fish_flow_init()?;
    ensure_gitignore()?;

    doctor::run(crate::cli::DoctorOpts {})?;

    println!("\n✅ flow health checks passed.");
    Ok(())
}

fn ensure_fish_shell() -> Result<()> {
    let shell = env::var("SHELL").unwrap_or_default();
    if !shell.contains("fish") {
        let fish = which::which("fish")
            .context("fish is required; install it and ensure it is on PATH")?;
        bail!(
            "fish shell required. Run:\n  chsh -s {}",
            fish.display()
        );
    }
    Ok(())
}

fn ensure_fish_flow_init() -> Result<()> {
    let config_path = fish_config_path()?;
    let content = fs::read_to_string(&config_path).unwrap_or_default();
    if content.contains("# flow:start") {
        return Ok(());
    }

    println!(
        "⚠ flow fish integration missing in {}. Run: f shell-init fish",
        config_path.display()
    );
    Ok(())
}

fn ensure_gitignore() -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let Some(flow_path) = find_flow_toml_upwards(&cwd) else {
        return Ok(());
    };
    let root = flow_path.parent().unwrap_or(&cwd);

    if !root.join(".git").exists() {
        return Ok(());
    }

    add_gitignore_entry(root, ".ai/todos/*.bike")?;
    add_gitignore_entry(root, ".ai/review-log.jsonl")?;
    Ok(())
}

fn find_flow_toml_upwards(start: &PathBuf) -> Option<PathBuf> {
    let mut current = start.as_path();
    loop {
        let candidate = current.join("flow.toml");
        if candidate.exists() {
            return Some(candidate);
        }
        current = current.parent()?;
    }
}

fn fish_config_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("failed to resolve home directory")?;
    Ok(home.join("config").join("fish").join("config.fish"))
}

 
