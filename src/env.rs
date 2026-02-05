//! Environment variable management via the cloud backend with local fallback.
//!
//! Fetches, sets, and manages environment variables for projects
//! using the cloud API, with optional local storage when needed.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, TimeZone, Utc};
use reqwest::Url;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use which::which;

use crate::agent_setup;
use crate::cli::{EnvAction, ProjectEnvAction, TokenAction};
use crate::config;
use crate::deploy;
use crate::env_setup::{EnvSetupDefaults, run_env_setup};
use crate::storage::{
    create_jazz_worker_account, get_project_name as storage_project_name, sanitize_name,
};
use uuid::Uuid;

const DEFAULT_API_URL: &str = "https://myflow.sh";
const LOCAL_ENV_DIR: &str = "env-local";

/// Auth config stored in ~/.config/flow/auth.toml
#[derive(Debug, Serialize, Deserialize, Default)]
struct AuthConfig {
    token: Option<String>,
    api_url: Option<String>,
    token_source: Option<String>,
    ai_token: Option<String>,
    ai_api_url: Option<String>,
    ai_token_source: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct EnvReadUnlock {
    expires_at: i64,
}

/// An env var with optional description.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct EnvVar {
    pub value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Response from /api/env/:projectName
#[derive(Debug, Deserialize)]
struct EnvResponse {
    env: HashMap<String, String>,
    #[serde(default)]
    descriptions: HashMap<String, String>,
}

/// Response from POST /api/env/:projectName
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct SetEnvResponse {
    success: bool,
    project: String,
    environment: String,
}

/// Response from /api/env/personal
#[derive(Debug, Deserialize)]
struct PersonalEnvResponse {
    env: HashMap<String, String>,
}

/// Get the auth config path.
fn get_auth_config_path() -> PathBuf {
    let config_dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("flow");
    config_dir.join("auth.toml")
}

/// Load auth config.
fn load_auth_config_raw() -> Result<AuthConfig> {
    let path = get_auth_config_path();
    if !path.exists() {
        return Ok(AuthConfig::default());
    }
    let content =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    toml::from_str(&content).context("failed to parse auth.toml")
}

/// Load auth config and hydrate token from Keychain on macOS when configured.
fn load_auth_config() -> Result<AuthConfig> {
    let mut auth = load_auth_config_raw()?;
    if auth.token.is_none()
        && auth
            .token_source
            .as_deref()
            .map(|source| source == "keychain")
            .unwrap_or(false)
    {
        require_env_read_unlock()?;
        if let Some(token) = get_keychain_token(&get_api_url(&auth))? {
            auth.token = Some(token);
        }
    }
    Ok(auth)
}

fn load_ai_auth_config() -> Result<AuthConfig> {
    let mut auth = load_auth_config_raw()?;
    if auth.ai_token.is_none()
        && auth
            .ai_token_source
            .as_deref()
            .map(|source| source == "keychain")
            .unwrap_or(false)
    {
        if let Some(token) = get_keychain_ai_token(&get_ai_api_url(&auth))? {
            auth.ai_token = Some(token);
        }
    }
    Ok(auth)
}

/// Save auth config.
fn save_auth_config(config: &AuthConfig) -> Result<()> {
    let path = get_auth_config_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = toml::to_string_pretty(config)?;
    fs::write(&path, content)?;
    Ok(())
}

fn keychain_service(api_url: &str) -> String {
    format!("flow-cloud-token:{}", api_url)
}

fn keychain_service_ai(api_url: &str) -> String {
    format!("flow-ai-token:{}", api_url)
}

fn set_keychain_token(api_url: &str, token: &str) -> Result<()> {
    let service = keychain_service(api_url);
    let status = Command::new("security")
        .args([
            "add-generic-password",
            "-a",
            "flow",
            "-s",
            &service,
            "-w",
            token,
            "-U",
        ])
        .status()
        .context("failed to store token in Keychain")?;
    if !status.success() {
        bail!("failed to store token in Keychain");
    }
    Ok(())
}

fn set_keychain_ai_token(api_url: &str, token: &str) -> Result<()> {
    let service = keychain_service_ai(api_url);
    let status = Command::new("security")
        .args([
            "add-generic-password",
            "-a",
            "flow",
            "-s",
            &service,
            "-w",
            token,
            "-U",
        ])
        .status()
        .context("failed to store AI token in Keychain")?;
    if !status.success() {
        bail!("failed to store AI token in Keychain");
    }
    Ok(())
}

fn get_keychain_token(api_url: &str) -> Result<Option<String>> {
    if !cfg!(target_os = "macos") {
        return Ok(None);
    }

    let service = keychain_service(api_url);
    let output = Command::new("security")
        .args(["find-generic-password", "-a", "flow", "-s", &service, "-w"])
        .output()
        .context("failed to read token from Keychain")?;

    if output.status.success() {
        let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if token.is_empty() {
            return Ok(None);
        }
        return Ok(Some(token));
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("could not be found") || stderr.contains("SecKeychainSearchCopyNext") {
        return Ok(None);
    }

    bail!("failed to read token from Keychain: {}", stderr.trim());
}

fn get_keychain_ai_token(api_url: &str) -> Result<Option<String>> {
    if !cfg!(target_os = "macos") {
        return Ok(None);
    }

    let service = keychain_service_ai(api_url);
    let output = Command::new("security")
        .args(["find-generic-password", "-a", "flow", "-s", &service, "-w"])
        .output()
        .context("failed to read AI token from Keychain")?;

    if output.status.success() {
        let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if token.is_empty() {
            return Ok(None);
        }
        return Ok(Some(token));
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("could not be found") || stderr.contains("SecKeychainSearchCopyNext") {
        return Ok(None);
    }

    bail!("failed to read AI token from Keychain: {}", stderr.trim());
}

fn store_auth_token(auth: &mut AuthConfig, token: String) -> Result<()> {
    let api_url = get_api_url(auth);
    if cfg!(target_os = "macos") {
        if let Err(err) = set_keychain_token(&api_url, &token) {
            eprintln!("⚠ Failed to store token in Keychain: {}", err);
            eprintln!("  Falling back to auth.toml storage.");
            auth.token = Some(token);
            auth.token_source = None;
            return Ok(());
        }
        auth.token = None;
        auth.token_source = Some("keychain".to_string());
    } else {
        auth.token = Some(token);
        auth.token_source = None;
    }
    Ok(())
}

fn store_ai_auth_token(auth: &mut AuthConfig, token: String) -> Result<()> {
    let api_url = get_ai_api_url(auth);
    if cfg!(target_os = "macos") {
        if let Err(err) = set_keychain_ai_token(&api_url, &token) {
            eprintln!("⚠ Failed to store AI token in Keychain: {}", err);
            eprintln!("  Falling back to auth.toml storage.");
            auth.ai_token = Some(token);
            auth.ai_token_source = None;
            return Ok(());
        }
        auth.ai_token = None;
        auth.ai_token_source = Some("keychain".to_string());
    } else {
        auth.ai_token = Some(token);
        auth.ai_token_source = None;
    }
    Ok(())
}

fn get_env_unlock_path() -> PathBuf {
    let config_dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("flow");
    config_dir.join("env_read_unlock.json")
}

fn load_env_unlock() -> Option<EnvReadUnlock> {
    let path = get_env_unlock_path();
    let content = fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

fn unlock_expires_at(entry: &EnvReadUnlock) -> Option<DateTime<Utc>> {
    Utc.timestamp_opt(entry.expires_at, 0).single()
}

fn save_env_unlock(expires_at: DateTime<Utc>) -> Result<()> {
    let path = get_env_unlock_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let entry = EnvReadUnlock {
        expires_at: expires_at.timestamp(),
    };
    let content = serde_json::to_string_pretty(&entry)?;
    fs::write(&path, content)?;
    Ok(())
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
    if std::env::var("FLOW_NO_TOUCH_ID").is_ok() || !std::io::stdin().is_terminal() {
        bail!("Touch ID prompt requires an interactive terminal");
    }

    let reason = "Flow needs Touch ID to read env vars.";
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

fn unlock_env_read() -> Result<()> {
    if !cfg!(target_os = "macos") {
        println!("Touch ID unlock is not available on this OS.");
        return Ok(());
    }

    if let Some(entry) = load_env_unlock() {
        if let Some(expires_at) = unlock_expires_at(&entry) {
            if expires_at > Utc::now() {
                let local_expiry = expires_at.with_timezone(&Local);
                println!(
                    "Env read access already unlocked until {}",
                    local_expiry.format("%Y-%m-%d %H:%M %Z")
                );
                return Ok(());
            }
        }
    }

    println!("Touch ID required to read env vars.");
    prompt_touch_id()?;
    let expires_at = next_local_midnight_utc()?;
    save_env_unlock(expires_at)?;
    let local_expiry = expires_at.with_timezone(&Local);
    println!(
        "✓ Env read access unlocked until {}",
        local_expiry.format("%Y-%m-%d %H:%M %Z")
    );
    Ok(())
}

fn require_env_read_unlock() -> Result<()> {
    if !cfg!(target_os = "macos") {
        return Ok(());
    }

    if let Some(entry) = load_env_unlock() {
        if let Some(expires_at) = unlock_expires_at(&entry) {
            if expires_at > Utc::now() {
                return Ok(());
            }
        }
    }

    unlock_env_read()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnvScope {
    Project,
    Personal,
}

#[derive(Debug, Clone)]
struct EnvTargetConfig {
    project_name: String,
    env_space: Option<String>,
    env_space_kind: EnvScope,
}

#[derive(Debug, Clone)]
enum EnvTarget {
    Project { name: String },
    Personal { space: Option<String> },
}

fn parse_env_space_kind(value: Option<&str>) -> EnvScope {
    match value.map(|s| s.trim().to_ascii_lowercase()) {
        Some(ref v) if v == "personal" || v == "user" || v == "private" => EnvScope::Personal,
        _ => EnvScope::Project,
    }
}

fn load_env_target_config() -> Result<EnvTargetConfig> {
    let cwd = std::env::current_dir()?;
    if let Some(flow_path) = find_flow_toml(&cwd) {
        let cfg = config::load(&flow_path)?;
        let project_name = if let Some(name) = cfg.project_name {
            name
        } else if let Some(parent) = flow_path.parent() {
            parent
                .file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| "unnamed".to_string())
        } else {
            "unnamed".to_string()
        };
        let env_space = cfg.env_space.and_then(|value| {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        });
        let env_space_kind = parse_env_space_kind(cfg.env_space_kind.as_deref());
        return Ok(EnvTargetConfig {
            project_name,
            env_space,
            env_space_kind,
        });
    }

    let project_name = cwd
        .file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "unnamed".to_string());
    Ok(EnvTargetConfig {
        project_name,
        env_space: None,
        env_space_kind: EnvScope::Project,
    })
}

fn resolve_env_target() -> Result<EnvTarget> {
    let cfg = load_env_target_config()?;
    Ok(match cfg.env_space_kind {
        EnvScope::Personal => EnvTarget::Personal {
            space: cfg.env_space,
        },
        EnvScope::Project => EnvTarget::Project {
            name: cfg.env_space.unwrap_or(cfg.project_name),
        },
    })
}

fn resolve_personal_target() -> Result<EnvTarget> {
    let cfg = load_env_target_config()?;
    Ok(EnvTarget::Personal {
        space: cfg.env_space,
    })
}

fn env_target_label(target: &EnvTarget) -> String {
    match target {
        EnvTarget::Project { name } => name.clone(),
        EnvTarget::Personal { space } => space.clone().unwrap_or_else(|| "personal".to_string()),
    }
}

fn local_env_enabled() -> bool {
    if let Some(backend) = config::preferred_env_backend() {
        match backend.as_str() {
            "local" => return true,
            "cloud" | "remote" => return false,
            _ => {}
        }
    }

    match std::env::var("FLOW_ENV_BACKEND")
        .ok()
        .map(|v| v.to_ascii_lowercase())
        .as_deref()
    {
        Some("local") => true,
        Some("cloud") | Some("remote") => false,
        _ => std::env::var("FLOW_ENV_LOCAL")
            .ok()
            .map(|v| {
                let v = v.to_ascii_lowercase();
                v == "1" || v == "true" || v == "yes"
            })
            .unwrap_or(false),
    }
}

fn local_env_root() -> Result<PathBuf> {
    let base = config::ensure_global_config_dir()?;
    let path = base.join(LOCAL_ENV_DIR);
    fs::create_dir_all(&path)?;
    Ok(path)
}

fn sanitize_env_segment(value: &str) -> String {
    let mut out = String::new();
    let mut last_sep = false;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
            out.push(ch);
            last_sep = false;
        } else if !last_sep {
            out.push('_');
            last_sep = true;
        }
    }
    let trimmed = out.trim_matches('_').to_string();
    if trimmed.is_empty() {
        "unnamed".to_string()
    } else {
        trimmed
    }
}

fn local_env_path(target: &EnvTarget, environment: &str) -> Result<PathBuf> {
    let root = local_env_root()?;
    let target_label = sanitize_env_segment(&env_target_label(target));
    let env_label = sanitize_env_segment(if environment.trim().is_empty() {
        "production"
    } else {
        environment
    });
    let dir = root.join(target_label);
    fs::create_dir_all(&dir)?;
    Ok(dir.join(format!("{env_label}.env")))
}

fn read_local_env_vars(target: &EnvTarget, environment: &str) -> Result<HashMap<String, String>> {
    let path = local_env_path(target, environment)?;
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let content = fs::read_to_string(&path)?;
    Ok(parse_env_file(&content))
}

/// Read keys from the local personal env store without cloud access.
pub fn fetch_local_personal_env_vars(keys: &[String]) -> Result<HashMap<String, String>> {
    let target = resolve_personal_target()?;
    let vars = read_local_env_vars(&target, "production")?;
    if keys.is_empty() {
        return Ok(vars);
    }
    let mut filtered = HashMap::new();
    for key in keys {
        if let Some(value) = vars.get(key) {
            filtered.insert(key.clone(), value.clone());
        }
    }
    Ok(filtered)
}

fn write_local_env_vars(
    target: &EnvTarget,
    environment: &str,
    vars: &HashMap<String, String>,
) -> Result<PathBuf> {
    let path = local_env_path(target, environment)?;
    let mut keys: Vec<_> = vars.keys().collect();
    keys.sort();

    let mut content = String::new();
    content.push_str(&format!(
        "# Local env store (flow)\n# Target: {}\n# Environment: {}\n",
        env_target_label(target),
        environment
    ));
    for key in keys {
        let value = &vars[key];
        let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
        content.push_str(&format!("{key}=\"{escaped}\"\n"));
    }
    fs::write(&path, content)?;
    Ok(path)
}

fn set_local_env_var(
    target: &EnvTarget,
    environment: &str,
    key: &str,
    value: &str,
) -> Result<PathBuf> {
    let mut vars = read_local_env_vars(target, environment)?;
    vars.insert(key.to_string(), value.to_string());
    write_local_env_vars(target, environment, &vars)
}

fn is_local_fallback_error(err: &anyhow::Error) -> bool {
    let msg = err.to_string().to_ascii_lowercase();
    msg.contains("not logged in")
        || msg.contains("failed to connect to cloud")
        || msg.contains("unauthorized")
}

fn env_target_name_for_tokens(target: &EnvTarget) -> Result<String> {
    match target {
        EnvTarget::Project { name } => Ok(name.clone()),
        EnvTarget::Personal { space } => space.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "Personal env space name required for service tokens. Set env_space in flow.toml."
            )
        }),
    }
}

fn resolve_env_file_path() -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;

    if let Some(flow_path) = find_flow_toml(&cwd) {
        let project_root = flow_path.parent().unwrap_or(&cwd);
        let cfg = config::load(&flow_path)?;
        if let Some(cf_cfg) = cfg.cloudflare {
            if let Some(env_file) = cf_cfg.env_file {
                let env_file = env_file.trim();
                if !env_file.is_empty() {
                    let expanded = config::expand_path(env_file);
                    return Ok(project_root.join(expanded));
                }
            }
        }
        return Ok(project_root.join(".env"));
    }

    Ok(cwd.join(".env"))
}

/// Get API URL from config or default.
fn get_api_url(auth: &AuthConfig) -> String {
    auth.api_url
        .clone()
        .unwrap_or_else(|| DEFAULT_API_URL.to_string())
}

fn get_ai_api_url(auth: &AuthConfig) -> String {
    auth.ai_api_url
        .clone()
        .unwrap_or_else(|| DEFAULT_API_URL.to_string())
}

pub fn load_ai_auth_token() -> Result<Option<String>> {
    let auth = load_ai_auth_config()?;
    Ok(auth.ai_token)
}

pub fn load_ai_api_url() -> Result<String> {
    let auth = load_auth_config_raw()?;
    Ok(get_ai_api_url(&auth))
}

pub fn save_ai_auth_token(token: String, api_url: Option<String>) -> Result<()> {
    let mut auth = load_auth_config_raw()?;
    if let Some(api_url) = api_url {
        auth.ai_api_url = Some(api_url);
    }
    store_ai_auth_token(&mut auth, token)?;
    save_auth_config(&auth)?;
    Ok(())
}

fn find_flow_toml(start: &PathBuf) -> Option<PathBuf> {
    let mut current = start.clone();
    loop {
        let candidate = current.join("flow.toml");
        if candidate.exists() {
            return Some(candidate);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn is_cloud_source(source: Option<&str>) -> bool {
    matches!(
        source.map(|s| s.to_ascii_lowercase()).as_deref(),
        Some("cloud") | Some("remote") | Some("myflow")
    )
}

fn format_default_hint(value: &str) -> String {
    value.to_string()
}

pub fn get_personal_env_var(key: &str) -> Result<Option<String>> {
    if local_env_enabled() {
        let vars = fetch_local_personal_env_vars(&[key.to_string()])?;
        return Ok(vars.get(key).cloned());
    }

    let auth = load_auth_config()?;
    let token = match auth.token.as_ref() {
        Some(t) => t,
        None => return Ok(None),
    };
    require_env_read_unlock()?;

    let api_url = get_api_url(&auth);
    let target = resolve_personal_target()?;
    let mut url = Url::parse(&format!("{}/api/env/personal", api_url))?;
    url.query_pairs_mut().append_pair("keys", key);
    if let EnvTarget::Personal { ref space } = target {
        if let Some(space) = space.as_ref() {
            url.query_pairs_mut().append_pair("space", &space);
        }
    }

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;

    let resp = client
        .get(url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .context("failed to connect to cloud")?;

    if resp.status() == 401 {
        return Ok(None);
    }

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        bail!("API error {}: {}", status, body);
    }

    let data: PersonalEnvResponse = resp.json().context("failed to parse response")?;
    Ok(data.env.get(key).cloned())
}

/// Fuzzy search personal env vars and copy selected value to clipboard.
fn fuzzy_select_env() -> Result<()> {
    require_env_read_unlock()?;

    // Fetch all personal env vars
    let target = resolve_personal_target()?;
    let vars = fetch_env_vars(&target, "production", &[], false)?;
    if vars.is_empty() {
        println!("No personal env vars found.");
        println!("Set one with: f env set KEY=VALUE");
        return Ok(());
    }

    // Format for fzf: KEY=VALUE (showing first 40 chars of value)
    let mut lines: Vec<String> = vars
        .iter()
        .map(|(k, v)| {
            let preview = if v.len() > 40 {
                format!("{}...", &v[..40])
            } else {
                v.clone()
            };
            format!("{}\t{}", k, preview)
        })
        .collect();
    lines.sort();

    let input = lines.join("\n");

    // Run fzf
    let mut child = Command::new("fzf")
        .args([
            "--height=40%",
            "--reverse",
            "--delimiter=\t",
            "--with-nth=1",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .context("Failed to run fzf. Is it installed?")?;

    if let Some(stdin) = child.stdin.as_mut() {
        use std::io::Write;
        stdin.write_all(input.as_bytes())?;
    }

    let output = child.wait_with_output()?;
    if !output.status.success() {
        // User cancelled
        return Ok(());
    }

    let selected = String::from_utf8_lossy(&output.stdout);
    let selected = selected.trim();
    if selected.is_empty() {
        return Ok(());
    }

    // Extract key from selection
    let key = selected.split('\t').next().unwrap_or(selected);

    // Get the full value
    if let Some(value) = vars.get(key) {
        if std::env::var("FLOW_NO_CLIPBOARD").is_ok() || !std::io::stdin().is_terminal() {
            println!("Clipboard disabled; skipping copy.");
        } else {
            // Copy to clipboard
            let mut pbcopy = Command::new("pbcopy")
                .stdin(std::process::Stdio::piped())
                .spawn()
                .context("Failed to run pbcopy")?;

            if let Some(stdin) = pbcopy.stdin.as_mut() {
                use std::io::Write;
                stdin.write_all(value.as_bytes())?;
            }
            pbcopy.wait()?;

            println!("Copied {} to clipboard", key);
        }
    }

    Ok(())
}

/// Run the env subcommand.
pub fn run(action: Option<EnvAction>) -> Result<()> {
    // No action = fuzzy search personal envs and copy value
    let Some(action) = action else {
        let auth = load_auth_config()?;
        if auth.token.is_some() {
            return fuzzy_select_env();
        }
        return status();
    };

    match action {
        EnvAction::Sync => agent_setup::run()?,
        EnvAction::Unlock => unlock_env_read()?,
        EnvAction::Login => login()?,
        EnvAction::New => new_env_template()?,
        EnvAction::Pull { environment } => pull(&environment)?,
        EnvAction::Push { environment } => push(&environment)?,
        EnvAction::Guide { environment } => guide(&environment)?,
        EnvAction::Apply => {
            let cwd = std::env::current_dir()?;
            let flow_path = find_flow_toml(&cwd)
                .ok_or_else(|| anyhow::anyhow!("flow.toml not found. Run `f init` first."))?;
            let project_root = flow_path.parent().map(|p| p.to_path_buf()).unwrap_or(cwd);
            let flow_config = config::load(&flow_path)?;
            deploy::apply_cloudflare_env(&project_root, Some(&flow_config))?;
        }
        EnvAction::Bootstrap => {
            let cwd = std::env::current_dir()?;
            let flow_path = find_flow_toml(&cwd)
                .ok_or_else(|| anyhow::anyhow!("flow.toml not found. Run `f init` first."))?;
            let project_root = flow_path.parent().map(|p| p.to_path_buf()).unwrap_or(cwd);
            let flow_config = config::load(&flow_path)?;
            bootstrap_cloudflare_secrets(&project_root, &flow_config)?;
        }
        EnvAction::Keys => {
            show_keys()?;
        }
        EnvAction::Setup {
            env_file,
            environment,
        } => setup(env_file, environment)?,
        EnvAction::List { environment } => list(&environment)?,
        EnvAction::Set { pair, personal } => {
            let _ = personal;
            set_personal_env_var_from_pair(&pair)?;
        }
        EnvAction::Delete { keys } => delete_personal_env_vars(&keys)?,
        EnvAction::Project { action } => run_project_env_action(action)?,
        EnvAction::Status => status()?,
        EnvAction::Get {
            keys,
            personal,
            environment,
            format,
        } => get_vars(&keys, personal, &environment, &format)?,
        EnvAction::Run {
            personal,
            environment,
            keys,
            command,
        } => run_with_env(personal, &environment, &keys, &command)?,
        EnvAction::Token { action } => run_token_action(action)?,
    }

    Ok(())
}

fn run_token_action(action: TokenAction) -> Result<()> {
    match action {
        TokenAction::Create { name, permissions } => token_create(name.as_deref(), &permissions)?,
        TokenAction::List => token_list()?,
        TokenAction::Revoke { name } => token_revoke(&name)?,
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct EnvTemplate {
    id: &'static str,
    title: &'static str,
    key: &'static str,
    description: &'static str,
    instructions: &'static [&'static str],
}

fn env_templates() -> Vec<EnvTemplate> {
    vec![EnvTemplate {
        id: "cloudflare",
        title: "Cloudflare API token",
        key: "CLOUDFLARE_API_TOKEN",
        description: "Token used by wrangler to deploy Workers/Pages.",
        instructions: &[
            "Open https://dash.cloudflare.com/profile/api-tokens",
            "Create a token (Template: Edit Cloudflare Workers or Custom)",
            "Permissions: Workers Scripts:Edit, Workers Routes:Edit, Pages:Edit",
            "Add Zone:Read + DNS:Edit for your domain",
            "Copy the token value",
        ],
    }]
}

fn new_env_template() -> Result<()> {
    ensure_env_login()?;

    let templates = env_templates();
    if templates.is_empty() {
        println!("No env templates available.");
        return Ok(());
    }
    let Some(template) = select_env_template(&templates)? else {
        println!("No template selected.");
        return Ok(());
    };

    println!("Template: {}", template.title);
    println!("Key: {}", template.key);
    println!("{}", template.description);
    println!();
    println!("How to get it:");
    for step in template.instructions {
        println!("  - {}", step);
    }
    println!();

    let label = format!("Enter {} token (input hidden): ", template.id);
    let value = prompt_secret(&label)?;
    let Some(value) = value else {
        println!("No token entered; nothing saved.");
        return Ok(());
    };

    set_personal_env_var(template.key, &value)?;

    println!();
    println!("Saved {} to personal envs.", template.key);
    Ok(())
}

fn select_env_template(templates: &[EnvTemplate]) -> Result<Option<EnvTemplate>> {
    if templates.is_empty() {
        return Ok(None);
    }

    let use_fzf = std::io::stdin().is_terminal() && which("fzf").is_ok();
    if use_fzf {
        let mut lines = Vec::new();
        for template in templates {
            lines.push(format!("{}\t{}", template.id, template.title));
        }
        let input = lines.join("\n");

        let mut child = Command::new("fzf")
            .args([
                "--height=40%",
                "--reverse",
                "--delimiter=\t",
                "--with-nth=1,2",
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .context("Failed to run fzf. Is it installed?")?;

        if let Some(stdin) = child.stdin.as_mut() {
            use std::io::Write;
            stdin.write_all(input.as_bytes())?;
        }

        let output = child.wait_with_output()?;
        if !output.status.success() {
            return Ok(None);
        }

        let selected = String::from_utf8_lossy(&output.stdout);
        let selected = selected.trim();
        if selected.is_empty() {
            return Ok(None);
        }
        let id = selected.split('\t').next().unwrap_or(selected);
        return Ok(templates.iter().copied().find(|t| t.id == id));
    }

    println!("Available templates:");
    for (idx, template) in templates.iter().enumerate() {
        println!("  {}. {} ({})", idx + 1, template.title, template.key);
    }
    println!();
    let selection = prompt_line("Select a template number (blank to cancel): ")?;
    let Some(selection) = selection else {
        return Ok(None);
    };
    let idx: usize = selection.trim().parse().context("Invalid selection")?;
    if idx == 0 || idx > templates.len() {
        bail!("Selection out of range");
    }
    Ok(Some(templates[idx - 1]))
}

fn ensure_env_login() -> Result<()> {
    let auth = load_auth_config()?;
    if auth.token.is_some() {
        return Ok(());
    }

    if !std::io::stdin().is_terminal() {
        bail!("Not logged in. Run `f env login` first.");
    }

    if prompt_confirm("Not logged in. Run `f env login` now? (y/N): ")? {
        login()?;
        return Ok(());
    }

    bail!("Not logged in. Run `f env login` first.");
}

fn run_project_env_action(action: ProjectEnvAction) -> Result<()> {
    match action {
        ProjectEnvAction::Set { pair, environment } => {
            set_project_env_var_from_pair(&pair, &environment)?
        }
        ProjectEnvAction::Delete { keys, environment } => {
            delete_project_env_vars(&keys, &environment)?
        }
        ProjectEnvAction::List { environment } => list(&environment)?,
    }
    Ok(())
}

fn set_personal_env_var_from_pair(pair: &str) -> Result<()> {
    let (key, value) = pair
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("Invalid format. Use KEY=VALUE"))?;
    set_personal_env_var(key.trim(), value.trim())
}

fn set_project_env_var_from_pair(pair: &str, environment: &str) -> Result<()> {
    let (key, value) = pair
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("Invalid format. Use KEY=VALUE"))?;
    set_project_env_var_internal(key.trim(), value.trim(), environment, None)
}

pub(crate) fn delete_personal_env_vars(keys: &[String]) -> Result<()> {
    let auth = load_auth_config()?;
    let token = auth
        .token
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Not logged in. Run `f env login` first."))?;

    if keys.is_empty() {
        bail!("No keys specified");
    }

    let api_url = get_api_url(&auth);
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let target = resolve_personal_target()?;
    let mut url = Url::parse(&format!("{}/api/env/personal", api_url))?;
    if let EnvTarget::Personal { ref space } = target {
        if let Some(space) = space.as_ref() {
            url.query_pairs_mut().append_pair("space", space);
        }
    }
    let body = serde_json::json!({ "keys": keys });

    let resp = client
        .delete(url)
        .header("Authorization", format!("Bearer {}", token))
        .json(&body)
        .send()
        .context("failed to connect to cloud")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        bail!("API error {}: {}", status, body);
    }

    println!("✓ Deleted {} key(s)", keys.len());
    Ok(())
}

fn delete_project_env_vars(keys: &[String], environment: &str) -> Result<()> {
    let auth = load_auth_config()?;
    let token = auth
        .token
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Not logged in. Run `f env login` first."))?;

    if keys.is_empty() {
        bail!("No keys specified");
    }

    let api_url = get_api_url(&auth);
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let target = resolve_env_target()?;

    let url = match &target {
        EnvTarget::Personal { space } => {
            let mut url = Url::parse(&format!("{}/api/env/personal", api_url))?;
            if let Some(space) = space {
                url.query_pairs_mut().append_pair("space", space);
            }
            url
        }
        EnvTarget::Project { name } => Url::parse(&format!("{}/api/env/{}", api_url, name))?,
    };
    let body = serde_json::json!({
        "keys": keys,
        "environment": environment,
    });

    let resp = client
        .delete(url)
        .header("Authorization", format!("Bearer {}", token))
        .json(&body)
        .send()
        .context("failed to connect to cloud")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        bail!("API error {}: {}", status, body);
    }

    let target_label = match target {
        EnvTarget::Personal { space } => {
            format!(
                "personal{}",
                space.map(|s| format!(":{}", s)).unwrap_or_default()
            )
        }
        EnvTarget::Project { name } => name,
    };
    println!(
        "✓ Deleted {} key(s) from {} ({})",
        keys.len(),
        target_label,
        environment
    );
    Ok(())
}

/// Login / set token.
fn login() -> Result<()> {
    let mut auth = load_auth_config_raw()?;

    println!("Cloud Environment Manager");
    println!("─────────────────────────────");
    println!();
    println!("To get a token:");
    println!("  1. Go to {} and sign in", DEFAULT_API_URL);
    println!("  2. Go to Settings → API Tokens");
    println!("  3. Create a new token");
    println!();

    let api_url = prompt_line_default("API base URL", Some(DEFAULT_API_URL))?;
    if let Some(api_url) = api_url {
        auth.api_url = Some(api_url);
    }

    print!("Enter your API token: ");
    io::stdout().flush()?;

    let mut token = String::new();
    io::stdin().read_line(&mut token)?;
    let token = token.trim().to_string();

    if token.is_empty() {
        bail!("Token cannot be empty");
    }

    if !token.starts_with("cloud_") {
        println!("Warning: Token doesn't start with 'cloud_' - are you sure this is correct?");
    }

    store_auth_token(&mut auth, token)?;
    save_auth_config(&auth)?;

    println!();
    if auth.token_source.as_deref() == Some("keychain") {
        println!("✓ Token saved to Keychain");
    } else {
        println!("✓ Token saved to {}", get_auth_config_path().display());
    }
    println!();
    println!("You can now use:");
    println!("  f env pull    - Fetch env vars for this project");
    println!("  f env push    - Push local .env to cloud");
    println!("  f env list    - List env vars");

    Ok(())
}

/// Pull env vars from cloud and write to .env.
fn pull(environment: &str) -> Result<()> {
    let target = resolve_env_target()?;
    let label = env_target_label(&target);
    println!("Fetching envs for '{}' ({})...", label, environment);

    let vars = fetch_env_vars(&target, environment, &[], true)?;

    if vars.is_empty() {
        println!("No env vars found for '{}' ({})", label, environment);
        return Ok(());
    }

    // Write to .env
    let mut content = String::new();
    content.push_str(&format!(
        "# Environment: {} (pulled from cloud)\n",
        environment
    ));
    content.push_str(&format!("# Space: {}\n", label));
    content.push_str("#\n");

    let mut keys: Vec<_> = vars.keys().collect();
    keys.sort();

    for key in keys {
        let value = &vars[key];
        // Escape quotes in value
        let escaped = value.replace('\"', "\\\"");
        content.push_str(&format!("{}=\"{}\"\n", key, escaped));
    }

    let env_path = resolve_env_file_path()?;
    if let Some(parent) = env_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&env_path, &content)?;

    println!("✓ Wrote {} env vars to {}", vars.len(), env_path.display());

    Ok(())
}

/// Push local .env to cloud.
fn push(environment: &str) -> Result<()> {
    let env_path = resolve_env_file_path()?;
    if !env_path.exists() {
        bail!("env file not found: {}", env_path.display());
    }

    let content = fs::read_to_string(&env_path)?;
    let vars = parse_env_file(&content);

    if vars.is_empty() {
        println!("No env vars found in .env");
        return Ok(());
    }

    push_vars(environment, vars)
}

fn push_vars(environment: &str, vars: HashMap<String, String>) -> Result<()> {
    if vars.is_empty() {
        println!("No env vars selected.");
        return Ok(());
    }

    let auth = load_auth_config()?;
    let token = auth
        .token
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Not logged in. Run `f env login` first."))?;
    let api_url = get_api_url(&auth);
    let target = resolve_env_target()?;
    let label = env_target_label(&target);

    println!(
        "Pushing {} env vars to '{}' ({})...",
        vars.len(),
        label,
        environment
    );

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let url = match &target {
        EnvTarget::Personal { space } => {
            let mut url = Url::parse(&format!("{}/api/env/personal", api_url))?;
            if let Some(space) = space {
                url.query_pairs_mut().append_pair("space", space);
            }
            url
        }
        EnvTarget::Project { name } => Url::parse(&format!("{}/api/env/{}", api_url, name))?,
    };
    let body = serde_json::json!({
        "vars": vars,
        "environment": environment,
    });

    let resp = client
        .post(url)
        .header("Authorization", format!("Bearer {}", token))
        .json(&body)
        .send()
        .context("failed to connect to cloud")?;

    if resp.status() == 401 {
        bail!("Unauthorized. Check your token with `f env login`.");
    }

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        bail!("API error {}: {}", status, body);
    }

    if matches!(target, EnvTarget::Project { .. }) {
        let _: SetEnvResponse = resp.json().context("failed to parse response")?;
    }

    println!("✓ Pushed {} env vars to cloud", vars.len());

    Ok(())
}

fn guide(environment: &str) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let flow_path = find_flow_toml(&cwd)
        .ok_or_else(|| anyhow::anyhow!("flow.toml not found. Run `f init` first."))?;
    let cfg = config::load(&flow_path)?;

    let cf_cfg = cfg
        .cloudflare
        .as_ref()
        .context("No [cloudflare] section in flow.toml")?;

    let mut required = Vec::new();
    let mut seen = HashSet::new();
    for key in cf_cfg.env_keys.iter().chain(cf_cfg.env_vars.iter()) {
        if seen.insert(key.clone()) {
            required.push(key.clone());
        }
    }

    if required.is_empty() {
        bail!(
            "No env keys configured. Add cloudflare.env_keys or cloudflare.env_vars to flow.toml."
        );
    }

    println!("Checking required env vars for '{}'...", environment);
    let existing = match fetch_project_env_vars(environment, &required) {
        Ok(vars) => vars,
        Err(err) => {
            let msg = format!("{err:#}");
            if msg.contains("Project not found.") {
                println!("  (project not found yet; will create on first set)");
                HashMap::new()
            } else {
                return Err(err);
            }
        }
    };
    let var_keys: HashSet<String> = cf_cfg.env_vars.iter().cloned().collect();

    let mut missing = Vec::new();
    for key in &required {
        if existing
            .get(key)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
        {
            println!("  ✓ {}", key);
        } else {
            println!("  ✗ {} (missing)", key);
            missing.push(key.clone());
        }
    }

    if missing.is_empty() {
        println!("✓ All required env vars are set.");
        return Ok(());
    }

    println!();
    println!("Enter missing values (leave empty to skip).");
    for key in missing {
        let default_value = cf_cfg.env_defaults.get(&key).map(|value| value.as_str());
        let is_secret = !var_keys.contains(&key);
        let value = prompt_value(&key, default_value, is_secret)?;

        if let Some(value) = value {
            set_project_env_var(&key, &value, environment, None)?;
        }
    }

    Ok(())
}

fn bootstrap_cloudflare_secrets(project_root: &Path, cfg: &config::Config) -> Result<()> {
    let cf_cfg = cfg
        .cloudflare
        .as_ref()
        .context("No [cloudflare] section in flow.toml")?;

    if cf_cfg.bootstrap_secrets.is_empty() {
        bail!("No bootstrap secrets configured. Add cloudflare.bootstrap_secrets to flow.toml.");
    }

    println!("Bootstrap Cloudflare secrets");
    println!("─────────────────────────────");
    println!("Enter values (leave empty to skip).");

    let mut values = HashMap::new();
    let mut generated_env_token: Option<String> = None;
    let needs_env_account = cf_cfg
        .bootstrap_secrets
        .iter()
        .any(|key| key == "JAZZ_WORKER_ACCOUNT" || key == "JAZZ_WORKER_SECRET");
    let needs_auth_account = cf_cfg.bootstrap_secrets.iter().any(|key| {
        key == "JAZZ_AUTH_WORKER_ACCOUNT_ID" || key == "JAZZ_AUTH_WORKER_ACCOUNT_SECRET"
    });

    if needs_env_account || needs_auth_account {
        let project = storage_project_name()?;
        let default_env_name = format!("{}-jazz-env", sanitize_name(&project));
        let default_auth_name = format!("{}-jazz-auth", sanitize_name(&project));
        let default_peer = "wss://cloud.jazz.tools/?key=cloud@myflow.sh";

        if needs_env_account {
            if prompt_confirm("Generate a new Jazz env-store account now? (y/N): ")? {
                println!("Creating Jazz env-store account...");
                let name = cf_cfg
                    .bootstrap_jazz_name
                    .as_deref()
                    .unwrap_or(&default_env_name);
                let peer = cf_cfg
                    .bootstrap_jazz_peer
                    .as_deref()
                    .unwrap_or(default_peer);
                let creds = create_jazz_worker_account(peer, name)?;
                values.insert("JAZZ_WORKER_ACCOUNT".to_string(), creds.account_id);
                values.insert("JAZZ_WORKER_SECRET".to_string(), creds.agent_secret);
                println!("✓ Jazz env-store account created");
            }
        }

        if needs_auth_account {
            if prompt_confirm("Generate a new Jazz auth account now? (y/N): ")? {
                println!("Creating Jazz auth account...");
                let name = cf_cfg
                    .bootstrap_jazz_auth_name
                    .as_deref()
                    .unwrap_or(&default_auth_name);
                let peer = cf_cfg
                    .bootstrap_jazz_auth_peer
                    .as_deref()
                    .unwrap_or(default_peer);
                let creds = create_jazz_worker_account(peer, name)?;
                values.insert("JAZZ_AUTH_WORKER_ACCOUNT_ID".to_string(), creds.account_id);
                values.insert(
                    "JAZZ_AUTH_WORKER_ACCOUNT_SECRET".to_string(),
                    creds.agent_secret,
                );
                println!("✓ Jazz auth account created");
            }
        }
    }

    for key in &cf_cfg.bootstrap_secrets {
        if values.contains_key(key) {
            continue;
        }
        if key == "ENV_API_TOKEN" || key == "FLOW_ENV_TOKEN" {
            let value = prompt_secret(&format!("{} (leave empty to auto-generate): ", key))?;
            let value = match value {
                Some(value) => value,
                None => {
                    if let Some(existing) = generated_env_token.clone() {
                        existing
                    } else {
                        let token = generate_env_api_token();
                        generated_env_token = Some(token.clone());
                        token
                    }
                }
            };
            values.insert(key.clone(), value);
            continue;
        }

        let value = prompt_secret(&format!("{}: ", key))?;
        if let Some(value) = value {
            values.insert(key.clone(), value);
        }
    }

    values.retain(|_, value| !value.trim().is_empty());

    if values.is_empty() {
        println!("No secrets provided; nothing to set.");
        return Ok(());
    }

    println!("Setting Cloudflare secrets...");
    deploy::set_cloudflare_secrets(project_root, Some(cfg), &values)?;
    println!("✓ Cloudflare secrets updated");

    let mut auth = load_auth_config_raw()?;
    let bootstrap_token = values
        .get("ENV_API_TOKEN")
        .or_else(|| values.get("FLOW_ENV_TOKEN"))
        .cloned();
    if let Some(token) = bootstrap_token {
        store_auth_token(&mut auth, token)?;
        let needs_default_api = auth
            .api_url
            .as_deref()
            .map(|url| url.contains("workers.dev"))
            .unwrap_or(true);
        if needs_default_api {
            auth.api_url = Some(DEFAULT_API_URL.to_string());
        }
        save_auth_config(&auth)?;
    }

    let env_name = cf_cfg
        .environment
        .clone()
        .unwrap_or_else(|| "production".to_string());
    let mut env_key_set: HashSet<String> = HashSet::new();
    for key in cf_cfg.env_keys.iter().chain(cf_cfg.env_vars.iter()) {
        env_key_set.insert(key.clone());
    }
    for (key, value) in &values {
        if env_key_set.contains(key) {
            if let Err(err) = set_project_env_var(key, value, &env_name, None) {
                eprintln!("⚠ Failed to store {} in env store: {}", key, err);
            }
        }
    }

    if generated_env_token.is_some() {
        if auth.token_source.as_deref() == Some("keychain") {
            println!("✓ Saved ENV_API_TOKEN to Keychain");
        } else {
            println!(
                "✓ Saved ENV_API_TOKEN to {}",
                get_auth_config_path().display()
            );
        }
    }

    Ok(())
}

fn prompt_line(label: &str) -> Result<Option<String>> {
    print!("{}", label);
    io::stdout().flush()?;

    let mut value = String::new();
    io::stdin().read_line(&mut value)?;
    let value = value.trim().to_string();
    if value.is_empty() {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

fn prompt_line_default(key: &str, default_value: Option<&str>) -> Result<Option<String>> {
    let label = if let Some(default_value) = default_value {
        format!("{} [{}]: ", key, default_value)
    } else {
        format!("{}: ", key)
    };
    let value = prompt_line(&label)?;
    if value.is_none() {
        Ok(default_value.map(|value| value.to_string()))
    } else {
        Ok(value)
    }
}

fn prompt_value(key: &str, default_value: Option<&str>, secret: bool) -> Result<Option<String>> {
    if secret {
        return prompt_secret(&format!("{}: ", key));
    }

    let default_value = default_value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    });

    let label = if let Some(default_value) = default_value {
        format!("{} [{}]: ", key, default_value)
    } else {
        format!("{}: ", key)
    };

    let value = prompt_line(&label)?;
    if value.is_none() {
        Ok(default_value.map(|value| value.to_string()))
    } else {
        Ok(value)
    }
}

fn prompt_confirm(label: &str) -> Result<bool> {
    print!("{}", label);
    io::stdout().flush()?;

    if std::io::stdin().is_terminal() {
        if let Ok(()) = crossterm::terminal::enable_raw_mode() {
            let read = crossterm::event::read();
            let _ = crossterm::terminal::disable_raw_mode();
            if let Ok(crossterm::event::Event::Key(key)) = read {
                println!();
                return Ok(matches!(
                    key.code,
                    crossterm::event::KeyCode::Char('y' | 'Y')
                ));
            }
        }
    }

    let value = prompt_line("")?;
    Ok(matches!(
        value
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "y" | "yes"
    ))
}

fn generate_env_api_token() -> String {
    format!("cloud_{}", Uuid::new_v4().simple())
}

fn prompt_secret(label: &str) -> Result<Option<String>> {
    let value = rpassword::prompt_password(label)?;
    let value = value.trim().to_string();
    if value.is_empty() {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

fn setup(env_file: Option<PathBuf>, environment: Option<String>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let flow_path = find_flow_toml(&cwd);
    let (project_root, flow_cfg) = if let Some(path) = flow_path {
        let cfg = config::load(&path)?;
        let root = path.parent().unwrap_or(&cwd).to_path_buf();
        (root, Some(cfg))
    } else {
        (cwd, None)
    };

    let cf_cfg = flow_cfg.as_ref().and_then(|cfg| cfg.cloudflare.as_ref());
    let default_env = environment
        .clone()
        .or_else(|| cf_cfg.and_then(|cfg| cfg.environment.clone()));

    if env_file.is_none() {
        if let Some(cfg) = cf_cfg {
            if is_cloud_source(cfg.env_source.as_deref()) {
                let env = default_env.unwrap_or_else(|| "production".to_string());
                return guide(&env);
            }
        }
    }

    let defaults = EnvSetupDefaults {
        env_file,
        environment: default_env,
    };

    let Some(result) = run_env_setup(&project_root, defaults)? else {
        return Ok(());
    };

    if !result.apply {
        println!("Env setup canceled.");
        return Ok(());
    }

    let Some(env_file) = result.env_file else {
        println!("No env file selected; nothing to push.");
        return Ok(());
    };

    let content = fs::read_to_string(&env_file)
        .with_context(|| format!("failed to read {}", env_file.display()))?;
    let vars = parse_env_file(&content);

    if vars.is_empty() {
        println!("No env vars found in {}", env_file.display());
        return Ok(());
    }

    if result.selected_keys.is_empty() {
        println!("No keys selected; nothing to push.");
        return Ok(());
    }

    let mut selected = HashMap::new();
    for key in result.selected_keys {
        if let Some(value) = vars.get(&key) {
            selected.insert(key, value.clone());
        }
    }

    if selected.is_empty() {
        println!("No matching keys found in {}", env_file.display());
        return Ok(());
    }

    push_vars(&result.environment, selected)
}

fn show_keys() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let flow_path = find_flow_toml(&cwd)
        .ok_or_else(|| anyhow::anyhow!("flow.toml not found. Run `f init` first."))?;
    let cfg = config::load(&flow_path)?;
    let cf_cfg = cfg
        .cloudflare
        .as_ref()
        .context("No [cloudflare] section in flow.toml")?;

    let label = resolve_env_target()
        .map(|target| env_target_label(&target))
        .unwrap_or_else(|_| {
            cfg.project_name
                .clone()
                .unwrap_or_else(|| "unknown".to_string())
        });

    println!("Env keys for {}", label);
    println!("─────────────────────────────");
    if let Some(source) = cf_cfg.env_source.as_deref() {
        println!("Source: {}", source);
    }
    if let Some(environment) = cf_cfg.environment.as_deref() {
        println!("Environment: {}", environment);
    }
    if let Some(apply) = cf_cfg.env_apply.as_deref() {
        println!("Apply: {}", apply);
    }
    println!();

    let mut secrets = cf_cfg.env_keys.clone();
    secrets.sort();
    let mut vars = cf_cfg.env_vars.clone();
    vars.sort();

    if secrets.is_empty() && vars.is_empty() {
        println!("No env keys configured.");
        return Ok(());
    }

    if !secrets.is_empty() {
        println!("Secrets:");
        for key in &secrets {
            if cf_cfg.env_defaults.contains_key(key) {
                println!("  {}  (default set)", key);
            } else {
                println!("  {}", key);
            }
        }
        println!();
    }

    if !vars.is_empty() {
        println!("Vars:");
        for key in &vars {
            let default_value = cf_cfg
                .env_defaults
                .get(key)
                .map(|value| format_default_hint(value));
            if let Some(default_value) = default_value {
                println!("  {} = {}", key, default_value);
            } else {
                println!("  {}", key);
            }
        }
        println!();
    }

    let mut extra_defaults: Vec<_> = cf_cfg
        .env_defaults
        .keys()
        .filter(|key| !secrets.contains(*key) && !vars.contains(*key))
        .cloned()
        .collect();
    extra_defaults.sort();

    if !extra_defaults.is_empty() {
        println!("Defaults (not in env_keys/env_vars):");
        for key in extra_defaults {
            if let Some(value) = cf_cfg.env_defaults.get(&key) {
                println!("  {} = {}", key, format_default_hint(value));
            }
        }
    }

    Ok(())
}

/// List env vars for this project.
fn list(environment: &str) -> Result<()> {
    if local_env_enabled() {
        let target = resolve_personal_target()?;
        let label = env_target_label(&target);
        let vars = read_local_env_vars(&target, environment)?;

        println!("Space: {}", label);
        println!("Environment: {}", environment);
        println!("Backend: local");
        println!("─────────────────────────────");

        if vars.is_empty() {
            println!("No env vars set.");
            return Ok(());
        }

        let mut keys: Vec<_> = vars.keys().collect();
        keys.sort();

        for key in keys {
            let value = &vars[key];
            let masked = if value.len() > 8 {
                format!("{}...", &value[..4])
            } else {
                "****".to_string()
            };
            println!("  {} = {}", key, masked);
        }

        println!();
        println!("{} env var(s)", vars.len());
        return Ok(());
    }

    let target = resolve_env_target()?;
    let label = env_target_label(&target);

    let auth = load_auth_config()?;
    let token = auth
        .token
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Not logged in. Run `f env login` first."))?;
    require_env_read_unlock()?;

    let api_url = get_api_url(&auth);
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let url = match &target {
        EnvTarget::Personal { space } => {
            let mut url = Url::parse(&format!("{}/api/env/personal", api_url))?;
            url.query_pairs_mut()
                .append_pair("environment", environment);
            if let Some(space) = space {
                url.query_pairs_mut().append_pair("space", space);
            }
            url
        }
        EnvTarget::Project { name } => {
            let mut url = Url::parse(&format!("{}/api/env/{}", api_url, name))?;
            url.query_pairs_mut()
                .append_pair("environment", environment);
            url
        }
    };
    let resp = client
        .get(url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .context("failed to connect to cloud")?;

    if resp.status() == 401 {
        bail!("Unauthorized. Check your token with `f env login`.");
    }

    if resp.status() == 404 {
        match target {
            EnvTarget::Personal { .. } => bail!("Personal env vars not found."),
            EnvTarget::Project { .. } => bail!("Project '{}' not found.", label),
        }
    }

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        bail!("API error {}: {}", status, body);
    }

    let (vars, descriptions) = match target {
        EnvTarget::Personal { .. } => {
            let data: PersonalEnvResponse = resp.json().context("failed to parse response")?;
            (data.env, None)
        }
        EnvTarget::Project { .. } => {
            let data: EnvResponse = resp.json().context("failed to parse response")?;
            (data.env, Some(data.descriptions))
        }
    };

    println!("Space: {}", label);
    println!("Environment: {}", environment);
    println!("─────────────────────────────");

    if vars.is_empty() {
        println!("No env vars set.");
        return Ok(());
    }

    let mut keys: Vec<_> = vars.keys().collect();
    keys.sort();

    for key in keys {
        let value = &vars[key];
        // Mask the value (show first 4 chars if long enough)
        let masked = if value.len() > 8 {
            format!("{}...", &value[..4])
        } else {
            "****".to_string()
        };

        // Show description if available
        if let Some(desc) = descriptions.as_ref().and_then(|map| map.get(key)) {
            println!("  {} = {}  # {}", key, masked, desc);
        } else {
            println!("  {} = {}", key, masked);
        }
    }

    println!();
    println!("{} env var(s)", vars.len());

    Ok(())
}

/// Set a personal (global) env var.
pub(crate) fn set_personal_env_var(key: &str, value: &str) -> Result<()> {
    if key.is_empty() {
        bail!("Key cannot be empty");
    }

    let target = resolve_personal_target()?;
    let environment = "production";

    if local_env_enabled() {
        let path = set_local_env_var(&target, environment, key, value)?;
        println!(
            "✓ Set personal env var locally: {} (stored at {})",
            key,
            path.display()
        );
        return Ok(());
    }

    let auth = load_auth_config()?;
    let token = match auth.token.as_ref() {
        Some(token) => token,
        None => {
            if std::io::stdin().is_terminal()
                && prompt_confirm("Not logged in to cloud. Store locally instead? (y/N): ")?
            {
                let path = set_local_env_var(&target, environment, key, value)?;
                println!(
                    "✓ Set personal env var locally: {} (stored at {})",
                    key,
                    path.display()
                );
                return Ok(());
            }
            bail!("Not logged in. Run `f env login` first.");
        }
    };

    let api_url = get_api_url(&auth);

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let mut url = Url::parse(&format!("{}/api/env/personal", api_url))?;
    if let EnvTarget::Personal { ref space } = target {
        if let Some(space) = space.as_ref() {
            url.query_pairs_mut().append_pair("space", space);
        }
    }
    let mut vars = HashMap::new();
    vars.insert(key.to_string(), value.to_string());

    let body = serde_json::json!({
        "vars": vars,
    });

    let resp = client
        .post(url)
        .header("Authorization", format!("Bearer {}", token))
        .json(&body)
        .send()
        .context("failed to connect to cloud")?;

    if resp.status() == 401 {
        if std::io::stdin().is_terminal()
            && prompt_confirm("Cloud auth failed. Store locally instead? (y/N): ")?
        {
            let path = set_local_env_var(&target, environment, key, value)?;
            println!(
                "✓ Set personal env var locally: {} (stored at {})",
                key,
                path.display()
            );
            return Ok(());
        }
        bail!("Unauthorized. Check your token with `f env login`.");
    }

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        let err = anyhow::anyhow!("API error {}: {}", status, body);
        if is_local_fallback_error(&err)
            && std::io::stdin().is_terminal()
            && prompt_confirm("Cloud unavailable. Store locally instead? (y/N): ")?
        {
            let path = set_local_env_var(&target, environment, key, value)?;
            println!(
                "✓ Set personal env var locally: {} (stored at {})",
                key,
                path.display()
            );
            return Ok(());
        }
        return Err(err);
    }

    println!("✓ Set personal env var: {}", key);
    Ok(())
}

pub fn set_project_env_var(
    key: &str,
    value: &str,
    environment: &str,
    description: Option<&str>,
) -> Result<()> {
    set_project_env_var_internal(key, value, environment, description)
}

fn set_project_env_var_internal(
    key: &str,
    value: &str,
    environment: &str,
    description: Option<&str>,
) -> Result<()> {
    if key.is_empty() {
        bail!("Key cannot be empty");
    }

    let target = resolve_env_target()?;
    if local_env_enabled() {
        let path = set_local_env_var(&target, environment, key, value)?;
        println!(
            "✓ Set env var locally: {} ({} stored at {})",
            key,
            environment,
            path.display()
        );
        return Ok(());
    }

    let auth = load_auth_config()?;
    let token = match auth.token.as_ref() {
        Some(token) => token,
        None => {
            if std::io::stdin().is_terminal()
                && prompt_confirm("Not logged in to cloud. Store locally instead? (y/N): ")?
            {
                let path = set_local_env_var(&target, environment, key, value)?;
                println!(
                    "✓ Set env var locally: {} ({} stored at {})",
                    key,
                    environment,
                    path.display()
                );
                return Ok(());
            }
            bail!("Not logged in. Run `f env login` first.");
        }
    };

    let api_url = get_api_url(&auth);
    let resolved_value = value.to_string();

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let url = match &target {
        EnvTarget::Personal { space } => {
            let mut url = Url::parse(&format!("{}/api/env/personal", api_url))?;
            if let Some(space) = space {
                url.query_pairs_mut().append_pair("space", space);
            }
            url
        }
        EnvTarget::Project { name } => Url::parse(&format!("{}/api/env/{}", api_url, name))?,
    };
    let mut vars = HashMap::new();
    vars.insert(key.to_string(), resolved_value.clone());

    let mut body = serde_json::json!({
        "vars": vars,
        "environment": environment,
    });

    // Add description if provided
    if let Some(desc) = description {
        let mut descriptions = HashMap::new();
        descriptions.insert(key.to_string(), desc.to_string());
        body["descriptions"] = serde_json::json!(descriptions);
    }

    let resp = client
        .post(url)
        .header("Authorization", format!("Bearer {}", token))
        .json(&body)
        .send()
        .context("failed to connect to cloud")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        let err = anyhow::anyhow!("API error {}: {}", status, body);
        if is_local_fallback_error(&err)
            && std::io::stdin().is_terminal()
            && prompt_confirm("Cloud unavailable. Store locally instead? (y/N): ")?
        {
            let path = set_local_env_var(&target, environment, key, value)?;
            println!(
                "✓ Set env var locally: {} ({} stored at {})",
                key,
                environment,
                path.display()
            );
            return Ok(());
        }
        return Err(err);
    }

    let masked = if resolved_value.len() > 8 {
        format!("{}...", &resolved_value[..4])
    } else {
        "****".to_string()
    };

    if let Some(desc) = description {
        println!("✓ Set {}={} ({}) - {}", key, masked, environment, desc);
    } else {
        println!("✓ Set {}={} ({})", key, masked, environment);
    }

    Ok(())
}

/// Show current auth status.
fn status() -> Result<()> {
    if local_env_enabled() {
        println!("Local Environment Manager");
        println!("─────────────────────────────");
        if let Ok(root) = local_env_root() {
            println!("Root: {}", root.display());
        }
        if let Ok(target) = resolve_env_target() {
            println!("Space: {}", env_target_label(&target));
        }
        println!();
        println!("Commands:");
        println!("  f env list    - List env vars");
        println!("  f env set K=V - Set env var");
        println!("  f env get ... - Read env vars");
        println!("  f env run -- <cmd> - Run with env vars injected");
        println!("  f env keys    - Show configured env keys");
        println!("  f env guide   - Guided env setup from flow.toml");
        return Ok(());
    }

    let auth = load_auth_config_raw()?;

    println!("Cloud Environment Manager");
    println!("─────────────────────────────");

    let api_url = get_api_url(&auth);
    if let Some(ref token) = auth.token {
        let masked = format!("{}...", &token[..7.min(token.len())]);
        println!("Token: {}", masked);
        println!("API:   {}", api_url);
    } else if auth.token_source.as_deref() == Some("keychain") {
        println!("Token: stored in Keychain");
        println!("API:   {}", api_url);
    } else {
        println!("Status: Not logged in");
        println!();
        println!("Run `f env login` to authenticate.");
        return Ok(());
    }

    if let Ok(target) = resolve_env_target() {
        println!("Space: {}", env_target_label(&target));
    }

    println!();
    println!("Commands:");
    println!("  f env sync    - Sync project settings");
    println!("  f env unlock  - Unlock env reads (Touch ID on macOS)");
    println!("  f env pull    - Fetch env vars");
    println!("  f env push    - Push .env to cloud");
    println!("  f env guide   - Guided env setup from flow.toml");
    println!("  f env apply   - Apply cloud envs to Cloudflare");
    println!("  f env bootstrap - Bootstrap Cloudflare secrets");
    println!("  f env setup   - Interactive env setup");
    println!("  f env list    - List env vars");
    println!("  f env keys    - Show configured env keys");
    println!("  f env set K=V - Set env var");

    Ok(())
}

/// Parse a .env file into key-value pairs.
pub(crate) fn parse_env_file(content: &str) -> HashMap<String, String> {
    let mut vars = HashMap::new();

    for line in content.lines() {
        let line = line.trim();

        // Skip empty lines and comments
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Parse KEY=VALUE
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            let value = value.trim();

            // Remove surrounding quotes
            let value = value
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .or_else(|| value.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
                .unwrap_or(value);

            if !key.is_empty() {
                vars.insert(key.to_string(), value.to_string());
            }
        }
    }

    vars
}

/// Fetch env vars from cloud (personal or project).
fn fetch_env_vars(
    target: &EnvTarget,
    environment: &str,
    keys: &[String],
    include_environment: bool,
) -> Result<HashMap<String, String>> {
    if local_env_enabled() {
        return read_local_env_vars(target, environment);
    }

    let auth = load_auth_config()?;
    let token = match auth.token.as_ref() {
        Some(token) => token,
        None => {
            if std::io::stdin().is_terminal()
                && prompt_confirm("Not logged in to cloud. Read local envs instead? (y/N): ")?
            {
                return read_local_env_vars(target, environment);
            }
            bail!("Not logged in. Run `f env login` first.");
        }
    };
    require_env_read_unlock()?;

    let api_url = get_api_url(&auth);
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let mut url = match target {
        EnvTarget::Personal { space } => {
            let mut url = Url::parse(&format!("{}/api/env/personal", api_url))?;
            if include_environment {
                url.query_pairs_mut()
                    .append_pair("environment", environment);
            }
            if let Some(space) = space {
                url.query_pairs_mut().append_pair("space", space);
            }
            url
        }
        EnvTarget::Project { name } => {
            let mut url = Url::parse(&format!("{}/api/env/{}", api_url, name))?;
            url.query_pairs_mut()
                .append_pair("environment", environment);
            url
        }
    };

    if !keys.is_empty() {
        url.query_pairs_mut().append_pair("keys", &keys.join(","));
    }

    let resp = client
        .get(url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .context("failed to connect to cloud")?;

    if resp.status() == 401 {
        if std::io::stdin().is_terminal()
            && prompt_confirm("Cloud auth failed. Read local envs instead? (y/N): ")?
        {
            return read_local_env_vars(target, environment);
        }
        bail!("Unauthorized. Check your token with `f env login`.");
    }

    if resp.status() == 404 {
        match target {
            EnvTarget::Personal { .. } => bail!("Personal env vars not found."),
            EnvTarget::Project { .. } => {
                bail!("Project not found. Create it with `f env push` first.")
            }
        }
    }

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        let err = anyhow::anyhow!("API error {}: {}", status, body);
        if is_local_fallback_error(&err)
            && std::io::stdin().is_terminal()
            && prompt_confirm("Cloud unavailable. Read local envs instead? (y/N): ")?
        {
            return read_local_env_vars(target, environment);
        }
        return Err(err);
    }

    match target {
        EnvTarget::Personal { .. } => {
            let data: PersonalEnvResponse = resp.json().context("failed to parse response")?;
            Ok(data.env)
        }
        EnvTarget::Project { .. } => {
            let data: EnvResponse = resp.json().context("failed to parse response")?;
            Ok(data.env)
        }
    }
}

pub fn fetch_project_env_vars(
    environment: &str,
    keys: &[String],
) -> Result<HashMap<String, String>> {
    let target = resolve_env_target()?;
    fetch_env_vars(&target, environment, keys, true)
}

pub fn fetch_personal_env_vars(keys: &[String]) -> Result<HashMap<String, String>> {
    let target = resolve_personal_target()?;
    fetch_env_vars(&target, "production", keys, false)
}

/// Get specific env vars and print to stdout.
fn get_vars(keys: &[String], personal: bool, environment: &str, format: &str) -> Result<()> {
    let target = if personal {
        resolve_personal_target()?
    } else {
        resolve_env_target()?
    };
    let vars = fetch_env_vars(&target, environment, keys, !personal)?;

    if vars.is_empty() {
        bail!("No env vars found");
    }

    match format {
        "json" => {
            let json = serde_json::to_string_pretty(&vars)?;
            println!("{}", json);
        }
        "value" => {
            if keys.len() != 1 {
                bail!("'value' format requires exactly one key");
            }
            let key = &keys[0];
            if let Some(value) = vars.get(key) {
                print!("{}", value); // No newline for piping
            } else {
                bail!("Key '{}' not found", key);
            }
        }
        "env" | _ => {
            // Default: KEY=VALUE format
            let mut sorted_keys: Vec<_> = vars.keys().collect();
            sorted_keys.sort();
            for key in sorted_keys {
                let value = &vars[key];
                // Escape for shell
                let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
                println!("{}=\"{}\"", key, escaped);
            }
        }
    }

    Ok(())
}

/// Run a command with env vars injected from cloud.
fn run_with_env(
    personal: bool,
    environment: &str,
    keys: &[String],
    command: &[String],
) -> Result<()> {
    if command.is_empty() {
        bail!("No command specified");
    }

    let target = if personal {
        resolve_personal_target()?
    } else {
        resolve_env_target()?
    };
    let vars = fetch_env_vars(&target, environment, keys, !personal)?;

    let (cmd, args) = command.split_first().unwrap();

    let mut child = Command::new(cmd);
    child.args(args);

    // Inject env vars
    for (key, value) in &vars {
        child.env(key, value);
    }

    let status = child
        .status()
        .with_context(|| format!("failed to run '{}'", cmd))?;

    std::process::exit(status.code().unwrap_or(1));
}

// =============================================================================
// Service Token Management
// =============================================================================

#[derive(Debug, Deserialize)]
struct CreateTokenResponse {
    #[allow(dead_code)]
    success: bool,
    token: String,
    #[serde(rename = "projectName")]
    project_name: String,
    name: String,
    permissions: String,
}

#[derive(Debug, Deserialize)]
struct TokenEntry {
    name: String,
    #[serde(rename = "projectName")]
    project_name: String,
    permissions: String,
    #[serde(rename = "createdAt")]
    #[allow(dead_code)]
    created_at: Option<String>,
    #[serde(rename = "lastUsedAt")]
    last_used_at: Option<String>,
    revoked: bool,
}

#[derive(Debug, Deserialize)]
struct ListTokensResponse {
    tokens: Vec<TokenEntry>,
}

/// Create a new service token for the current project.
fn token_create(name: Option<&str>, permissions: &str) -> Result<()> {
    let auth = load_auth_config()?;
    let token = auth
        .token
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Not logged in. Run `f env login` first."))?;

    let target = resolve_env_target()?;
    let project = env_target_name_for_tokens(&target)?;
    let default_name = format!("{}-service", project);
    let token_name = name.unwrap_or(&default_name);
    let api_url = get_api_url(&auth);

    println!("Creating service token for '{}'...", project);

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let url = format!("{}/api/env/tokens", api_url);
    let body = serde_json::json!({
        "projectName": project,
        "name": token_name,
        "permissions": permissions,
    });

    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", token))
        .json(&body)
        .send()
        .context("failed to connect to cloud")?;

    if resp.status() == 401 {
        bail!("Unauthorized. Check your token with `f env login`.");
    }

    if resp.status() == 403 {
        bail!("You don't own this project.");
    }

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        bail!("API error {}: {}", status, body);
    }

    let data: CreateTokenResponse = resp.json().context("failed to parse response")?;

    println!();
    println!("✓ Service token created!");
    println!();
    println!("Token:       {}", data.token);
    println!("Project:     {}", data.project_name);
    println!("Name:        {}", data.name);
    println!("Permissions: {}", data.permissions);
    println!();
    println!("IMPORTANT: Save this token now. It won't be shown again.");
    println!();
    println!(
        "This token can ONLY access env vars for '{}'.",
        data.project_name
    );
    println!("If the host is compromised, revoke it with:");
    println!("  f env token revoke {}", data.name);

    Ok(())
}

/// List service tokens for the current user.
fn token_list() -> Result<()> {
    let auth = load_auth_config()?;
    let token = auth
        .token
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Not logged in. Run `f env login` first."))?;

    let api_url = get_api_url(&auth);

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let url = format!("{}/api/env/tokens", api_url);

    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .context("failed to connect to cloud")?;

    if resp.status() == 401 {
        bail!("Unauthorized. Check your token with `f env login`.");
    }

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        bail!("API error {}: {}", status, body);
    }

    let data: ListTokensResponse = resp.json().context("failed to parse response")?;

    if data.tokens.is_empty() {
        println!("No service tokens found.");
        println!();
        println!("Create one with: f env token create");
        return Ok(());
    }

    println!("Service Tokens");
    println!("─────────────────────────────");

    for entry in &data.tokens {
        let status = if entry.revoked { " (revoked)" } else { "" };
        println!(
            "  {} → {} [{}]{}",
            entry.name, entry.project_name, entry.permissions, status
        );
        if let Some(last_used) = &entry.last_used_at {
            println!("    Last used: {}", last_used);
        }
    }

    println!();
    println!("{} token(s)", data.tokens.len());

    Ok(())
}

/// Revoke a service token.
fn token_revoke(name: &str) -> Result<()> {
    let auth = load_auth_config()?;
    let token = auth
        .token
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Not logged in. Run `f env login` first."))?;

    let target = resolve_env_target()?;
    let project = env_target_name_for_tokens(&target)?;
    let api_url = get_api_url(&auth);

    println!("Revoking token '{}' for project '{}'...", name, project);

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let url = format!("{}/api/env/tokens", api_url);
    let body = serde_json::json!({
        "name": name,
        "projectName": project,
    });

    let resp = client
        .delete(&url)
        .header("Authorization", format!("Bearer {}", token))
        .json(&body)
        .send()
        .context("failed to connect to cloud")?;

    if resp.status() == 401 {
        bail!("Unauthorized. Check your token with `f env login`.");
    }

    if resp.status() == 404 {
        bail!("Token '{}' not found for project '{}'.", name, project);
    }

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        bail!("API error {}: {}", status, body);
    }

    println!("✓ Token '{}' revoked.", name);
    println!();
    println!("Any host using this token will no longer be able to fetch env vars.");

    Ok(())
}
