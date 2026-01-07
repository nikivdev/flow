use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

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
    } else if let Some(flow_sock) = flow_agent_status() {
        Some(flow_sock)
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
    let Some(sock) = preferred_agent_sock() else {
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

pub fn ensure_git_ssh_command_for_sock(sock: &Path, force: bool) -> Result<bool> {
    let desired = format!(
        "ssh -o IdentityAgent={} -o IdentitiesOnly=yes",
        shell_escape(sock)
    );

    if !force {
        if let Some(current) = git_config_get("core.sshCommand")? {
            let current = current.trim();
            if !current.is_empty() && !current.contains("IdentityAgent=") {
                return Ok(false);
            }
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

pub fn clear_git_https_insteadof() -> Result<bool> {
    let desired = ["git@github.com:", "ssh://git@github.com/"];
    let mut changed = false;

    if remove_url_rewrite("url.https://github.com/.insteadOf", &desired)? {
        changed = true;
    }
    if remove_url_rewrite("url.https://github.com/.pushInsteadOf", &desired)? {
        changed = true;
    }

    Ok(changed)
}

pub fn ensure_flow_agent() -> Result<PathBuf> {
    if let Some(sock) = flow_agent_status() {
        return Ok(sock);
    }

    let sock = flow_agent_sock();
    if sock.exists() {
        if probe_agent(&sock) {
            return Ok(sock);
        }
        let _ = fs::remove_file(&sock);
    }
    let state_path = flow_agent_state_path();
    if let Some(parent) = state_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let output = Command::new("ssh-agent")
        .args(["-a", sock.to_string_lossy().as_ref(), "-s"])
        .output()
        .context("failed to start ssh-agent")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("ssh-agent failed: {}", stderr.trim());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let pid = parse_agent_output(&stdout, "SSH_AGENT_PID")
        .and_then(|val| val.parse::<u32>().ok())
        .context("failed to parse ssh-agent pid")?;
    let sock_path = parse_agent_output(&stdout, "SSH_AUTH_SOCK")
        .map(PathBuf::from)
        .unwrap_or_else(|| sock.clone());

    let state = FlowAgentState {
        pid,
        sock: sock_path.clone(),
    };
    let content = serde_json::to_string_pretty(&state)?;
    fs::write(&state_path, content)?;

    Ok(sock_path)
}

pub fn flow_agent_status() -> Option<PathBuf> {
    let sock = flow_agent_sock();
    if !sock.exists() {
        return None;
    }

    if let Some(state) = load_flow_agent_state() {
        if !pid_alive(state.pid) {
            return None;
        }
        return Some(state.sock);
    }

    if probe_agent(&sock) {
        return Some(sock);
    }

    None
}

fn preferred_agent_sock() -> Option<PathBuf> {
    let env_sock = std::env::var_os("SSH_AUTH_SOCK").map(PathBuf::from);
    if env_sock.as_ref().map(|p| p.exists()).unwrap_or(false) {
        return env_sock;
    }
    if let Some(flow_sock) = flow_agent_status() {
        return Some(flow_sock);
    }
    find_1password_sock()
}

#[derive(Debug, Serialize, Deserialize)]
struct FlowAgentState {
    pid: u32,
    sock: PathBuf,
}

fn flow_agent_sock() -> PathBuf {
    config::global_config_dir()
        .join("ssh")
        .join("agent.sock")
}

fn flow_agent_state_path() -> PathBuf {
    config::global_config_dir()
        .join("ssh")
        .join("agent.json")
}

fn load_flow_agent_state() -> Option<FlowAgentState> {
    let path = flow_agent_state_path();
    let content = fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

fn pid_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn parse_agent_output(stdout: &str, key: &str) -> Option<String> {
    for part in stdout.split(&[';', '\n'][..]) {
        let trimmed = part.trim();
        let needle = format!("{}=", key);
        if let Some(rest) = trimmed.strip_prefix(&needle) {
            return Some(rest.trim().to_string());
        }
    }
    None
}

fn probe_agent(sock: &Path) -> bool {
    let status = match Command::new("ssh-add")
        .args(["-l"])
        .env("SSH_AUTH_SOCK", sock)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) => status,
        Err(_) => return false,
    };

    match status.code() {
        Some(2) | None => false,
        _ => true,
    }
}

fn has_agent_socket() -> bool {
    let env_sock = std::env::var_os("SSH_AUTH_SOCK").map(PathBuf::from);
    if env_sock.as_ref().map(|p| p.exists()).unwrap_or(false) {
        return true;
    }
    if flow_agent_status().is_some() {
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

fn git_config_unset_all(key: &str, value: &str) -> Result<()> {
    let status = Command::new("git")
        .args(["config", "--global", "--unset-all", key, value])
        .status()
        .context("failed to run git config")?;

    if !status.success() {
        anyhow::bail!("git config --global --unset-all {} failed", key);
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

fn remove_url_rewrite(key: &str, desired: &[&str]) -> Result<bool> {
    let existing = git_config_get_all(key)?;
    let mut changed = false;

    for value in desired {
        if existing.iter().any(|val| val == value) {
            git_config_unset_all(key, value)?;
            changed = true;
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var_os(key);
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(value) = self.previous.take() {
                unsafe {
                    std::env::set_var(self.key, value);
                }
            } else {
                unsafe {
                    std::env::remove_var(self.key);
                }
            }
        }
    }

    #[test]
    fn env_truthy_matches_expected_values() {
        let _guard = EnvVarGuard::set("FLOW_TEST_BOOL", "true");
        assert!(env_truthy("FLOW_TEST_BOOL"));
        drop(_guard);

        for value in ["1", "yes", "on", "TRUE"] {
            let _guard = EnvVarGuard::set("FLOW_TEST_BOOL", value);
            assert!(env_truthy("FLOW_TEST_BOOL"), "value {}", value);
        }

        let _guard = EnvVarGuard::set("FLOW_TEST_BOOL", "0");
        assert!(!env_truthy("FLOW_TEST_BOOL"));
    }

    #[test]
    fn prefer_ssh_respects_force_flags() {
        {
            let _https = EnvVarGuard::set("FLOW_FORCE_HTTPS", "1");
            let _ssh = EnvVarGuard::set("FLOW_FORCE_SSH", "1");
            assert!(!prefer_ssh());
        }
        {
            let _https = EnvVarGuard::set("FLOW_FORCE_HTTPS", "0");
            let _ssh = EnvVarGuard::set("FLOW_FORCE_SSH", "1");
            assert!(prefer_ssh());
        }
    }

    #[test]
    fn shell_escape_handles_single_quotes() {
        let path = Path::new("/tmp/has'quote");
        let escaped = shell_escape(path);
        assert_eq!(escaped, "'/tmp/has'\\''quote'");
    }

    #[test]
    fn parse_agent_output_reads_values() {
        let sample = "SSH_AUTH_SOCK=/tmp/agent.sock; export SSH_AUTH_SOCK;\nSSH_AGENT_PID=4242; export SSH_AGENT_PID;\n";
        assert_eq!(
            parse_agent_output(sample, "SSH_AUTH_SOCK"),
            Some("/tmp/agent.sock".to_string())
        );
        assert_eq!(
            parse_agent_output(sample, "SSH_AGENT_PID"),
            Some("4242".to_string())
        );
    }
}
