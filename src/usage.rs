use std::collections::{BTreeSet, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use hmac::{Hmac, Mac};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;
use uuid::Uuid;

use crate::config;

const DEFAULT_ANALYTICS_ENDPOINT: &str = "http://127.0.0.1:7331/v1/trace";
const QUEUE_FILE_NAME: &str = "usage-queue.jsonl";
const STATE_FILE_NAME: &str = "analytics.toml";
const MAX_QUEUE_BYTES: usize = 10 * 1024 * 1024;
const MAX_BATCH_SIZE: usize = 100;

static FLUSH_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AnalyticsConsent {
    Unknown,
    Enabled,
    Disabled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalyticsState {
    pub consent: AnalyticsConsent,
    pub install_id: String,
    pub local_secret: String,
    pub prompted_at_ms: Option<u64>,
    pub updated_at_ms: u64,
}

impl AnalyticsState {
    fn new_unknown() -> Self {
        Self {
            consent: AnalyticsConsent::Unknown,
            install_id: Uuid::new_v4().to_string(),
            local_secret: Uuid::new_v4().to_string(),
            prompted_at_ms: None,
            updated_at_ms: now_ms(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AnalyticsRuntimeConfig {
    pub enabled: Option<bool>,
    pub endpoint: String,
    pub sample_rate: f32,
}

#[derive(Debug, Clone)]
pub struct CommandCapture {
    pub at_ms: u64,
    pub command_path: String,
    pub flags_used: Vec<String>,
    pub interactive: bool,
    pub ci: bool,
    pub flow_version: String,
    pub os: String,
    pub arch: String,
}

#[derive(Debug, Clone)]
pub struct AnalyticsStatus {
    pub consent: AnalyticsConsent,
    pub effective_enabled: bool,
    pub install_id: String,
    pub endpoint: String,
    pub queue_path: PathBuf,
    pub queued_events: usize,
}

pub fn command_capture(raw_args: &[String]) -> CommandCapture {
    CommandCapture {
        at_ms: now_ms(),
        command_path: command_path(raw_args),
        flags_used: extract_flags(raw_args),
        interactive: io::stdin().is_terminal() && io::stdout().is_terminal(),
        ci: env_flag("CI"),
        flow_version: env!("CARGO_PKG_VERSION").to_string(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
    }
}

pub fn is_analytics_command(raw_args: &[String]) -> bool {
    if let Some(command) = raw_args.iter().skip(1).find(|arg| !arg.starts_with('-')) {
        return command == "analytics";
    }
    command_path(raw_args).starts_with("analytics")
}

pub fn maybe_prompt_for_opt_in(is_analytics_command: bool, succeeded: bool) {
    if !succeeded || is_analytics_command {
        return;
    }
    if env_flag("FLOW_ANALYTICS_DISABLE") || env_flag("FLOW_ANALYTICS_FORCE") {
        return;
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return;
    }

    let mut state = match load_or_init_state() {
        Ok(state) => state,
        Err(_) => return,
    };

    if state.consent != AnalyticsConsent::Unknown || state.prompted_at_ms.is_some() {
        return;
    }

    print!("Enable anonymous usage tracking to improve Flow? [y/N/later]: ");
    let _ = io::stdout().flush();
    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_err() {
        return;
    }
    let answer = input.trim().to_ascii_lowercase();
    match answer.as_str() {
        "y" | "yes" => {
            state.consent = AnalyticsConsent::Enabled;
            state.prompted_at_ms = Some(now_ms());
            state.updated_at_ms = now_ms();
            let _ = save_state(&state);
            println!("Anonymous usage tracking enabled.");
        }
        "later" => {
            state.prompted_at_ms = Some(now_ms());
            state.updated_at_ms = now_ms();
            let _ = save_state(&state);
            println!("You can enable later with: f analytics enable");
        }
        _ => {
            state.consent = AnalyticsConsent::Disabled;
            state.prompted_at_ms = Some(now_ms());
            state.updated_at_ms = now_ms();
            let _ = save_state(&state);
            println!("Anonymous usage tracking disabled.");
        }
    }
}

pub fn record_command_result(capture: &CommandCapture, duration: Duration, result: &Result<()>) {
    let runtime_cfg = runtime_config();
    let mut state = match load_or_init_state() {
        Ok(state) => state,
        Err(_) => return,
    };

    if !should_capture(&state, &runtime_cfg) {
        return;
    }

    if state.install_id.is_empty() {
        state.install_id = Uuid::new_v4().to_string();
        let _ = save_state(&state);
    }

    let event = json!({
        "type": "flow.command",
        "name": capture.command_path,
        "ok": result.is_ok(),
        "at": capture.at_ms,
        "source": "flow-cli",
        "payload": {
            "install_id": state.install_id,
            "command_path": capture.command_path,
            "success": result.is_ok(),
            "exit_code": Option::<i32>::None,
            "duration_ms": duration.as_millis().min(u64::MAX as u128) as u64,
            "flags_used": capture.flags_used,
            "flow_version": capture.flow_version,
            "os": capture.os,
            "arch": capture.arch,
            "interactive": capture.interactive,
            "ci": capture.ci,
            "project_fingerprint": project_fingerprint(&state.local_secret),
        }
    });

    if append_event_to_queue(&event).is_err() {
        return;
    }
    spawn_flush_worker(runtime_cfg.endpoint);
}

pub fn status() -> Result<AnalyticsStatus> {
    let state = load_or_init_state()?;
    let runtime_cfg = runtime_config();
    let queue_path = queue_path();
    let queued_events = read_queue_lines()?.len();
    Ok(AnalyticsStatus {
        consent: state.consent,
        effective_enabled: should_capture(&state, &runtime_cfg),
        install_id: state.install_id,
        endpoint: runtime_cfg.endpoint,
        queue_path,
        queued_events,
    })
}

pub fn set_consent(consent: AnalyticsConsent) -> Result<()> {
    let mut state = load_or_init_state()?;
    state.consent = consent;
    state.updated_at_ms = now_ms();
    if state.prompted_at_ms.is_none() {
        state.prompted_at_ms = Some(now_ms());
    }
    save_state(&state)
}

pub fn export_queue() -> Result<String> {
    let path = queue_path();
    if !path.exists() {
        return Ok(String::new());
    }
    fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))
}

pub fn purge_queue() -> Result<()> {
    let path = queue_path();
    if path.exists() {
        fs::remove_file(&path).with_context(|| format!("failed to remove {}", path.display()))?;
    }
    Ok(())
}

fn command_path(raw_args: &[String]) -> String {
    let known_commands: HashSet<&'static str> = HashSet::from([
        "search",
        "global",
        "hub",
        "init",
        "shell-init",
        "shell",
        "new",
        "home",
        "archive",
        "doctor",
        "health",
        "tasks",
        "run",
        "last-cmd",
        "last-cmd-full",
        "fish-last",
        "fish-last-full",
        "fish-install",
        "rerun",
        "ps",
        "kill",
        "logs",
        "trace",
        "projects",
        "sessions",
        "active",
        "server",
        "web",
        "match",
        "ask",
        "commit",
        "commit-queue",
        "pr",
        "gitignore",
        "review",
        "commitSimple",
        "commitWithCheck",
        "undo",
        "fix",
        "fixup",
        "changes",
        "diff",
        "hash",
        "daemon",
        "supervisor",
        "ai",
        "codex",
        "claude",
        "env",
        "otp",
        "auth",
        "services",
        "push",
        "ssh",
        "macos",
        "todo",
        "ext",
        "deps",
        "skills",
        "db",
        "tools",
        "notify",
        "commits",
        "setup",
        "agents",
        "hive",
        "sync",
        "checkout",
        "switch",
        "info",
        "upstream",
        "deploy",
        "prod",
        "publish",
        "repos",
        "code",
        "migrate",
        "parallel",
        "docs",
        "upgrade",
        "latest",
        "release",
        "install",
        "registry",
        "proxy",
        "analytics",
    ]);

    let mut parts = Vec::new();
    for arg in raw_args.iter().skip(1) {
        if arg == "--" {
            break;
        }
        if arg.starts_with('-') {
            continue;
        }
        parts.push(arg.as_str());
    }

    if parts.is_empty() {
        return "palette".to_string();
    }

    let first = parts[0];
    if !known_commands.contains(first) {
        return "task-shortcut".to_string();
    }

    let command_with_actions: HashSet<&'static str> = HashSet::from([
        "skills",
        "analytics",
        "ai",
        "trace",
        "proxy",
        "daemon",
        "env",
        "services",
        "todo",
        "ext",
        "deps",
        "tools",
        "agents",
        "hive",
        "sync",
        "release",
        "install",
        "registry",
    ]);

    if command_with_actions.contains(first) && parts.len() > 1 {
        let second = parts[1];
        if second != "force" && second != "review" && !second.starts_with('-') {
            return format!("{}.{}", first, second);
        }
    }

    first.to_string()
}

fn extract_flags(raw_args: &[String]) -> Vec<String> {
    let mut set = BTreeSet::new();
    for arg in raw_args.iter().skip(1) {
        if arg == "--" {
            break;
        }
        if let Some(rest) = arg.strip_prefix("--") {
            if rest.is_empty() {
                continue;
            }
            let name = rest.split('=').next().unwrap_or_default().trim();
            if !name.is_empty() {
                set.insert(name.to_string());
            }
            continue;
        }
        if let Some(rest) = arg.strip_prefix('-') {
            if rest.is_empty() || rest.starts_with('-') {
                continue;
            }
            for ch in rest.chars() {
                if ch.is_ascii_alphanumeric() {
                    set.insert(ch.to_string());
                }
            }
        }
    }
    set.into_iter().collect()
}

fn project_fingerprint(secret: &str) -> Option<String> {
    if secret.is_empty() {
        return None;
    }
    let cwd = std::env::current_dir().ok()?;
    let canonical = cwd.canonicalize().unwrap_or(cwd);
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(canonical.to_string_lossy().as_bytes());
    let bytes = mac.finalize().into_bytes();
    let full = hex::encode(bytes);
    Some(full.chars().take(16).collect())
}

fn should_capture(state: &AnalyticsState, runtime: &AnalyticsRuntimeConfig) -> bool {
    if env_flag("FLOW_ANALYTICS_DISABLE") {
        return false;
    }
    if env_flag("FLOW_ANALYTICS_FORCE") {
        return true;
    }
    if let Some(enabled) = runtime.enabled {
        return enabled;
    }
    state.consent == AnalyticsConsent::Enabled
}

fn runtime_config() -> AnalyticsRuntimeConfig {
    let mut enabled = None;
    let mut endpoint = DEFAULT_ANALYTICS_ENDPOINT.to_string();
    let mut sample_rate = 1.0f32;

    if let Some(cfg) = load_project_analytics_config() {
        enabled = cfg.enabled;
        if let Some(v) = cfg.endpoint {
            if !v.trim().is_empty() {
                endpoint = v.trim().to_string();
            }
        }
        if let Some(v) = cfg.sample_rate {
            sample_rate = v.clamp(0.0, 1.0);
        }
    }

    if sample_rate < 1.0 {
        let random = (now_ms() % 10_000) as f32 / 10_000.0;
        if random > sample_rate {
            enabled = Some(false);
        }
    }

    AnalyticsRuntimeConfig {
        enabled,
        endpoint,
        sample_rate,
    }
}

fn load_project_analytics_config() -> Option<config::AnalyticsConfig> {
    let mut current = std::env::current_dir().ok()?;
    loop {
        let candidate = current.join("flow.toml");
        if candidate.exists() {
            let cfg = config::load_or_default(&candidate);
            return cfg.analytics;
        }
        if !current.pop() {
            break;
        }
    }

    let global = config::default_config_path();
    if global.exists() {
        return config::load_or_default(global).analytics;
    }

    None
}

fn load_or_init_state() -> Result<AnalyticsState> {
    let path = state_path();
    if path.exists() {
        let contents = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let mut state: AnalyticsState = toml::from_str(&contents)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        if state.install_id.trim().is_empty() {
            state.install_id = Uuid::new_v4().to_string();
            state.updated_at_ms = now_ms();
            save_state(&state)?;
        }
        if state.local_secret.trim().is_empty() {
            state.local_secret = Uuid::new_v4().to_string();
            state.updated_at_ms = now_ms();
            save_state(&state)?;
        }
        return Ok(state);
    }

    let state = AnalyticsState::new_unknown();
    save_state(&state)?;
    Ok(state)
}

fn save_state(state: &AnalyticsState) -> Result<()> {
    let path = state_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let payload = toml::to_string_pretty(state).context("failed to encode analytics state")?;
    fs::write(&path, payload).with_context(|| format!("failed to write {}", path.display()))
}

fn state_path() -> PathBuf {
    config::global_config_dir().join(STATE_FILE_NAME)
}

fn queue_path() -> PathBuf {
    config::global_state_dir().join(QUEUE_FILE_NAME)
}

fn append_event_to_queue(event: &Value) -> Result<()> {
    let path = queue_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    let line = serde_json::to_string(event).context("failed to encode analytics event")?;
    writeln!(file, "{line}").with_context(|| format!("failed to append {}", path.display()))?;
    enforce_queue_limit(&path)?;
    Ok(())
}

fn enforce_queue_limit(path: &PathBuf) -> Result<()> {
    let metadata =
        fs::metadata(path).with_context(|| format!("failed to stat {}", path.display()))?;
    if metadata.len() as usize <= MAX_QUEUE_BYTES {
        return Ok(());
    }

    let lines = read_queue_lines()?;
    if lines.is_empty() {
        return Ok(());
    }
    let keep = lines.len().saturating_sub(lines.len() / 4).max(1);
    let trimmed: String = lines
        .into_iter()
        .rev()
        .take(keep)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|line| format!("{line}\n"))
        .collect();
    fs::write(path, trimmed).with_context(|| format!("failed to trim {}", path.display()))
}

fn spawn_flush_worker(endpoint: String) {
    if FLUSH_IN_PROGRESS
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }

    std::thread::spawn(move || {
        let _ = flush_queue(&endpoint);
        FLUSH_IN_PROGRESS.store(false, Ordering::SeqCst);
    });
}

fn flush_queue(endpoint: &str) -> Result<()> {
    let path = queue_path();
    if !path.exists() {
        return Ok(());
    }

    let lines = read_queue_lines()?;
    if lines.is_empty() {
        return Ok(());
    }

    let client = Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
        .context("failed to build analytics HTTP client")?;

    let mut sent = 0usize;
    for line in lines.iter().take(MAX_BATCH_SIZE) {
        let value: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                sent += 1;
                continue;
            }
        };
        let response = client
            .post(endpoint)
            .header("content-type", "application/json")
            .json(&value)
            .send();
        match response {
            Ok(resp) if resp.status().is_success() => {
                sent += 1;
            }
            _ => break,
        }
    }

    if sent == 0 {
        return Ok(());
    }

    let remaining: String = lines
        .into_iter()
        .skip(sent)
        .map(|line| format!("{line}\n"))
        .collect();
    fs::write(&path, remaining).with_context(|| format!("failed to rewrite {}", path.display()))
}

fn read_queue_lines() -> Result<Vec<String>> {
    let path = queue_path();
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.to_string())
        .collect())
}

fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_flags_without_values() {
        let args = vec![
            "f".to_string(),
            "commit".to_string(),
            "--sync".to_string(),
            "-nt".to_string(),
            "--message=hello".to_string(),
            "arg".to_string(),
        ];
        let flags = extract_flags(&args);
        assert!(flags.contains(&"sync".to_string()));
        assert!(flags.contains(&"n".to_string()));
        assert!(flags.contains(&"t".to_string()));
        assert!(flags.contains(&"message".to_string()));
        assert!(!flags.contains(&"hello".to_string()));
    }

    #[test]
    fn unknown_commands_map_to_task_shortcut() {
        let args = vec![
            "f".to_string(),
            "dev".to_string(),
            "--port".to_string(),
            "3000".to_string(),
        ];
        assert_eq!(command_path(&args), "task-shortcut");
    }
}
