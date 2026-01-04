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
    run_at(&project_root)
}

/// Bootstrap a project at the provided root path.
pub fn run_at(project_root: &Path) -> Result<()> {

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

    // Create flow.toml if missing
    if create_flow_toml_if_missing(&project_root)? {
        println!("✓ Created flow.toml");
    }

    // Materialize .claude/ and .codex/ from .ai/
    materialize_tool_folders(&project_root)?;

    println!("\n✓ Project ready");
    println!("\nStructure:");
    println!("  .ai/");
    println!("  ├── actions/      # Tracked - fixer scripts");
    println!("  ├── skills/       # Tracked - shared skills");
    println!("  ├── tools/        # Tracked - shared tools");
    println!("  ├── flox/         # Tracked - flox manifest");
    println!("  ├── docs/         # Tracked - auto-generated docs");
    println!("  ├── agents.md     # Tracked - agent instructions");
    println!("  └── internal/     # Gitignored - private data");
    println!("  .claude/          # Gitignored - symlinks to .ai/");
    println!("  .codex/           # Gitignored - symlinks to .ai/");
    println!("  .flox/            # Gitignored - symlinks to .ai/flox/");
    Ok(())
}

pub fn is_bootstrapped(project_root: &Path) -> bool {
    has_checkpoint(project_root, checkpoints::AI_FOLDER_CREATED)
        && has_checkpoint(project_root, checkpoints::GITIGNORE_UPDATED)
        && has_checkpoint(project_root, checkpoints::DB_SCHEMA_CREATED)
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
        ai_dir.join("flox"),
        ai_dir.join("docs"),
    ];

    // Private folders (gitignored)
    let internal_dirs = [
        internal_dir.clone(),
        internal_dir.join("checkpoints"),
        internal_dir.join("sessions"),
        internal_dir.join("db"),
        internal_dir.join("commits"),
    ];

    for dir in public_dirs.iter().chain(internal_dirs.iter()) {
        fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    }

    Ok(())
}

const DEFAULT_FLOW_TEMPLATE: &str = r#"# flow

[[tasks]]
name = "setup"
command = ""
description = "Project setup (fill me)"

[[tasks]]
name = "dev"
command = ""
description = "Start dev server (fill me)"
"#;

const RUST_FLOW_TEMPLATE: &str = r#"version = 1

[[tasks]]
name = "dev"
command = "cargo run"
description = "Run the default binary"
dependencies = ["cargo"]

[[tasks]]
name = "build"
command = "cargo build"
description = "Build the project"
dependencies = ["cargo"]

[[tasks]]
name = "test"
command = "cargo test"
description = "Run tests"
dependencies = ["cargo"]

[[tasks]]
name = "fmt"
command = "cargo fmt"
description = "Format Rust code"
dependencies = ["cargo"]

[[tasks]]
name = "clippy"
command = "cargo clippy"
description = "Lint with clippy"
dependencies = ["cargo"]

[deps]
cargo = "cargo"
"#;

fn create_flow_toml_if_missing(project_root: &Path) -> Result<bool> {
    let flow_path = project_root.join("flow.toml");
    if flow_path.exists() {
        return Ok(false);
    }

    let template = if project_root.join("Cargo.toml").exists() {
        RUST_FLOW_TEMPLATE
    } else {
        DEFAULT_FLOW_TEMPLATE
    };

    fs::write(&flow_path, template)
        .with_context(|| format!("failed to write {}", flow_path.display()))?;
    Ok(true)
}

/// Materialize .claude/, .codex/, and .flox/ folders with symlinks to .ai/
fn materialize_tool_folders(project_root: &Path) -> Result<()> {
    use std::os::unix::fs::symlink;

    let ai_dir = project_root.join(".ai");
    let agents_source = ai_dir.join("agents.md");
    let skills_source = ai_dir.join("skills");

    // Materialize .claude/ and .codex/
    for tool_dir in [".claude", ".codex"] {
        let tool_path = project_root.join(tool_dir);
        fs::create_dir_all(&tool_path)?;

        // Symlink skills -> ../.ai/skills
        let skills_link = tool_path.join("skills");
        if !skills_link.exists() && skills_source.exists() {
            let _ = symlink("../.ai/skills", &skills_link);
        }

        // Symlink agents.md -> ../.ai/agents.md
        let agents_link = tool_path.join("agents.md");
        if !agents_link.exists() && agents_source.exists() {
            let _ = symlink("../.ai/agents.md", &agents_link);
        }
    }

    // Materialize .flox/ from .ai/flox/
    let flox_source = ai_dir.join("flox");
    let manifest_source = flox_source.join("manifest.toml");
    if manifest_source.exists() {
        let flox_dir = project_root.join(".flox");
        let flox_env_dir = flox_dir.join("env");
        fs::create_dir_all(&flox_env_dir)?;

        // Symlink manifest.toml -> ../../.ai/flox/manifest.toml
        let manifest_link = flox_env_dir.join("manifest.toml");
        if !manifest_link.exists() {
            let _ = symlink("../../.ai/flox/manifest.toml", &manifest_link);
        }

        // Create env.json files that flox expects
        let env_json = flox_dir.join("env.json");
        if !env_json.exists() {
            fs::write(&env_json, r#"{
  "version": 1,
  "manifest": "env/manifest.toml",
  "lockfile": "env/manifest.lock"
}"#)?;
        }

        let env_env_json = flox_env_dir.join("env.json");
        if !env_env_json.exists() {
            let manifest_path = flox_env_dir.join("manifest.toml");
            let lockfile_path = flox_env_dir.join("manifest.lock");
            fs::write(&env_env_json, format!(r#"{{
  "version": 1,
  "manifest": "{}",
  "lockfile": "{}"
}}"#, manifest_path.display(), lockfile_path.display()))?;
        }
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

/// Flow gitignore patterns.
const FLOW_GITIGNORE_SECTION: &str = "\
# flow
.ai/internal/
.claude/
.codex/
.flox/
";

/// Add flow section to .gitignore if not already present.
fn update_gitignore(project_root: &Path) -> Result<()> {
    let gitignore_path = project_root.join(".gitignore");

    let content = if gitignore_path.exists() {
        fs::read_to_string(&gitignore_path)?
    } else {
        String::new()
    };

    // Check if flow section already exists
    if content.contains("# flow") {
        return Ok(());
    }

    // Also check if all patterns are already present (legacy)
    let has_ai_internal = content.lines().any(|l| l.trim() == ".ai/internal/");
    let has_claude = content.lines().any(|l| l.trim() == ".claude/");
    let has_codex = content.lines().any(|l| l.trim() == ".codex/");
    let has_flox = content.lines().any(|l| l.trim() == ".flox/");

    if has_ai_internal && has_claude && has_codex && has_flox {
        return Ok(());
    }

    // Add flow section
    let mut new_content = content;
    if !new_content.is_empty() && !new_content.ends_with('\n') {
        new_content.push('\n');
    }
    if !new_content.is_empty() && !new_content.ends_with("\n\n") {
        new_content.push('\n');
    }
    new_content.push_str(FLOW_GITIGNORE_SECTION);
    fs::write(&gitignore_path, new_content)?;

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
        assert!(content.contains("# flow"));
        assert!(content.contains(".ai/internal/"));
        assert!(content.contains(".claude/"));
        assert!(content.contains(".codex/"));
    }

    #[test]
    fn test_update_gitignore_existing() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        fs::write(root.join(".gitignore"), "node_modules/\n").unwrap();

        update_gitignore(root).unwrap();

        let content = fs::read_to_string(root.join(".gitignore")).unwrap();
        assert!(content.contains("node_modules/"));
        assert!(content.contains("# flow"));
        assert!(content.contains(".ai/internal/"));
        assert!(content.contains(".claude/"));
        assert!(content.contains(".codex/"));
    }

    #[test]
    fn test_update_gitignore_already_present() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        fs::write(root.join(".gitignore"), "# flow\n.ai/internal/\n.claude/\n.codex/\n").unwrap();

        update_gitignore(root).unwrap();

        let content = fs::read_to_string(root.join(".gitignore")).unwrap();
        // Should not duplicate
        assert_eq!(content.matches("# flow").count(), 1);
        assert_eq!(content.matches(".ai/internal/").count(), 1);
    }
}
