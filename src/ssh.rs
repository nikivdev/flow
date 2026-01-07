use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use crate::config;

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
