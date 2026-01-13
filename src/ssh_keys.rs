use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use bs58;
use chrono::{DateTime, Local, TimeZone, Utc};
use cojson_core::crypto::{get_sealer_id, new_x25519_private_key, seal, unseal};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::cli::SshAction;
use crate::{config, env, ssh};

const DEFAULT_TTL_HOURS: u64 = 24;
const KEY_PRIVATE: &str = "SSH_PRIVATE_KEY_B64";
const KEY_PRIVATE_SEALED: &str = "SSH_PRIVATE_KEY_SEALED_B64";
const KEY_PRIVATE_SEALED_NONCE: &str = "SSH_PRIVATE_KEY_SEALED_NONCE_B64";
const KEY_PRIVATE_SEALER_ID: &str = "SSH_PRIVATE_KEY_SEALER_ID";
const KEY_PUBLIC: &str = "SSH_PUBLIC_KEY";
const KEY_FINGERPRINT: &str = "SSH_FINGERPRINT";

pub(crate) const DEFAULT_KEY_NAME: &str = "default";
const DEFAULT_SSH_MODE: &str = "force";

#[derive(Debug, Serialize, Deserialize)]
struct SealerIdentity {
    sealer_secret: String,
    sealer_id: String,
}

struct SealedKeyPayload {
    sealed_b64: String,
    nonce_b64: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct SshKeyUnlock {
    expires_at: i64,
}

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

    let key_name = configured_key_name();
    unlock(&key_name, ttl_hours)
}

fn setup(name: &str, unlock_after: bool) -> Result<()> {
    let key_name = normalize_name(name);
    let tmp_dir = std::env::temp_dir().join(format!("flow-ssh-{}", Uuid::new_v4()));
    fs::create_dir_all(&tmp_dir)?;
    let key_path = tmp_dir.join("id_ed25519");

    let comment = format!(
        "flow@{}",
        std::env::var("USER").unwrap_or_else(|_| "flow".to_string())
    );
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

    let identity = load_or_create_sealer_identity()?;
    let sealed = seal_private_key(private_key.as_bytes(), &identity)?;
    let (env_private_plain, env_public, env_fingerprint) = key_env_keys(&key_name);
    let (env_private_sealed, env_private_nonce, env_private_sealer_id) =
        key_env_sealed_keys(&key_name);

    env::set_personal_env_var(&env_private_sealed, &sealed.sealed_b64)?;
    env::set_personal_env_var(&env_private_nonce, &sealed.nonce_b64)?;
    env::set_personal_env_var(&env_private_sealer_id, &identity.sealer_id)?;
    env::set_personal_env_var(&env_public, public_key.trim())?;

    if let Some(fingerprint) = compute_fingerprint(&public_key_path) {
        let _ = env::set_personal_env_var(&env_fingerprint, &fingerprint);
    }

    let _ = fs::remove_dir_all(&tmp_dir);

    if let Err(err) = env::delete_personal_env_vars(&[env_private_plain.clone()]) {
        eprintln!(
            "Warning: failed to delete legacy plaintext key {}: {}",
            env_private_plain, err
        );
    }

    println!("Stored SSH key in cloud as '{}' (sealed).", key_name);
    println!("Public key:\n{}", public_key.trim());
    println!("Add it to GitHub: https://github.com/settings/keys");
    ensure_global_ssh_config(&key_name)?;
    let wrapper = ensure_flow_ssh_wrapper(&key_name)?;
    let _ = ssh::ensure_git_ssh_command_wrapper(&wrapper, true);

    if unlock_after {
        unlock(&key_name, DEFAULT_TTL_HOURS)?;
    }

    Ok(())
}

fn unlock(name: &str, ttl_hours: u64) -> Result<()> {
    let key_name = normalize_name(name);
    require_ssh_key_unlock()?;
    let (env_private_plain, _env_public, _env_fingerprint) = key_env_keys(&key_name);
    let (env_private_sealed, env_private_nonce, env_private_sealer_id) =
        key_env_sealed_keys(&key_name);

    let vars = env::fetch_personal_env_vars(&[
        env_private_sealed.clone(),
        env_private_nonce.clone(),
        env_private_sealer_id.clone(),
        env_private_plain.clone(),
    ])?;

    let private_key = if vars.contains_key(&env_private_sealed)
        || vars.contains_key(&env_private_nonce)
    {
        let sealed_b64 = vars.get(&env_private_sealed).ok_or_else(|| {
            anyhow::anyhow!(
                "SSH key sealed payload is missing ({}). Run `f ssh setup` again.",
                env_private_sealed
            )
        })?;
        let nonce_b64 = vars.get(&env_private_nonce).ok_or_else(|| {
            anyhow::anyhow!(
                "SSH key sealed nonce is missing ({}). Run `f ssh setup` again.",
                env_private_nonce
            )
        })?;
        let identity = load_sealer_identity()?.ok_or_else(|| {
            anyhow::anyhow!(
                "Local SSH seal identity not found. Run `f ssh setup` on this machine first."
            )
        })?;
        if let Some(expected_id) = vars.get(&env_private_sealer_id) {
            if expected_id.trim() != identity.sealer_id {
                bail!(
                    "Stored SSH key is sealed for a different device. Run `f ssh setup` to create a new key or copy {} from the original device.",
                    sealer_identity_path()?.display()
                );
            }
        }
        unseal_private_key(sealed_b64, nonce_b64, &identity)?
    } else if let Some(private_b64) = vars.get(&env_private_plain) {
        eprintln!(
            "Warning: using legacy plaintext SSH key from cloud; run `f ssh setup` to seal it."
        );
        STANDARD
            .decode(private_b64.as_bytes())
            .context("failed to decode SSH private key")?
    } else {
        bail!("SSH key not found in cloud. Run `f ssh setup` first.");
    };

    let sock = ssh::ensure_flow_agent()?;
    let result = add_key_to_agent(&private_key, &sock, ttl_hours);
    let mut private_key = private_key;
    private_key.fill(0);
    result?;

    let wrapper = ensure_flow_ssh_wrapper(&key_name)?;
    let _ = ssh::ensure_git_ssh_command_wrapper(&wrapper, true);
    let _ = ssh::clear_git_https_insteadof();
    println!("✓ SSH key unlocked (ttl: {}h)", ttl_hours);

    Ok(())
}

fn status(name: &str) -> Result<()> {
    let key_name = normalize_name(name);
    let (env_private_plain, env_public, env_fingerprint) = key_env_keys(&key_name);
    let (env_private_sealed, env_private_nonce, env_private_sealer_id) =
        key_env_sealed_keys(&key_name);
    let vars = match env::fetch_personal_env_vars(&[
        env_private_plain.clone(),
        env_public.clone(),
        env_fingerprint.clone(),
        env_private_sealed.clone(),
        env_private_nonce.clone(),
        env_private_sealer_id.clone(),
    ]) {
        Ok(vars) => vars,
        Err(err) => {
            println!("Unable to query cloud: {}", err);
            return Ok(());
        }
    };
    let has_plain = vars.contains_key(&env_private_plain);
    let has_sealed =
        vars.contains_key(&env_private_sealed) && vars.contains_key(&env_private_nonce);
    let has_pub = vars.contains_key(&env_public);
    let fingerprint = vars.get(&env_fingerprint).cloned().unwrap_or_default();
    let sealer_id = vars
        .get(&env_private_sealer_id)
        .cloned()
        .unwrap_or_default();
    let local_identity = load_sealer_identity().ok().flatten();

    let agent = ssh::flow_agent_status();

    println!("Key: {}", key_name);
    println!(
        "Stored in cloud (sealed): {}",
        if has_sealed { "yes" } else { "no" }
    );
    println!(
        "Stored in cloud (plaintext): {}",
        if has_plain { "yes" } else { "no" }
    );
    println!("Public key stored: {}", if has_pub { "yes" } else { "no" });
    if !fingerprint.is_empty() {
        println!("Fingerprint: {}", fingerprint);
    }
    if !sealer_id.is_empty() {
        println!("Sealer id: {}", sealer_id);
    }
    println!(
        "Local seal identity: {}",
        if local_identity.is_some() {
            "yes"
        } else {
            "no"
        }
    );
    match agent {
        Some(sock) => println!("Flow SSH agent: running ({})", sock.display()),
        None => println!("Flow SSH agent: not running"),
    }
    Ok(())
}

fn ensure_global_ssh_config(key_name: &str) -> Result<()> {
    let config_path = config::default_config_path();
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let contents = if config_path.exists() {
        fs::read_to_string(&config_path)
            .with_context(|| format!("failed to read {}", config_path.display()))?
    } else {
        String::new()
    };

    let updated = upsert_ssh_block(&contents, DEFAULT_SSH_MODE, key_name);
    if updated != contents {
        fs::write(&config_path, updated)
            .with_context(|| format!("failed to write {}", config_path.display()))?;
        println!(
            "Configured Flow to use SSH keys from cloud (mode={}, key={}).",
            DEFAULT_SSH_MODE, key_name
        );
    }

    Ok(())
}

fn upsert_ssh_block(input: &str, mode: &str, key_name: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut in_ssh = false;
    let mut saw_ssh = false;
    let mut saw_mode = false;
    let mut saw_key = false;
    let ends_with_newline = input.ends_with('\n');

    for line in input.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            if in_ssh {
                if !saw_mode {
                    out.push(format!("mode = \"{}\"", mode));
                    saw_mode = true;
                }
                if !saw_key {
                    out.push(format!("key_name = \"{}\"", key_name));
                    saw_key = true;
                }
            }

            in_ssh = trimmed == "[ssh]";
            if in_ssh {
                saw_ssh = true;
            }
            out.push(line.to_string());
            continue;
        }

        if in_ssh {
            if trimmed.starts_with("mode") && trimmed.contains('=') {
                out.push(format!("mode = \"{}\"", mode));
                saw_mode = true;
                continue;
            }
            if trimmed.starts_with("key_name") && trimmed.contains('=') {
                out.push(format!("key_name = \"{}\"", key_name));
                saw_key = true;
                continue;
            }
        }

        out.push(line.to_string());
    }

    if in_ssh {
        if !saw_mode {
            out.push(format!("mode = \"{}\"", mode));
        }
        if !saw_key {
            out.push(format!("key_name = \"{}\"", key_name));
        }
    }

    if !saw_ssh {
        if !out.is_empty() {
            out.push(String::new());
        }
        out.push("[ssh]".to_string());
        out.push(format!("mode = \"{}\"", mode));
        out.push(format!("key_name = \"{}\"", key_name));
    }

    let mut rendered = out.join("\n");
    if ends_with_newline || rendered.is_empty() {
        rendered.push('\n');
    }
    rendered
}

fn configured_key_name() -> String {
    let config_path = config::default_config_path();
    if config_path.exists() {
        if let Ok(cfg) = config::load(&config_path) {
            if let Some(ssh_cfg) = cfg.ssh {
                if let Some(name) = ssh_cfg.key_name {
                    if !name.trim().is_empty() {
                        return name;
                    }
                }
            }
        }
    }

    DEFAULT_KEY_NAME.to_string()
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

fn key_env_sealed_keys(name: &str) -> (String, String, String) {
    if name == "default" {
        (
            format!("FLOW_{}", KEY_PRIVATE_SEALED),
            format!("FLOW_{}", KEY_PRIVATE_SEALED_NONCE),
            format!("FLOW_{}", KEY_PRIVATE_SEALER_ID),
        )
    } else {
        let suffix = sanitize_env_suffix(name);
        (
            format!("FLOW_{}_{}", KEY_PRIVATE_SEALED, suffix),
            format!("FLOW_{}_{}", KEY_PRIVATE_SEALED_NONCE, suffix),
            format!("FLOW_{}_{}", KEY_PRIVATE_SEALER_ID, suffix),
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

fn ensure_ssh_state_dir() -> Result<PathBuf> {
    let base = config::ensure_global_state_dir()?;
    let dir = base.join("ssh");
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o700);
        fs::set_permissions(&dir, perms)
            .with_context(|| format!("failed to chmod {}", dir.display()))?;
    }
    Ok(dir)
}

fn ensure_flow_ssh_wrapper(key_name: &str) -> Result<PathBuf> {
    let dir = ensure_ssh_state_dir()?;
    let path = dir.join("flow-ssh");
    let sock = ssh::flow_agent_sock_path();
    let sock_escaped = escape_double_quotes(&sock.to_string_lossy());
    let key_arg = shell_escape_arg(key_name);

    let content = format!(
        r#"#!/usr/bin/env bash
set -euo pipefail
SOCK="{sock}"
if [[ -S "$SOCK" ]]; then
  if SSH_AUTH_SOCK="$SOCK" ssh-add -l >/dev/null 2>&1; then
    exec /usr/bin/ssh -o IdentityAgent="$SOCK" -o IdentitiesOnly=yes -o BatchMode=yes "$@"
  fi
fi
if command -v f >/dev/null 2>&1; then
  f ssh unlock --name {key}
elif command -v flow >/dev/null 2>&1; then
  flow ssh unlock --name {key}
fi
exec /usr/bin/ssh -o IdentityAgent="$SOCK" -o IdentitiesOnly=yes -o BatchMode=yes "$@"
"#,
        sock = sock_escaped,
        key = key_arg
    );

    if !path.exists() || fs::read_to_string(&path).unwrap_or_default() != content {
        write_executable_file(&path, content.as_bytes())?;
    }

    Ok(path)
}

fn sealer_identity_path() -> Result<PathBuf> {
    Ok(ensure_ssh_state_dir()?.join("sealer.json"))
}

fn ssh_unlock_path() -> Result<PathBuf> {
    Ok(ensure_ssh_state_dir()?.join("unlock.json"))
}

fn load_ssh_unlock() -> Option<SshKeyUnlock> {
    let path = ssh_unlock_path().ok()?;
    let content = fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

fn save_ssh_unlock(expires_at: DateTime<Utc>) -> Result<()> {
    let path = ssh_unlock_path()?;
    let entry = SshKeyUnlock {
        expires_at: expires_at.timestamp(),
    };
    let content = serde_json::to_string_pretty(&entry)?;
    fs::write(&path, content)?;
    Ok(())
}

fn unlock_expires_at(entry: &SshKeyUnlock) -> Option<DateTime<Utc>> {
    DateTime::<Utc>::from_timestamp(entry.expires_at, 0)
}

fn next_local_midnight_utc() -> Result<DateTime<Utc>> {
    let now = Local::now();
    let tomorrow = now
        .date_naive()
        .succ_opt()
        .ok_or_else(|| anyhow::anyhow!("failed to calculate next day"))?;
    let naive = tomorrow
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| anyhow::anyhow!("failed to build midnight time"))?;
    let local_dt = Local
        .from_local_datetime(&naive)
        .single()
        .or_else(|| Local.from_local_datetime(&naive).earliest())
        .ok_or_else(|| anyhow::anyhow!("failed to resolve local midnight"))?;
    Ok(local_dt.with_timezone(&Utc))
}

fn prompt_touch_id() -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("Touch ID is not available on this OS");
    }

    let reason = "Flow needs Touch ID to unlock SSH keys.";
    let reason = reason.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!(
        r#"ObjC.import('stdlib');
ObjC.import('Foundation');
ObjC.import('LocalAuthentication');
const context = $.LAContext.alloc.init;
const policy = $.LAPolicyDeviceOwnerAuthenticationWithBiometrics;
const error = Ref();
if (!context.canEvaluatePolicyError(policy, error)) {{
  $.exit(2);
}}
let ok = false;
let done = false;
context.evaluatePolicyLocalizedReasonReply(policy, "{reason}", function(success, err) {{
  ok = success;
  done = true;
}});
const runLoop = $.NSRunLoop.currentRunLoop;
while (!done) {{
  runLoop.runUntilDate($.NSDate.dateWithTimeIntervalSinceNow(0.1));
}}
$.exit(ok ? 0 : 1);"#
    );

    let status = Command::new("osascript")
        .args(["-l", "JavaScript", "-e", &script])
        .status()
        .context("failed to launch Touch ID prompt")?;

    match status.code() {
        Some(0) => Ok(()),
        Some(1) => bail!("Touch ID verification failed"),
        Some(2) => bail!("Touch ID is not available on this device"),
        _ => bail!("Touch ID verification failed"),
    }
}

fn unlock_ssh_key() -> Result<()> {
    if !cfg!(target_os = "macos") {
        println!("Touch ID unlock is not available on this OS.");
        return Ok(());
    }

    if let Some(entry) = load_ssh_unlock() {
        if let Some(expires_at) = unlock_expires_at(&entry) {
            if expires_at > Utc::now() {
                let local_expiry = expires_at.with_timezone(&Local);
                println!(
                    "SSH key access already unlocked until {}",
                    local_expiry.format("%Y-%m-%d %H:%M %Z")
                );
                return Ok(());
            }
        }
    }

    println!("Touch ID required to unlock SSH keys.");
    prompt_touch_id()?;
    let expires_at = next_local_midnight_utc()?;
    save_ssh_unlock(expires_at)?;
    let local_expiry = expires_at.with_timezone(&Local);
    println!(
        "✓ SSH key access unlocked until {}",
        local_expiry.format("%Y-%m-%d %H:%M %Z")
    );
    Ok(())
}

fn require_ssh_key_unlock() -> Result<()> {
    if !cfg!(target_os = "macos") {
        return Ok(());
    }

    if let Some(entry) = load_ssh_unlock() {
        if let Some(expires_at) = unlock_expires_at(&entry) {
            if expires_at > Utc::now() {
                return Ok(());
            }
        }
    }

    unlock_ssh_key()
}

fn load_sealer_identity() -> Result<Option<SealerIdentity>> {
    let path = sealer_identity_path()?;
    if !path.exists() {
        return Ok(None);
    }

    let content =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut identity: SealerIdentity =
        serde_json::from_str(&content).context("failed to parse SSH sealer identity")?;
    if identity.sealer_secret.trim().is_empty() {
        bail!("SSH sealer identity is missing its secret");
    }

    let derived_id = get_sealer_id(&identity.sealer_secret).context("invalid SSH sealer secret")?;
    if identity.sealer_id != derived_id {
        identity.sealer_id = derived_id;
        let updated = serde_json::to_string_pretty(&identity)?;
        write_private_key(&path, updated.as_bytes())?;
    }

    Ok(Some(identity))
}

fn load_or_create_sealer_identity() -> Result<SealerIdentity> {
    if let Some(identity) = load_sealer_identity()? {
        return Ok(identity);
    }

    let identity = create_sealer_identity()?;
    let path = sealer_identity_path()?;
    let content = serde_json::to_string_pretty(&identity)?;
    write_private_key(&path, content.as_bytes())?;
    Ok(identity)
}

fn create_sealer_identity() -> Result<SealerIdentity> {
    let private_key = new_x25519_private_key();
    let sealer_secret = format!("sealerSecret_z{}", bs58::encode(&private_key).into_string());
    let sealer_id = get_sealer_id(&sealer_secret).context("failed to derive SSH sealer id")?;
    Ok(SealerIdentity {
        sealer_secret,
        sealer_id,
    })
}

fn seal_private_key(private_key: &[u8], identity: &SealerIdentity) -> Result<SealedKeyPayload> {
    let mut nonce_material = [0u8; 32];
    nonce_material[..16].copy_from_slice(Uuid::new_v4().as_bytes());
    nonce_material[16..].copy_from_slice(Uuid::new_v4().as_bytes());

    let sealed = seal(
        private_key,
        &identity.sealer_secret,
        &identity.sealer_id,
        &nonce_material,
    )
    .context("failed to seal SSH private key")?;
    Ok(SealedKeyPayload {
        sealed_b64: STANDARD.encode(sealed),
        nonce_b64: STANDARD.encode(nonce_material),
    })
}

fn unseal_private_key(
    sealed_b64: &str,
    nonce_b64: &str,
    identity: &SealerIdentity,
) -> Result<Vec<u8>> {
    let sealed = STANDARD
        .decode(sealed_b64.as_bytes())
        .context("failed to decode sealed SSH key")?;
    let nonce_material = STANDARD
        .decode(nonce_b64.as_bytes())
        .context("failed to decode sealed SSH nonce")?;
    if nonce_material.is_empty() {
        bail!("sealed SSH nonce is empty");
    }

    let unsealed = unseal(
        &sealed,
        &identity.sealer_secret,
        &identity.sealer_id,
        &nonce_material,
    )
    .context("failed to unseal SSH private key")?;
    Ok(unsealed.into())
}

fn add_key_to_agent(private_key: &[u8], sock: &Path, ttl_hours: u64) -> Result<()> {
    let ttl_seconds = ttl_hours.saturating_mul(3600).to_string();
    let mut child = Command::new("ssh-add")
        .arg("-t")
        .arg(&ttl_seconds)
        .arg("-")
        .env("SSH_AUTH_SOCK", sock)
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("failed to run ssh-add")?;

    {
        let stdin = child
            .stdin
            .as_mut()
            .context("failed to open ssh-add stdin")?;
        stdin
            .write_all(private_key)
            .context("failed to write SSH key to ssh-add")?;
    }

    let status = child.wait().context("failed to wait for ssh-add")?;
    if !status.success() {
        bail!("ssh-add failed");
    }

    Ok(())
}

fn write_private_key(path: &PathBuf, content: &[u8]) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let mut file = options
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o600);
        fs::set_permissions(path, perms)
            .with_context(|| format!("failed to chmod {}", path.display()))?;
    }

    file.write_all(content)
        .with_context(|| format!("failed to write {}", path.display()))?;

    Ok(())
}

fn write_executable_file(path: &PathBuf, content: &[u8]) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o700);
    }

    let mut file = options
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o700);
        fs::set_permissions(path, perms)
            .with_context(|| format!("failed to chmod {}", path.display()))?;
    }

    file.write_all(content)
        .with_context(|| format!("failed to write {}", path.display()))?;

    Ok(())
}

fn escape_double_quotes(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn shell_escape_arg(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.')
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
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
    fn key_env_sealed_keys_uses_expected_prefixes() {
        let (sealed, nonce, sealer) = key_env_sealed_keys("default");
        assert_eq!(sealed, "FLOW_SSH_PRIVATE_KEY_SEALED_B64");
        assert_eq!(nonce, "FLOW_SSH_PRIVATE_KEY_SEALED_NONCE_B64");
        assert_eq!(sealer, "FLOW_SSH_PRIVATE_KEY_SEALER_ID");

        let (sealed, nonce, sealer) = key_env_sealed_keys("work");
        assert_eq!(sealed, "FLOW_SSH_PRIVATE_KEY_SEALED_B64_WORK");
        assert_eq!(nonce, "FLOW_SSH_PRIVATE_KEY_SEALED_NONCE_B64_WORK");
        assert_eq!(sealer, "FLOW_SSH_PRIVATE_KEY_SEALER_ID_WORK");
    }

    #[test]
    fn seal_private_key_roundtrip() {
        let identity = create_sealer_identity().expect("identity");
        let payload = seal_private_key(b"PRIVATE_KEY", &identity).expect("seal");
        let unsealed =
            unseal_private_key(&payload.sealed_b64, &payload.nonce_b64, &identity).expect("unseal");
        assert_eq!(unsealed, b"PRIVATE_KEY");
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

    #[test]
    fn upsert_ssh_block_adds_when_missing() {
        let updated = upsert_ssh_block("", "force", "default");
        assert!(updated.contains("[ssh]"));
        assert!(updated.contains("mode = \"force\""));
        assert!(updated.contains("key_name = \"default\""));
    }

    #[test]
    fn upsert_ssh_block_updates_existing_values() {
        let input = "[ssh]\nmode = \"auto\"\nkey_name = \"work\"\n";
        let updated = upsert_ssh_block(input, "force", "default");
        assert!(updated.contains("mode = \"force\""));
        assert!(updated.contains("key_name = \"default\""));
        assert!(!updated.contains("mode = \"auto\""));
        assert!(!updated.contains("key_name = \"work\""));
    }
}
