use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use uuid::Uuid;

use crate::cli::SshAction;
use crate::{config, env, ssh};

const DEFAULT_TTL_HOURS: u64 = 24;
const KEY_PRIVATE: &str = "SSH_PRIVATE_KEY_B64";
const KEY_PUBLIC: &str = "SSH_PUBLIC_KEY";
const KEY_FINGERPRINT: &str = "SSH_FINGERPRINT";

pub(crate) const DEFAULT_KEY_NAME: &str = "default";

pub fn run(action: Option<SshAction>) -> Result<()> {
    match action {
        Some(SshAction::Setup { name, no_unlock }) => setup(&name, !no_unlock),
        Some(SshAction::Unlock { name, ttl_hours }) => unlock(&name, ttl_hours),
        Some(SshAction::Status { name }) => status(&name),
        None => status(DEFAULT_KEY_NAME),
    }
}

pub(crate) fn ensure_default_identity(ttl_hours: u64) -> Result<()> {
    if ssh::has_identities() {
        return Ok(());
    }

    unlock(DEFAULT_KEY_NAME, ttl_hours)
}

fn setup(name: &str, unlock_after: bool) -> Result<()> {
    let key_name = normalize_name(name);
    let tmp_dir = std::env::temp_dir().join(format!("flow-ssh-{}", Uuid::new_v4()));
    fs::create_dir_all(&tmp_dir)?;
    let key_path = tmp_dir.join("id_ed25519");

    let comment = format!("flow@{}", std::env::var("USER").unwrap_or_else(|_| "flow".to_string()));
    let status = Command::new("ssh-keygen")
        .args([
            "-t",
            "ed25519",
            "-N",
            "",
            "-C",
            &comment,
            "-f",
            key_path.to_string_lossy().as_ref(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to run ssh-keygen")?;
    if !status.success() {
        bail!("ssh-keygen failed");
    }

    let private_key = fs::read_to_string(&key_path)
        .with_context(|| format!("failed to read {}", key_path.display()))?;
    let public_key_path = key_path.with_extension("pub");
    let public_key = fs::read_to_string(&public_key_path)
        .with_context(|| format!("failed to read {}", public_key_path.display()))?;

    let private_b64 = STANDARD.encode(private_key.as_bytes());
    let (env_private, env_public, env_fingerprint) = key_env_keys(&key_name);

    env::set_personal_env_var(&env_private, &private_b64)?;
    env::set_personal_env_var(&env_public, public_key.trim())?;

    if let Some(fingerprint) = compute_fingerprint(&public_key_path) {
        let _ = env::set_personal_env_var(&env_fingerprint, &fingerprint);
    }

    let _ = fs::remove_dir_all(&tmp_dir);

    println!("Stored SSH key in 1focus as '{}'.", key_name);
    println!("Public key:\n{}", public_key.trim());
    println!("Add it to GitHub: https://github.com/settings/keys");

    if unlock_after {
        unlock(&key_name, DEFAULT_TTL_HOURS)?;
    }

    Ok(())
}

fn unlock(name: &str, ttl_hours: u64) -> Result<()> {
    let key_name = normalize_name(name);
    let (env_private, _env_public, _env_fingerprint) = key_env_keys(&key_name);

    let vars = env::fetch_personal_env_vars(&[env_private.clone()])?;
    let private_b64 = vars
        .get(&env_private)
        .ok_or_else(|| anyhow::anyhow!("SSH key not found in 1focus. Run `f ssh setup` first."))?;

    let private_key = STANDARD
        .decode(private_b64.as_bytes())
        .context("failed to decode SSH private key")?;

    let ssh_dir = config::global_config_dir().join("ssh");
    fs::create_dir_all(&ssh_dir)?;
    let key_path = ssh_dir.join(format!("id_ed25519_{}", key_name));
    write_private_key(&key_path, &private_key)?;

    let sock = ssh::ensure_flow_agent()?;
    let ttl_seconds = ttl_hours.saturating_mul(3600).to_string();

    let status = Command::new("ssh-add")
        .arg("-t")
        .arg(&ttl_seconds)
        .arg(key_path.to_string_lossy().as_ref())
        .env("SSH_AUTH_SOCK", &sock)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to run ssh-add")?;
    if !status.success() {
        bail!("ssh-add failed");
    }

    let _ = fs::remove_file(&key_path);

    let _ = ssh::ensure_git_ssh_command_for_sock(&sock, true);
    let _ = ssh::clear_git_https_insteadof();
    println!("✓ SSH key unlocked (ttl: {}h)", ttl_hours);

    Ok(())
}

fn status(name: &str) -> Result<()> {
    let key_name = normalize_name(name);
    let (env_private, env_public, env_fingerprint) = key_env_keys(&key_name);
    let vars = match env::fetch_personal_env_vars(&[
        env_private.clone(),
        env_public.clone(),
        env_fingerprint.clone(),
    ]) {
        Ok(vars) => vars,
        Err(err) => {
            println!("Unable to query 1focus: {}", err);
            return Ok(());
        }
    };
    let has_key = vars.contains_key(&env_private);
    let has_pub = vars.contains_key(&env_public);
    let fingerprint = vars
        .get(&env_fingerprint)
        .cloned()
        .unwrap_or_default();

    let agent = ssh::flow_agent_status();

    println!("Key: {}", key_name);
    println!("Stored in 1focus: {}", if has_key { "yes" } else { "no" });
    println!("Public key stored: {}", if has_pub { "yes" } else { "no" });
    if !fingerprint.is_empty() {
        println!("Fingerprint: {}", fingerprint);
    }
    match agent {
        Some(sock) => println!("Flow SSH agent: running ({})", sock.display()),
        None => println!("Flow SSH agent: not running"),
    }
    Ok(())
}

fn key_env_keys(name: &str) -> (String, String, String) {
    if name == "default" {
        (
            format!("FLOW_{}", KEY_PRIVATE),
            format!("FLOW_{}", KEY_PUBLIC),
            format!("FLOW_{}", KEY_FINGERPRINT),
        )
    } else {
        let suffix = sanitize_env_suffix(name);
        (
            format!("FLOW_{}_{}", KEY_PRIVATE, suffix),
            format!("FLOW_{}_{}", KEY_PUBLIC, suffix),
            format!("FLOW_{}_{}", KEY_FINGERPRINT, suffix),
        )
    }
}

fn normalize_name(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        "default".to_string()
    } else {
        trimmed.to_string()
    }
}

fn sanitize_env_suffix(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn write_private_key(path: &PathBuf, content: &[u8]) -> Result<()> {
    let mut file = fs::File::create(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    file.write_all(content)
        .with_context(|| format!("failed to write {}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o600);
        fs::set_permissions(path, perms)
            .with_context(|| format!("failed to chmod {}", path.display()))?;
    }

    Ok(())
}

fn compute_fingerprint(public_key_path: &PathBuf) -> Option<String> {
    let output = Command::new("ssh-keygen")
        .args(["-lf", public_key_path.to_string_lossy().as_ref()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.split_whitespace().nth(1).map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_name_defaults_to_default() {
        assert_eq!(normalize_name(""), "default");
        assert_eq!(normalize_name("   "), "default");
        assert_eq!(normalize_name("work"), "work");
    }

    #[test]
    fn sanitize_env_suffix_normalizes() {
        assert_eq!(sanitize_env_suffix("dev-ops"), "DEV_OPS");
        assert_eq!(sanitize_env_suffix("aB9"), "AB9");
        assert_eq!(sanitize_env_suffix("with space"), "WITH_SPACE");
    }

    #[test]
    fn key_env_keys_uses_expected_prefixes() {
        let (priv_key, pub_key, fp) = key_env_keys("default");
        assert_eq!(priv_key, "FLOW_SSH_PRIVATE_KEY_B64");
        assert_eq!(pub_key, "FLOW_SSH_PUBLIC_KEY");
        assert_eq!(fp, "FLOW_SSH_FINGERPRINT");

        let (priv_key, pub_key, fp) = key_env_keys("work");
        assert_eq!(priv_key, "FLOW_SSH_PRIVATE_KEY_B64_WORK");
        assert_eq!(pub_key, "FLOW_SSH_PUBLIC_KEY_WORK");
        assert_eq!(fp, "FLOW_SSH_FINGERPRINT_WORK");
    }

    #[test]
    fn write_private_key_sets_permissions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("id_ed25519_test");
        write_private_key(&path, b"PRIVATE").expect("write key");

        let content = fs::read_to_string(&path).expect("read key");
        assert_eq!(content, "PRIVATE");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}
