//! Standalone invariant checking for projects.
//!
//! Reads [invariants] from flow.toml and checks the working tree or staged diff.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::config::{self, InvariantsConfig};

/// A single invariant finding.
#[derive(Debug)]
pub struct Finding {
    pub severity: String,
    pub category: String,
    pub message: String,
    pub file: Option<String>,
}

/// Result of running invariant checks.
#[derive(Debug)]
pub struct Report {
    pub findings: Vec<Finding>,
    pub invariants_loaded: bool,
    pub mode: String,
}

/// Check project invariants against the working tree.
/// If `staged_only` is true, checks only staged (cached) diff.
pub fn check(root: &Path, staged_only: bool) -> Result<Report> {
    let cfg = config::load_or_default(root.join("flow.toml"));
    let Some(inv) = cfg.invariants else {
        println!("No [invariants] section in flow.toml");
        return Ok(Report {
            findings: Vec::new(),
            invariants_loaded: false,
            mode: "off".to_string(),
        });
    };
    let mode = inv.mode.as_deref().unwrap_or("warn").to_ascii_lowercase();
    if mode == "off" {
        println!("Invariants are disabled (mode=off).");
        return Ok(Report {
            findings: Vec::new(),
            invariants_loaded: true,
            mode,
        });
    }

    let mut findings = Vec::new();

    // Get diff.
    let diff_args = if staged_only {
        vec!["diff", "--cached"]
    } else {
        vec!["diff", "HEAD"]
    };
    let diff = git_capture(root, &diff_args).unwrap_or_default();
    let changed_files = changed_files_from_diff(&diff);

    // 1. Forbidden patterns in diff.
    check_forbidden_patterns(&inv, &diff, &mut findings);

    // 2. Dependency policy.
    if let Some(deps_config) = &inv.deps {
        let policy = deps_config.policy.as_deref().unwrap_or("approval_required");
        if policy == "approval_required" && !deps_config.approved.is_empty() {
            check_deps(root, &changed_files, &deps_config.approved, &mut findings);
        }
    }

    // 3. File size limits.
    if let Some(files_config) = &inv.files {
        if let Some(max_lines) = files_config.max_lines {
            check_file_sizes(root, &changed_files, max_lines, &mut findings);
        }
    }

    // Print results.
    print_report(&inv, &findings);

    let has_blocking = findings
        .iter()
        .any(|f| f.severity == "critical" || f.severity == "warning");
    if mode == "block" && has_blocking {
        anyhow::bail!(
            "Invariant violations found (mode=block): {} finding(s)",
            findings.len()
        );
    }

    Ok(Report {
        findings,
        invariants_loaded: true,
        mode,
    })
}

fn check_forbidden_patterns(inv: &InvariantsConfig, diff: &str, findings: &mut Vec<Finding>) {
    // Skip flow.toml itself — it contains the forbidden list definitions.
    let skip_files = ["flow.toml"];

    for pattern in &inv.forbidden {
        let pat_lower = pattern.to_lowercase();
        let mut current_file: Option<String> = None;
        let mut skip_current = false;
        for line in diff.lines() {
            if line.starts_with("+++ b/") {
                let file = line
                    .strip_prefix("+++ b/")
                    .unwrap_or("")
                    .trim()
                    .trim_matches('"');
                current_file = Some(file.to_string());
                skip_current = skip_files.iter().any(|s| file.ends_with(s));
                continue;
            }
            if current_file
                .as_deref()
                .is_some_and(|f| f.trim().trim_matches('"').ends_with("flow.toml"))
            {
                continue;
            }
            if skip_current {
                continue;
            }
            if !line.starts_with('+') || line.starts_with("+++") {
                continue;
            }
            if line.to_lowercase().contains(&pat_lower) {
                findings.push(Finding {
                    severity: "warning".to_string(),
                    category: "forbidden".to_string(),
                    message: format!("Forbidden pattern '{}' found", pattern),
                    file: current_file.clone(),
                });
                break;
            }
        }
    }
}

fn check_deps(
    root: &Path,
    changed_files: &[String],
    approved: &[String],
    findings: &mut Vec<Finding>,
) {
    // Check all package.json files in repo, not just changed ones.
    let pkg_files: Vec<PathBuf> = if changed_files.iter().any(|f| f.ends_with("package.json")) {
        changed_files
            .iter()
            .filter(|f| f.ends_with("package.json"))
            .map(|f| root.join(f))
            .collect()
    } else {
        // Also check existing package.json for a full health scan.
        find_package_jsons(root)
    };

    for pkg_path in pkg_files {
        let Ok(contents) = fs::read_to_string(&pkg_path) else {
            continue;
        };
        let rel = pkg_path
            .strip_prefix(root)
            .unwrap_or(&pkg_path)
            .display()
            .to_string();
        check_unapproved_deps(&contents, approved, &rel, findings);
    }
}

fn check_unapproved_deps(
    package_json: &str,
    approved: &[String],
    file_path: &str,
    findings: &mut Vec<Finding>,
) {
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(package_json) else {
        return;
    };

    let dep_sections = ["dependencies", "devDependencies", "peerDependencies"];
    for section in &dep_sections {
        if let Some(deps) = parsed.get(section).and_then(|v| v.as_object()) {
            for dep_name in deps.keys() {
                if !approved.iter().any(|a| a == dep_name) {
                    findings.push(Finding {
                        severity: "warning".to_string(),
                        category: "deps".to_string(),
                        message: format!("'{}' ({}) not on approved list", dep_name, section),
                        file: Some(file_path.to_string()),
                    });
                }
            }
        }
    }
}

fn check_file_sizes(
    root: &Path,
    changed_files: &[String],
    max_lines: u32,
    findings: &mut Vec<Finding>,
) {
    for file in changed_files {
        let full = root.join(file);
        if let Ok(contents) = fs::read_to_string(&full) {
            let line_count = contents.lines().count() as u32;
            if line_count > max_lines {
                findings.push(Finding {
                    severity: "warning".to_string(),
                    category: "files".to_string(),
                    message: format!("{} lines (max {})", line_count, max_lines),
                    file: Some(file.clone()),
                });
            }
        }
    }
}

fn find_package_jsons(root: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let root_pkg = root.join("package.json");
    if root_pkg.exists() {
        result.push(root_pkg);
    }
    // Check common subdirs.
    for subdir in &["api/ts", "web", "packages"] {
        let pkg = root.join(subdir).join("package.json");
        if pkg.exists() {
            result.push(pkg);
        }
    }
    result
}

fn print_report(inv: &InvariantsConfig, findings: &[Finding]) {
    println!("Invariants loaded from flow.toml\n");

    if let Some(style) = inv.architecture_style.as_deref() {
        println!("  Architecture: {}", style);
    }
    if !inv.non_negotiable.is_empty() {
        println!("  Non-negotiable rules: {}", inv.non_negotiable.len());
    }
    if !inv.forbidden.is_empty() {
        println!("  Forbidden patterns: {}", inv.forbidden.len());
    }
    if !inv.terminology.is_empty() {
        println!("  Terminology terms: {}", inv.terminology.len());
    }
    if let Some(deps) = &inv.deps {
        println!("  Approved deps: {}", deps.approved.len());
    }
    if let Some(files) = &inv.files {
        if let Some(max) = files.max_lines {
            println!("  Max lines per file: {}", max);
        }
    }

    println!();

    if findings.is_empty() {
        println!("No findings.");
        return;
    }

    let warnings = findings.iter().filter(|f| f.severity == "warning").count();
    let notes = findings.iter().filter(|f| f.severity == "note").count();
    let criticals = findings.iter().filter(|f| f.severity == "critical").count();

    println!(
        "Findings: {} critical, {} warning, {} note\n",
        criticals, warnings, notes
    );

    for f in findings {
        let icon = match f.severity.as_str() {
            "critical" => "!!",
            "warning" => "!",
            _ => "i",
        };
        let loc = f.file.as_deref().unwrap_or("(repo)");
        println!("  [{}:{}] {} — {}", icon, f.category, loc, f.message);
    }
}

fn changed_files_from_diff(diff: &str) -> Vec<String> {
    let mut files = Vec::new();
    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            if path != "/dev/null" {
                files.push(path.to_string());
            }
        }
    }
    files.sort();
    files.dedup();
    files
}

fn git_capture(workdir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .current_dir(workdir)
        .args(args)
        .output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn forbidden_scan_ignores_flow_toml_lines() {
        let inv = InvariantsConfig {
            forbidden: vec!["useState(".to_string()],
            terminology: HashMap::new(),
            ..Default::default()
        };
        let diff = r#"diff --git a/flow.toml b/flow.toml
+++ b/flow.toml
+forbidden = ["useState("]
diff --git a/web/app.tsx b/web/app.tsx
+++ b/web/app.tsx
+const x = useState(0)
"#;

        let mut findings = Vec::new();
        check_forbidden_patterns(&inv, diff, &mut findings);

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].file.as_deref(), Some("web/app.tsx"));
    }

    #[test]
    fn dep_scan_marks_unapproved_as_warning() {
        let pkg = r#"{
          "dependencies": { "react": "^18.0.0", "@reatom/core": "^3.0.0" }
        }"#;
        let approved = vec!["@reatom/core".to_string()];
        let mut findings = Vec::new();

        check_unapproved_deps(pkg, &approved, "package.json", &mut findings);

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, "warning");
        assert!(findings[0].message.contains("react"));
    }
}
