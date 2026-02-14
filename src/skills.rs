//! Codex skills management.
//!
//! Skills are stored in .ai/skills/<name>/skill.md (gitignored by default).

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::json;

use crate::cli::{SkillsAction, SkillsCommand, SkillsFetchAction, SkillsFetchCommand};
use crate::config;
use crate::start;

const DEFAULT_ENV_SKILL: &str = include_str!("../.ai/skills/env/skill.md");
const DEFAULT_QUALITY_BUN_FEATURE_DELIVERY_SKILL: &str =
    include_str!("../.ai/skills/quality-bun-feature-delivery/skill.md");

#[derive(Debug, Default)]
pub struct SkillsEnforceSummary {
    pub task_skills_created: usize,
    pub task_skills_updated: usize,
    pub installed_skills: Vec<String>,
}

impl SkillsEnforceSummary {
    pub fn is_noop(&self) -> bool {
        self.task_skills_created == 0
            && self.task_skills_updated == 0
            && self.installed_skills.is_empty()
    }
}

#[derive(Debug, Clone, Copy)]
struct SkillSyncOptions {
    generate_openai_yaml: bool,
    task_skill_allow_implicit_invocation: bool,
}

impl Default for SkillSyncOptions {
    fn default() -> Self {
        Self {
            generate_openai_yaml: true,
            task_skill_allow_implicit_invocation: false,
        }
    }
}

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
        SkillsAction::Publish { name } => publish_skill(&name)?,
        SkillsAction::Search { query } => list_remote_skills(query.as_deref())?,
        SkillsAction::Sync => sync_skills()?,
        SkillsAction::Reload => reload_skills()?,
        SkillsAction::Fetch(fetch) => fetch_skills(&fetch)?,
    }

    Ok(())
}

/// Get the skills directory for the current project.
fn get_skills_dir() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    Ok(get_skills_dir_at(&cwd))
}

fn get_skills_dir_at(project_root: &Path) -> PathBuf {
    project_root.join(".ai").join("skills")
}

pub fn read_skill_content_at(project_root: &Path, name: &str) -> Result<Option<String>> {
    let skill_dir = get_skills_dir_at(project_root).join(name);
    let Some(skill_file) = find_skill_file(&skill_dir) else {
        return Ok(None);
    };
    let content = fs::read_to_string(&skill_file)
        .with_context(|| format!("failed to read {}", skill_file.display()))?;
    Ok(Some(content))
}

pub fn read_skill_frontmatter_field_at(
    project_root: &Path,
    name: &str,
    field: &str,
) -> Result<Option<String>> {
    let Some(content) = read_skill_content_at(project_root, name)? else {
        return Ok(None);
    };
    Ok(parse_frontmatter_field(&content, field))
}

pub fn read_skill_version_at(project_root: &Path, name: &str) -> Result<Option<u32>> {
    let Some(raw) = read_skill_frontmatter_field_at(project_root, name, "version")? else {
        return Ok(None);
    };
    let trimmed = raw.trim().trim_matches('"').trim_matches('\'');
    if trimmed.is_empty() {
        return Ok(None);
    }
    match trimmed.parse::<u32>() {
        Ok(version) => Ok(Some(version)),
        Err(_) => Ok(None),
    }
}

fn skill_file_lower(skill_dir: &Path) -> PathBuf {
    skill_dir.join("skill.md")
}

fn skill_file_upper(skill_dir: &Path) -> PathBuf {
    skill_dir.join("SKILL.md")
}

fn find_skill_file(skill_dir: &Path) -> Option<PathBuf> {
    let lower = skill_file_lower(skill_dir);
    if lower.exists() {
        return Some(lower);
    }
    let upper = skill_file_upper(skill_dir);
    if upper.exists() {
        return Some(upper);
    }
    None
}

fn normalize_single_skill_file(skill_dir: &Path) -> Result<bool> {
    let lower = skill_file_lower(skill_dir);
    if lower.exists() {
        return Ok(false);
    }
    let upper = skill_file_upper(skill_dir);
    if !upper.exists() {
        return Ok(false);
    }
    fs::rename(&upper, &lower)?;
    Ok(true)
}

fn normalize_skill_files(skills_dir: &Path) -> Result<usize> {
    if !skills_dir.exists() {
        return Ok(0);
    }
    let mut renamed = 0usize;
    for entry in fs::read_dir(skills_dir).context("failed to read skills directory")? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() && normalize_single_skill_file(&path)? {
            renamed += 1;
        }
    }
    Ok(renamed)
}

/// Ensure symlinks exist from .claude/skills and .codex/skills to .ai/skills
fn ensure_symlinks() -> Result<()> {
    let cwd = std::env::current_dir()?;
    ensure_symlinks_at(&cwd)
}

fn ensure_symlinks_at(project_root: &Path) -> Result<()> {
    let ai_skills = project_root.join(".ai").join("skills");

    if !ai_skills.exists() {
        return Ok(());
    }

    // Create .claude/skills -> .ai/skills
    let claude_dir = project_root.join(".claude");
    let claude_skills = claude_dir.join("skills");
    create_symlink_if_needed(&ai_skills, &claude_dir, &claude_skills)?;

    // Create .codex/skills -> .ai/skills
    let codex_dir = project_root.join(".codex");
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

            let description =
                find_skill_file(&path).and_then(|skill_file| parse_skill_description(&skill_file));

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
fn parse_skill_description(path: &Path) -> Option<String> {
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

fn resolve_skill_sync_options(skills_cfg: Option<&config::SkillsConfig>) -> SkillSyncOptions {
    let mut options = SkillSyncOptions::default();
    if let Some(codex_cfg) = skills_cfg.and_then(|cfg| cfg.codex.as_ref()) {
        if let Some(value) = codex_cfg.generate_openai_yaml {
            options.generate_openai_yaml = value;
        }
        if let Some(value) = codex_cfg.task_skill_allow_implicit_invocation {
            options.task_skill_allow_implicit_invocation = value;
        }
    }
    options
}

fn should_force_reload_after_sync(skills_cfg: Option<&config::SkillsConfig>) -> bool {
    skills_cfg
        .and_then(|cfg| cfg.codex.as_ref())
        .and_then(|cfg| cfg.force_reload_after_sync)
        .unwrap_or(true)
}

fn task_name_to_display_name(task_name: &str) -> String {
    task_name
        .split(['-', '_', ' '])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            let Some(first) = chars.next() else {
                return String::new();
            };
            format!(
                "{}{}",
                first.to_ascii_uppercase(),
                chars.as_str().to_ascii_lowercase()
            )
        })
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let mut out = String::new();
    for (idx, ch) in value.chars().enumerate() {
        if idx >= max_chars.saturating_sub(1) {
            break;
        }
        out.push(ch);
    }
    out.push('…');
    out
}

fn yaml_quote(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', " ")
    )
}

fn render_task_skill_openai_yaml(
    task: &config::TaskConfig,
    allow_implicit_invocation: bool,
) -> String {
    let desc = task.description.as_deref().unwrap_or("Flow task");
    let display_name = task_name_to_display_name(&task.name);
    let short_description = truncate_chars(desc, 64);
    let default_prompt = format!("Use ${} to {}.", task.name, desc.trim_end_matches('.'));

    format!(
        "interface:\n  display_name: {}\n  short_description: {}\n  default_prompt: {}\n\npolicy:\n  allow_implicit_invocation: {}\n",
        yaml_quote(&display_name),
        yaml_quote(&short_description),
        yaml_quote(&default_prompt),
        if allow_implicit_invocation {
            "true"
        } else {
            "false"
        }
    )
}

fn write_task_skill_metadata(
    skill_dir: &Path,
    task: &config::TaskConfig,
    options: SkillSyncOptions,
) -> Result<()> {
    if !options.generate_openai_yaml {
        return Ok(());
    }

    let agents_dir = skill_dir.join("agents");
    let metadata_path = agents_dir.join("openai.yaml");
    let content = render_task_skill_openai_yaml(task, options.task_skill_allow_implicit_invocation);

    let should_write = match fs::read_to_string(&metadata_path) {
        Ok(existing) => existing != content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
        Err(err) => return Err(err.into()),
    };
    if should_write {
        fs::create_dir_all(&agents_dir)?;
        fs::write(&metadata_path, content)?;
    }

    Ok(())
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
    let skill_dir = skills_dir.join(name);
    let Some(skill_file) = find_skill_file(&skill_dir) else {
        bail!("Skill '{}' not found", name);
    };

    let content = fs::read_to_string(&skill_file).context("failed to read skill.md")?;

    println!("{}", content);

    Ok(())
}

/// Edit a skill in the user's editor.
fn edit_skill(name: &str) -> Result<()> {
    let skills_dir = get_skills_dir()?;
    let skill_dir = skills_dir.join(name);
    let skill_file = if normalize_single_skill_file(&skill_dir)? {
        skill_file_lower(&skill_dir)
    } else if let Some(path) = find_skill_file(&skill_dir) {
        path
    } else {
        bail!(
            "Skill '{}' not found. Create it with: f skills new {}",
            name,
            name
        );
    };

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

/// Publish a local skill to the shared registry.
fn publish_skill(name: &str) -> Result<()> {
    let skills_dir = get_skills_dir()?;
    let skill_dir = skills_dir.join(name);
    let Some(skill_file) = find_skill_file(&skill_dir) else {
        bail!(
            "Skill '{}' not found locally. Create it first with: f skills new {}",
            name,
            name
        );
    };

    let content = fs::read_to_string(&skill_file).context("failed to read skill.md")?;

    // Parse description from YAML frontmatter
    let description = parse_frontmatter_field(&content, "description")
        .unwrap_or_else(|| format!("{} skill", name));

    // Get auth token
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    let token = myflow_token(&cwd).ok_or_else(|| {
        anyhow::anyhow!(
            "No myflow token found. Set MYFLOW_TOKEN env var, add myflow_token to flow.toml, or run `f auth login`"
        )
    })?;

    println!("Publishing skill '{}'...", name);

    let client = reqwest::blocking::Client::new();
    let response = client
        .put(SKILLS_API_URL)
        .header("Authorization", format!("Bearer {}", token))
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "name": name,
            "description": description,
            "content": content,
            "source": "flow-cli",
        }))
        .send()
        .context("failed to publish skill to registry")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        bail!("Failed to publish skill: HTTP {} — {}", status, body);
    }

    println!("Published skill '{}' to registry.", name);
    println!("Others can install it with: f skills install {}", name);

    Ok(())
}

/// Parse a field value from YAML frontmatter (between --- delimiters).
fn parse_frontmatter_field(content: &str, field: &str) -> Option<String> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return None;
    }
    let after_start = &trimmed[3..];
    let end = after_start.find("\n---")?;
    let frontmatter = &after_start[..end];
    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(&format!("{}:", field)) {
            let value = rest.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Get myflow auth token from env, flow.toml, or auth.toml.
fn myflow_token(repo_root: &Path) -> Option<String> {
    // 1. Check env var
    if let Ok(value) = std::env::var("MYFLOW_TOKEN") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    // 2. Check flow.toml
    let local_config = repo_root.join("flow.toml");
    if local_config.exists() {
        if let Ok(cfg) = config::load(&local_config) {
            if let Some(token) = cfg.options.myflow_token {
                let trimmed = token.trim().to_string();
                if !trimmed.is_empty() {
                    return Some(trimmed);
                }
            }
        }
    }

    // 3. Fall back to ~/.config/flow/auth.toml token
    let config_dir = dirs::config_dir()?.join("flow");
    let auth_path = config_dir.join("auth.toml");
    if auth_path.exists() {
        if let Ok(content) = fs::read_to_string(&auth_path) {
            if let Ok(auth) = toml::from_str::<toml::Value>(&content) {
                if let Some(token) = auth.get("token").and_then(|v| v.as_str()) {
                    let trimmed = token.trim();
                    if !trimmed.is_empty() {
                        return Some(trimmed.to_string());
                    }
                }
            }
        }
    }

    None
}

const SKILLS_API_URL: &str = "https://myflow.sh/api/skills";

fn codex_skills_dir() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("CODEX_HOME").map(PathBuf::from) {
        return Some(home.join("skills"));
    }
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(home.join(".codex").join("skills"))
}

fn read_local_skill_content(name: &str) -> Option<String> {
    let skills_dir = codex_skills_dir()?;
    // Codex skills typically store the body in SKILL.md.
    let candidates = [
        skills_dir.join(name).join("SKILL.md"),
        skills_dir.join(name).join("skill.md"),
    ];
    for path in candidates {
        if let Ok(content) = fs::read_to_string(&path) {
            if !content.trim().is_empty() {
                return Some(content);
            }
        }
    }
    None
}

fn load_seq_config(project_root: &Path) -> Result<Option<config::SkillsSeqConfig>> {
    let flow_toml = project_root.join("flow.toml");
    if !flow_toml.exists() {
        return Ok(None);
    }
    let cfg = config::load(&flow_toml)?;
    Ok(cfg.skills.and_then(|skills| skills.seq))
}

fn default_seq_repo() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        return home.join("code").join("seq");
    }
    PathBuf::from("/Users/nikiv/code/seq")
}

fn resolve_path_arg(raw: &str, base: &Path) -> PathBuf {
    let expanded = config::expand_path(raw);
    if expanded.is_absolute() {
        expanded
    } else {
        base.join(expanded)
    }
}

fn resolve_seq_script_path(
    project_root: &Path,
    fetch: &SkillsFetchCommand,
    seq_cfg: Option<&config::SkillsSeqConfig>,
) -> PathBuf {
    if let Some(raw) = fetch
        .script_path
        .as_deref()
        .or_else(|| seq_cfg.and_then(|cfg| cfg.script_path.as_deref()))
    {
        return resolve_path_arg(raw, project_root);
    }

    let repo = if let Some(raw) = fetch
        .seq_repo
        .as_deref()
        .or_else(|| seq_cfg.and_then(|cfg| cfg.seq_repo.as_deref()))
    {
        resolve_path_arg(raw, project_root)
    } else {
        default_seq_repo()
    };
    repo.join("tools").join("teach_deps.py")
}

fn fetch_skills(fetch: &SkillsFetchCommand) -> Result<()> {
    let project_root = std::env::current_dir().context("failed to get current directory")?;
    let seq_cfg = load_seq_config(&project_root)?;
    let seq_cfg_ref = seq_cfg.as_ref();

    if let Some(mode) = seq_cfg_ref.and_then(|cfg| cfg.mode.as_deref()) {
        if mode != "local-cli" {
            println!(
                "warning: [skills.seq] mode='{}' is not implemented yet; using local-cli",
                mode
            );
        }
    }

    let script_path = resolve_seq_script_path(&project_root, fetch, seq_cfg_ref);
    if !script_path.exists() {
        bail!(
            "seq teach script not found at {} (set [skills.seq].script_path or --script-path)",
            script_path.display()
        );
    }

    let out_dir = fetch
        .out_dir
        .clone()
        .or_else(|| seq_cfg_ref.and_then(|cfg| cfg.out_dir.clone()))
        .unwrap_or_else(|| ".ai/skills".to_string());

    let scraper_base_url = fetch
        .scraper_base_url
        .clone()
        .or_else(|| seq_cfg_ref.and_then(|cfg| cfg.scraper_base_url.clone()));
    let scraper_api_key = fetch
        .scraper_api_key
        .clone()
        .or_else(|| seq_cfg_ref.and_then(|cfg| cfg.scraper_api_key.clone()));
    let cache_ttl_hours = fetch
        .cache_ttl_hours
        .or_else(|| seq_cfg_ref.and_then(|cfg| cfg.cache_ttl_hours));
    let allow_direct_fallback = fetch.allow_direct_fallback
        || seq_cfg_ref
            .and_then(|cfg| cfg.allow_direct_fallback)
            .unwrap_or(false);
    let mem_events_path = fetch
        .mem_events_path
        .clone()
        .or_else(|| seq_cfg_ref.and_then(|cfg| cfg.mem_events_path.clone()));

    let mut args: Vec<String> = Vec::new();
    let force = match &fetch.action {
        SkillsFetchAction::Dep {
            deps,
            ecosystem,
            force,
        } => {
            if deps.is_empty() {
                bail!("skills fetch dep requires at least one dependency");
            }
            args.push("dep".to_string());
            args.extend(deps.iter().cloned());
            if let Some(eco) = ecosystem {
                args.push("--ecosystem".to_string());
                args.push(eco.clone());
            }
            *force
        }
        SkillsFetchAction::Auto {
            top,
            ecosystems,
            force,
        } => {
            args.push("auto".to_string());
            let resolved_top = top.or_else(|| seq_cfg_ref.and_then(|cfg| cfg.top));
            if let Some(value) = resolved_top {
                args.push("--top".to_string());
                args.push(value.to_string());
            }
            let resolved_ecosystems = ecosystems
                .clone()
                .or_else(|| seq_cfg_ref.and_then(|cfg| cfg.ecosystems.clone()));
            if let Some(value) = resolved_ecosystems {
                args.push("--ecosystems".to_string());
                args.push(value);
            }
            *force
        }
        SkillsFetchAction::Url { urls, name, force } => {
            if urls.is_empty() {
                bail!("skills fetch url requires at least one URL");
            }
            args.push("url".to_string());
            args.extend(urls.iter().cloned());
            if let Some(value) = name {
                args.push("--name".to_string());
                args.push(value.clone());
            }
            *force
        }
    };

    args.push("--repo".to_string());
    args.push(project_root.display().to_string());
    args.push("--out-dir".to_string());
    args.push(out_dir.clone());

    if force {
        args.push("--force".to_string());
    }
    if let Some(value) = scraper_base_url {
        args.push("--scraper-base-url".to_string());
        args.push(value);
    }
    if let Some(value) = cache_ttl_hours {
        args.push("--cache-ttl-hours".to_string());
        args.push(value.to_string());
    }
    if allow_direct_fallback {
        args.push("--allow-direct-fallback".to_string());
    }
    if fetch.no_mem_events {
        args.push("--no-mem-events".to_string());
    }
    if let Some(value) = mem_events_path {
        args.push("--mem-events-path".to_string());
        args.push(value);
    }

    let mut cmd = Command::new("python3");
    cmd.arg(&script_path);
    cmd.args(&args);
    cmd.current_dir(&project_root);
    if let Some(api_key) = scraper_api_key {
        cmd.env("SEQ_SCRAPER_API_KEY", api_key);
    }

    let status = cmd.status().context("failed to run seq teach script")?;
    if !status.success() {
        if let Some(code) = status.code() {
            bail!("skills fetch failed with exit code {}", code);
        }
        bail!("skills fetch failed: process terminated by signal");
    }

    let out_path = {
        let parsed = PathBuf::from(&out_dir);
        if parsed.is_absolute() {
            parsed
        } else {
            project_root.join(parsed)
        }
    };
    let renamed = normalize_skill_files(&out_path)?;
    ensure_symlinks_at(&project_root)?;

    println!("Fetched skills via seq into {}", out_path.display());
    if renamed > 0 {
        println!("Normalized {} SKILL.md file(s) to skill.md", renamed);
    }
    println!("Symlinked to .claude/skills/ and .codex/skills/");

    Ok(())
}

/// Install a skill from the global skills registry.
fn install_skill(name: &str) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    let installed = install_skill_inner(&cwd, name, false, false)?;
    if installed {
        let flow_toml = cwd.join("flow.toml");
        let cfg = if flow_toml.exists() {
            config::load_or_default(&flow_toml)
        } else {
            config::Config::default()
        };
        maybe_reload_codex_skills(&cwd, cfg.skills.as_ref(), "skills install");
    }
    Ok(())
}

fn install_skill_inner(
    project_root: &Path,
    name: &str,
    allow_existing: bool,
    quiet: bool,
) -> Result<bool> {
    let skills_dir = get_skills_dir_at(project_root);
    let skill_dir = skills_dir.join(name);

    if skill_dir.exists() {
        if allow_existing {
            return Ok(false);
        }
        bail!(
            "Skill '{}' already exists locally. Remove it first with: f skills remove {}",
            name,
            name
        );
    }

    // Prefer local Codex skills (e.g. ~/.codex/skills/<name>/SKILL.md) when present.
    if let Some(content) = read_local_skill_content(name) {
        if !quiet {
            println!("Installing skill '{}' from local Codex skills...", name);
        }

        fs::create_dir_all(&skill_dir)?;
        fs::write(skill_dir.join("skill.md"), content)?;

        ensure_symlinks_at(project_root)?;

        if !quiet {
            println!("Installed skill: {}", name);
            println!("  Source: local (~/.codex/skills/)");
        }

        return Ok(true);
    }

    if !quiet {
        println!("Fetching skill '{}' from registry...", name);
    }

    // Fetch skill from API.
    let url = format!("{}?name={}", SKILLS_API_URL, name);
    let response = reqwest::blocking::get(&url).context("failed to fetch skill from registry")?;

    if response.status() == 404 {
        bail!(
            "Skill '{}' not found in local Codex skills or registry",
            name
        );
    }

    if !response.status().is_success() {
        bail!("Failed to fetch skill: HTTP {}", response.status());
    }

    let skill: SkillResponse = response.json().context("failed to parse skill response")?;

    // Create skill directory and write skill.md.
    fs::create_dir_all(&skill_dir)?;
    fs::write(skill_dir.join("skill.md"), &skill.content)?;

    // Ensure symlinks
    ensure_symlinks_at(project_root)?;

    if !quiet {
        println!("Installed skill: {}", name);
        println!(
            "  Source: {}",
            skill.source.unwrap_or_else(|| "unknown".to_string())
        );
        if let Some(author) = skill.author {
            println!("  Author: {}", author);
        }
    }

    Ok(true)
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

fn codex_write_msg(writer: &mut dyn Write, msg: &serde_json::Value) -> Result<()> {
    let mut line = serde_json::to_string(msg)?;
    line.push('\n');
    writer.write_all(line.as_bytes())?;
    writer.flush()?;
    Ok(())
}

fn codex_read_response(
    lines: &mut std::io::Lines<std::io::BufReader<std::process::ChildStdout>>,
    expected_id: u64,
    deadline: Instant,
) -> Result<serde_json::Value> {
    loop {
        if Instant::now() >= deadline {
            bail!("codex app-server response timed out");
        }
        let line = match lines.next() {
            Some(Ok(line)) => line,
            Some(Err(err)) => bail!("failed to read from codex app-server: {}", err),
            None => bail!("codex app-server closed stdout unexpectedly"),
        };
        if line.trim().is_empty() {
            continue;
        }
        let msg: serde_json::Value = serde_json::from_str(&line)
            .with_context(|| format!("invalid JSON from codex app-server: {}", line))?;
        if msg.get("id").and_then(|v| v.as_u64()) == Some(expected_id) {
            if let Some(err) = msg.get("error") {
                let message = err
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown codex app-server error");
                bail!("codex app-server error: {}", message);
            }
            return Ok(msg);
        }
    }
}

fn reload_codex_skills_for_cwd(cwd: &Path) -> Result<usize> {
    let codex_bin = std::env::var("CODEX_BIN")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "codex".to_string());

    let mut child = Command::new(&codex_bin)
        .arg("app-server")
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to run codex app-server")?;

    let mut stdin = child.stdin.take().context("missing codex stdin")?;
    let stdout = child.stdout.take().context("missing codex stdout")?;
    let mut lines = BufReader::new(stdout).lines();
    let handshake_deadline = Instant::now() + Duration::from_secs(15);

    codex_write_msg(
        &mut stdin,
        &json!({
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": { "name": "flow", "title": "Flow CLI", "version": "0.1.0" },
                "capabilities": { "experimentalApi": true }
            }
        }),
    )?;
    let _ = codex_read_response(&mut lines, 1, handshake_deadline)
        .context("codex app-server did not respond to initialize")?;
    codex_write_msg(&mut stdin, &json!({ "method": "initialized" }))?;

    let op_deadline = Instant::now() + Duration::from_secs(20);
    codex_write_msg(
        &mut stdin,
        &json!({
            "id": 2,
            "method": "skills/list",
            "params": {
                "cwds": [cwd.to_string_lossy().to_string()],
                "forceReload": true
            }
        }),
    )?;
    let response = codex_read_response(&mut lines, 2, op_deadline)?;

    let skill_count = response
        .pointer("/result/data/0/skills")
        .and_then(|v| v.as_array())
        .map(|skills| skills.len())
        .unwrap_or(0);
    let error_count = response
        .pointer("/result/data/0/errors")
        .and_then(|v| v.as_array())
        .map(|errors| errors.len())
        .unwrap_or(0);

    let _ = codex_write_msg(
        &mut stdin,
        &json!({
            "id": 3,
            "method": "shutdown"
        }),
    );
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();

    if error_count > 0 {
        eprintln!(
            "warning: Codex reported {} skill loader error(s) while reloading",
            error_count
        );
    }

    Ok(skill_count)
}

pub(crate) fn maybe_reload_codex_skills(
    project_root: &Path,
    skills_cfg: Option<&config::SkillsConfig>,
    reason: &str,
) {
    if !should_force_reload_after_sync(skills_cfg) {
        return;
    }

    match reload_codex_skills_for_cwd(project_root) {
        Ok(skill_count) => {
            println!(
                "Codex skills reloaded ({} skills) after {}",
                skill_count, reason
            );
        }
        Err(err) => {
            eprintln!(
                "warning: failed to force-reload Codex skills after {}: {}",
                reason, err
            );
        }
    }
}

fn reload_skills() -> Result<()> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    let skill_count = reload_codex_skills_for_cwd(&cwd)?;
    println!("Codex skills reloaded ({} skills)", skill_count);
    Ok(())
}

fn render_task_skill(task: &config::TaskConfig) -> String {
    let desc = task.description.as_deref().unwrap_or("Flow task");
    let command = task.command.lines().collect::<Vec<_>>().join("\n");
    format!(
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
        task.name, desc, task.name, desc, task.name, command
    )
}

fn sync_tasks_to_skills(
    skills_dir: &Path,
    tasks: &[config::TaskConfig],
    options: SkillSyncOptions,
) -> Result<(usize, usize)> {
    fs::create_dir_all(skills_dir)?;

    let mut created = 0;
    let mut updated = 0;

    for task in tasks {
        let skill_dir = skills_dir.join(&task.name);
        let skill_file = skill_dir.join("skill.md");
        let content = render_task_skill(task);
        let existed = skill_file.exists();
        let should_write = match fs::read_to_string(&skill_file) {
            Ok(existing) => existing != content,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
            Err(err) => return Err(err.into()),
        };

        if should_write {
            fs::create_dir_all(&skill_dir)?;
            fs::write(&skill_file, content)?;
            if existed {
                updated += 1;
            } else {
                created += 1;
            }
        }

        write_task_skill_metadata(&skill_dir, task, options)?;
    }

    Ok((created, updated))
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
    let options = resolve_skill_sync_options(cfg.skills.as_ref());
    let (created, updated) = sync_tasks_to_skills(&skills_dir, &cfg.tasks, options)?;

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
    maybe_reload_codex_skills(&cwd, cfg.skills.as_ref(), "skills sync");

    Ok(())
}

pub(crate) fn enforce_skills_from_config(
    project_root: &Path,
    cfg: &config::Config,
) -> Result<SkillsEnforceSummary> {
    let Some(skills_cfg) = cfg.skills.as_ref() else {
        return Ok(SkillsEnforceSummary::default());
    };

    let skills_dir = get_skills_dir_at(project_root);
    let mut summary = SkillsEnforceSummary::default();

    if skills_cfg.sync_tasks {
        let options = resolve_skill_sync_options(Some(skills_cfg));
        let (created, updated) = sync_tasks_to_skills(&skills_dir, &cfg.tasks, options)?;
        summary.task_skills_created = created;
        summary.task_skills_updated = updated;
        ensure_symlinks_at(project_root)?;
    }

    for name in &skills_cfg.install {
        let installed = install_skill_inner(project_root, name, true, true)?;
        if installed {
            summary.installed_skills.push(name.clone());
        }
    }

    Ok(summary)
}

pub fn ensure_default_skills_at(project_root: &Path) -> Result<()> {
    let skills_dir = get_skills_dir_at(project_root);
    fs::create_dir_all(&skills_dir)?;

    start::update_gitignore(project_root)?;

    let env_dir = skills_dir.join("env");
    let env_file = env_dir.join("skill.md");
    let should_write = if env_file.exists() {
        let content = fs::read_to_string(&env_file).unwrap_or_default();
        content.contains("source: flow-default")
    } else {
        true
    };

    if should_write {
        fs::create_dir_all(&env_dir)?;
        fs::write(&env_file, DEFAULT_ENV_SKILL)?;
    }

    let quality_dir = skills_dir.join("quality-bun-feature-delivery");
    let quality_file = quality_dir.join("skill.md");
    let should_write_quality = if quality_file.exists() {
        let content = fs::read_to_string(&quality_file).unwrap_or_default();
        content.contains("source: flow-default")
    } else {
        true
    };
    if should_write_quality {
        fs::create_dir_all(&quality_dir)?;
        fs::write(&quality_file, DEFAULT_QUALITY_BUN_FEATURE_DELIVERY_SKILL)?;
    }

    ensure_symlinks_at(project_root)?;

    Ok(())
}

pub fn auto_sync_skills() {
    let Ok(cwd) = std::env::current_dir() else {
        return;
    };

    let mut current = cwd.clone();
    let flow_toml = loop {
        let candidate = current.join("flow.toml");
        if candidate.exists() {
            break Some(candidate);
        }
        if !current.pop() {
            break None;
        }
    };

    let Some(flow_toml) = flow_toml else {
        return;
    };
    let Some(project_root) = flow_toml.parent() else {
        return;
    };

    let cfg = match config::load(&flow_toml) {
        Ok(cfg) => Some(cfg),
        Err(err) => {
            tracing::debug!(?err, "failed to load flow.toml for skills sync");
            None
        }
    };

    if let Err(err) = ensure_default_skills_at(project_root) {
        tracing::debug!(?err, "failed to auto-sync default skills");
    }

    if let Some(cfg) = cfg {
        if let Err(err) = enforce_skills_from_config(project_root, &cfg) {
            tracing::debug!(?err, "failed to auto-sync configured skills");
        }
    }
}

pub fn ensure_project_skills_at(
    project_root: &Path,
    cfg: &config::Config,
) -> Result<SkillsEnforceSummary> {
    ensure_default_skills_at(project_root)?;
    enforce_skills_from_config(project_root, cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample_task(name: &str, description: Option<&str>) -> config::TaskConfig {
        config::TaskConfig {
            name: name.to_string(),
            command: "echo hi".to_string(),
            delegate_to_hub: false,
            activate_on_cd_to_root: false,
            dependencies: Vec::new(),
            description: description.map(|v| v.to_string()),
            shortcuts: Vec::new(),
            interactive: false,
            confirm_on_match: false,
            on_cancel: None,
            output_file: None,
        }
    }

    #[test]
    fn task_openai_yaml_defaults_to_no_implicit_invocation() {
        let task = sample_task("deploy-all", Some("Deploy all services safely"));
        let yaml = render_task_skill_openai_yaml(&task, false);
        assert!(yaml.contains("display_name: \"Deploy All\""));
        assert!(yaml.contains("allow_implicit_invocation: false"));
        assert!(
            yaml.contains("default_prompt: \"Use $deploy-all to Deploy all services safely.\"")
        );
    }

    #[test]
    fn task_openai_yaml_can_enable_implicit_invocation() {
        let task = sample_task("build-web", Some("Build web assets"));
        let yaml = render_task_skill_openai_yaml(&task, true);
        assert!(yaml.contains("allow_implicit_invocation: true"));
    }

    #[test]
    fn ensure_default_skills_writes_quality_bun_skill() {
        let dir = tempdir().expect("tempdir");
        ensure_default_skills_at(dir.path()).expect("default skills should be written");

        let env = dir.path().join(".ai/skills/env/skill.md");
        let quality = dir
            .path()
            .join(".ai/skills/quality-bun-feature-delivery/skill.md");

        assert!(env.exists(), "env default skill should exist");
        assert!(quality.exists(), "quality skill should exist");

        let quality_content = fs::read_to_string(&quality).expect("quality skill readable");
        assert!(quality_content.contains("name: quality-bun-feature-delivery"));
        assert!(quality_content.contains("version: 2"));
        assert!(quality_content.contains("source: flow-default"));
    }
}
