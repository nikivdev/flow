use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STD};
use reqwest;

use crate::cli::HealthOpts;
use crate::config;
use crate::doctor;
use crate::env as flow_env;
use crate::setup::add_gitignore_entry;

pub fn run(_opts: HealthOpts) -> Result<()> {
    println!("Running flow health checks...\n");

    ensure_fish_shell()?;
    ensure_fish_flow_init()?;
    ensure_gitignore()?;

    doctor::run(crate::cli::DoctorOpts {})?;
    ensure_ai_server()?;
    ensure_unhash()?;
    ensure_rise_health()?;
    ensure_linsa_base_health()?;
    ensure_zerg_ai_health()?;

    println!("\n✅ flow health checks passed.");
    Ok(())
}

fn ensure_fish_shell() -> Result<()> {
    let shell = env::var("SHELL").unwrap_or_default();
    if !shell.contains("fish") {
        let fish = which::which("fish")
            .context("fish is required; install it and ensure it is on PATH")?;
        bail!("fish shell required. Run:\n  chsh -s {}", fish.display());
    }
    Ok(())
}

fn ensure_fish_flow_init() -> Result<()> {
    let config_path = fish_config_path()?;
    let content = fs::read_to_string(&config_path).unwrap_or_default();
    if content.contains("# flow:start") {
        return Ok(());
    }

    println!(
        "⚠ flow fish integration missing in {}. Run: f shell-init fish",
        config_path.display()
    );
    Ok(())
}

fn ensure_gitignore() -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let Some(flow_path) = find_flow_toml_upwards(&cwd) else {
        return Ok(());
    };
    let root = flow_path.parent().unwrap_or(&cwd);

    if !root.join(".git").exists() {
        return Ok(());
    }

    add_gitignore_entry(root, ".ai/todos/*.bike")?;
    add_gitignore_entry(root, ".ai/review-log.jsonl")?;
    Ok(())
}

fn ensure_ai_server() -> Result<()> {
    let keys = config::global_env_keys();
    let mut resolved: HashMap<String, String> = HashMap::new();
    let mut missing = Vec::new();

    for key in &keys {
        if let Ok(value) = env::var(key) {
            if !value.trim().is_empty() {
                resolved.insert(key.clone(), value);
                continue;
            }
        }
        missing.push(key.clone());
    }

    if !missing.is_empty() {
        if let Ok(vars) = flow_env::fetch_personal_env_vars(&missing) {
            for (key, value) in vars {
                if !value.trim().is_empty() {
                    resolved.insert(key, value);
                }
            }
        }
    }

    let url = resolved.get("AI_SERVER_URL").cloned().unwrap_or_default();
    let model = resolved
        .get("AI_SERVER_MODEL")
        .cloned()
        .unwrap_or_default();
    let token = resolved
        .get("AI_SERVER_TOKEN")
        .cloned()
        .unwrap_or_default();

    if url.trim().is_empty() {
        println!("⚠️  AI server env not configured (AI_SERVER_URL).");
        println!("   Set it once: f env set --personal AI_SERVER_URL=http://127.0.0.1:7331");
        return Ok(());
    }

    let base = base_ai_url(&url);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(800))
        .build()
        .context("failed to create http client")?;

    let health_url = format!("{}/health", base);
    let mut ok = client
        .get(&health_url)
        .send()
        .map(|resp| resp.status().is_success())
        .unwrap_or(false);

    if !ok {
        let models_url = format!("{}/v1/models", base);
        ok = client
            .get(&models_url)
            .send()
            .map(|resp| resp.status().is_success())
            .unwrap_or(false);
    }

    if ok {
        println!("✅ AI server reachable at {}", base);
    } else {
        println!("⚠️  AI server not reachable at {}", base);
        println!("   Start it with your ai server repo (e.g. f daemon start ai-server).");
    }

    if model.trim().is_empty() {
        println!("⚠️  AI_SERVER_MODEL not set. Example: f env set --personal AI_SERVER_MODEL=zai-glm-4.7");
    }

    if token.trim().is_empty() {
        println!("ℹ️  AI_SERVER_TOKEN not set (ok if server is open).");
    }

    Ok(())
}

fn ensure_unhash() -> Result<()> {
    match which::which("unhash") {
        Ok(path) => {
            println!("✅ unhash binary found at {}", path.display());
        }
        Err(_) => {
            println!("⚠️  unhash not found on PATH.");
            println!("   Install with: cd ~/code/unhash && f deploy");
            return Ok(());
        }
    }

    let key = env::var("UNHASH_KEY").ok().filter(|v| !v.trim().is_empty());
    let key = match key {
        Some(value) => Some(value),
        None => flow_env::get_personal_env_var("UNHASH_KEY").ok().flatten(),
    };

    match key {
        Some(value) => {
            if is_valid_unhash_key(&value) {
                println!("✅ UNHASH_KEY configured (env or flow env)");
            } else {
                println!("⚠️  UNHASH_KEY is invalid (expected 32-byte base64 or hex).");
                println!("   Fix with: unhash keygen | f env set UNHASH_KEY=...");
            }
        }
        None => {
            println!("⚠️  UNHASH_KEY not set.");
            println!("   Run: unhash health --setup");
        }
    }

    Ok(())
}

fn ensure_rise_health() -> Result<()> {
    let rise_bin = match which::which("rise") {
        Ok(path) => path,
        Err(_) => {
            println!("ℹ️  rise not installed; skipping.");
            return Ok(());
        }
    };

    let rise_root = config::expand_path("~/code/rise");
    if !rise_root.exists() {
        println!("ℹ️  rise repo not found at {}; skipping.", rise_root.display());
        return Ok(());
    }

    let supports_health = Command::new(&rise_bin)
        .arg("help")
        .output()
        .ok()
        .and_then(|output| {
            let mut combined = String::from_utf8_lossy(&output.stdout).to_string();
            combined.push_str(&String::from_utf8_lossy(&output.stderr));
            Some(combined.contains("rise health"))
        })
        .unwrap_or(false);

    if !supports_health {
        println!("ℹ️  rise health not available; skipping.");
        return Ok(());
    }

    let status = Command::new(&rise_bin)
        .arg("health")
        .current_dir(&rise_root)
        .status();

    match status {
        Ok(status) if status.success() => {
            println!("✅ rise health ok");
        }
        Ok(status) => {
            println!("⚠️  rise health failed (exit {}).", status.code().unwrap_or(-1));
        }
        Err(err) => {
            println!("⚠️  failed to run rise health: {}", err);
        }
    }

    Ok(())
}

fn ensure_linsa_base_health() -> Result<()> {
    let base_root = config::expand_path("~/code/org/linsa/base");
    if !base_root.exists() {
        println!("ℹ️  ~/code/org/linsa/base not installed; skipping.");
        return Ok(());
    }

    if base_root.join("flow.toml").exists() {
        println!("✅ linsa/base found at {}", base_root.display());
    } else {
        println!("⚠️  linsa/base found but flow.toml missing: {}", base_root.display());
    }

    Ok(())
}

fn ensure_zerg_ai_health() -> Result<()> {
    let zerg_root = config::expand_path("~/code/zerg/ai");
    if !zerg_root.exists() {
        println!("ℹ️  ~/code/zerg/ai not installed; skipping.");
        return Ok(());
    }

    let url = env::var("ZERG_AI_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| env::var("AI_SERVER_URL").ok().filter(|v| !v.trim().is_empty()))
        .unwrap_or_else(|| "http://127.0.0.1:7331".to_string());

    let base = base_ai_url(&url);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(800))
        .build()
        .context("failed to create http client")?;

    let health_url = format!("{}/health", base);
    let ok = client
        .get(&health_url)
        .send()
        .map(|resp| resp.status().is_success())
        .unwrap_or(false);

    if ok {
        println!("✅ zerg/ai reachable at {}", base);
    } else {
        println!("⚠️  zerg/ai not reachable at {}", base);
    }

    Ok(())
}

fn is_valid_unhash_key(raw: &str) -> bool {
    if let Ok(bytes) = BASE64_STD.decode(raw.trim()) {
        if bytes.len() == 32 {
            return true;
        }
    }
    if let Some(bytes) = decode_hex(raw.trim()) {
        if bytes.len() == 32 {
            return true;
        }
    }
    false
}

fn decode_hex(input: &str) -> Option<Vec<u8>> {
    let bytes = input.as_bytes();
    if bytes.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut i = 0;
    while i < bytes.len() {
        let hi = hex_value(bytes[i])?;
        let lo = hex_value(bytes[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Some(out)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn base_ai_url(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    if let Some(idx) = trimmed.find("/v1/") {
        return trimmed[..idx].to_string();
    }
    trimmed.to_string()
}

fn find_flow_toml_upwards(start: &PathBuf) -> Option<PathBuf> {
    let mut current = start.as_path();
    loop {
        let candidate = current.join("flow.toml");
        if candidate.exists() {
            return Some(candidate);
        }
        current = current.parent()?;
    }
}

fn fish_config_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("failed to resolve home directory")?;
    Ok(home.join("config").join("fish").join("config.fish"))
}
