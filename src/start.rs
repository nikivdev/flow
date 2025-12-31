//! Project bootstrap and initialization.
//!
//! Creates `.ai/` folder structure with public (tracked) and internal (gitignored) sections.
//!
//! Structure:
//!   .ai/
//!   ├── actions/        # TRACKED - fixer/action scripts
//!   ├── skills/         # TRACKED - shared skills
//!   ├── tools/          # TRACKED - shared tools
//!   ├── review.md       # TRACKED - review instructions
//!   └── internal/       # GITIGNORED - private data
//!       ├── sessions/   # AI session data
//!       ├── checkpoints/
//!       ├── db/
//!       └── *.json      # Various state files

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Checkpoint names for tracking completed actions.
pub mod checkpoints {
    pub const AI_FOLDER_CREATED: &str = "ai_folder_created";
    pub const GITIGNORE_UPDATED: &str = "gitignore_updated";
    pub const DB_SCHEMA_CREATED: &str = "db_schema_created";
}

/// Run the start command to bootstrap the project.
pub fn run() -> Result<()> {
    let project_root = std::env::current_dir()?;

    // Create .ai/ folder structure
    if !has_checkpoint(&project_root, checkpoints::AI_FOLDER_CREATED) {
        create_ai_folder(&project_root)?;
        set_checkpoint(&project_root, checkpoints::AI_FOLDER_CREATED)?;
        println!("✓ Created .ai/ folder structure");
    }

    // Add .ai/internal/ to .gitignore
    if !has_checkpoint(&project_root, checkpoints::GITIGNORE_UPDATED) {
        update_gitignore(&project_root)?;
        set_checkpoint(&project_root, checkpoints::GITIGNORE_UPDATED)?;
        println!("✓ Updated .gitignore");
    }

    // Create database schema
    if !has_checkpoint(&project_root, checkpoints::DB_SCHEMA_CREATED) {
        create_db_schema(&project_root)?;
        set_checkpoint(&project_root, checkpoints::DB_SCHEMA_CREATED)?;
        println!("✓ Created .ai/internal/db/ with schema");
    }

    println!("\n✓ Project ready");
    println!("\nStructure:");
    println!("  .ai/");
    println!("  ├── actions/      # Tracked - fixer scripts");
    println!("  ├── skills/       # Tracked - shared skills");
    println!("  ├── tools/        # Tracked - shared tools");
    println!("  └── internal/     # Gitignored - private data");
    println!("      ├── sessions/ # AI conversation history");
    println!("      ├── db/       # SQLite database");
    println!("      └── ...       # Checkpoints, logs, etc.");
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
    project_root
        .join(".ai")
        .join("internal")
        .join("checkpoints")
        .join(name)
}

/// Create the .ai/ folder structure.
fn create_ai_folder(project_root: &Path) -> Result<()> {
    let ai_dir = project_root.join(".ai");
    let internal_dir = ai_dir.join("internal");

    // Public folders (tracked in git)
    let public_dirs = [
        ai_dir.clone(),
        ai_dir.join("actions"),
        ai_dir.join("skills"),
        ai_dir.join("tools"),
    ];

    // Private folders (gitignored)
    let internal_dirs = [
        internal_dir.clone(),
        internal_dir.join("checkpoints"),
        internal_dir.join("sessions"),
        internal_dir.join("db"),
    ];

    for dir in public_dirs.iter().chain(internal_dirs.iter()) {
        fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    }

    Ok(())
}

/// Create the database schema files.
fn create_db_schema(project_root: &Path) -> Result<()> {
    let db_dir = project_root.join(".ai/internal/db");
    fs::create_dir_all(&db_dir)?;

    // Create schema.ts with drizzle-orm schema
    let schema_path = db_dir.join("schema.ts");
    if !schema_path.exists() {
        fs::write(&schema_path, SCHEMA_TEMPLATE)?;
    }

    // Create index.ts for database connection
    let index_path = db_dir.join("index.ts");
    if !index_path.exists() {
        fs::write(&index_path, DB_INDEX_TEMPLATE)?;
    }

    // Create package.json for dependencies
    let package_path = db_dir.join("package.json");
    if !package_path.exists() {
        fs::write(&package_path, DB_PACKAGE_TEMPLATE)?;
    }

    Ok(())
}

const SCHEMA_TEMPLATE: &str = r#"// .ai/internal/db/schema.ts
// Database schema for AI project data using drizzle-orm
import { sqliteTable, text, integer, blob } from "drizzle-orm/sqlite-core"

// Research notes and findings
export const research = sqliteTable("research", {
  id: text("id").primaryKey(),
  title: text("title").notNull(),
  content: text("content").notNull(),
  source: text("source"), // URL, file path, or reference
  tags: text("tags"), // JSON array of tags
  createdAt: integer("created_at", { mode: "timestamp" }).notNull(),
  updatedAt: integer("updated_at", { mode: "timestamp" }),
})

// Tasks and todos tracked by agents
export const tasks = sqliteTable("tasks", {
  id: text("id").primaryKey(),
  title: text("title").notNull(),
  description: text("description"),
  status: text("status").notNull().default("pending"), // pending, in_progress, completed, blocked
  priority: integer("priority").default(0),
  parentId: text("parent_id"), // for subtasks
  createdAt: integer("created_at", { mode: "timestamp" }).notNull(),
  completedAt: integer("completed_at", { mode: "timestamp" }),
})

// Files being tracked or generated
export const files = sqliteTable("files", {
  id: text("id").primaryKey(),
  path: text("path").notNull().unique(),
  contentHash: text("content_hash"),
  description: text("description"),
  generatedBy: text("generated_by"), // agent/tool that created it
  createdAt: integer("created_at", { mode: "timestamp" }).notNull(),
  updatedAt: integer("updated_at", { mode: "timestamp" }),
})

// Key-value store for agent memory/state
export const memory = sqliteTable("memory", {
  key: text("key").primaryKey(),
  value: text("value").notNull(), // JSON serialized
  expiresAt: integer("expires_at", { mode: "timestamp" }),
  createdAt: integer("created_at", { mode: "timestamp" }).notNull(),
})

// External service connections and context
export const connections = sqliteTable("connections", {
  id: text("id").primaryKey(),
  service: text("service").notNull(), // github, x, linear, etc.
  accountId: text("account_id"),
  metadata: text("metadata"), // JSON with service-specific data
  syncedAt: integer("synced_at", { mode: "timestamp" }),
  createdAt: integer("created_at", { mode: "timestamp" }).notNull(),
})
"#;

const DB_INDEX_TEMPLATE: &str = r#"// .ai/internal/db/index.ts
// Database connection and utilities
import { drizzle } from "drizzle-orm/bun-sqlite"
import { Database } from "bun:sqlite"
import * as schema from "./schema"

const sqlite = new Database(".ai/internal/db/db.sqlite")
export const db = drizzle(sqlite, { schema })

// Re-export schema for convenience
export * from "./schema"

// Helper to generate IDs
export const genId = () => crypto.randomUUID()

// Helper to get current timestamp
export const now = () => new Date()
"#;

const DB_PACKAGE_TEMPLATE: &str = r#"{
  "name": "@ai/db",
  "type": "module",
  "dependencies": {
    "drizzle-orm": "^0.38.0"
  },
  "devDependencies": {
    "drizzle-kit": "^0.30.0"
  },
  "scripts": {
    "generate": "drizzle-kit generate",
    "migrate": "drizzle-kit migrate",
    "studio": "drizzle-kit studio"
  }
}
"#;

/// Add .ai/internal/ to .gitignore if not already present.
/// Only the internal folder is gitignored - actions, skills, tools are tracked.
fn update_gitignore(project_root: &Path) -> Result<()> {
    let gitignore_path = project_root.join(".gitignore");

    let content = if gitignore_path.exists() {
        fs::read_to_string(&gitignore_path)?
    } else {
        String::new()
    };

    // Check if .ai/internal/ is already in .gitignore
    let internal_patterns = [".ai/internal/", ".ai/internal", "/.ai/internal/", "/.ai/internal"];
    let already_ignored = content
        .lines()
        .any(|line| internal_patterns.iter().any(|p| line.trim() == *p));

    if !already_ignored {
        let mut new_content = content;
        if !new_content.is_empty() && !new_content.ends_with('\n') {
            new_content.push('\n');
        }
        new_content.push_str(".ai/internal/\n");
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

        // Public folders
        assert!(root.join(".ai").exists());
        assert!(root.join(".ai/actions").exists());
        assert!(root.join(".ai/skills").exists());
        assert!(root.join(".ai/tools").exists());
        // Internal folders
        assert!(root.join(".ai/internal").exists());
        assert!(root.join(".ai/internal/checkpoints").exists());
        assert!(root.join(".ai/internal/sessions").exists());
        assert!(root.join(".ai/internal/db").exists());
    }

    #[test]
    fn test_update_gitignore_new_file() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        update_gitignore(root).unwrap();

        let content = fs::read_to_string(root.join(".gitignore")).unwrap();
        assert!(content.contains(".ai/internal/"));
    }

    #[test]
    fn test_update_gitignore_existing() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        fs::write(root.join(".gitignore"), "node_modules/\n").unwrap();

        update_gitignore(root).unwrap();

        let content = fs::read_to_string(root.join(".gitignore")).unwrap();
        assert!(content.contains("node_modules/"));
        assert!(content.contains(".ai/internal/"));
    }

    #[test]
    fn test_update_gitignore_already_present() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        fs::write(root.join(".gitignore"), ".ai/internal/\nnode_modules/\n").unwrap();

        update_gitignore(root).unwrap();

        let content = fs::read_to_string(root.join(".gitignore")).unwrap();
        // Should not duplicate
        assert_eq!(content.matches(".ai/internal/").count(), 1);
    }
}
