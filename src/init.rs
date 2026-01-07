use std::{fs, path::{Path, PathBuf}};

use anyhow::{Context, Result, bail};

use crate::cli::InitOpts;

const TEMPLATE: &str = r#"# flow

[[tasks]]
name = "setup"
command = ""
description = "Project setup (fill me)"

[[tasks]]
name = "dev"
command = ""
description = "Start dev server (fill me)"
"#;

pub(crate) fn write_template(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create directory {}", parent.display()))?;
        }
    }

    fs::write(path, TEMPLATE)
        .with_context(|| format!("failed to write {}", path.display()))?;
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
