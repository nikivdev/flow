//! Fuzzy search through all Flow CLI commands.

use anyhow::{Context, Result};
use clap::{Command, CommandFactory};
use serde::Serialize;
use std::io::Write;
use std::process::{Command as Cmd, Stdio};

use crate::cli::Cli;

/// Entry format compatible with the `cmd` tool's cache format.
#[derive(Serialize)]
struct Entry {
    command: String,
    short: Option<String>,
    long: Option<String>,
    description: String,
    entry_type: String,
}

#[derive(Serialize)]
struct CommandInfo {
    version: String,
    entries: Vec<Entry>,
}

/// Collect all commands recursively from clap's command tree.
fn collect_commands(cmd: &Command, prefix: &str, entries: &mut Vec<(String, String)>) {
    let name = cmd.get_name();
    let full_path = if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{} {}", prefix, name)
    };

    if let Some(about) = cmd.get_about() {
        entries.push((full_path.clone(), about.to_string()));
    }

    for sub in cmd.get_subcommands() {
        if !sub.is_hide_set() {
            collect_commands(sub, &full_path, entries);
        }
    }
}

/// Run fuzzy search over all Flow commands.
pub fn run() -> Result<()> {
    let cmd = Cli::command();
    let mut entries = Vec::new();

    for sub in cmd.get_subcommands() {
        if !sub.is_hide_set() {
            collect_commands(sub, "f", &mut entries);
        }
    }

    // Format for fzf: command<tab>description
    let input = entries
        .iter()
        .map(|(cmd, desc)| format!("{}\t{}", cmd, desc))
        .collect::<Vec<_>>()
        .join("\n");

    let mut fzf = Cmd::new("fzf")
        .args([
            "--height=50%",
            "--reverse",
            "--delimiter=\t",
            "--with-nth=1,2",
            "--preview-window=hidden",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("failed to spawn fzf - is it installed?")?;

    fzf.stdin.as_mut().unwrap().write_all(input.as_bytes())?;

    let output = fzf.wait_with_output()?;
    if !output.status.success() {
        return Ok(()); // User cancelled
    }

    let selected = String::from_utf8_lossy(&output.stdout)
        .trim()
        .split('\t')
        .next()
        .unwrap_or("")
        .to_string();

    if !selected.is_empty() {
        // Show help for selected command
        println!();
        let parts: Vec<&str> = selected.split_whitespace().skip(1).collect();
        let mut cmd = Cmd::new("f");
        cmd.args(&parts);
        cmd.arg("--help");
        cmd.status()?;
    }

    Ok(())
}

/// Collect all commands and flags recursively in cmd-tool format.
fn collect_entries(cmd: &Command, prefix: &str, entries: &mut Vec<Entry>) {
    let name = cmd.get_name();
    let full_path = if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{} {}", prefix, name)
    };

    // Add the subcommand itself
    if let Some(about) = cmd.get_about() {
        entries.push(Entry {
            command: full_path.clone(),
            short: None,
            long: None,
            description: about.to_string(),
            entry_type: "subcommand".to_string(),
        });
    }

    // Add flags/options for this command
    for arg in cmd.get_arguments() {
        if arg.is_hide_set() {
            continue;
        }
        let short = arg.get_short().map(|c| format!("-{}", c));
        let long = arg.get_long().map(|s| format!("--{}", s));

        // Skip if no flag representation
        if short.is_none() && long.is_none() {
            continue;
        }

        let description = arg.get_help().map(|h| h.to_string()).unwrap_or_default();

        entries.push(Entry {
            command: full_path.clone(),
            short,
            long,
            description,
            entry_type: "flag".to_string(),
        });
    }

    // Recurse into subcommands
    for sub in cmd.get_subcommands() {
        if !sub.is_hide_set() {
            collect_entries(sub, &full_path, entries);
        }
    }
}

/// Output all commands in JSON format compatible with the `cmd` tool.
pub fn print_full_json() -> Result<()> {
    let cmd = Cli::command();
    let mut entries = Vec::new();

    for sub in cmd.get_subcommands() {
        if !sub.is_hide_set() {
            collect_entries(sub, "f", &mut entries);
        }
    }

    // Also add top-level flags
    for arg in cmd.get_arguments() {
        if arg.is_hide_set() {
            continue;
        }
        let short = arg.get_short().map(|c| format!("-{}", c));
        let long = arg.get_long().map(|s| format!("--{}", s));

        if short.is_none() && long.is_none() {
            continue;
        }

        let description = arg.get_help().map(|h| h.to_string()).unwrap_or_default();

        entries.push(Entry {
            command: "f".to_string(),
            short,
            long,
            description,
            entry_type: "flag".to_string(),
        });
    }

    let version = env!("CARGO_PKG_VERSION").to_string();
    let info = CommandInfo { version, entries };

    let json = serde_json::to_string(&info)?;
    println!("{}", json);

    Ok(())
}
