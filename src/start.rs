//! Project bootstrap and initialization.
//!
//! Creates `.ai/` folder structure and manages checkpoints to avoid
//! re-running the same initialization steps.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Checkpoint names for tracking completed actions.
pub mod checkpoints {
    pub const AI_FOLDER_CREATED: &str = "ai_folder_created";
    pub const GITIGNORE_UPDATED: &str = "gitignore_updated";
}

/// Run the start command to bootstrap the project.
pub fn run() -> Result<()> {
    let project_root = std::env::current_dir()?;

    // Create .ai/ folder structure
    if !has_checkpoint(&project_root, checkpoints::AI_FOLDER_CREATED) {
        create_ai_folder(&project_root)?;
        set_checkpoint(&project_root, checkpoints::AI_FOLDER_CREATED)?;
        println!("✓ Created .ai/ folder");
    }

    // Add .ai/ to .gitignore
    if !has_checkpoint(&project_root, checkpoints::GITIGNORE_UPDATED) {
        update_gitignore(&project_root)?;
        set_checkpoint(&project_root, checkpoints::GITIGNORE_UPDATED)?;
        println!("✓ Added .ai/ to .gitignore");
    }

    println!("✓ Project ready");
    Ok(())
}

/// Check if a checkpoint exists.
pub fn has_checkpoint(project_root: &Path, name: &str) -> bool {
    checkpoint_path(project_root, name).exists()
}

/// Set a checkpoint (creates an empty file).
pub fn set_checkpoint(project_root: &Path, name: &str) -> Result<()> {
    let path = checkpoint_path(project_root, name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, "")?;
    Ok(())
}

/// Clear a checkpoint.
#[allow(dead_code)]
pub fn clear_checkpoint(project_root: &Path, name: &str) -> Result<()> {
    let path = checkpoint_path(project_root, name);
    if path.exists() {
        fs::remove_file(&path)?;
    }
    Ok(())
}

/// Get the path to a checkpoint file.
fn checkpoint_path(project_root: &Path, name: &str) -> PathBuf {
    project_root.join(".ai").join("checkpoints").join(name)
}

/// Create the .ai/ folder structure.
fn create_ai_folder(project_root: &Path) -> Result<()> {
    let ai_dir = project_root.join(".ai");

    // Create main .ai/ folder and subdirectories
    let dirs = [
        ai_dir.clone(),
        ai_dir.join("checkpoints"),
        ai_dir.join("sessions"),
        ai_dir.join("skills"),
    ];

    for dir in &dirs {
        fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    }

    Ok(())
}

/// Add .ai/ to .gitignore if not already present.
fn update_gitignore(project_root: &Path) -> Result<()> {
    let gitignore_path = project_root.join(".gitignore");

    let content = if gitignore_path.exists() {
        fs::read_to_string(&gitignore_path)?
    } else {
        String::new()
    };

    // Check if .ai/ is already in .gitignore
    let ai_patterns = [".ai/", ".ai", "/.ai/", "/.ai"];
    let already_ignored = content
        .lines()
        .any(|line| ai_patterns.iter().any(|p| line.trim() == *p));

    if !already_ignored {
        let mut new_content = content;
        if !new_content.is_empty() && !new_content.ends_with('\n') {
            new_content.push('\n');
        }
        new_content.push_str(".ai/\n");
        fs::write(&gitignore_path, new_content)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_checkpoint_lifecycle() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        assert!(!has_checkpoint(root, "test_checkpoint"));

        set_checkpoint(root, "test_checkpoint").unwrap();
        assert!(has_checkpoint(root, "test_checkpoint"));

        clear_checkpoint(root, "test_checkpoint").unwrap();
        assert!(!has_checkpoint(root, "test_checkpoint"));
    }

    #[test]
    fn test_create_ai_folder() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        create_ai_folder(root).unwrap();

        assert!(root.join(".ai").exists());
        assert!(root.join(".ai/checkpoints").exists());
        assert!(root.join(".ai/sessions").exists());
        assert!(root.join(".ai/skills").exists());
    }

    #[test]
    fn test_update_gitignore_new_file() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        update_gitignore(root).unwrap();

        let content = fs::read_to_string(root.join(".gitignore")).unwrap();
        assert!(content.contains(".ai/"));
    }

    #[test]
    fn test_update_gitignore_existing() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        fs::write(root.join(".gitignore"), "node_modules/\n").unwrap();

        update_gitignore(root).unwrap();

        let content = fs::read_to_string(root.join(".gitignore")).unwrap();
        assert!(content.contains("node_modules/"));
        assert!(content.contains(".ai/"));
    }

    #[test]
    fn test_update_gitignore_already_present() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        fs::write(root.join(".gitignore"), ".ai/\nnode_modules/\n").unwrap();

        update_gitignore(root).unwrap();

        let content = fs::read_to_string(root.join(".gitignore")).unwrap();
        // Should not duplicate
        assert_eq!(content.matches(".ai/").count(), 1);
    }
}
