//! Notify command - sends proposals and alerts to Lin app.

use crate::cli::NotifyCommand;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// Proposal format matching Lin's ProposalService.swift
#[derive(Debug, Serialize, Deserialize)]
struct Proposal {
    id: String,
    timestamp: i64,
    title: String,
    action: String,
    context: Option<String>,
    #[serde(rename = "expires_at")]
    expires_at: i64,
}

/// Alert format for Lin's NotificationBannerManager.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Alert {
    id: String,
    timestamp: i64,
    text: String,
    kind: String, // "info", "warning", "error", "success"
    #[serde(rename = "expires_at")]
    expires_at: i64,
}

/// Get the path to Lin's proposals.json file.
fn get_proposals_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Could not find home directory")?;
    let path = home
        .join("Library")
        .join("Application Support")
        .join("Lin")
        .join("proposals.json");
    Ok(path)
}

/// Run the notify command - write a proposal to Lin's proposals.json.
pub fn run(cmd: NotifyCommand) -> Result<()> {
    let proposals_path = get_proposals_path()?;

    // Ensure the directory exists
    if let Some(parent) = proposals_path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Read existing proposals
    let mut proposals: Vec<Proposal> = if proposals_path.exists() {
        let content = fs::read_to_string(&proposals_path)?;
        serde_json::from_str(&content).unwrap_or_default()
    } else {
        Vec::new()
    };

    // Get current timestamp
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("Time went backwards")?
        .as_secs() as i64;

    // Create title from action if not provided
    let title = cmd.title.unwrap_or_else(|| {
        // Extract a nice title from the action
        let action = &cmd.action;
        if action.starts_with("f ") {
            format!("Run: {}", &action[2..])
        } else {
            format!("Run: {}", action)
        }
    });

    // Create new proposal
    let proposal = Proposal {
        id: Uuid::new_v4().to_string(),
        timestamp: now,
        title,
        action: cmd.action.clone(),
        context: cmd.context,
        expires_at: now + cmd.expires as i64,
    };

    // Add to proposals
    proposals.push(proposal);

    // Write back
    let content = serde_json::to_string_pretty(&proposals)?;
    fs::write(&proposals_path, content)?;

    println!("Proposal sent to Lin: {}", cmd.action);

    Ok(())
}

// ============================================================================
// Alerts API (for commit rejections, errors, etc.)
// ============================================================================

/// Get the path to Lin's alerts.json file.
fn get_alerts_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Could not find home directory")?;
    let path = home
        .join("Library")
        .join("Application Support")
        .join("Lin")
        .join("alerts.json");
    Ok(path)
}

/// Alert kind for Lin's NotificationBannerManager.
#[derive(Debug, Clone, Copy)]
pub enum AlertKind {
    Info,
    Warning,
    Error,
    Success,
}

impl AlertKind {
    fn as_str(&self) -> &'static str {
        match self {
            AlertKind::Info => "info",
            AlertKind::Warning => "warning",
            AlertKind::Error => "error",
            AlertKind::Success => "success",
        }
    }
}

/// Send an alert to Lin's notification banner.
/// Alerts are shown as floating banners - errors/warnings stay for 10+ seconds.
pub fn send_alert(text: &str, kind: AlertKind) -> Result<()> {
    let alerts_path = get_alerts_path()?;

    // Ensure the directory exists
    if let Some(parent) = alerts_path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Read existing alerts
    let mut alerts: Vec<Alert> = if alerts_path.exists() {
        let content = fs::read_to_string(&alerts_path)?;
        serde_json::from_str(&content).unwrap_or_default()
    } else {
        Vec::new()
    };

    // Get current timestamp
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("Time went backwards")?
        .as_secs() as i64;

    // Determine expiry based on kind (warnings/errors stay longer)
    let duration = match kind {
        AlertKind::Error | AlertKind::Warning => 30, // 30 seconds for errors/warnings
        AlertKind::Success => 5,
        AlertKind::Info => 10,
    };

    // Create new alert
    let alert = Alert {
        id: Uuid::new_v4().to_string(),
        timestamp: now,
        text: text.to_string(),
        kind: kind.as_str().to_string(),
        expires_at: now + duration,
    };

    // Add to alerts
    alerts.push(alert);

    // Clean up old alerts (keep last 20)
    if alerts.len() > 20 {
        let skip_count = alerts.len() - 20;
        alerts = alerts.into_iter().skip(skip_count).collect();
    }

    // Write back
    let content = serde_json::to_string_pretty(&alerts)?;
    fs::write(&alerts_path, content)?;

    Ok(())
}

/// Send an error alert to Lin.
pub fn send_error(text: &str) -> Result<()> {
    send_alert(text, AlertKind::Error)
}

/// Send a warning alert to Lin.
pub fn send_warning(text: &str) -> Result<()> {
    send_alert(text, AlertKind::Warning)
}
