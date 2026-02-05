use std::env;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::cli::FishInstallOpts;
use crate::fish_trace;

pub fn run(opts: FishInstallOpts) -> Result<()> {
    let bin_dir = opts.bin_dir.unwrap_or_else(default_bin_dir);
    let fish_bin = bin_dir.join("fish");

    // Check if already installed
    if fish_bin.exists() && !opts.force {
        if is_traced_fish(&fish_bin)? {
            println!("Traced fish is already installed at {}", fish_bin.display());
            println!("Use --force to reinstall.");
            return Ok(());
        }
        if !opts.yes && !confirm_overwrite(&fish_bin)? {
            bail!("Aborted.");
        }
    }

    // Find fish source
    let source = match opts.source {
        Some(path) => {
            if !path.join("Cargo.toml").exists() {
                bail!(
                    "No Cargo.toml found at {}. Is this the fish-shell repo?",
                    path.display()
                );
            }
            path
        }
        None => {
            let Some(path) = fish_trace::fish_source_path() else {
                bail!(
                    "Could not find fish-shell source. Please specify --source or set FISH_SOURCE_PATH.\n\
                     Clone from: https://github.com/fish-shell/fish-shell"
                );
            };
            path
        }
    };

    println!("Building traced fish from {}", source.display());

    // Confirm before building
    if !opts.yes && io::stdin().is_terminal() {
        println!();
        println!("This will:");
        println!("  1. Build fish shell with release optimizations");
        println!("  2. Install to {}", fish_bin.display());
        println!("  3. Enable always-on I/O tracing (near-zero overhead)");
        println!();
        if !confirm("Proceed?")? {
            bail!("Aborted.");
        }
    }

    // Build release
    println!("Running: cargo build --release --locked");
    let status = Command::new("cargo")
        .args(["build", "--release", "--locked"])
        .current_dir(&source)
        .status()
        .context("failed to run cargo build")?;

    if !status.success() {
        bail!("cargo build failed");
    }

    // Install
    let built_bin = source.join("target/release/fish");
    if !built_bin.exists() {
        bail!("Built binary not found at {}", built_bin.display());
    }

    fs::create_dir_all(&bin_dir)
        .with_context(|| format!("failed to create {}", bin_dir.display()))?;

    fs::copy(&built_bin, &fish_bin)
        .with_context(|| format!("failed to copy to {}", fish_bin.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&fish_bin)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&fish_bin, perms)?;
    }

    println!();
    println!("Installed traced fish to {}", fish_bin.display());
    println!();
    println!("To use it:");
    println!("  exec {}", fish_bin.display());
    println!();
    println!("Or add {} to your PATH.", bin_dir.display());
    println!();
    println!("I/O tracing is enabled by default. View traces with:");
    println!("  f fish-last        # last command + output");
    println!("  f fish-last-full   # full details");
    println!("  f last-cmd         # (same as fish-last when traced fish is active)");

    if !path_in_env(&bin_dir) {
        println!();
        println!("Note: {} is not in your PATH.", bin_dir.display());
        println!("Add it with: fish_add_path {}", bin_dir.display());
    }

    Ok(())
}

fn default_bin_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local")
        .join("bin")
}

fn is_traced_fish(path: &PathBuf) -> Result<bool> {
    // Check if the fish binary has our tracing markers
    let output = Command::new(path)
        .args(["-c", "echo $fish_io_trace"])
        .output();

    // If it runs without error, it might be our fork
    // A more reliable check would be to look for specific version strings
    match output {
        Ok(out) => {
            // Our traced fish defaults to "metadata" mode
            let stdout = String::from_utf8_lossy(&out.stdout);
            // Check if fish_io_trace variable exists (it's set by default in our fork)
            Ok(stdout.trim() == "metadata" || stdout.contains("metadata"))
        }
        Err(_) => Ok(false),
    }
}

fn confirm_overwrite(path: &PathBuf) -> Result<bool> {
    if !io::stdin().is_terminal() {
        return Ok(false);
    }
    print!("{} already exists. Overwrite? [y/N]: ", path.display());
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

fn confirm(msg: &str) -> Result<bool> {
    if !io::stdin().is_terminal() {
        return Ok(true);
    }
    print!("{} [y/N]: ", msg);
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

fn path_in_env(bin_dir: &PathBuf) -> bool {
    let Ok(path) = env::var("PATH") else {
        return false;
    };
    env::split_paths(&path).any(|entry| entry == *bin_dir)
}
