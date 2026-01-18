use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use reqwest::blocking::Client;
use serde::Deserialize;

use crate::cli::AuthOpts;
use crate::env;

#[derive(Debug, Deserialize)]
struct DeviceStartResponse {
    device_code: String,
    user_code: String,
    verification_url: String,
    expires_in: u64,
    #[serde(default = "default_poll_interval")]
    interval: u64,
}

fn default_poll_interval() -> u64 {
    2
}

#[derive(Debug, Deserialize)]
struct DevicePollResponse {
    status: String,
    token: Option<String>,
}

pub fn run(opts: AuthOpts) -> Result<()> {
    login(opts.api_url)
}

fn login(api_url_override: Option<String>) -> Result<()> {
    let api_url = api_url_override
        .or_else(|| env::load_ai_api_url().ok())
        .unwrap_or_else(|| "https://myflow.sh".to_string());
    let api_url = api_url.trim().trim_end_matches('/').to_string();

    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to create HTTP client for auth")?;

    let start_url = format!("{}/api/auth/cli/start", api_url);
    let response = client
        .post(&start_url)
        .json(&serde_json::json!({"client": "flow"}))
        .send()
        .context("failed to start device auth")?;

    if !response.status().is_success() {
        bail!("device auth start failed: HTTP {}", response.status());
    }

    let payload: DeviceStartResponse = response
        .json()
        .context("failed to parse device auth response")?;

    println!("\nFlow auth with myflow");
    println!("───────────────────────");
    println!("Code: {}", payload.user_code);
    println!("Open: {}\n", payload.verification_url);

    open_in_browser(&payload.verification_url);

    let expires_at = Instant::now() + Duration::from_secs(payload.expires_in);
    let poll_url = format!("{}/api/auth/cli/poll", api_url);

    println!("Waiting for approval...");

    while Instant::now() < expires_at {
        sleep(Duration::from_secs(payload.interval.max(1)));

        let poll_response = client
            .post(&poll_url)
            .json(&serde_json::json!({"device_code": payload.device_code}))
            .send()
            .context("failed to poll device auth")?;

        if !poll_response.status().is_success() {
            bail!("device auth poll failed: HTTP {}", poll_response.status());
        }

        let poll: DevicePollResponse = poll_response
            .json()
            .context("failed to parse device auth poll response")?;

        match poll.status.as_str() {
            "approved" => {
                let token = poll
                    .token
                    .ok_or_else(|| anyhow!("device auth approved without token"))?;
                env::save_ai_auth_token(token, Some(api_url.clone()))?;
                println!("✓ Auth complete. You're ready to use Flow AI.");
                return Ok(());
            }
            "pending" => continue,
            "expired" => bail!("device code expired. Run `f auth` again."),
            "invalid" => bail!("device code invalid. Run `f auth` again."),
            other => bail!("unexpected auth status: {}", other),
        }
    }

    bail!("device code expired. Run `f auth` again.")
}

fn open_in_browser(url: &str) {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(url).status();
    }

    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).status();
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    println!("Open this URL in your browser: {}", url);
}
