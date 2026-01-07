use std::path::{Path, PathBuf};

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
