use std::path::Path;
use std::process::Command;

use anyhow::Result;

/// Show project information including git remotes and flow.toml settings.
pub fn run() -> Result<()> {
    let cwd = std::env::current_dir()?;

    println!("Project: {}", cwd.display());
    println!();

    // Show git info
    if cwd.join(".git").exists() {
        print_git_info(&cwd);
    } else {
        println!("Git: not a git repository");
    }

    println!();

    // Show flow.toml info
    if let Some(flow_config) = find_flow_toml(&cwd) {
        print_flow_info(&flow_config);
    } else {
        println!("Flow: no flow.toml found");
    }

    Ok(())
}

fn print_git_info(cwd: &Path) {
    // Current branch
    if let Some(branch) = git_current_branch(cwd) {
        println!("Branch: {}", branch);
    }

    // Remotes
    if let Some(remotes) = git_remotes(cwd) {
        if !remotes.is_empty() {
            println!();
            println!("Remotes:");
            for (name, url) in remotes {
                println!("  {} = {}", name, url);
            }
        }
    }

    // Check if upstream is configured
    if let Some(upstream) = git_remote_url(cwd, "upstream") {
        println!();
        println!("Upstream: {}", upstream);
        println!("  Run `f sync` to pull from upstream and push to origin");
    }
}

fn git_current_branch(cwd: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()?;

    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

fn git_remotes(cwd: &Path) -> Option<Vec<(String, String)>> {
    let output = Command::new("git")
        .args(["remote", "-v"])
        .current_dir(cwd)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut remotes = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for line in stdout.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 {
            let name = parts[0].to_string();
            let url = parts[1].to_string();
            let key = format!("{} {}", name, url);
            if seen.insert(key) {
                remotes.push((name, url));
            }
        }
    }

    Some(remotes)
}

fn git_remote_url(cwd: &Path, remote: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["remote", "get-url", remote])
        .current_dir(cwd)
        .output()
        .ok()?;

    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

fn find_flow_toml(start: &Path) -> Option<std::path::PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        let flow_toml = current.join("flow.toml");
        if flow_toml.exists() {
            return Some(flow_toml);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn print_flow_info(flow_toml: &Path) {
    let content = match std::fs::read_to_string(flow_toml) {
        Ok(c) => c,
        Err(_) => return,
    };

    let parsed: toml::Value = match content.parse() {
        Ok(v) => v,
        Err(_) => return,
    };

    println!("Flow: {}", flow_toml.display());

    // Show [flow] section info
    if let Some(flow) = parsed.get("flow").and_then(|v| v.as_table()) {
        if let Some(name) = flow.get("name").and_then(|v| v.as_str()) {
            println!("  name = {}", name);
        }
        if let Some(upstream) = flow.get("upstream").and_then(|v| v.as_str()) {
            println!("  upstream = {}", upstream);
        }
    }

    // Show [upstream] section if present
    if let Some(upstream) = parsed.get("upstream").and_then(|v| v.as_table()) {
        println!();
        println!("[upstream]");
        if let Some(url) = upstream.get("url").and_then(|v| v.as_str()) {
            println!("  url = {}", url);
        }
        if let Some(branch) = upstream.get("branch").and_then(|v| v.as_str()) {
            println!("  branch = {}", branch);
        }
    }

    // Show task count
    if let Some(tasks) = parsed.get("tasks").and_then(|v| v.as_table()) {
        println!();
        println!("Tasks: {}", tasks.len());
    }
}
