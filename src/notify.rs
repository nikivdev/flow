//! Notify command - sends proposals to Lin app for user approval.

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
