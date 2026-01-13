//! Codex skills management.
//!
//! Skills are stored in .ai/skills/<name>/skill.md

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::cli::{SkillsAction, SkillsCommand};
use crate::config;

/// Run the skills subcommand.
pub fn run(cmd: SkillsCommand) -> Result<()> {
    let action = cmd.action.unwrap_or(SkillsAction::List);

    match action {
        SkillsAction::List => list_skills()?,
        SkillsAction::New { name, description } => new_skill(&name, description.as_deref())?,
        SkillsAction::Show { name } => show_skill(&name)?,
        SkillsAction::Edit { name } => edit_skill(&name)?,
        SkillsAction::Remove { name } => remove_skill(&name)?,
        SkillsAction::Install { name } => install_skill(&name)?,
        SkillsAction::Search { query } => list_remote_skills(query.as_deref())?,
        SkillsAction::Sync => sync_skills()?,
    }

    Ok(())
}

/// Get the skills directory for the current project.
fn get_skills_dir() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    Ok(cwd.join(".ai").join("skills"))
}

/// Ensure symlinks exist from .claude/skills and .codex/skills to .ai/skills
fn ensure_symlinks() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let ai_skills = cwd.join(".ai").join("skills");

    if !ai_skills.exists() {
        return Ok(());
    }

    // Create .claude/skills -> .ai/skills
    let claude_dir = cwd.join(".claude");
    let claude_skills = claude_dir.join("skills");
    create_symlink_if_needed(&ai_skills, &claude_dir, &claude_skills)?;

    // Create .codex/skills -> .ai/skills
    let codex_dir = cwd.join(".codex");
    let codex_skills = codex_dir.join("skills");
    create_symlink_if_needed(&ai_skills, &codex_dir, &codex_skills)?;

    Ok(())
}

/// Create a symlink if it doesn't exist or points elsewhere.
fn create_symlink_if_needed(
    target: &PathBuf,
    parent_dir: &PathBuf,
    link_path: &PathBuf,
) -> Result<()> {
    // Create parent directory if needed
    if !parent_dir.exists() {
        fs::create_dir_all(parent_dir)?;
    }

    // Check if symlink already exists and points to correct target
    if link_path.is_symlink() {
        if let Ok(existing_target) = fs::read_link(link_path) {
            if existing_target == *target || existing_target == PathBuf::from("../.ai/skills") {
                return Ok(()); // Already correct
            }
        }
        // Wrong target, remove it
        fs::remove_file(link_path)?;
    } else if link_path.exists() {
        // It's a real directory, skip (don't overwrite user's files)
        return Ok(());
    }

    // Create relative symlink: .claude/skills -> ../.ai/skills
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        symlink("../.ai/skills", link_path)?;
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::symlink_dir;
        symlink_dir(target, link_path)?;
    }

    Ok(())
}

/// List all skills in the project.
fn list_skills() -> Result<()> {
    let skills_dir = get_skills_dir()?;

    if !skills_dir.exists() {
        println!("No skills found. Create one with: f skills new <name>");
        return Ok(());
    }

    let entries = fs::read_dir(&skills_dir).context("failed to read skills directory")?;

    let mut skills: Vec<(String, Option<String>)> = Vec::new();

    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();

            let skill_file = path.join("skill.md");
            let description = if skill_file.exists() {
                parse_skill_description(&skill_file)
            } else {
                None
            };

            skills.push((name, description));
        }
    }

    if skills.is_empty() {
        println!("No skills found. Create one with: f skills new <name>");
        return Ok(());
    }

    skills.sort_by(|a, b| a.0.cmp(&b.0));

    println!("Skills in .ai/skills/:\n");
    for (name, desc) in skills {
        if let Some(d) = desc {
            println!("  {} - {}", name, d);
        } else {
            println!("  {}", name);
        }
    }

    Ok(())
}

/// Parse the description from a skill.md file.
fn parse_skill_description(path: &PathBuf) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;

    // Look for description in YAML frontmatter
    if content.starts_with("---") {
        let parts: Vec<&str> = content.splitn(3, "---").collect();
        if parts.len() >= 2 {
            for line in parts[1].lines() {
                let line = line.trim();
                if line.starts_with("description:") {
                    return Some(line.trim_start_matches("description:").trim().to_string());
                }
            }
        }
    }

    None
}

/// Create a new skill.
fn new_skill(name: &str, description: Option<&str>) -> Result<()> {
    let skills_dir = get_skills_dir()?;
    let skill_dir = skills_dir.join(name);

    if skill_dir.exists() {
        bail!("Skill '{}' already exists", name);
    }

    // Create skill directory
    fs::create_dir_all(&skill_dir).context("failed to create skill directory")?;

    // Create skill.md
    let desc = description.unwrap_or("TODO: Add description");
    let content = format!(
        r#"---
name: {}
description: {}
---

# {}

## Instructions

TODO: Add instructions for this skill.

## Examples

```bash
# Example usage
```
"#,
        name, desc, name
    );

    let skill_file = skill_dir.join("skill.md");
    fs::write(&skill_file, content).context("failed to write skill.md")?;

    // Ensure symlinks exist for Claude Code and Codex
    ensure_symlinks()?;

    println!("Created skill: {}", skill_dir.display());
    println!("\nEdit it with: f skills edit {}", name);

    Ok(())
}

/// Show skill details.
fn show_skill(name: &str) -> Result<()> {
    let skills_dir = get_skills_dir()?;
    let skill_file = skills_dir.join(name).join("skill.md");

    if !skill_file.exists() {
        bail!("Skill '{}' not found", name);
    }

    let content = fs::read_to_string(&skill_file).context("failed to read skill.md")?;

    println!("{}", content);

    Ok(())
}

/// Edit a skill in the user's editor.
fn edit_skill(name: &str) -> Result<()> {
    let skills_dir = get_skills_dir()?;
    let skill_file = skills_dir.join(name).join("skill.md");

    if !skill_file.exists() {
        bail!(
            "Skill '{}' not found. Create it with: f skills new {}",
            name,
            name
        );
    }

    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vim".to_string());

    Command::new(&editor)
        .arg(&skill_file)
        .status()
        .with_context(|| format!("failed to open editor: {}", editor))?;

    Ok(())
}

/// Remove a skill.
fn remove_skill(name: &str) -> Result<()> {
    let skills_dir = get_skills_dir()?;
    let skill_dir = skills_dir.join(name);

    if !skill_dir.exists() {
        bail!("Skill '{}' not found", name);
    }

    fs::remove_dir_all(&skill_dir).context("failed to remove skill directory")?;

    println!("Removed skill: {}", name);

    Ok(())
}

const SKILLS_API_URL: &str = "https://myflow.sh/api/skills";

/// Install a skill from the global skills registry.
fn install_skill(name: &str) -> Result<()> {
    println!("Fetching skill '{}' from registry...", name);

    // Fetch skill from API
    let url = format!("{}?name={}", SKILLS_API_URL, name);
    let response = reqwest::blocking::get(&url).context("failed to fetch skill from registry")?;

    if response.status() == 404 {
        bail!("Skill '{}' not found in registry", name);
    }

    if !response.status().is_success() {
        bail!("Failed to fetch skill: HTTP {}", response.status());
    }

    let skill: SkillResponse = response.json().context("failed to parse skill response")?;

    // Create skill directory
    let skills_dir = get_skills_dir()?;
    let skill_dir = skills_dir.join(name);

    if skill_dir.exists() {
        bail!(
            "Skill '{}' already exists locally. Remove it first with: f skills remove {}",
            name,
            name
        );
    }

    fs::create_dir_all(&skill_dir)?;

    // Write skill.md
    let skill_file = skill_dir.join("skill.md");
    fs::write(&skill_file, &skill.content)?;

    // Ensure symlinks
    ensure_symlinks()?;

    println!("Installed skill: {}", name);
    println!(
        "  Source: {}",
        skill.source.unwrap_or_else(|| "unknown".to_string())
    );
    if let Some(author) = skill.author {
        println!("  Author: {}", author);
    }

    Ok(())
}

#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
struct SkillResponse {
    name: String,
    description: String,
    content: String,
    source: Option<String>,
    author: Option<String>,
}

/// List available skills from the registry.
fn list_remote_skills(search: Option<&str>) -> Result<()> {
    let url = if let Some(q) = search {
        format!("{}?search={}", SKILLS_API_URL, q)
    } else {
        SKILLS_API_URL.to_string()
    };

    let response = reqwest::blocking::get(&url).context("failed to fetch skills from registry")?;

    if !response.status().is_success() {
        bail!("Failed to fetch skills: HTTP {}", response.status());
    }

    let skills: Vec<SkillListItem> = response.json().context("failed to parse skills response")?;

    if skills.is_empty() {
        println!("No skills found in registry.");
        return Ok(());
    }

    println!("Available skills from registry:\n");
    for skill in skills {
        let source = skill.source.unwrap_or_else(|| "unknown".to_string());
        println!("  {} [{}]", skill.name, source);
        println!("    {}", skill.description);
        println!();
    }

    println!("Install with: f skills install <name>");

    Ok(())
}

#[derive(Debug, serde::Deserialize)]
struct SkillListItem {
    name: String,
    description: String,
    source: Option<String>,
}

/// Sync flow.toml tasks as skills.
fn sync_skills() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let flow_toml = cwd.join("flow.toml");

    if !flow_toml.exists() {
        bail!("No flow.toml found in current directory");
    }

    // Load flow.toml
    let cfg = config::load(&flow_toml)?;

    let skills_dir = get_skills_dir()?;
    fs::create_dir_all(&skills_dir)?;

    let mut created = 0;
    let mut updated = 0;

    for task in &cfg.tasks {
        let skill_dir = skills_dir.join(&task.name);
        let skill_file = skill_dir.join("skill.md");

        let existed = skill_file.exists();

        fs::create_dir_all(&skill_dir)?;

        let desc = task.description.as_deref().unwrap_or("Flow task");
        let content = format!(
            r#"---
name: {}
description: {}
source: flow.toml
---

# {}

{}

## Usage

Run this task with:

```bash
f {}
```

## Command

```bash
{}
```
"#,
            task.name,
            desc,
            task.name,
            desc,
            task.name,
            task.command.lines().collect::<Vec<_>>().join("\n")
        );

        fs::write(&skill_file, content)?;

        if existed {
            updated += 1;
        } else {
            created += 1;
        }
    }

    // Ensure symlinks exist for Claude Code and Codex
    ensure_symlinks()?;

    println!("Synced {} tasks from flow.toml", cfg.tasks.len());
    if created > 0 {
        println!("  Created: {}", created);
    }
    if updated > 0 {
        println!("  Updated: {}", updated);
    }
    println!("\nSymlinked to .claude/skills/ and .codex/skills/");

    Ok(())
}
