use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use chrono::Local;

use crate::ai_context;
use crate::cli::ArchiveOpts;

pub fn run(opts: ArchiveOpts) -> Result<()> {
    let root =
        ai_context::find_project_root().ok_or_else(|| anyhow::anyhow!("project root not found"))?;
    let root = fs::canonicalize(&root).unwrap_or(root);
    let project_name = root
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("project");

    let message = opts.message.trim();
    if message.is_empty() {
        bail!("archive message cannot be empty");
    }
    let slug = sanitize_segment(message);
    if slug.is_empty() {
        bail!("archive message must include at least one letter or number");
    }

    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("could not resolve home directory"))?
        .to_path_buf();
    let archive_root = home.join("archive").join("code");
    fs::create_dir_all(&archive_root).with_context(|| {
        format!(
            "failed to create archive directory {}",
            archive_root.display()
        )
    })?;

    let code_root = fs::canonicalize(home.join("code")).unwrap_or_else(|_| home.join("code"));
    let rel_path = root.strip_prefix(&code_root).ok();
    let (dest_parent, base_project) = if let Some(rel) = rel_path {
        let parent = rel
            .parent()
            .map(|p| archive_root.join(p))
            .unwrap_or_else(|| archive_root.clone());
        let name = rel
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(project_name)
            .to_string();
        (parent, name)
    } else {
        (archive_root.clone(), project_name.to_string())
    };

    fs::create_dir_all(&dest_parent).with_context(|| {
        format!(
            "failed to create archive directory {}",
            dest_parent.display()
        )
    })?;

    let date_suffix = Local::now()
        .format("%b-%d-%y")
        .to_string()
        .to_ascii_lowercase();
    let base_name = format!("{}-{}-{}", base_project, slug, date_suffix);
    let mut dest = dest_parent.join(&base_name);
    if dest.exists() {
        let suffix = Local::now().format("%Y%m%d-%H%M%S");
        dest = dest_parent.join(format!("{}-{}", base_name, suffix));
    }

    copy_dir_all(&root, &dest, &ArchiveFilter::default())?;
    println!("Archived {} -> {}", root.display(), dest.display());
    Ok(())
}

#[derive(Default)]
struct ArchiveFilter {
    skip_names: Vec<&'static str>,
}

impl ArchiveFilter {
    fn default() -> Self {
        Self {
            skip_names: vec![
                ".jj",
                "node_modules",
                "target",
                "dist",
                "build",
                ".next",
                ".turbo",
                ".cache",
            ],
        }
    }

    fn should_skip(&self, path: &Path) -> bool {
        path.file_name()
            .and_then(|name| name.to_str())
            .map(|name| self.skip_names.contains(&name))
            .unwrap_or(false)
    }
}

fn copy_dir_all(from: &Path, to: &Path, filter: &ArchiveFilter) -> Result<()> {
    fs::create_dir_all(to).with_context(|| format!("failed to create {}", to.display()))?;
    for entry in fs::read_dir(from).with_context(|| format!("failed to read {}", from.display()))? {
        let entry = entry?;
        let path = entry.path();
        if filter.should_skip(&path) {
            continue;
        }
        let file_type = entry.file_type()?;
        let target = to.join(entry.file_name());

        if target.exists() {
            bail!("Refusing to overwrite {}", target.display());
        }

        if file_type.is_dir() {
            copy_dir_all(&path, &target, filter)?;
        } else if file_type.is_file() {
            fs::copy(&path, &target)
                .with_context(|| format!("failed to copy {}", path.display()))?;
        } else if file_type.is_symlink() {
            let link_target = fs::read_link(&path)
                .with_context(|| format!("failed to read link {}", path.display()))?;
            copy_symlink(&link_target, &target)?;
        }
    }
    Ok(())
}

fn copy_symlink(target: &Path, dest: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, dest)
            .with_context(|| format!("failed to create symlink {}", dest.display()))?;
        return Ok(());
    }
    #[cfg(not(unix))]
    {
        let metadata =
            fs::metadata(target).with_context(|| format!("failed to read {}", target.display()))?;
        if metadata.is_dir() {
            copy_dir_all(target, dest, &ArchiveFilter::default())?;
        } else {
            fs::copy(target, dest)
                .with_context(|| format!("failed to copy {}", target.display()))?;
        }
        Ok(())
    }
}

fn sanitize_segment(value: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}
