use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use anyhow::Result;
use serde::Deserialize;

/// Show project information including git remotes and flow.toml settings.
pub fn run() -> Result<()> {
    let cwd = std::env::current_dir()?;

    println!("Project: {}", cwd.display());
    println!();

    if let Some(git) = git_info(&cwd) {
        print_git_info(&git);
    } else {
        println!("Git: not a git repository");
    }

    println!();

    // Show flow.toml info
    if let Some(flow_config) = crate::project_snapshot::find_flow_toml_upwards(&cwd) {
        print_flow_info(&flow_config);
    } else {
        println!("Flow: no flow.toml found");
    }

    Ok(())
}

#[derive(Debug)]
struct GitInfo {
    branch: Option<String>,
    remotes: Vec<(String, String)>,
}

#[derive(Debug)]
struct GitRepoPaths {
    git_dir: PathBuf,
    common_dir: PathBuf,
}

fn print_git_info(git: &GitInfo) {
    if let Some(branch) = git.branch.as_deref() {
        println!("Branch: {}", branch);
    }

    if !git.remotes.is_empty() {
        println!();
        println!("Remotes:");
        for (name, url) in &git.remotes {
            println!("  {} = {}", name, url);
        }
    }

    if let Some((_, upstream)) = git
        .remotes
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("upstream"))
    {
        println!();
        println!("Upstream: {}", upstream);
        println!("  Run `f sync` to pull from upstream and push to origin");
    }
}

fn git_info(cwd: &Path) -> Option<GitInfo> {
    let repo_root = find_git_root(cwd)?;
    let repo = resolve_git_paths(&repo_root)?;
    let branch = read_git_branch(&repo.git_dir.join("HEAD"));
    let remotes = parse_git_remotes(&repo.common_dir.join("config"));
    Some(GitInfo { branch, remotes })
}

fn find_git_root(start: &Path) -> Option<PathBuf> {
    let mut current = if start.is_dir() {
        start.to_path_buf()
    } else {
        start.parent()?.to_path_buf()
    };
    loop {
        let dot_git = current.join(".git");
        if dot_git.is_dir() || dot_git.is_file() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn resolve_git_paths(repo_root: &Path) -> Option<GitRepoPaths> {
    let dot_git = repo_root.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        resolve_git_dir_file(&dot_git)?
    };
    let common_dir = resolve_common_git_dir(&git_dir);
    Some(GitRepoPaths {
        git_dir,
        common_dir,
    })
}

fn resolve_git_dir_file(dot_git_file: &Path) -> Option<PathBuf> {
    let content = fs::read_to_string(dot_git_file).ok()?;
    let gitdir = content.strip_prefix("gitdir:")?.trim();
    let path = PathBuf::from(gitdir);
    let resolved = if path.is_absolute() {
        path
    } else {
        dot_git_file.parent()?.join(path)
    };
    Some(resolved.canonicalize().unwrap_or(resolved))
}

fn resolve_common_git_dir(git_dir: &Path) -> PathBuf {
    let commondir = git_dir.join("commondir");
    let Ok(content) = fs::read_to_string(&commondir) else {
        return git_dir.to_path_buf();
    };
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return git_dir.to_path_buf();
    }
    let path = PathBuf::from(trimmed);
    let resolved = if path.is_absolute() {
        path
    } else {
        git_dir.join(path)
    };
    resolved.canonicalize().unwrap_or(resolved)
}

fn read_git_branch(head_path: &Path) -> Option<String> {
    let content = fs::read_to_string(head_path).ok()?;
    let head = content.trim();
    let branch = head.strip_prefix("ref: refs/heads/")?.trim();
    if branch.is_empty() {
        None
    } else {
        Some(branch.to_string())
    }
}

fn parse_git_remotes(config_path: &Path) -> Vec<(String, String)> {
    let Ok(content) = fs::read_to_string(config_path) else {
        return Vec::new();
    };

    let mut remotes = Vec::new();
    let mut seen = HashSet::new();
    let mut current_remote: Option<String> = None;

    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            current_remote = parse_remote_section(line);
            continue;
        }
        let Some(remote) = current_remote.as_deref() else {
            continue;
        };
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("url") {
            let url = value.trim().to_string();
            let dedupe_key = format!("{remote}\n{url}");
            if seen.insert(dedupe_key) {
                remotes.push((remote.to_string(), url));
            }
        }
    }

    remotes
}

fn parse_remote_section(section: &str) -> Option<String> {
    let inner = section.strip_prefix('[')?.strip_suffix(']')?.trim();
    let rest = inner.strip_prefix("remote")?.trim();
    let name = rest.strip_prefix('"')?.strip_suffix('"')?.trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

fn print_flow_info(flow_toml: &Path) {
    let content = match std::fs::read_to_string(flow_toml) {
        Ok(c) => c,
        Err(_) => return,
    };

    let parsed: InfoConfig = match toml::from_str(&content) {
        Ok(v) => v,
        Err(_) => return,
    };

    println!("Flow: {}", flow_toml.display());

    // Show [flow] section info
    if let Some(flow) = parsed.flow.as_ref() {
        if let Some(name) = flow.name.as_deref() {
            println!("  name = {}", name);
        }
        if let Some(upstream) = flow.upstream.as_deref() {
            println!("  upstream = {}", upstream);
        }
    }

    if let Some(upstream) = parsed.upstream.as_ref() {
        println!();
        println!("[upstream]");
        if let Some(url) = upstream.url.as_deref() {
            println!("  url = {}", url);
        }
        if let Some(branch) = upstream.branch.as_deref() {
            println!("  branch = {}", branch);
        }
    }

    if !parsed.tasks.is_empty() {
        println!();
        println!("Tasks: {}", parsed.tasks.len());
    }
}

#[derive(Debug, Deserialize)]
struct InfoConfig {
    #[serde(default)]
    flow: Option<InfoFlowSection>,
    #[serde(default)]
    upstream: Option<InfoUpstreamSection>,
    #[serde(default)]
    tasks: Vec<InfoTaskSection>,
}

#[derive(Debug, Deserialize)]
struct InfoFlowSection {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    upstream: Option<String>,
}

#[derive(Debug, Deserialize)]
struct InfoUpstreamSection {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    branch: Option<String>,
}

#[derive(Debug, Deserialize)]
struct InfoTaskSection {}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{
        find_git_root, parse_git_remotes, read_git_branch, resolve_common_git_dir,
        resolve_git_dir_file,
    };

    #[test]
    fn parse_git_remotes_reads_unique_remote_urls() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("config");
        fs::write(
            &path,
            r#"
[remote "origin"]
    url = git@github.com:nikivdev/flow.git
    fetch = +refs/heads/*:refs/remotes/origin/*
[remote "origin"]
    url = git@github.com:nikivdev/flow.git
[remote "upstream"]
    url = git@github.com:openai/codex.git
"#,
        )
        .expect("write config");

        let remotes = parse_git_remotes(&path);
        assert_eq!(remotes.len(), 2);
        assert_eq!(remotes[0].0, "origin");
        assert_eq!(remotes[1].0, "upstream");
    }

    #[test]
    fn read_git_branch_reads_symbolic_head() {
        let dir = tempdir().expect("tempdir");
        let head = dir.path().join("HEAD");
        fs::write(&head, "ref: refs/heads/main\n").expect("write head");
        assert_eq!(read_git_branch(&head).as_deref(), Some("main"));
    }

    #[test]
    fn resolve_git_dir_file_supports_relative_gitdir() {
        let dir = tempdir().expect("tempdir");
        let repo = dir.path().join("repo");
        let actual = dir.path().join("actual-git");
        fs::create_dir_all(&repo).expect("repo dir");
        fs::create_dir_all(&actual).expect("git dir");
        let dot_git = repo.join(".git");
        fs::write(&dot_git, "gitdir: ../actual-git\n").expect("write gitdir");

        let resolved = resolve_git_dir_file(&dot_git).expect("resolve gitdir");
        assert_eq!(resolved, actual.canonicalize().unwrap_or(actual));
    }

    #[test]
    fn resolve_common_git_dir_uses_commondir_when_present() {
        let dir = tempdir().expect("tempdir");
        let git_dir = dir.path().join("git/worktrees/repo");
        let common = dir.path().join("git");
        fs::create_dir_all(&git_dir).expect("gitdir");
        fs::create_dir_all(&common).expect("common dir");
        fs::write(git_dir.join("commondir"), "../..\n").expect("write commondir");

        let resolved = resolve_common_git_dir(&git_dir);
        assert_eq!(resolved, common.canonicalize().unwrap_or(common));
    }

    #[test]
    fn find_git_root_walks_up_to_repo_root() {
        let dir = tempdir().expect("tempdir");
        let repo = dir.path().join("repo");
        let nested = repo.join("a/b");
        fs::create_dir_all(repo.join(".git")).expect("git dir");
        fs::create_dir_all(&nested).expect("nested dir");

        let root = find_git_root(&nested).expect("git root");
        assert_eq!(root, repo);
    }
}
