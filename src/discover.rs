//! Fast discovery of nested flow.toml files in a project.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};

use crate::config::{self, CommandFileConfig, TaskConfig, TaskResolutionConfig};
use crate::fixup;

/// A task with its source location information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredTask {
    /// The task configuration.
    pub task: TaskConfig,
    /// Absolute path to the flow.toml containing this task.
    pub config_path: PathBuf,
    /// Relative path from the discovery root to the config file's directory.
    /// Empty string for root-level tasks.
    pub relative_dir: String,
    /// Depth from the discovery root (0 = root, 1 = immediate subdirectory, etc.)
    pub depth: usize,
    /// Primary scope label used for display and selector prefixes (e.g. "mobile", "root").
    pub scope: String,
    /// Scope aliases accepted during selector matching.
    pub scope_aliases: Vec<String>,
}

impl DiscoveredTask {
    /// Format a display label showing the relative path for nested tasks.
    pub fn path_label(&self) -> Option<String> {
        if self.relative_dir.is_empty() {
            None
        } else {
            Some(self.relative_dir.clone())
        }
    }

    /// Case-insensitive scope match against discovered aliases.
    pub fn matches_scope(&self, scope: &str) -> bool {
        let needle = normalize_scope_token(scope);
        if needle.is_empty() {
            return false;
        }
        self.scope_aliases.iter().any(|alias| alias == &needle)
    }
}

/// Result of discovering flow.toml files in a directory tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryResult {
    /// All discovered tasks, sorted by depth (root first).
    pub tasks: Vec<DiscoveredTask>,
    /// The root config path (if exists).
    pub root_config: Option<PathBuf>,
    /// Root task-resolution policy (if configured).
    pub root_task_resolution: Option<TaskResolutionConfig>,
}

#[derive(Debug, Clone)]
pub(crate) struct DiscoveryArtifacts {
    pub result: DiscoveryResult,
    pub watched_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
struct DiscoveryConfigFile {
    #[serde(
        default,
        rename = "name",
        alias = "project_name",
        alias = "project-name"
    )]
    project_name: Option<String>,
    #[serde(default)]
    tasks: Vec<TaskConfig>,
    #[serde(
        default,
        rename = "task_resolution",
        alias = "task-resolution",
        alias = "taskResolution"
    )]
    task_resolution: Option<TaskResolutionConfig>,
    #[serde(default, rename = "commands")]
    command_files: Vec<CommandFileConfig>,
}

#[derive(Debug, Clone)]
struct LoadedDiscoveryConfig {
    project_name: Option<String>,
    tasks: Vec<TaskConfig>,
    task_resolution: Option<TaskResolutionConfig>,
}

/// Discover all flow.toml files starting from the given root directory.
/// Uses the `ignore` crate for fast, gitignore-aware traversal.
///
/// Tasks are returned sorted by depth (root-level first, then nested).
pub fn discover_tasks(root: &Path) -> Result<DiscoveryResult> {
    let root = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir()?.join(root)
    };
    let root = root.canonicalize().unwrap_or(root);
    discover_tasks_from_root(root)
}

pub(crate) fn discover_tasks_from_root(root: PathBuf) -> Result<DiscoveryResult> {
    Ok(discover_tasks_from_root_artifacts(root)?.result)
}

pub(crate) fn discover_tasks_from_root_artifacts(root: PathBuf) -> Result<DiscoveryArtifacts> {
    let mut discovered: Vec<DiscoveredTask> = Vec::new();
    let mut root_config: Option<PathBuf> = None;
    let mut root_task_resolution: Option<TaskResolutionConfig> = None;
    let mut watched_paths = Vec::new();
    push_watched_path(&mut watched_paths, &root);

    // Check if root itself has a flow.toml
    let root_flow_toml = root.join("flow.toml");
    if root_flow_toml.exists() {
        match load_discovery_config(&root_flow_toml, &mut Vec::new(), &mut watched_paths) {
            Ok(cfg) => {
                let (scope, scope_aliases) = infer_scope_metadata("", cfg.project_name.as_deref());
                root_config = Some(root_flow_toml.clone());
                root_task_resolution = cfg.task_resolution.clone();
                for task in &cfg.tasks {
                    discovered.push(DiscoveredTask {
                        task: task.clone(),
                        config_path: root_flow_toml.clone(),
                        relative_dir: String::new(),
                        depth: 0,
                        scope: scope.clone(),
                        scope_aliases: scope_aliases.clone(),
                    });
                }
            }
            Err(e) => {
                eprintln!(
                    "Warning: failed to parse {}: {:#}",
                    root_flow_toml.display(),
                    e
                );
            }
        }
    }

    // Walk subdirectories looking for flow.toml files
    // Use the ignore crate which respects .gitignore and is very fast
    let walker = WalkBuilder::new(&root)
        .hidden(true) // skip hidden directories
        .git_ignore(true) // respect .gitignore
        .git_global(true) // respect global gitignore
        .git_exclude(true) // respect .git/info/exclude
        .max_depth(Some(10)) // reasonable depth limit
        .filter_entry(|entry| {
            // Skip common directories that won't have flow.toml we care about
            if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
                let name = entry.file_name().to_string_lossy();
                // Skip these directories entirely
                !matches!(
                    name.as_ref(),
                    "node_modules"
                        | "target"
                        | "dist"
                        | "build"
                        | ".git"
                        | ".hg"
                        | ".svn"
                        | "__pycache__"
                        | ".pytest_cache"
                        | ".mypy_cache"
                        | "venv"
                        | ".venv"
                        | "vendor"
                        | "Pods"
                        | ".cargo"
                        | ".rustup"
                )
            } else {
                true
            }
        })
        .build();

    for entry in walker.flatten() {
        let path = entry.path();
        if path.is_dir() {
            push_watched_path(&mut watched_paths, path);
        }

        // Skip the root (already handled above)
        if path == root {
            continue;
        }

        // We're looking for directories that contain flow.toml
        if !path.is_dir() {
            continue;
        }

        let flow_toml = path.join("flow.toml");
        if !flow_toml.exists() {
            continue;
        }

        // Parse the config
        let cfg = match load_discovery_config(&flow_toml, &mut Vec::new(), &mut watched_paths) {
            Ok(c) => c,
            Err(_) => continue, // Skip invalid configs
        };

        // Calculate relative path from root
        let relative_dir = path
            .strip_prefix(&root)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();

        // Calculate depth
        let depth = relative_dir.matches('/').count()
            + relative_dir.matches('\\').count()
            + if relative_dir.is_empty() { 0 } else { 1 };
        let (scope, scope_aliases) =
            infer_scope_metadata(&relative_dir, cfg.project_name.as_deref());

        for task in cfg.tasks {
            discovered.push(DiscoveredTask {
                task,
                config_path: flow_toml.clone(),
                relative_dir: relative_dir.clone(),
                depth,
                scope: scope.clone(),
                scope_aliases: scope_aliases.clone(),
            });
        }
    }

    // Sort by depth (root first), then by task name for stability
    discovered.sort_by(|a, b| {
        a.depth
            .cmp(&b.depth)
            .then_with(|| a.relative_dir.cmp(&b.relative_dir))
            .then_with(|| a.task.name.cmp(&b.task.name))
    });

    Ok(DiscoveryArtifacts {
        result: DiscoveryResult {
            tasks: discovered,
            root_config,
            root_task_resolution,
        },
        watched_paths,
    })
}

fn normalize_scope_token(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.trim().chars() {
        let ch = ch.to_ascii_lowercase();
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '/' | '.') {
            out.push(ch);
        } else if ch.is_whitespace() {
            out.push('-');
        }
    }
    out.trim_matches('-').trim_matches('/').to_string()
}

fn infer_scope_metadata(relative_dir: &str, project_name: Option<&str>) -> (String, Vec<String>) {
    let mut aliases: Vec<String> = Vec::new();
    let mut push_alias = |raw: &str| {
        let normalized = normalize_scope_token(raw);
        if !normalized.is_empty() && !aliases.iter().any(|v| v == &normalized) {
            aliases.push(normalized);
        }
    };

    if let Some(name) = project_name {
        push_alias(name);
    } else if relative_dir.trim().is_empty() {
        push_alias("root");
    } else {
        if let Some(leaf) = std::path::Path::new(relative_dir)
            .file_name()
            .and_then(|s| s.to_str())
        {
            push_alias(leaf);
        }
        push_alias(relative_dir);
    }

    let primary = aliases
        .first()
        .cloned()
        .unwrap_or_else(|| "root".to_string());
    (primary, aliases)
}

fn push_watched_path(paths: &mut Vec<PathBuf>, path: &Path) {
    if !paths.iter().any(|existing| existing == path) {
        paths.push(path.to_path_buf());
    }
}

fn load_discovery_config(
    path: &Path,
    visited: &mut Vec<PathBuf>,
    watched_paths: &mut Vec<PathBuf>,
) -> Result<LoadedDiscoveryConfig> {
    let canonical = path.canonicalize()?;
    if visited.contains(&canonical) {
        anyhow::bail!(
            "cycle detected while loading config includes: {}",
            path.display()
        );
    }
    visited.push(canonical.clone());
    push_watched_path(watched_paths, &canonical);

    let contents = fs::read_to_string(&canonical)?;
    let mut cfg = parse_discovery_config(&canonical, &contents)?;

    let mut project_name = cfg.project_name.take();
    let mut tasks = cfg.tasks;
    let mut task_resolution = cfg.task_resolution.take();

    for include in cfg.command_files {
        let include_path = config::resolve_include_path(&canonical, &include.path);
        let included = load_discovery_config(&include_path, visited, watched_paths)?;
        if project_name.is_none() {
            project_name = included.project_name;
        }
        if task_resolution.is_none() {
            task_resolution = included.task_resolution;
        }
        tasks.extend(included.tasks);
    }

    visited.pop();
    Ok(LoadedDiscoveryConfig {
        project_name,
        tasks,
        task_resolution,
    })
}

fn parse_discovery_config(path: &Path, contents: &str) -> Result<DiscoveryConfigFile> {
    match toml::from_str(contents) {
        Ok(cfg) => Ok(cfg),
        Err(err) => {
            let fix = fixup::fix_toml_content(contents);
            if fix.fixes_applied.is_empty() {
                Err(err).with_context(|| {
                    format!(
                        "failed to parse flow discovery config at {}",
                        path.display()
                    )
                })
            } else {
                let fixed = fixup::apply_fixes_to_content(contents, &fix.fixes_applied);
                fs::write(path, &fixed).with_context(|| {
                    format!(
                        "failed to write auto-fixed discovery config at {}",
                        path.display()
                    )
                })?;
                toml::from_str(&fixed).with_context(|| {
                    format!(
                        "failed to parse flow discovery config at {} (after auto-fix)",
                        path.display()
                    )
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_flow_toml(dir: &Path, content: &str) {
        fs::write(dir.join("flow.toml"), content).unwrap();
    }

    #[test]
    fn discovers_root_tasks() {
        let tmp = TempDir::new().unwrap();
        write_flow_toml(
            tmp.path(),
            r#"
[[tasks]]
name = "test"
command = "echo test"
"#,
        );

        let result = discover_tasks(tmp.path()).unwrap();
        assert_eq!(result.tasks.len(), 1);
        assert_eq!(result.tasks[0].task.name, "test");
        assert_eq!(result.tasks[0].depth, 0);
        assert!(result.tasks[0].relative_dir.is_empty());
        assert_eq!(result.tasks[0].scope, "root");
    }

    #[test]
    fn discovers_nested_tasks() {
        let tmp = TempDir::new().unwrap();
        write_flow_toml(
            tmp.path(),
            r#"
[[tasks]]
name = "root-task"
command = "echo root"
"#,
        );

        let nested = tmp.path().join("packages/api");
        fs::create_dir_all(&nested).unwrap();
        write_flow_toml(
            &nested,
            r#"
[[tasks]]
name = "api-task"
command = "echo api"
"#,
        );

        let result = discover_tasks(tmp.path()).unwrap();
        assert_eq!(result.tasks.len(), 2);

        // Root task should come first
        assert_eq!(result.tasks[0].task.name, "root-task");
        assert_eq!(result.tasks[0].depth, 0);

        // Nested task second
        assert_eq!(result.tasks[1].task.name, "api-task");
        assert!(result.tasks[1].depth > 0);
        assert!(result.tasks[1].relative_dir.contains("packages"));
        assert_eq!(result.tasks[1].scope, "api");
    }

    #[test]
    fn skips_node_modules() {
        let tmp = TempDir::new().unwrap();
        write_flow_toml(
            tmp.path(),
            r#"
[[tasks]]
name = "root"
command = "echo root"
"#,
        );

        let node_modules = tmp.path().join("node_modules/some-pkg");
        fs::create_dir_all(&node_modules).unwrap();
        write_flow_toml(
            &node_modules,
            r#"
[[tasks]]
name = "should-skip"
command = "echo skip"
"#,
        );

        let result = discover_tasks(tmp.path()).unwrap();
        assert_eq!(result.tasks.len(), 1);
        assert_eq!(result.tasks[0].task.name, "root");
    }
}
