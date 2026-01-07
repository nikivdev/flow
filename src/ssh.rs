use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use crate::config;

pub fn prefer_ssh() -> bool {
    if env_truthy("FLOW_FORCE_HTTPS") {
        return false;
    }
    if env_truthy("FLOW_FORCE_SSH") {
        return true;
    }
    has_agent_socket()
}

pub fn ensure_ssh_env() {
    let env_sock = std::env::var_os("SSH_AUTH_SOCK").map(PathBuf::from);
    let env_sock_valid = env_sock.as_ref().map(|p| p.exists()).unwrap_or(false);

    let sock = if env_sock_valid {
        env_sock.clone()
    } else {
        find_1password_sock()
    };

    let Some(sock) = sock else {
        return;
    };

    // SAFETY: We're setting env vars at startup before spawning threads
    unsafe {
        if !env_sock_valid {
            std::env::set_var("SSH_AUTH_SOCK", &sock);
        }

        if std::env::var_os("GIT_SSH_COMMAND").is_none() {
            let escaped = shell_escape(&sock);
            std::env::set_var(
                "GIT_SSH_COMMAND",
                format!(
                    "ssh -o IdentityAgent={} -o IdentitiesOnly=yes -o BatchMode=yes",
                    escaped
                ),
            );
        }
    }
}

pub fn ensure_git_ssh_command() -> Result<bool> {
    let Some(sock) = find_1password_sock() else {
        return Ok(false);
    };

    let desired = format!(
        "ssh -o IdentityAgent={} -o IdentitiesOnly=yes",
        shell_escape(&sock)
    );

    if let Some(current) = git_config_get("core.sshCommand")? {
        let current = current.trim();
        if current == desired {
            return Ok(false);
        }
        if !current.is_empty() && !current.contains("IdentityAgent=") {
            return Ok(false);
        }
    }

    git_config_set("core.sshCommand", &desired)?;
    Ok(true)
}

pub fn ensure_git_https_insteadof() -> Result<bool> {
    let desired = ["git@github.com:", "ssh://git@github.com/"];
    let mut changed = false;

    if add_url_rewrite("url.https://github.com/.insteadOf", &desired)? {
        changed = true;
    }
    if add_url_rewrite("url.https://github.com/.pushInsteadOf", &desired)? {
        changed = true;
    }

    Ok(changed)
}

fn has_agent_socket() -> bool {
    let env_sock = std::env::var_os("SSH_AUTH_SOCK").map(PathBuf::from);
    if env_sock.as_ref().map(|p| p.exists()).unwrap_or(false) {
        return true;
    }
    find_1password_sock().is_some()
}

fn find_1password_sock() -> Option<PathBuf> {
    let candidates = [
        "~/Library/Group Containers/2BUA8C4S2C.com.1password/t/agent.sock",
        "~/.1password/agent.sock",
    ];

    for candidate in candidates {
        let path = config::expand_path(candidate);
        if path.exists() {
            return Some(path);
        }
    }

    None
}

fn env_truthy(key: &str) -> bool {
    let Some(value) = std::env::var_os(key) else {
        return false;
    };
    let value = value.to_string_lossy().to_lowercase();
    matches!(value.as_str(), "1" | "true" | "yes" | "on")
}

fn git_config_get(key: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .args(["config", "--global", "--get", key])
        .output()
        .context("failed to run git config")?;

    if !output.status.success() {
        return Ok(None);
    }

    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() {
        return Ok(None);
    }
    Ok(Some(value))
}

fn git_config_get_all(key: &str) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args(["config", "--global", "--get-all", key])
        .output()
        .context("failed to run git config")?;

    if !output.status.success() {
        return Ok(Vec::new());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect())
}

fn git_config_set(key: &str, value: &str) -> Result<()> {
    let status = Command::new("git")
        .args(["config", "--global", key, value])
        .status()
        .context("failed to run git config")?;

    if !status.success() {
        anyhow::bail!("git config --global {} failed", key);
    }

    Ok(())
}

fn git_config_add(key: &str, value: &str) -> Result<()> {
    let status = Command::new("git")
        .args(["config", "--global", "--add", key, value])
        .status()
        .context("failed to run git config")?;

    if !status.success() {
        anyhow::bail!("git config --global --add {} failed", key);
    }

    Ok(())
}

fn add_url_rewrite(key: &str, desired: &[&str]) -> Result<bool> {
    let existing = git_config_get_all(key)?;
    let mut changed = false;

    for value in desired {
        if existing.iter().any(|val| val == value) {
            continue;
        }
        git_config_add(key, value)?;
        changed = true;
    }

    Ok(changed)
}

fn shell_escape(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let mut escaped = String::with_capacity(raw.len() + 2);
    escaped.push('\'');
    for ch in raw.chars() {
        if ch == '\'' {
            escaped.push_str("'\\''");
        } else {
            escaped.push(ch);
        }
    }
    escaped.push('\'');
    escaped
}
