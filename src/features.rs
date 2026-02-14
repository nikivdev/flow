//! Feature registry: read/write `.ai/features/*.md` files with YAML frontmatter.
//!
//! Features are committed project knowledge describing what capabilities exist,
//! which files implement them, and whether they have docs/tests.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Parsed feature file from `.ai/features/<name>.md`.
#[derive(Debug, Clone)]
pub struct FeatureEntry {
    pub name: String,
    pub description: String,
    pub status: String,
    pub files: Vec<String>,
    pub tests: Vec<String>,
    pub coverage: String,
    pub added_in: String,
    pub last_verified: String,
    pub created_at: String,
    pub updated_at: String,
    /// Markdown body after the frontmatter.
    pub content: String,
}

/// A feature whose tracked files overlap with the current diff.
#[derive(Debug)]
pub struct StaleFeature {
    pub name: String,
    pub stale_files: Vec<String>,
}

/// Return the `.ai/features/` directory for a project.
fn features_dir(project_root: &Path) -> PathBuf {
    project_root.join(".ai").join("features")
}

/// List all feature names in `.ai/features/`.
pub fn list_features(project_root: &Path) -> Result<Vec<String>> {
    let dir = features_dir(project_root);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in std::fs::read_dir(&dir).context("reading .ai/features/")? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().map_or(false, |e| e == "md") {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                names.push(stem.to_string());
            }
        }
    }
    names.sort();
    Ok(names)
}

/// Load a single feature file and parse its YAML frontmatter.
pub fn load_feature(path: &Path) -> Result<FeatureEntry> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading feature file {}", path.display()))?;
    parse_feature_file(&raw, path)
}

/// Load all features from `.ai/features/`.
pub fn load_all_features(project_root: &Path) -> Result<Vec<FeatureEntry>> {
    let dir = features_dir(project_root);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut features = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().map_or(false, |e| e == "md") {
            match load_feature(&path) {
                Ok(f) => features.push(f),
                Err(e) => eprintln!("warning: skipping {}: {}", path.display(), e),
            }
        }
    }
    Ok(features)
}

/// Scan `.ai/features/` and identify which are stale relative to the current diff.
pub fn scan_features(project_root: &Path, changed_files: &[String]) -> Vec<StaleFeature> {
    let features = match load_all_features(project_root) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };

    let changed_set: std::collections::HashSet<&str> =
        changed_files.iter().map(|s| s.as_str()).collect();

    let mut stale = Vec::new();
    for feat in &features {
        let overlap: Vec<String> = feat
            .files
            .iter()
            .filter(|f| changed_set.contains(f.as_str()))
            .cloned()
            .collect();
        if !overlap.is_empty() {
            stale.push(StaleFeature {
                name: feat.name.clone(),
                stale_files: overlap,
            });
        }
    }
    stale
}

/// Write/update a feature file with YAML frontmatter + markdown body.
pub fn save_feature(project_root: &Path, entry: &FeatureEntry) -> Result<()> {
    let dir = features_dir(project_root);
    std::fs::create_dir_all(&dir).context("creating .ai/features/")?;

    let path = dir.join(format!("{}.md", entry.name));
    let content = render_feature_file(entry);
    std::fs::write(&path, content)
        .with_context(|| format!("writing feature file {}", path.display()))?;
    Ok(())
}

/// Update the `last_verified` field of an existing feature.
pub fn update_feature_verified(project_root: &Path, name: &str, commit_sha: &str) -> Result<()> {
    let path = features_dir(project_root).join(format!("{}.md", name));
    if !path.exists() {
        return Ok(());
    }
    let mut entry = load_feature(&path)?;
    entry.last_verified = commit_sha.to_string();
    entry.updated_at = chrono_now();
    save_feature(project_root, &entry)
}

/// Update the test files list for an existing feature.
pub fn update_feature_tests(project_root: &Path, name: &str, test_files: &[String]) -> Result<()> {
    let path = features_dir(project_root).join(format!("{}.md", name));
    if !path.exists() {
        return Ok(());
    }
    let mut entry = load_feature(&path)?;
    // Merge new test files
    for tf in test_files {
        if !entry.tests.contains(tf) {
            entry.tests.push(tf.clone());
        }
    }
    if !test_files.is_empty() && entry.coverage == "none" {
        entry.coverage = "partial".to_string();
    }
    entry.updated_at = chrono_now();
    save_feature(project_root, &entry)
}

/// Apply quality results from the AI review: create new feature docs, update existing ones.
/// Returns a list of action descriptions (e.g., "created foo.md").
pub(crate) fn apply_quality_results(
    project_root: &Path,
    quality: &crate::commit::QualityResult,
    commit_sha: &str,
) -> Result<Vec<String>> {
    let mut actions = Vec::new();
    let now = chrono_now();

    // 1. Write new feature docs
    for new_feat in &quality.new_features {
        let entry = FeatureEntry {
            name: new_feat.name.clone(),
            description: new_feat.description.clone(),
            status: "active".to_string(),
            files: new_feat.files.clone(),
            tests: Vec::new(),
            coverage: "none".to_string(),
            added_in: commit_sha.to_string(),
            last_verified: commit_sha.to_string(),
            created_at: now.clone(),
            updated_at: now.clone(),
            content: new_feat.doc_content.clone(),
        };
        save_feature(project_root, &entry)?;
        actions.push(format!("created {}.md", new_feat.name));
    }

    // 2. Update last_verified for touched features that are current
    for touched in &quality.features_touched {
        if touched.doc_current {
            update_feature_verified(project_root, &touched.name, commit_sha)?;
        }
        if touched.has_tests {
            update_feature_tests(project_root, &touched.name, &touched.test_files)?;
        }
    }

    Ok(actions)
}

/// Build context about existing features for the AI review prompt.
pub fn features_context_for_review(project_root: &Path, changed_files: &[String]) -> String {
    let features = match load_all_features(project_root) {
        Ok(f) => f,
        Err(_) => return String::new(),
    };

    if features.is_empty() {
        return String::new();
    }

    let stale = scan_features(project_root, changed_files);
    let stale_names: std::collections::HashSet<&str> =
        stale.iter().map(|s| s.name.as_str()).collect();

    let mut ctx = String::from("\nExisting documented features in .ai/features/:\n");
    for feat in &features {
        let stale_marker = if stale_names.contains(feat.name.as_str()) {
            " [STALE - files changed in this diff]"
        } else {
            ""
        };
        ctx.push_str(&format!(
            "- {} ({}): {}{}\n",
            feat.name, feat.status, feat.description, stale_marker
        ));
    }
    ctx
}

// ── Internal helpers ────────────────────────────────────────────────

fn chrono_now() -> String {
    // Simple ISO 8601 without chrono dependency
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Format as basic ISO-ish timestamp
    format!("{}Z", now)
}

fn parse_feature_file(raw: &str, path: &Path) -> Result<FeatureEntry> {
    // Split frontmatter from content
    let (frontmatter, body) = if raw.starts_with("---\n") || raw.starts_with("---\r\n") {
        let after_open = if raw.starts_with("---\r\n") { 5 } else { 4 };
        if let Some(end) = raw[after_open..].find("\n---") {
            let fm_end = after_open + end;
            let body_start = fm_end + 4; // skip \n---
            let body_start = if raw[body_start..].starts_with('\n') {
                body_start + 1
            } else if raw[body_start..].starts_with("\r\n") {
                body_start + 2
            } else {
                body_start
            };
            (
                &raw[after_open..fm_end],
                raw[body_start..].trim().to_string(),
            )
        } else {
            ("", raw.to_string())
        }
    } else {
        ("", raw.to_string())
    };

    let fm = parse_yaml_frontmatter(frontmatter);
    let name = fm
        .get("name")
        .cloned()
        .or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_default();

    Ok(FeatureEntry {
        name,
        description: fm.get("description").cloned().unwrap_or_default(),
        status: fm
            .get("status")
            .cloned()
            .unwrap_or_else(|| "active".to_string()),
        files: parse_yaml_list(fm.get("files").map(|s| s.as_str()).unwrap_or("")),
        tests: parse_yaml_list(fm.get("tests").map(|s| s.as_str()).unwrap_or("")),
        coverage: fm
            .get("coverage")
            .cloned()
            .unwrap_or_else(|| "none".to_string()),
        added_in: fm.get("added_in").cloned().unwrap_or_default(),
        last_verified: fm.get("last_verified").cloned().unwrap_or_default(),
        created_at: fm.get("created_at").cloned().unwrap_or_default(),
        updated_at: fm.get("updated_at").cloned().unwrap_or_default(),
        content: body,
    })
}

/// Minimal YAML frontmatter parser for key: value pairs.
fn parse_yaml_frontmatter(fm: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut current_key = String::new();
    let mut in_list = false;
    let mut list_items = Vec::new();

    for line in fm.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Check if this is a list item
        if trimmed.starts_with("- ") && in_list {
            let value = trimmed[2..].trim().to_string();
            list_items.push(value);
            continue;
        }

        // If we were building a list, save it
        if in_list && !list_items.is_empty() {
            map.insert(current_key.clone(), list_items.join("\n"));
            list_items.clear();
            in_list = false;
        }

        // Parse key: value
        if let Some(colon_pos) = trimmed.find(':') {
            let key = trimmed[..colon_pos].trim().to_string();
            let value = trimmed[colon_pos + 1..].trim().to_string();
            if value.is_empty() {
                // This might be the start of a list
                current_key = key;
                in_list = true;
            } else {
                map.insert(key, value);
            }
        }
    }

    // Save any trailing list
    if in_list && !list_items.is_empty() {
        map.insert(current_key, list_items.join("\n"));
    }

    map
}

/// Parse a YAML list stored as newline-separated values.
fn parse_yaml_list(raw: &str) -> Vec<String> {
    if raw.is_empty() {
        return Vec::new();
    }
    raw.lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

fn render_feature_file(entry: &FeatureEntry) -> String {
    let mut out = String::from("---\n");
    out.push_str(&format!("name: {}\n", entry.name));
    out.push_str(&format!("description: {}\n", entry.description));
    out.push_str(&format!("status: {}\n", entry.status));

    if !entry.files.is_empty() {
        out.push_str("files:\n");
        for f in &entry.files {
            out.push_str(&format!("  - {}\n", f));
        }
    } else {
        out.push_str("files:\n");
    }

    if !entry.tests.is_empty() {
        out.push_str("tests:\n");
        for t in &entry.tests {
            out.push_str(&format!("  - {}\n", t));
        }
    } else {
        out.push_str("tests:\n");
    }

    out.push_str(&format!("coverage: {}\n", entry.coverage));
    out.push_str(&format!("added_in: {}\n", entry.added_in));
    out.push_str(&format!("last_verified: {}\n", entry.last_verified));
    out.push_str(&format!("created_at: {}\n", entry.created_at));
    out.push_str(&format!("updated_at: {}\n", entry.updated_at));
    out.push_str("---\n\n");
    out.push_str(&entry.content);
    if !entry.content.ends_with('\n') {
        out.push('\n');
    }
    out
}
