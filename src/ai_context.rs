//! AI context loading from `.ai/context/` directories.
//!
//! This module provides functionality to load contextual AI instructions
//! from `.ai/context/` directories in the project root.
//!
//! ## Directory Structure
//!
//! ```
//! .ai/
//!   context/
//!     commands/        # Command-specific context
//!       sync.md        # Context for `f sync`
//!       deploy.md      # Context for `f deploy`
//!       ...
//!     tasks/           # Task-specific context (by task name)
//!       build.md
//!       test.md
//!       ...
//!     project.md       # General project context
//! ```
//!
//! ## Usage
//!
//! Context files are markdown with rules, patterns, and instructions
//! that get included in AI prompts for better conflict resolution,
//! code generation, and task execution.

use std::fs;
use std::path::{Path, PathBuf};

/// Find the project root by looking for common markers.
pub fn find_project_root() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    let mut current = cwd.as_path();

    loop {
        // Check for .ai/context directory
        if current.join(".ai/context").exists() {
            return Some(current.to_path_buf());
        }
        // Check for other common project markers
        if current.join(".git").exists()
            || current.join("flow.toml").exists()
            || current.join("Cargo.toml").exists()
            || current.join("package.json").exists()
        {
            return Some(current.to_path_buf());
        }
        current = current.parent()?;
    }
}

/// Load context for a specific flow command (e.g., "sync", "deploy").
pub fn load_command_context(command: &str) -> Option<String> {
    let root = find_project_root()?;
    let context_path = root.join(".ai/context/commands").join(format!("{}.md", command));
    load_context_file(&context_path)
}

/// Load context for a specific task name.
pub fn load_task_context(task_name: &str) -> Option<String> {
    let root = find_project_root()?;
    let context_path = root.join(".ai/context/tasks").join(format!("{}.md", task_name));
    load_context_file(&context_path)
}

/// Load the general project context.
pub fn load_project_context() -> Option<String> {
    let root = find_project_root()?;
    let context_path = root.join(".ai/context/project.md");
    load_context_file(&context_path)
}

/// Load all relevant context for a command, combining project + command context.
pub fn load_full_command_context(command: &str) -> String {
    let mut context = String::new();

    if let Some(project_ctx) = load_project_context() {
        context.push_str("## Project Context\n\n");
        context.push_str(&project_ctx);
        context.push_str("\n\n");
    }

    if let Some(cmd_ctx) = load_command_context(command) {
        context.push_str(&format!("## {} Command Context\n\n", command));
        context.push_str(&cmd_ctx);
        context.push_str("\n\n");
    }

    context
}

/// Load a context file if it exists.
fn load_context_file(path: &Path) -> Option<String> {
    if path.exists() {
        fs::read_to_string(path).ok()
    } else {
        None
    }
}

/// Check if any AI context exists for the current project.
pub fn has_ai_context() -> bool {
    find_project_root()
        .map(|root| root.join(".ai/context").exists())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_project_root_returns_some_in_git_repo() {
        // This test assumes we're running from within a git repo
        let root = find_project_root();
        assert!(root.is_some() || std::env::var("CI").is_ok());
    }
}
