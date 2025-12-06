//! Fast discovery of nested flow.toml files in a project.

use std::path::{Path, PathBuf};

use anyhow::Result;
use ignore::WalkBuilder;

use crate::config::{self, Config, TaskConfig};

/// A task with its source location information.
#[derive(Debug, Clone)]
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
}

/// Result of discovering flow.toml files in a directory tree.
#[derive(Debug)]
pub struct DiscoveryResult {
    /// All discovered tasks, sorted by depth (root first).
    pub tasks: Vec<DiscoveredTask>,
    /// The root config path (if exists).
    pub root_config: Option<PathBuf>,
    /// Root config object (if exists).
    pub root_cfg: Option<Config>,
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

    let mut discovered: Vec<DiscoveredTask> = Vec::new();
    let mut root_config: Option<PathBuf> = None;
    let mut root_cfg: Option<Config> = None;

    // Check if root itself has a flow.toml
    let root_flow_toml = root.join("flow.toml");
    if root_flow_toml.exists() {
        if let Ok(cfg) = config::load(&root_flow_toml) {
            root_config = Some(root_flow_toml.clone());
            for task in &cfg.tasks {
                discovered.push(DiscoveredTask {
                    task: task.clone(),
                    config_path: root_flow_toml.clone(),
                    relative_dir: String::new(),
                    depth: 0,
                });
            }
            root_cfg = Some(cfg);
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
        let cfg = match config::load(&flow_toml) {
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

        for task in cfg.tasks {
            discovered.push(DiscoveredTask {
                task,
                config_path: flow_toml.clone(),
                relative_dir: relative_dir.clone(),
                depth,
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

    Ok(DiscoveryResult {
        tasks: discovered,
        root_config,
        root_cfg,
    })
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
