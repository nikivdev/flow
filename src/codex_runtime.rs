use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{codex_skill_eval, config};

const RUNTIME_VERSION: u32 = 1;
const RUNTIME_PREFIX: &str = "flow-runtime-";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexRuntimeSkill {
    pub name: String,
    pub kind: String,
    pub path: String,
    pub trigger: String,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub original_name: Option<String>,
    #[serde(default)]
    pub estimated_chars: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexRuntimeState {
    pub version: u32,
    pub token: String,
    pub created_at_unix: u64,
    pub target_path: String,
    pub query: String,
    pub skills: Vec<CodexRuntimeSkill>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexRuntimeActivation {
    pub state_path: PathBuf,
    pub skills: Vec<CodexRuntimeSkill>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexExternalSkill {
    pub source_name: String,
    pub name: String,
    pub path: String,
    pub description: String,
    pub estimated_chars: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexSkillSourceSnapshot {
    pub name: String,
    pub path: String,
    pub enabled: bool,
    pub skill_count: usize,
    pub skills: Vec<CodexExternalSkill>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexInstalledSkillSnapshot {
    pub name: String,
    pub path: String,
    pub description: String,
    pub runtime_managed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexRuntimeStateSnapshot {
    pub token: String,
    pub created_at_unix: u64,
    pub target_path: String,
    pub query: String,
    pub skills: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexSkillsDashboardSnapshot {
    pub target_path: String,
    pub sources: Vec<CodexSkillSourceSnapshot>,
    pub installed_skills: Vec<CodexInstalledSkillSnapshot>,
    pub recent_runtime_states: Vec<CodexRuntimeStateSnapshot>,
    pub runtime_states_for_target: usize,
}

#[derive(Debug, Clone)]
struct RuntimeSkillCandidate {
    score: f64,
    skill: CodexRuntimeSkill,
    source_dir: Option<PathBuf>,
}

impl CodexRuntimeActivation {
    pub fn markers(&self) -> Vec<String> {
        self.skills
            .iter()
            .map(|skill| format!("${}", skill.name))
            .collect()
    }

    pub fn inject_into_prompt(&self, prompt: &str) -> String {
        let mut lines = self.markers();
        lines.push(String::new());
        lines.push(prompt.trim().to_string());
        lines.join("\n")
    }
}

fn runtime_root() -> Result<PathBuf> {
    Ok(config::ensure_global_state_dir()?
        .join("codex")
        .join("runtime"))
}

fn runtime_roots() -> Vec<PathBuf> {
    config::global_state_dir_candidates()
        .into_iter()
        .map(|root| root.join("codex").join("runtime"))
        .collect()
}

fn runtime_states_dir() -> Result<PathBuf> {
    let dir = runtime_root()?.join("states");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn runtime_skills_dir() -> Result<PathBuf> {
    let dir = runtime_root()?.join("skills");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn agents_skill_root() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".agents/skills")
}

fn codex_global_skill_root() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codex/skills")
}

fn slugify(value: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in value.chars() {
        let mapped = if ch.is_ascii_alphanumeric() {
            Some(ch.to_ascii_lowercase())
        } else {
            Some('-')
        };
        if let Some(mapped) = mapped {
            if mapped == '-' {
                if !out.is_empty() && !last_dash {
                    out.push('-');
                    last_dash = true;
                }
            } else {
                out.push(mapped);
                last_dash = false;
            }
        }
    }
    out.trim_matches('-').to_string()
}

fn parse_frontmatter_field(content: &str, field: &str) -> Option<String> {
    let after_start = content.strip_prefix("---\n")?;
    let end = after_start.find("\n---")?;
    let frontmatter = &after_start[..end];
    for line in frontmatter.lines() {
        let trimmed = line.trim();
        let prefix = format!("{field}:");
        if let Some(value) = trimmed.strip_prefix(&prefix) {
            return Some(
                value
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .to_string(),
            );
        }
    }
    None
}

fn default_skill_sources() -> Vec<config::CodexSkillSourceConfig> {
    let vercel_path = config::expand_path("~/repos/vercel-labs/skills");
    if looks_like_skill_source_root(&vercel_path) {
        return vec![config::CodexSkillSourceConfig {
            name: "vercel-labs-skills".to_string(),
            path: "~/repos/vercel-labs/skills".to_string(),
            enabled: Some(true),
        }];
    }
    Vec::new()
}

fn configured_skill_sources(
    codex_cfg: &config::CodexConfig,
) -> Vec<config::CodexSkillSourceConfig> {
    let mut sources = if codex_cfg.skill_sources.is_empty() {
        default_skill_sources()
    } else {
        codex_cfg.skill_sources.clone()
    };
    sources.retain(|source| source.enabled.unwrap_or(true));
    sources
}

fn looks_like_skill_source_root(root: &Path) -> bool {
    collect_skill_dirs(root)
        .map(|dirs| !dirs.is_empty())
        .unwrap_or(false)
}

fn collect_skill_dirs(root: &Path) -> Result<Vec<PathBuf>> {
    let mut dirs = BTreeSet::new();
    let nested_root = root.join("skills");
    for base in [nested_root.as_path(), root] {
        if !base.is_dir() {
            continue;
        }
        for entry in fs::read_dir(base)? {
            let entry = entry?;
            let skill_dir = entry.path();
            if !skill_dir.is_dir() {
                continue;
            }
            if skill_dir.join("SKILL.md").is_file() {
                dirs.insert(skill_dir);
            }
        }
    }
    Ok(dirs.into_iter().collect())
}

fn discover_source_skills(
    source: &config::CodexSkillSourceConfig,
) -> Result<Vec<CodexExternalSkill>> {
    let root = config::expand_path(&source.path);
    let skill_dirs = collect_skill_dirs(&root)?;
    let mut skills = Vec::new();
    for skill_dir in skill_dirs {
        let skill_file = skill_dir.join("SKILL.md");
        let raw = fs::read_to_string(&skill_file)
            .with_context(|| format!("failed to read {}", skill_file.display()))?;
        let name = parse_frontmatter_field(&raw, "name")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| {
                skill_dir
                    .file_name()
                    .map(|value| value.to_string_lossy().to_string())
                    .unwrap_or_else(|| "skill".to_string())
            });
        let description = parse_frontmatter_field(&raw, "description").unwrap_or_default();
        skills.push(CodexExternalSkill {
            source_name: source.name.clone(),
            name,
            path: skill_dir.display().to_string(),
            description,
            estimated_chars: raw.chars().count(),
        });
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(skills)
}

fn tokenize_keywords(value: &str) -> Vec<String> {
    value
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(|part| part.to_ascii_lowercase())
        .filter(|part| {
            part.len() >= 4
                && !matches!(
                    part.as_str(),
                    "skill"
                        | "skills"
                        | "with"
                        | "from"
                        | "that"
                        | "this"
                        | "used"
                        | "when"
                        | "help"
                        | "helps"
                        | "agent"
                        | "agents"
                        | "their"
                        | "into"
                        | "your"
                )
        })
        .collect()
}

fn match_external_skill(query: &str, skill: &CodexExternalSkill) -> f64 {
    let normalized_query = query.to_ascii_lowercase();
    let skill_phrase = tokenize_keywords(&skill.name).join(" ");
    if !skill_phrase.is_empty() && normalized_query.contains(&skill_phrase) {
        return 1.0;
    }

    let mut terms = tokenize_keywords(&skill.name);
    terms.extend(tokenize_keywords(&skill.description));
    terms.sort();
    terms.dedup();
    if terms.is_empty() {
        return 0.0;
    }
    let hits = terms
        .iter()
        .filter(|term| normalized_query.contains(term.as_str()))
        .count();
    hits as f64 / terms.len().min(6) as f64
}

fn copy_dir_recursive(source: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source_path)?;
        if metadata.is_dir() {
            copy_dir_recursive(&source_path, &dest_path)?;
        } else if metadata.file_type().is_symlink() {
            let target = fs::read_link(&source_path)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(target, &dest_path)?;
            #[cfg(windows)]
            {
                if metadata.is_dir() {
                    std::os::windows::fs::symlink_dir(target, &dest_path)?;
                } else {
                    std::os::windows::fs::symlink_file(target, &dest_path)?;
                }
            }
        } else {
            fs::copy(&source_path, &dest_path)?;
        }
    }
    Ok(())
}

fn rewrite_skill_name(content: &str, name: &str) -> String {
    if let Some(after_start) = content.strip_prefix("---\n") {
        if let Some(end) = after_start.find("\n---") {
            let mut lines = after_start[..end]
                .lines()
                .map(|line| {
                    if line.trim_start().starts_with("name:") {
                        format!("name: {name}")
                    } else {
                        line.to_string()
                    }
                })
                .collect::<Vec<_>>();
            if !lines
                .iter()
                .any(|line| line.trim_start().starts_with("name:"))
            {
                lines.insert(0, format!("name: {name}"));
            }
            return format!("---\n{}\n---{}", lines.join("\n"), &after_start[end..]);
        }
    }

    format!("---\nname: {name}\n---\n\n{content}")
}

fn allocate_plan_path(root: &Path, stem: &str) -> PathBuf {
    let candidate = root.join(format!("{stem}.md"));
    if !candidate.exists() {
        return candidate;
    }

    let mut index = 2usize;
    loop {
        let next = root.join(format!("{stem}-{index}.md"));
        if !next.exists() {
            return next;
        }
        index += 1;
    }
}

fn derive_plan_title(body: &str) -> String {
    for raw_line in body.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix('#') {
            let cleaned = rest.trim().trim_start_matches('#').trim();
            if !cleaned.is_empty() {
                return cleaned.to_string();
            }
        }
        return line.to_string();
    }
    "Plan".to_string()
}

fn append_session_footer(body: &str, session_id: Option<&str>) -> String {
    let trimmed = body.trim_end();
    let Some(session_id) = session_id.map(str::trim).filter(|value| !value.is_empty()) else {
        return trimmed.to_string();
    };
    let footer = format!("Made from {} Codex session.", session_id);
    if trimmed.ends_with(&footer) {
        return trimmed.to_string();
    }
    format!("{trimmed}\n\n{footer}")
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0)
}

fn runtime_token(target_path: &Path, query: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(target_path.to_string_lossy().as_bytes());
    hasher.update(b"\n");
    hasher.update(query.as_bytes());
    hasher.update(b"\n");
    hasher.update(std::process::id().to_string().as_bytes());
    hasher.update(b"\n");
    hasher.update(unix_now().to_string().as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    digest[..12.min(digest.len())].to_string()
}

fn plan_skill_name(token: &str) -> String {
    format!("{RUNTIME_PREFIX}plan-{token}")
}

fn build_plan_skill_markdown(skill_name: &str) -> String {
    format!(
        r#"---
name: {skill_name}
description: Write the finished markdown plan for this task into `~/plan` using `f codex runtime write-plan`. Use only for the current task.
policy:
  allow_implicit_invocation: false
---

# Flow Runtime Plan Writer

Use this only when the user asks to write, save, or document a plan.

## Command

Write the plan with:

```bash
cat <<'EOF' | f codex runtime write-plan --title "<short title>"
<markdown plan body>
EOF
```

The command prints the absolute path after writing.

## Hard rules

- write the finished plan to `~/plan`
- keep the chat response short
- end with the absolute path on its own line
- do not leave the plan only in chat when the user explicitly asked to write it
"#
    )
}

fn looks_like_plan_request(query: &str) -> bool {
    let normalized = query.to_ascii_lowercase();
    [
        "write plan",
        "save this plan",
        "save the plan",
        "document the plan",
        "put the plan in ~/plan",
        "write this up as a plan",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

pub fn discover_external_skills(
    _target_path: &Path,
    codex_cfg: &config::CodexConfig,
) -> Result<Vec<CodexExternalSkill>> {
    let mut out = Vec::new();
    for source in configured_skill_sources(codex_cfg) {
        out.extend(discover_source_skills(&source)?);
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

pub fn dashboard_snapshot(
    target_path: &Path,
    codex_cfg: &config::CodexConfig,
    recent_limit: usize,
) -> Result<CodexSkillsDashboardSnapshot> {
    let target_display = target_path.display().to_string();
    let mut sources = Vec::new();
    for source in configured_skill_sources(codex_cfg) {
        let skills = discover_source_skills(&source)?;
        sources.push(CodexSkillSourceSnapshot {
            name: source.name,
            path: config::expand_path(&source.path).display().to_string(),
            enabled: source.enabled.unwrap_or(true),
            skill_count: skills.len(),
            skills,
        });
    }
    sources.sort_by(|a, b| a.name.cmp(&b.name));

    let installed_skills = discover_installed_skills()?;
    let runtime_states = load_runtime_states()?;
    let runtime_states_for_target = runtime_states
        .iter()
        .filter(|state| state.target_path == target_display)
        .count();
    let recent_runtime_states = runtime_states
        .into_iter()
        .take(recent_limit)
        .map(|state| CodexRuntimeStateSnapshot {
            token: state.token,
            created_at_unix: state.created_at_unix,
            target_path: state.target_path,
            query: state.query,
            skills: state
                .skills
                .into_iter()
                .map(|skill| skill.original_name.unwrap_or(skill.name))
                .collect(),
        })
        .collect();

    Ok(CodexSkillsDashboardSnapshot {
        target_path: target_display,
        sources,
        installed_skills,
        recent_runtime_states,
        runtime_states_for_target,
    })
}

fn discover_installed_skills() -> Result<Vec<CodexInstalledSkillSnapshot>> {
    let root = codex_global_skill_root();
    if !root.is_dir() {
        return Ok(Vec::new());
    }

    let mut installed = Vec::new();
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        let skill_dir = entry.path();
        if !skill_dir.is_dir() {
            continue;
        }
        let skill_file = skill_dir.join("SKILL.md");
        if !skill_file.is_file() {
            continue;
        }
        let raw = fs::read_to_string(&skill_file)
            .with_context(|| format!("failed to read {}", skill_file.display()))?;
        let name = parse_frontmatter_field(&raw, "name")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| entry.file_name().to_string_lossy().to_string());
        installed.push(CodexInstalledSkillSnapshot {
            runtime_managed: name.starts_with(RUNTIME_PREFIX),
            name,
            path: skill_dir.display().to_string(),
            description: parse_frontmatter_field(&raw, "description").unwrap_or_default(),
        });
    }
    installed.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(installed)
}

pub fn format_external_skills(skills: &[CodexExternalSkill]) -> String {
    if skills.is_empty() {
        return "No external Codex skill sources discovered.".to_string();
    }

    let mut lines = vec!["# codex skill-source".to_string()];
    for skill in skills {
        lines.push(format!(
            "- {} [{}] {} chars",
            skill.name, skill.source_name, skill.estimated_chars
        ));
        if !skill.description.is_empty() {
            lines.push(format!("  {}", skill.description));
        }
    }
    lines.join("\n")
}

pub fn sync_external_skills(
    target_path: &Path,
    codex_cfg: &config::CodexConfig,
    selected_skills: &[String],
    force: bool,
) -> Result<usize> {
    let discovered = discover_external_skills(target_path, codex_cfg)?;
    let selected = selected_skills
        .iter()
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    let root = codex_global_skill_root();
    fs::create_dir_all(&root)?;

    let mut installed = 0usize;
    for skill in discovered {
        if !selected.is_empty()
            && !selected
                .iter()
                .any(|value| value == &skill.name.to_ascii_lowercase())
        {
            continue;
        }
        let dest = root.join(&skill.name);
        if dest.exists() {
            if !force {
                continue;
            }
            fs::remove_dir_all(&dest)
                .with_context(|| format!("failed to replace {}", dest.display()))?;
        }
        copy_dir_recursive(Path::new(&skill.path), &dest)?;
        installed += 1;
    }
    Ok(installed)
}

pub fn prepare_runtime_activation(
    target_path: &Path,
    query: &str,
    enabled: bool,
    codex_cfg: &config::CodexConfig,
) -> Result<Option<CodexRuntimeActivation>> {
    if !enabled {
        return Ok(None);
    }

    let token = runtime_token(target_path, query);
    let state_path = runtime_states_dir()?.join(format!("{token}.json"));
    let skills_root = runtime_skills_dir()?.join(&token);
    fs::create_dir_all(&skills_root)?;
    let scorecard = codex_skill_eval::load_scorecard(target_path)?;

    let mut candidates = Vec::new();
    if looks_like_plan_request(query) {
        let skill_name = plan_skill_name(&token);
        let skill_dir = skills_root.join(&skill_name);
        let markdown = build_plan_skill_markdown(&skill_name);
        fs::create_dir_all(&skill_dir)?;
        fs::write(skill_dir.join("SKILL.md"), &markdown)?;
        let scorecard_score = scorecard
            .as_ref()
            .and_then(|value| {
                value
                    .skills
                    .iter()
                    .find(|skill| skill.name == "plan_write")
                    .map(|skill| skill.score)
            })
            .unwrap_or(0.0);
        let score = 2.5 + scorecard_score / 100.0 - markdown.chars().count() as f64 / 5000.0;
        candidates.push(RuntimeSkillCandidate {
            score,
            skill: CodexRuntimeSkill {
                name: skill_name,
                kind: "plan_write".to_string(),
                path: skill_dir.display().to_string(),
                trigger: "write plan".to_string(),
                source: Some("flow".to_string()),
                original_name: Some("plan_write".to_string()),
                estimated_chars: Some(markdown.chars().count()),
            },
            source_dir: None,
        });
    }

    for external in discover_external_skills(target_path, codex_cfg)? {
        let match_score = match_external_skill(query, &external);
        if match_score < 0.55 {
            continue;
        }
        let scorecard_score = scorecard
            .as_ref()
            .and_then(|value| {
                value
                    .skills
                    .iter()
                    .find(|skill| skill.name == external.name)
                    .map(|skill| skill.score)
            })
            .unwrap_or(0.0);
        let runtime_name = format!(
            "{RUNTIME_PREFIX}ext-{}-{}-{}",
            slugify(&external.source_name),
            slugify(&external.name),
            token
        );
        let score =
            match_score * 2.0 + scorecard_score / 100.0 - external.estimated_chars as f64 / 6000.0;
        candidates.push(RuntimeSkillCandidate {
            score,
            skill: CodexRuntimeSkill {
                name: runtime_name,
                kind: "external".to_string(),
                path: skills_root
                    .join(format!(
                        "{}-{}",
                        slugify(&external.source_name),
                        slugify(&external.name)
                    ))
                    .display()
                    .to_string(),
                trigger: external.name.clone(),
                source: Some(external.source_name.clone()),
                original_name: Some(external.name.clone()),
                estimated_chars: Some(external.estimated_chars),
            },
            source_dir: Some(PathBuf::from(&external.path)),
        });
    }

    if candidates.is_empty() {
        return Ok(None);
    }

    candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut total_chars = 0usize;
    let mut selected = Vec::new();
    for candidate in candidates {
        let estimated = candidate.skill.estimated_chars.unwrap_or(0);
        if !selected.is_empty() && total_chars + estimated > 8000 {
            continue;
        }
        total_chars += estimated;
        selected.push(candidate);
        if selected.len() >= 2 {
            break;
        }
    }

    let mut skills = Vec::new();
    for candidate in selected {
        if let Some(source_dir) = candidate.source_dir.as_ref() {
            let materialized_dir = skills_root.join(format!(
                "{}-{}",
                slugify(candidate.skill.source.as_deref().unwrap_or("external")),
                slugify(
                    candidate
                        .skill
                        .original_name
                        .as_deref()
                        .unwrap_or(candidate.skill.name.as_str())
                )
            ));
            copy_dir_recursive(source_dir, &materialized_dir)?;
            let skill_file = materialized_dir.join("SKILL.md");
            let raw = fs::read_to_string(&skill_file)
                .with_context(|| format!("failed to read {}", skill_file.display()))?;
            fs::write(&skill_file, rewrite_skill_name(&raw, &candidate.skill.name))
                .with_context(|| format!("failed to rewrite {}", skill_file.display()))?;
            let mut skill = candidate.skill.clone();
            skill.path = materialized_dir.display().to_string();
            skills.push(skill);
        } else {
            skills.push(candidate.skill);
        }
    }

    let state = CodexRuntimeState {
        version: RUNTIME_VERSION,
        token,
        created_at_unix: unix_now(),
        target_path: target_path.display().to_string(),
        query: query.to_string(),
        skills: skills.clone(),
    };
    fs::write(&state_path, serde_json::to_vec_pretty(&state)?)?;

    Ok(Some(CodexRuntimeActivation { state_path, skills }))
}

pub fn load_runtime_states() -> Result<Vec<CodexRuntimeState>> {
    let mut states = Vec::new();
    for dir in runtime_roots().into_iter().map(|root| root.join("states")) {
        if !dir.exists() {
            continue;
        }
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let Ok(raw) = fs::read(&path) else {
                continue;
            };
            let Ok(state) = serde_json::from_slice::<CodexRuntimeState>(&raw) else {
                continue;
            };
            states.push(state);
        }
    }
    states.sort_by(|a, b| b.created_at_unix.cmp(&a.created_at_unix));
    states.dedup_by(|a, b| a.token == b.token);
    Ok(states)
}

pub fn clear_runtime_states() -> Result<usize> {
    let mut removed = 0usize;
    for root in runtime_roots() {
        let states_dir = root.join("states");
        if states_dir.exists() {
            for entry in fs::read_dir(&states_dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.is_file() {
                    fs::remove_file(&path)?;
                    removed += 1;
                }
            }
        }

        let skills_dir = root.join("skills");
        if skills_dir.exists() {
            fs::remove_dir_all(&skills_dir)?;
        }
    }

    let user_root = agents_skill_root();
    if user_root.exists() {
        for entry in fs::read_dir(&user_root)? {
            let entry = entry?;
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
                continue;
            };
            if !name.starts_with(RUNTIME_PREFIX) {
                continue;
            }
            let meta = fs::symlink_metadata(&path)?;
            if meta.file_type().is_symlink() || meta.is_file() {
                fs::remove_file(&path)?;
            } else if meta.is_dir() {
                fs::remove_dir_all(&path)?;
            }
        }
    }

    Ok(removed)
}

pub fn format_runtime_states(states: &[CodexRuntimeState]) -> String {
    if states.is_empty() {
        return "No Flow-managed Codex runtime skills.".to_string();
    }

    let mut lines = vec!["# codex runtime".to_string()];
    for state in states {
        lines.push(format!("- token: {}", state.token));
        lines.push(format!("  target: {}", state.target_path));
        lines.push(format!("  query: {}", state.query));
        lines.push(format!(
            "  skills: {}",
            state
                .skills
                .iter()
                .map(|skill| {
                    skill
                        .original_name
                        .as_deref()
                        .unwrap_or(skill.name.as_str())
                })
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    lines.join("\n")
}

fn load_runtime_state_from_env() -> Option<CodexRuntimeState> {
    let raw_path = env::var("FLOW_CODEX_RUNTIME_STATE_PATH")
        .ok()
        .or_else(|| env::var("FLOW_CODEX_RUNTIME_STATE").ok())?;
    let path = PathBuf::from(raw_path);
    let raw = fs::read(path).ok()?;
    serde_json::from_slice::<CodexRuntimeState>(&raw).ok()
}

pub fn write_plan_from_stdin(
    title: Option<&str>,
    stem: Option<&str>,
    dir: Option<&str>,
    source_session: Option<&str>,
) -> Result<PathBuf> {
    let mut body = String::new();
    io::stdin()
        .read_to_string(&mut body)
        .context("failed to read plan body from stdin")?;
    if body.trim().is_empty() {
        bail!("plan body is empty");
    }

    let root = dir
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join("plan"))
        })
        .unwrap_or_else(|| PathBuf::from("./plan"));
    fs::create_dir_all(&root)?;

    let resolved_title = title
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| derive_plan_title(&body));
    let mut resolved_stem = stem
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| slugify(&resolved_title));
    if !resolved_stem.ends_with("-plan") {
        resolved_stem.push_str("-plan");
    }

    let session = source_session
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            env::var("CODEX_THREAD_ID")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        });

    let path = allocate_plan_path(&root, &resolved_stem);
    let final_body = append_session_footer(&body, session.as_deref());
    fs::write(&path, final_body + "\n")?;
    if let Some(runtime_state) = load_runtime_state_from_env() {
        let _ = codex_skill_eval::log_outcome(&codex_skill_eval::CodexSkillOutcomeEvent {
            version: 1,
            recorded_at_unix: unix_now(),
            runtime_token: Some(runtime_state.token),
            session_id: session.clone(),
            target_path: Some(runtime_state.target_path),
            kind: "plan_written".to_string(),
            skill_names: runtime_state
                .skills
                .iter()
                .map(|skill| {
                    skill
                        .original_name
                        .clone()
                        .unwrap_or_else(|| skill.name.clone())
                })
                .collect(),
            artifact_path: Some(path.display().to_string()),
            success: 1.0,
        });
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn plan_request_detection_stays_specific() {
        assert!(looks_like_plan_request("write plan"));
        assert!(looks_like_plan_request("Please document the plan"));
        assert!(!looks_like_plan_request("document this feature"));
        assert!(!looks_like_plan_request("planning support cleanup"));
    }

    #[test]
    fn runtime_markers_prefix_prompt() {
        let activation = CodexRuntimeActivation {
            state_path: PathBuf::from("/tmp/runtime.json"),
            skills: vec![CodexRuntimeSkill {
                name: "flow-runtime-plan-abc".to_string(),
                kind: "plan_write".to_string(),
                path: "/tmp/skill".to_string(),
                trigger: "write plan".to_string(),
                source: Some("flow".to_string()),
                original_name: Some("plan_write".to_string()),
                estimated_chars: Some(120),
            }],
        };

        assert_eq!(
            activation.inject_into_prompt("write plan"),
            "$flow-runtime-plan-abc\n\nwrite plan"
        );
    }

    #[test]
    fn session_footer_is_added_once() {
        let once = append_session_footer("# Plan", Some("019c"));
        let twice = append_session_footer(&once, Some("019c"));
        assert_eq!(once, twice);
        assert!(once.ends_with("Made from 019c Codex session."));
    }

    #[test]
    fn discover_external_skills_supports_nested_repo_layout() {
        let temp = tempdir().expect("tempdir");
        let source_root = temp.path().join("vercel-skills");
        let skill_dir = source_root.join("skills").join("find-skills");
        fs::create_dir_all(&skill_dir).expect("create nested skill dir");
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: find-skills\ndescription: Find repo skills.\n---\n",
        )
        .expect("write skill");

        let cfg = config::CodexConfig {
            skill_sources: vec![config::CodexSkillSourceConfig {
                name: "nested".to_string(),
                path: source_root.display().to_string(),
                enabled: Some(true),
            }],
            ..Default::default()
        };

        let skills = discover_external_skills(temp.path(), &cfg).expect("discover nested skills");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "find-skills");
        assert_eq!(skills[0].source_name, "nested");
    }

    #[test]
    fn discover_external_skills_supports_flat_repo_layout() {
        let temp = tempdir().expect("tempdir");
        let source_root = temp.path().join("dimillian-skills");
        let skill_dir = source_root.join("react-component-performance");
        fs::create_dir_all(&skill_dir).expect("create flat skill dir");
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: react-component-performance\ndescription: Optimize React renders.\n---\n",
        )
        .expect("write skill");

        let cfg = config::CodexConfig {
            skill_sources: vec![config::CodexSkillSourceConfig {
                name: "flat".to_string(),
                path: source_root.display().to_string(),
                enabled: Some(true),
            }],
            ..Default::default()
        };

        let skills = discover_external_skills(temp.path(), &cfg).expect("discover flat skills");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "react-component-performance");
        assert_eq!(skills[0].source_name, "flat");
    }
}
