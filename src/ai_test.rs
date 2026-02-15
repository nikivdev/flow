use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::cli::AiTestNewOpts;

pub fn run(opts: AiTestNewOpts) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let project_root = find_project_root(&cwd)?;
    let base_dir = project_root.join(normalize_relative_dir(&opts.dir)?);
    let rel_file = normalize_test_name(&opts.name, opts.spec)?;
    let full_path = base_dir.join(&rel_file);

    if full_path.exists() && !opts.force {
        bail!(
            "scratch test already exists: {} (use --force to overwrite)",
            full_path.display()
        );
    }

    if let Some(parent) = full_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let title = rel_file
        .to_string_lossy()
        .replace('\\', "/")
        .trim_end_matches(".ts")
        .trim_end_matches(".tsx")
        .trim_end_matches(".js")
        .trim_end_matches(".jsx")
        .trim_end_matches(".mjs")
        .trim_end_matches(".cjs")
        .to_string();

    let template = format!(
        "import {{ describe, it }} from \"bun:test\";\n\n\
describe(\"{}\", () => {{\n\
  it.todo(\"add assertions\");\n\
}});\n",
        title
    );

    fs::write(&full_path, template)
        .with_context(|| format!("failed to write {}", full_path.display()))?;

    let relative_to_project = full_path
        .strip_prefix(&project_root)
        .unwrap_or(&full_path)
        .to_path_buf();
    println!("Created scratch test: {}", relative_to_project.display());
    println!("Run: f ai-test");
    println!("Watch: f ai-test-watch");
    Ok(())
}

fn find_project_root(start: &Path) -> Result<PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        if current.join("flow.toml").exists() {
            return Ok(current);
        }
        if !current.pop() {
            bail!("no flow.toml found in current directory or parents");
        }
    }
}

fn normalize_relative_dir(raw: &str) -> Result<PathBuf> {
    let path = Path::new(raw);
    if path.is_absolute() {
        bail!("--dir must be relative to project root");
    }
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::Normal(s) => out.push(s),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("--dir must not contain parent traversal")
            }
        }
    }
    if out.as_os_str().is_empty() {
        bail!("--dir cannot be empty");
    }
    Ok(out)
}

fn normalize_test_name(raw: &str, use_spec: bool) -> Result<PathBuf> {
    let mut segments: Vec<String> = raw
        .replace('\\', "/")
        .split('/')
        .filter(|s| !s.trim().is_empty())
        .map(sanitize_segment)
        .filter(|s| !s.is_empty())
        .collect();
    if segments.is_empty() {
        bail!("name must contain at least one non-empty path segment");
    }

    let file = segments.pop().expect("checked non-empty");
    let file = normalize_file_component(&file, use_spec);
    let mut out = PathBuf::new();
    for segment in segments {
        out.push(segment);
    }
    out.push(file);
    Ok(out)
}

fn sanitize_segment(raw: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in raw.chars() {
        let keep = ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.';
        let next = if keep { ch } else { '-' };
        if next == '-' {
            if prev_dash {
                continue;
            }
            prev_dash = true;
        } else {
            prev_dash = false;
        }
        out.push(next);
    }
    out.trim_matches(&['-', '.'][..]).to_string()
}

fn normalize_file_component(file: &str, use_spec: bool) -> String {
    const KNOWN_EXTS: &[&str] = &["ts", "tsx", "js", "jsx", "mjs", "cjs"];
    let suffix = if use_spec { "spec" } else { "test" };

    if let Some((stem, ext)) = file.rsplit_once('.') {
        let ext_lower = ext.to_ascii_lowercase();
        if KNOWN_EXTS.contains(&ext_lower.as_str()) {
            if stem.ends_with(".test") || stem.ends_with(".spec") {
                return file.to_string();
            }
            return format!("{stem}.{suffix}.{ext_lower}");
        }
    }

    if file.contains(".test.") || file.contains(".spec.") {
        return file.to_string();
    }

    format!("{file}.{suffix}.ts")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_test_suffix_for_plain_name() {
        let path = normalize_test_name("auth-login", false).unwrap();
        assert_eq!(path, PathBuf::from("auth-login.test.ts"));
    }

    #[test]
    fn preserves_existing_test_suffix() {
        let path = normalize_test_name("chat/loading.test.ts", true).unwrap();
        assert_eq!(path, PathBuf::from("chat/loading.test.ts"));
    }

    #[test]
    fn adds_spec_before_extension() {
        let path = normalize_test_name("chat/loading.tsx", true).unwrap();
        assert_eq!(path, PathBuf::from("chat/loading.spec.tsx"));
    }
}
