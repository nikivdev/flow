//! AI tools management - execute TypeScript tools via localcode/bun.
//!
//! Tools are stored in .ai/tools/<name>.ts

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::cli::{ToolsAction, ToolsCommand};

/// Run the tools subcommand.
pub fn run(cmd: ToolsCommand) -> Result<()> {
    let action = cmd.action.unwrap_or(ToolsAction::List);

    match action {
        ToolsAction::List => list_tools()?,
        ToolsAction::Run { name, args } => run_tool(&name, args)?,
        ToolsAction::New {
            name,
            description,
            ai,
        } => new_tool(&name, description.as_deref(), ai)?,
        ToolsAction::Edit { name } => edit_tool(&name)?,
        ToolsAction::Remove { name } => remove_tool(&name)?,
    }

    Ok(())
}

/// Get the tools directory for the current project.
fn get_tools_dir() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    Ok(cwd.join(".ai").join("tools"))
}

/// Find the localcode binary (our opencode fork).
fn find_localcode() -> Option<PathBuf> {
    // Check ~/.local/bin/localcode first
    if let Some(home) = dirs::home_dir() {
        let local_bin = home.join(".local/bin/localcode");
        if local_bin.exists() {
            return Some(local_bin);
        }
    }

    // Fall back to PATH
    which::which("localcode").ok()
}

/// List all tools in the project.
fn list_tools() -> Result<()> {
    let tools_dir = get_tools_dir()?;

    if !tools_dir.exists() {
        println!("No tools found. Create one with: f tools new <name>");
        return Ok(());
    }

    let entries = fs::read_dir(&tools_dir).context("failed to read tools directory")?;

    let mut tools: Vec<(String, Option<String>)> = Vec::new();

    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        if path.extension().map_or(false, |e| e == "ts") {
            let name = path
                .file_stem()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();

            let description = parse_tool_description(&path);
            tools.push((name, description));
        }
    }

    if tools.is_empty() {
        println!("No tools found. Create one with: f tools new <name>");
        return Ok(());
    }

    tools.sort_by(|a, b| a.0.cmp(&b.0));

    println!("Tools in .ai/tools/:\n");
    for (name, desc) in tools {
        if let Some(d) = desc {
            println!("  {} - {}", name, d);
        } else {
            println!("  {}", name);
        }
    }

    println!("\nRun with: f tools run <name>");

    Ok(())
}

/// Parse description from first comment line in a .ts file.
fn parse_tool_description(path: &PathBuf) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("// ") {
            return Some(trimmed.trim_start_matches("// ").to_string());
        }
        if trimmed.starts_with("///") {
            return Some(trimmed.trim_start_matches("///").trim().to_string());
        }
        // Skip empty lines at the top
        if !trimmed.is_empty() && !trimmed.starts_with("//") {
            break;
        }
    }

    None
}

/// Run a tool via bun.
fn run_tool(name: &str, args: Vec<String>) -> Result<()> {
    let tools_dir = get_tools_dir()?;
    let tool_file = tools_dir.join(format!("{}.ts", name));

    if !tool_file.exists() {
        bail!(
            "Tool '{}' not found. Create it with: f tools new {}",
            name,
            name
        );
    }

    let status = Command::new("bun")
        .arg("run")
        .arg(&tool_file)
        .args(&args)
        .status()
        .context("failed to run bun")?;

    if !status.success() {
        bail!("Tool '{}' exited with status: {}", name, status);
    }

    Ok(())
}

/// Create a new tool.
fn new_tool(name: &str, description: Option<&str>, use_ai: bool) -> Result<()> {
    let tools_dir = get_tools_dir()?;
    fs::create_dir_all(&tools_dir).context("failed to create tools directory")?;

    let tool_file = tools_dir.join(format!("{}.ts", name));

    if tool_file.exists() {
        bail!("Tool '{}' already exists", name);
    }

    if use_ai {
        // Use localcode to generate the tool
        let localcode = find_localcode();
        if localcode.is_none() {
            bail!(
                "localcode not found. Install it with:\n  \
                 cd <opencode-repo> && flow link"
            );
        }

        let desc = description.unwrap_or(name);
        let prompt = format!(
            "Create a TypeScript tool for Bun called '{}' that: {}\n\n\
             Requirements:\n\
             - Use Bun APIs (Bun.$, Bun.file, etc.)\n\
             - Add a description comment at the top\n\
             - Handle CLI args via Bun.argv\n\
             - Save to: {}",
            name,
            desc,
            tool_file.display()
        );

        println!("Generating tool '{}' with AI...\n", name);

        let status = Command::new(localcode.unwrap())
            .arg("--print")
            .arg(&prompt)
            .status()
            .context("failed to run localcode")?;

        if !status.success() {
            bail!("AI generation failed with status: {}", status);
        }

        if tool_file.exists() {
            println!("\nCreated tool: {}", tool_file.display());
            println!("Run it with:  f tools run {}", name);
        }
    } else {
        // Create template
        let desc = description.unwrap_or("TODO: Add description");
        let content = format!(
            r#"// {desc}

import {{ $ }} from "bun"

const args = Bun.argv.slice(2)

// TODO: Implement tool logic
console.log("{name} tool running with args:", args)
"#,
            desc = desc,
            name = name
        );

        fs::write(&tool_file, content).context("failed to write tool file")?;

        println!("Created tool: {}", tool_file.display());
        println!("\nEdit it with: f tools edit {}", name);
        println!("Run it with:  f tools run {}", name);
    }

    Ok(())
}

/// Edit a tool in the user's editor.
fn edit_tool(name: &str) -> Result<()> {
    let tools_dir = get_tools_dir()?;
    let tool_file = tools_dir.join(format!("{}.ts", name));

    if !tool_file.exists() {
        bail!(
            "Tool '{}' not found. Create it with: f tools new {}",
            name,
            name
        );
    }

    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vim".to_string());

    Command::new(&editor)
        .arg(&tool_file)
        .status()
        .with_context(|| format!("failed to open editor: {}", editor))?;

    Ok(())
}

/// Remove a tool.
fn remove_tool(name: &str) -> Result<()> {
    let tools_dir = get_tools_dir()?;
    let tool_file = tools_dir.join(format!("{}.ts", name));

    if !tool_file.exists() {
        bail!("Tool '{}' not found", name);
    }

    fs::remove_file(&tool_file).context("failed to remove tool file")?;

    println!("Removed tool: {}", name);

    Ok(())
}
