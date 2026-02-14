use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

use crate::cli::InitOpts;

const TEMPLATE: &str = r#"version = 1

[[tasks]]
name = "setup"
command = ""
description = "Project setup (fill me)"
shortcuts = ["s"]

[[tasks]]
name = "dev"
command = ""
description = "Start dev server (fill me)"
dependencies = ["setup"]
shortcuts = ["d"]

[skills]
sync_tasks = true
install = ["quality-bun-feature-delivery"]

[skills.codex]
generate_openai_yaml = true
force_reload_after_sync = true
task_skill_allow_implicit_invocation = false

[commit.skill_gate]
mode = "block"
required = ["quality-bun-feature-delivery"]

[commit.skill_gate.min_version]
quality-bun-feature-delivery = 2

# Bun-focused optional test gate:
#
#[commit.testing]
#mode = "block"
#runner = "bun"
#bun_repo_strict = true
#require_related_tests = true
#max_local_gate_seconds = 20
"#;

pub(crate) fn write_template(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create directory {}", parent.display()))?;
        }
    }

    fs::write(path, TEMPLATE).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

pub fn run(opts: InitOpts) -> Result<()> {
    let target = resolve_path(opts.path);
    if target.exists() {
        bail!("{} already exists; refusing to overwrite", target.display());
    }

    write_template(&target)?;
    println!("created {}", target.display());
    Ok(())
}

fn resolve_path(path: Option<PathBuf>) -> PathBuf {
    match path {
        Some(p) if p.is_absolute() => p,
        Some(p) => std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(p),
        None => std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("flow.toml"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_includes_codex_skill_baseline() {
        assert!(TEMPLATE.contains("[skills]"));
        assert!(TEMPLATE.contains("install = [\"quality-bun-feature-delivery\"]"));
        assert!(TEMPLATE.contains("[skills.codex]"));
        assert!(TEMPLATE.contains("[commit.skill_gate]"));
        assert!(TEMPLATE.contains("quality-bun-feature-delivery = 2"));
    }
}
