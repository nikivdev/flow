//! AI session management for Claude Code and Codex integration.
//!
//! Tracks and manages AI coding sessions per project, allowing users to:
//! - List sessions for the current project (Claude, Codex, or both)
//! - Save/bookmark sessions with names
//! - Resume sessions
//! - Add notes to sessions
//! - Copy session history to clipboard

use std::collections::HashMap;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::debug;

use crate::cli::{AiAction, ProviderAiAction};

/// AI provider type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Claude,
    Codex,
    All,
}

/// Stored session metadata in .ai/sessions/<provider>/index.json
#[derive(Debug, Serialize, Deserialize, Default)]
struct SessionIndex {
    /// Map of user-friendly names to session metadata
    sessions: HashMap<String, SavedSession>,
}

#[derive(Debug, Serialize)]
pub struct WebSession {
    pub id: String,
    pub provider: String,
    pub timestamp: Option<String>,
    pub name: Option<String>,
    pub messages: Vec<WebSessionMessage>,
    pub started_at: Option<String>,
    pub last_message_at: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct WebSessionMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Serialize)]
pub struct SessionHistory {
    pub session_id: String,
    pub provider: String,
    pub started_at: Option<String>,
    pub last_message_at: Option<String>,
    pub messages: Vec<WebSessionMessage>,
}

struct SessionMessages {
    messages: Vec<WebSessionMessage>,
    started_at: Option<String>,
    last_message_at: Option<String>,
}

impl Default for SessionMessages {
    fn default() -> Self {
        Self {
            messages: Vec::new(),
            started_at: None,
            last_message_at: None,
        }
    }
}

/// Commit checkpoint stored in .ai/commit-checkpoints.json
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct CommitCheckpoints {
    /// Last commit checkpoint
    pub last_commit: Option<CommitCheckpoint>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CommitCheckpoint {
    /// When this checkpoint was created
    pub timestamp: String,
    /// Session ID that was active
    pub session_id: Option<String>,
    /// Timestamp of the last entry included in that commit
    pub last_entry_timestamp: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct SavedSession {
    /// Session ID (UUID)
    id: String,
    /// Which provider this session is from
    #[serde(default = "default_provider")]
    provider: String,
    /// Optional description
    description: Option<String>,
    /// When this session was saved
    saved_at: String,
    /// Last resumed timestamp
    last_resumed: Option<String>,
}

fn default_provider() -> String {
    "claude".to_string()
}

/// Session info extracted from session files
#[derive(Debug, Clone)]
struct AiSession {
    /// Session ID (UUID)
    session_id: String,
    /// Which provider (claude, codex)
    provider: Provider,
    /// First message timestamp
    timestamp: Option<String>,
    /// Last message timestamp
    last_message_at: Option<String>,
    /// Last user/assistant message text
    last_message: Option<String>,
    /// First user message (as summary)
    first_message: Option<String>,
    /// First error summary (for sessions that never produced a user message)
    error_summary: Option<String>,
}

/// Entry from a session .jsonl file (we only parse what we need)
#[derive(Debug, Deserialize)]
struct JsonlEntry {
    timestamp: Option<String>,
    message: Option<SessionMessage>,
    #[serde(rename = "type")]
    entry_type: Option<String>,
    subtype: Option<String>,
    level: Option<String>,
    error: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct CodexEntry {
    timestamp: Option<String>,
    #[serde(rename = "type")]
    entry_type: Option<String>,
    payload: Option<serde_json::Value>,
    role: Option<String>,
    content: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct SessionMessage {
    role: Option<String>,
    content: Option<serde_json::Value>,
}

/// Run a provider-specific action (for top-level `f codex` / `f claude` commands).
pub fn run_provider(provider: Provider, action: Option<ProviderAiAction>) -> Result<()> {
    match action {
        None => quick_start_session(provider)?,
        Some(ProviderAiAction::List) => list_sessions(provider)?,
        Some(ProviderAiAction::New) => new_session(provider)?,
        Some(ProviderAiAction::Resume { session }) => resume_session(session, provider)?,
        Some(ProviderAiAction::Copy { session }) => copy_session(session, provider)?,
        Some(ProviderAiAction::Context {
            session,
            count,
            path,
        }) => copy_context(session, provider, count, path)?,
    }
    Ok(())
}

/// Run the ai subcommand.
pub fn run(action: Option<AiAction>) -> Result<()> {
    let action = action.unwrap_or(AiAction::List);

    match action {
        AiAction::List => list_sessions(Provider::All)?,
        AiAction::Claude { action } => match action {
            None => quick_start_session(Provider::Claude)?,
            Some(ProviderAiAction::List) => list_sessions(Provider::Claude)?,
            Some(ProviderAiAction::New) => new_session(Provider::Claude)?,
            Some(ProviderAiAction::Resume { session }) => {
                resume_session(session, Provider::Claude)?
            }
            Some(ProviderAiAction::Copy { session }) => copy_session(session, Provider::Claude)?,
            Some(ProviderAiAction::Context {
                session,
                count,
                path,
            }) => copy_context(session, Provider::Claude, count, path)?,
        },
        AiAction::Codex { action } => match action {
            None => quick_start_session(Provider::Codex)?,
            Some(ProviderAiAction::List) => list_sessions(Provider::Codex)?,
            Some(ProviderAiAction::New) => new_session(Provider::Codex)?,
            Some(ProviderAiAction::Resume { session }) => resume_session(session, Provider::Codex)?,
            Some(ProviderAiAction::Copy { session }) => copy_session(session, Provider::Codex)?,
            Some(ProviderAiAction::Context {
                session,
                count,
                path,
            }) => copy_context(session, Provider::Codex, count, path)?,
        },
        AiAction::Resume { session } => resume_session(session, Provider::All)?,
        AiAction::Save { name, id } => save_session(&name, id)?,
        AiAction::Notes { session } => open_notes(&session)?,
        AiAction::Remove { session } => remove_session(&session)?,
        AiAction::Init => init_ai_folder()?,
        AiAction::Import => import_sessions()?,
        AiAction::Copy { session } => copy_session(session, Provider::All)?,
        AiAction::CopyClaude { search } => {
            let query = if search.is_empty() {
                None
            } else {
                Some(search.join(" "))
            };
            copy_last_session(Provider::Claude, query)?
        }
        AiAction::CopyCodex { search } => {
            let query = if search.is_empty() {
                None
            } else {
                Some(search.join(" "))
            };
            copy_last_session(Provider::Codex, query)?
        }
        AiAction::Context {
            session,
            count,
            path,
        } => copy_context(session, Provider::All, count, path)?,
    }

    Ok(())
}

/// Get checkpoint file path for a project.
fn get_checkpoint_path(project_path: &PathBuf) -> PathBuf {
    project_path
        .join(".ai")
        .join("internal")
        .join("commit-checkpoints.json")
}

/// Load commit checkpoints.
pub fn load_checkpoints(project_path: &PathBuf) -> Result<CommitCheckpoints> {
    let path = get_checkpoint_path(project_path);
    if !path.exists() {
        return Ok(CommitCheckpoints::default());
    }
    let content =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&content).context("failed to parse commit-checkpoints.json")
}

/// Save commit checkpoints.
pub fn save_checkpoint(project_path: &PathBuf, checkpoint: CommitCheckpoint) -> Result<()> {
    let path = get_checkpoint_path(project_path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let checkpoints = CommitCheckpoints {
        last_commit: Some(checkpoint),
    };
    let content = serde_json::to_string_pretty(&checkpoints)?;
    fs::write(&path, content)?;
    Ok(())
}

/// Log review result for tracking async commits.
pub fn log_review_result(
    project_path: &PathBuf,
    issues_found: bool,
    issues: &[String],
    context_chars: usize,
    review_time_secs: u64,
) {
    let log_path = project_path
        .join(".ai")
        .join("internal")
        .join("review-log.jsonl");
    if let Some(parent) = log_path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let entry = json!({
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "issues_found": issues_found,
        "issue_count": issues.len(),
        "context_chars": context_chars,
        "review_time_secs": review_time_secs,
    });

    if let Ok(mut file) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        let _ = writeln!(file, "{}", entry);
    }
}

/// Log commit review details for later analysis.
pub fn log_commit_review(
    project_path: &PathBuf,
    commit_sha: &str,
    branch: &str,
    message: &str,
    review_model: &str,
    reviewer: &str,
    issues_found: bool,
    issues: &[String],
    summary: Option<&str>,
    timed_out: bool,
    context_chars: usize,
) {
    let log_dir = project_path.join(".ai").join("internal").join("commits");
    let log_path = log_dir.join("review-log.jsonl");
    if let Some(parent) = log_path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let entry = json!({
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "commit_sha": commit_sha,
        "branch": branch,
        "message": message,
        "review": {
            "model": review_model,
            "reviewer": reviewer,
            "issues_found": issues_found,
            "issue_count": issues.len(),
            "issues": issues,
            "summary": summary,
            "timed_out": timed_out,
        },
        "context_chars": context_chars,
    });

    if let Ok(mut file) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        let _ = writeln!(file, "{}", entry);
    }
}

#[derive(Debug, Serialize)]
pub struct CommitReviewSummary {
    pub model: String,
    pub reviewer: String,
    pub issues_found: bool,
    pub issues: Vec<String>,
    pub summary: Option<String>,
    pub timed_out: bool,
}

/// Log commit metadata (with optional review data) for later analysis.
pub fn log_commit_event(
    project_path: &PathBuf,
    commit_sha: &str,
    branch: &str,
    message: &str,
    author_name: &str,
    author_email: &str,
    command: &str,
    review: Option<CommitReviewSummary>,
    context_chars: Option<usize>,
) {
    let log_dir = project_path.join(".ai").join("internal").join("commits");
    let log_path = log_dir.join("log.jsonl");
    if let Some(parent) = log_path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let entry = json!({
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "commit_sha": commit_sha,
        "branch": branch,
        "message": message,
        "author": {
            "name": author_name,
            "email": author_email,
        },
        "command": command,
        "review": review,
        "context_chars": context_chars,
    });

    if let Ok(mut file) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        let _ = writeln!(file, "{}", entry);
    }
}

/// Get AI session context since the last commit checkpoint.
/// Returns all exchanges from the checkpoint timestamp to now.
pub fn get_context_since_checkpoint() -> Result<Option<String>> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    get_context_since_checkpoint_for_path(&cwd)
}

/// Get AI session context since the last commit checkpoint for a specific path.
pub fn get_context_since_checkpoint_for_path(project_path: &PathBuf) -> Result<Option<String>> {
    let checkpoints = load_checkpoints(project_path).unwrap_or_default();

    // Get sessions for both Claude and Codex
    let sessions = read_sessions_for_path(Provider::All, project_path)?;

    if sessions.is_empty() {
        return Ok(None);
    }

    // Read context since checkpoint
    let since_ts = checkpoints
        .last_commit
        .as_ref()
        .and_then(|c| c.last_entry_timestamp.clone());

    let mut combined = String::new();
    let since_info = if since_ts.is_some() {
        " (since last commit)"
    } else {
        " (full session - no previous commit)"
    };

    for session in sessions {
        let provider_name = match session.provider {
            Provider::Claude => "Claude Code",
            Provider::Codex => "Codex",
            Provider::All => "AI",
        };

        if let Ok((context, last_ts)) = read_context_since(
            &session.session_id,
            session.provider,
            since_ts.as_deref(),
            project_path,
        ) {
            if context.trim().is_empty() {
                continue;
            }
            if !combined.is_empty() {
                combined.push_str("\n\n");
            }
            combined.push_str(&format!(
                "=== {} Session Context{} ===\nLast entry: {}\n\n{}\n\n=== End Session Context ===",
                provider_name,
                since_info,
                last_ts.unwrap_or_else(|| "unknown".to_string()),
                context
            ));
        }
    }

    if combined.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(combined))
    }
}

/// Structured AI session data for GitEdit sync.
#[derive(Debug, Serialize, Clone)]
pub struct GitEditSessionData {
    pub session_id: String,
    pub provider: String,
    pub started_at: Option<String>,
    pub last_activity_at: Option<String>,
    pub exchanges: Vec<GitEditExchange>,
    pub context_summary: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct GitEditExchange {
    pub user_message: String,
    pub assistant_message: String,
    pub timestamp: String,
}

/// Get session IDs quickly for early hash generation.
/// Returns (session_ids, checkpoint_timestamp) for hashing before full data load.
pub fn get_session_ids_for_hash(project_path: &PathBuf) -> Result<(Vec<String>, Option<String>)> {
    let checkpoints = load_checkpoints(project_path).unwrap_or_default();
    let sessions = read_sessions_for_path(Provider::All, project_path)?;

    let checkpoint_ts = checkpoints
        .last_commit
        .as_ref()
        .and_then(|c| c.last_entry_timestamp.clone());

    let session_ids: Vec<String> = sessions.iter().map(|s| s.session_id.clone()).collect();

    Ok((session_ids, checkpoint_ts))
}

/// Get structured AI session data for GitEdit sync.
/// Returns sessions with full exchange history since the last checkpoint.
pub fn get_sessions_for_gitedit(project_path: &PathBuf) -> Result<Vec<GitEditSessionData>> {
    let checkpoints = load_checkpoints(project_path).unwrap_or_default();
    let sessions = read_sessions_for_path(Provider::All, project_path)?;

    if sessions.is_empty() {
        return Ok(vec![]);
    }

    let since_ts = checkpoints
        .last_commit
        .as_ref()
        .and_then(|c| c.last_entry_timestamp.clone());

    let mut result = Vec::new();

    for session in sessions {
        let provider_name = match session.provider {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
            Provider::All => "unknown",
        };

        // Get full exchanges (not summarized)
        let exchanges = get_session_exchanges_since(
            &session.session_id,
            session.provider,
            since_ts.as_deref(),
            project_path,
        )?;

        if exchanges.is_empty() {
            continue;
        }

        // Get last timestamp from exchanges
        let last_activity = exchanges.last().map(|e| e.timestamp.clone());

        // Create context summary (first few words of first user message)
        let context_summary = exchanges.first().map(|e| {
            let msg = &e.user_message;
            let words: Vec<&str> = msg.split_whitespace().take(10).collect();
            let summary = words.join(" ");
            if msg.split_whitespace().count() > 10 {
                format!("{}...", summary)
            } else {
                summary
            }
        });

        result.push(GitEditSessionData {
            session_id: session.session_id.clone(),
            provider: provider_name.to_string(),
            started_at: session.timestamp.clone(),
            last_activity_at: last_activity,
            exchanges,
            context_summary,
        });
    }

    Ok(result)
}

/// Get full exchanges from a session since a timestamp.
fn get_session_exchanges_since(
    session_id: &str,
    provider: Provider,
    since_ts: Option<&str>,
    project_path: &PathBuf,
) -> Result<Vec<GitEditExchange>> {
    if provider == Provider::Codex {
        let session_file = find_codex_session_file(session_id);
        if let Some(session_file) = session_file {
            let (exchanges, _) = read_codex_exchanges(&session_file, since_ts)?;
            return Ok(exchanges
                .into_iter()
                .map(|(user, assistant, ts)| GitEditExchange {
                    user_message: user,
                    assistant_message: assistant,
                    timestamp: ts,
                })
                .collect());
        }
        return Ok(vec![]);
    }

    let path_str = project_path.to_string_lossy().to_string();
    let project_folder = path_to_project_name(&path_str);

    let projects_dir = get_claude_projects_dir();
    let session_file = projects_dir
        .join(&project_folder)
        .join(format!("{}.jsonl", session_id));

    if !session_file.exists() {
        return Ok(vec![]);
    }

    let content = fs::read_to_string(&session_file).context("failed to read session file")?;

    let mut exchanges: Vec<GitEditExchange> = Vec::new();
    let mut current_user: Option<String> = None;
    let mut current_ts: Option<String> = None;

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }

        if let Ok(entry) = serde_json::from_str::<JsonlEntry>(line) {
            let entry_ts = entry.timestamp.clone();

            // Skip entries before checkpoint
            if let (Some(since), Some(ts)) = (since_ts, &entry_ts) {
                if ts.as_str() <= since {
                    continue;
                }
            }

            if let Some(ref msg) = entry.message {
                let role = msg.role.as_deref().unwrap_or("unknown");

                let content_text = if let Some(ref content) = msg.content {
                    match content {
                        serde_json::Value::String(s) => s.clone(),
                        serde_json::Value::Array(arr) => arr
                            .iter()
                            .filter_map(|v| {
                                v.get("text")
                                    .and_then(|t| t.as_str())
                                    .map(|s| s.to_string())
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                        _ => continue,
                    }
                } else {
                    continue;
                };

                if content_text.is_empty() {
                    continue;
                }

                match role {
                    "user" => {
                        current_user = Some(content_text);
                        current_ts = entry_ts.clone();
                    }
                    "assistant" => {
                        let clean_text = strip_thinking_blocks(&content_text);
                        if clean_text.trim().is_empty() {
                            continue;
                        }
                        if let Some(user_msg) = current_user.take() {
                            let ts = current_ts.take().or(entry_ts).unwrap_or_default();
                            exchanges.push(GitEditExchange {
                                user_message: user_msg,
                                assistant_message: clean_text,
                                timestamp: ts,
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(exchanges)
}

/// Get the last entry timestamp from the current session (for saving checkpoint).
pub fn get_last_entry_timestamp() -> Result<Option<(String, String)>> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    get_last_entry_timestamp_for_path(&cwd)
}

/// Get the last entry timestamp for sessions associated with a specific path.
pub fn get_last_entry_timestamp_for_path(
    project_path: &PathBuf,
) -> Result<Option<(String, String)>> {
    let sessions = read_sessions_for_path(Provider::All, project_path)?;

    if sessions.is_empty() {
        return Ok(None);
    }

    let mut best: Option<(String, String)> = None;
    for session in sessions {
        if let Some(ts) =
            get_session_last_timestamp(&session.session_id, session.provider, project_path)?
        {
            let is_newer = best.as_ref().map_or(true, |(_, best_ts)| ts > *best_ts);
            if is_newer {
                best = Some((session.session_id.clone(), ts));
            }
        }
    }

    Ok(best)
}

/// Get the last timestamp from a session file.
fn get_session_last_timestamp(
    session_id: &str,
    provider: Provider,
    project_path: &PathBuf,
) -> Result<Option<String>> {
    if provider == Provider::Codex {
        let session_file = find_codex_session_file(session_id);
        let Some(session_file) = session_file else {
            return Ok(None);
        };
        return get_codex_last_timestamp(&session_file);
    }

    let path_str = project_path.to_string_lossy().to_string();
    let project_folder = path_to_project_name(&path_str);

    let projects_dir = match provider {
        Provider::Claude | Provider::All => get_claude_projects_dir(),
        Provider::Codex => get_codex_projects_dir(),
    };

    let session_file = projects_dir
        .join(&project_folder)
        .join(format!("{}.jsonl", session_id));

    if !session_file.exists() {
        return Ok(None);
    }

    let content = fs::read_to_string(&session_file).context("failed to read session file")?;

    let mut last_ts: Option<String> = None;
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<JsonlEntry>(line) {
            if let Some(ts) = entry.timestamp {
                last_ts = Some(ts);
            }
        }
    }

    Ok(last_ts)
}

/// Read context from session since a given timestamp.
fn read_context_since(
    session_id: &str,
    provider: Provider,
    since_ts: Option<&str>,
    project_path: &PathBuf,
) -> Result<(String, Option<String>)> {
    if provider == Provider::Codex {
        let session_file = find_codex_session_file(session_id).ok_or_else(|| {
            anyhow::anyhow!("Session file not found for Codex session {}", session_id)
        })?;
        return read_codex_context_since(&session_file, since_ts);
    }

    let path_str = project_path.to_string_lossy().to_string();
    let project_folder = path_to_project_name(&path_str);

    let projects_dir = match provider {
        Provider::Claude | Provider::All => get_claude_projects_dir(),
        Provider::Codex => get_codex_projects_dir(),
    };

    let session_file = projects_dir
        .join(&project_folder)
        .join(format!("{}.jsonl", session_id));

    if !session_file.exists() {
        bail!("Session file not found: {}", session_file.display());
    }

    let content = fs::read_to_string(&session_file).context("failed to read session file")?;

    // Collect exchanges after the checkpoint timestamp
    let mut exchanges: Vec<(String, String, String)> = Vec::new(); // (user_msg, assistant_msg, timestamp)
    let mut current_user: Option<String> = None;
    let mut current_ts: Option<String> = None;
    let mut last_ts: Option<String> = None;

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }

        if let Ok(entry) = serde_json::from_str::<JsonlEntry>(line) {
            let entry_ts = entry.timestamp.clone();

            // Skip entries before checkpoint
            if let (Some(since), Some(ts)) = (since_ts, &entry_ts) {
                if ts.as_str() <= since {
                    continue;
                }
            }

            if let Some(ref msg) = entry.message {
                let role = msg.role.as_deref().unwrap_or("unknown");

                let content_text = if let Some(ref content) = msg.content {
                    match content {
                        serde_json::Value::String(s) => s.clone(),
                        serde_json::Value::Array(arr) => arr
                            .iter()
                            .filter_map(|v| {
                                if let Some(text) = v.get("text").and_then(|t| t.as_str()) {
                                    Some(text.to_string())
                                } else {
                                    None
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                        _ => continue,
                    }
                } else {
                    continue;
                };

                if content_text.is_empty() {
                    continue;
                }

                match role {
                    "user" => {
                        current_user = Some(content_text);
                        current_ts = entry_ts.clone();
                    }
                    "assistant" => {
                        let clean_text = strip_thinking_blocks(&content_text);
                        if clean_text.trim().is_empty() {
                            continue;
                        }
                        if let Some(user_msg) = current_user.take() {
                            let ts = current_ts.take().or(entry_ts.clone()).unwrap_or_default();
                            exchanges.push((user_msg, clean_text, ts.clone()));
                            last_ts = Some(ts);
                        }
                    }
                    _ => {}
                }
            }

            if entry_ts.is_some() {
                last_ts = entry_ts;
            }
        }
    }

    if exchanges.is_empty() {
        return Ok((String::new(), last_ts));
    }

    // Optimization: prioritize recent exchanges, fit within reasonable budget
    // Keep it compact - extract intent, not full conversation
    const MAX_EXCHANGES: usize = 5;
    const MAX_USER_CHARS: usize = 500; // User requests are short
    const MAX_ASSIST_CHARS: usize = 300; // Just capture what was done, not full response

    let total_exchanges = exchanges.len();
    let exchanges_to_use: Vec<_> = if total_exchanges > MAX_EXCHANGES {
        exchanges
            .into_iter()
            .skip(total_exchanges - MAX_EXCHANGES)
            .collect()
    } else {
        exchanges
    };

    // Format compact context - focus on intent
    let mut context = String::new();

    if total_exchanges > MAX_EXCHANGES {
        context.push_str(&format!("[+{} earlier]\n", total_exchanges - MAX_EXCHANGES));
    }

    for (user_msg, assistant_msg, _ts) in &exchanges_to_use {
        // Extract first line/sentence of user msg as intent
        let user_intent = extract_intent(user_msg, MAX_USER_CHARS);
        let assist_summary = extract_intent(assistant_msg, MAX_ASSIST_CHARS);

        context.push_str(">");
        context.push_str(&user_intent);
        context.push('\n');
        context.push_str(&assist_summary);
        context.push_str("\n\n");
    }

    context = context.trim().to_string();

    Ok((context, last_ts))
}

/// Find the largest valid UTF-8 char boundary at or before `pos`.
fn floor_char_boundary(s: &str, pos: usize) -> usize {
    let mut end = pos.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// Truncate a message to max chars, preserving meaningful content
fn truncate_message(msg: &str, max_chars: usize) -> String {
    if msg.len() <= max_chars {
        return msg.to_string();
    }
    let end = floor_char_boundary(msg, max_chars);
    format!("{}...", &msg[..end])
}

/// Extract intent from a message - first meaningful content, truncated
fn extract_intent(msg: &str, max_chars: usize) -> String {
    // Skip common prefixes and get to the meat
    let clean = msg
        .trim()
        .trim_start_matches("I'll ")
        .trim_start_matches("I will ")
        .trim_start_matches("Let me ")
        .trim_start_matches("Sure, ")
        .trim_start_matches("Okay, ")
        .trim_start_matches("I'm going to ")
        .trim();

    // Take first line or sentence
    let first_part = clean
        .lines()
        .next()
        .unwrap_or(clean)
        .split(". ")
        .next()
        .unwrap_or(clean);

    truncate_message(first_part, max_chars)
}

fn read_codex_context_since(
    session_file: &PathBuf,
    since_ts: Option<&str>,
) -> Result<(String, Option<String>)> {
    let (exchanges, last_ts) = read_codex_exchanges(session_file, since_ts)?;

    if exchanges.is_empty() {
        return Ok((String::new(), last_ts));
    }

    // Optimization: only keep last N exchanges for efficiency
    const MAX_EXCHANGES: usize = 8;
    const MAX_MSG_CHARS: usize = 2000;

    let total_exchanges = exchanges.len();
    let exchanges_to_use: Vec<_> = if total_exchanges > MAX_EXCHANGES {
        exchanges
            .into_iter()
            .skip(total_exchanges - MAX_EXCHANGES)
            .collect()
    } else {
        exchanges
    };

    let mut context = String::new();

    // Add summary if we skipped older exchanges
    if total_exchanges > MAX_EXCHANGES {
        context.push_str(&format!(
            "[{} earlier exchanges omitted for brevity]\n\n",
            total_exchanges - MAX_EXCHANGES
        ));
    }

    for (user_msg, assistant_msg, _ts) in &exchanges_to_use {
        context.push_str("H: ");
        context.push_str(&truncate_message(user_msg, MAX_MSG_CHARS));
        context.push_str("\n\n");
        context.push_str("A: ");
        context.push_str(&truncate_message(assistant_msg, MAX_MSG_CHARS));
        context.push_str("\n\n");
    }

    while context.ends_with('\n') {
        context.pop();
    }
    context.push('\n');

    Ok((context, last_ts))
}

fn read_codex_last_context(session_file: &PathBuf, count: usize) -> Result<String> {
    let (exchanges, _last_ts) = read_codex_exchanges(session_file, None)?;

    if exchanges.is_empty() {
        bail!("No exchanges found in session");
    }

    let start = exchanges.len().saturating_sub(count);
    let last_exchanges = &exchanges[start..];

    let mut context = String::new();
    for (user_msg, assistant_msg, _ts) in last_exchanges {
        context.push_str("Human: ");
        context.push_str(user_msg);
        context.push_str("\n\n");
        context.push_str("Assistant: ");
        context.push_str(assistant_msg);
        context.push_str("\n\n");
    }

    while context.ends_with('\n') {
        context.pop();
    }
    context.push('\n');

    Ok(context)
}

fn read_codex_exchanges(
    session_file: &PathBuf,
    since_ts: Option<&str>,
) -> Result<(Vec<(String, String, String)>, Option<String>)> {
    let content = fs::read_to_string(session_file).context("failed to read session file")?;

    let mut exchanges: Vec<(String, String, String)> = Vec::new();
    let mut current_user: Option<String> = None;
    let mut current_ts: Option<String> = None;
    let mut last_ts: Option<String> = None;

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }

        let entry: CodexEntry = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let entry_ts = entry.timestamp.clone();
        if let Some(ts) = entry_ts.as_deref() {
            if let Some(since) = since_ts {
                if ts <= since {
                    continue;
                }
            }
        }

        if let Some((role, text)) = extract_codex_message(&entry) {
            if text.trim().is_empty() {
                continue;
            }

            match role.as_str() {
                "user" => {
                    current_user = Some(text);
                    current_ts = entry_ts.clone();
                }
                "assistant" => {
                    let clean_text = strip_thinking_blocks(&text);
                    if clean_text.trim().is_empty() {
                        continue;
                    }
                    if let Some(user_msg) = current_user.take() {
                        let ts = current_ts.take().or(entry_ts.clone()).unwrap_or_default();
                        exchanges.push((user_msg, clean_text, ts.clone()));
                        last_ts = Some(ts);
                    }
                }
                _ => {}
            }
        }

        if entry_ts.is_some() {
            last_ts = entry_ts;
        }
    }

    Ok((exchanges, last_ts))
}

fn get_codex_last_timestamp(session_file: &PathBuf) -> Result<Option<String>> {
    let content = fs::read_to_string(session_file).context("failed to read session file")?;
    let mut last_ts: Option<String> = None;

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }

        let entry: CodexEntry = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if let Some(ts) = entry.timestamp {
            last_ts = Some(ts);
            continue;
        }

        if let Some(payload_ts) = entry
            .payload
            .as_ref()
            .and_then(|p| p.get("timestamp"))
            .and_then(|v| v.as_str())
        {
            last_ts = Some(payload_ts.to_string());
        }
    }

    Ok(last_ts)
}

fn extract_codex_message(entry: &CodexEntry) -> Option<(String, String)> {
    let entry_type = entry.entry_type.as_deref();

    if entry_type == Some("response_item") {
        let payload = entry.payload.as_ref()?;
        if payload.get("type").and_then(|v| v.as_str()) != Some("message") {
            return None;
        }
        let role = payload.get("role").and_then(|v| v.as_str())?.to_string();
        let content = payload.get("content")?;
        let text = extract_codex_content_text(content)?;
        return Some((role, text));
    }

    if entry_type == Some("event_msg") {
        let payload = entry.payload.as_ref()?;
        let payload_type = payload.get("type").and_then(|v| v.as_str());
        if payload_type == Some("user_message") {
            let text = payload.get("message").and_then(|v| v.as_str())?.to_string();
            return Some(("user".to_string(), text));
        }
        if payload_type == Some("agent_message") {
            let text = payload.get("message").and_then(|v| v.as_str())?.to_string();
            return Some(("assistant".to_string(), text));
        }
    }

    if entry_type == Some("message") {
        let role = entry.role.as_deref()?.to_string();
        let content = entry.content.as_ref()?;
        let text = extract_codex_content_text(content)?;
        return Some((role, text));
    }

    None
}

/// Get recent AI session context for the current project.
/// Used by commit workflow to provide context for code review.
/// Returns the last N exchanges from the most recent sessions.
pub fn get_recent_session_context(max_exchanges: usize) -> Result<Option<String>> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;

    // Get sessions for both Claude and Codex
    let sessions = read_sessions_for_path(Provider::All, &cwd)?;

    if sessions.is_empty() {
        return Ok(None);
    }

    // Get the most recent session
    let recent_session = &sessions[0];

    // Read context from the most recent session
    match read_last_context(
        &recent_session.session_id,
        recent_session.provider,
        max_exchanges,
        &cwd,
    ) {
        Ok(context) => {
            if context.trim().is_empty() {
                Ok(None)
            } else {
                let provider_name = match recent_session.provider {
                    Provider::Claude => "Claude Code",
                    Provider::Codex => "Codex",
                    Provider::All => "AI",
                };
                Ok(Some(format!(
                    "=== Recent {} Session Context ===\n\n{}\n\n=== End Session Context ===",
                    provider_name, context
                )))
            }
        }
        Err(_) => Ok(None),
    }
}

/// Get the .ai/internal/sessions/claude directory for the current project.
fn get_ai_sessions_dir() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    Ok(cwd
        .join(".ai")
        .join("internal")
        .join("sessions")
        .join("claude"))
}

/// Get the index.json path.
fn get_index_path() -> Result<PathBuf> {
    Ok(get_ai_sessions_dir()?.join("index.json"))
}

/// Get the notes directory.
fn get_notes_dir() -> Result<PathBuf> {
    Ok(get_ai_sessions_dir()?.join("notes"))
}

/// Load the session index.
fn load_index() -> Result<SessionIndex> {
    let path = get_index_path()?;
    if !path.exists() {
        return Ok(SessionIndex::default());
    }
    let content =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&content).context("failed to parse index.json")
}

fn load_index_for_path(project_path: &Path) -> Result<SessionIndex> {
    let path = project_path
        .join(".ai")
        .join("internal")
        .join("sessions")
        .join("claude")
        .join("index.json");
    if !path.exists() {
        return Ok(SessionIndex::default());
    }
    let content =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&content).context("failed to parse index.json")
}

pub fn get_sessions_for_web(project_path: &PathBuf) -> Result<Vec<WebSession>> {
    let sessions = read_sessions_for_path(Provider::All, project_path)?;
    if sessions.is_empty() {
        return Ok(vec![]);
    }

    let index = load_index_for_path(project_path).unwrap_or_default();
    let mut output = Vec::with_capacity(sessions.len());

    for session in sessions {
        let provider = match session.provider {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
            Provider::All => "unknown",
        };
        let name = index
            .sessions
            .iter()
            .find(|(_, saved)| saved.id == session.session_id && saved.provider == provider)
            .map(|(name, _)| name.clone())
            .filter(|name| !is_auto_generated_name(name));
        let session_messages =
            read_session_messages_for_path(project_path, &session.session_id, session.provider)
                .unwrap_or_default();
        let started_at = session_messages
            .started_at
            .clone()
            .or_else(|| session.timestamp.clone());
        let last_message_at = session_messages
            .last_message_at
            .clone()
            .or_else(|| started_at.clone());

        output.push(WebSession {
            id: session.session_id,
            provider: provider.to_string(),
            timestamp: session.timestamp,
            name,
            messages: session_messages.messages,
            started_at,
            last_message_at,
        });
    }

    output.sort_by(|a, b| {
        let a_key = a
            .last_message_at
            .as_deref()
            .or(a.started_at.as_deref())
            .unwrap_or("");
        let b_key = b
            .last_message_at
            .as_deref()
            .or(b.started_at.as_deref())
            .unwrap_or("");
        b_key.cmp(a_key)
    });

    Ok(output)
}

fn read_session_messages_for_path(
    project_path: &Path,
    session_id: &str,
    provider: Provider,
) -> Result<SessionMessages> {
    match provider {
        Provider::Codex => read_codex_messages(session_id),
        Provider::Claude | Provider::All => read_claude_messages_for_path(project_path, session_id),
    }
}

fn read_claude_messages_for_path(project_path: &Path, session_id: &str) -> Result<SessionMessages> {
    let path_str = project_path.to_string_lossy().to_string();
    let project_folder = path_to_project_name(&path_str);
    let session_file = get_claude_projects_dir()
        .join(&project_folder)
        .join(format!("{}.jsonl", session_id));

    if !session_file.exists() {
        bail!("Session file not found: {}", session_file.display());
    }

    let content = fs::read_to_string(&session_file).context("failed to read session file")?;
    let mut messages = Vec::new();
    let mut started_at: Option<String> = None;
    let mut last_message_at: Option<String> = None;

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<JsonlEntry>(line) else {
            continue;
        };
        let Some(ref msg) = entry.message else {
            continue;
        };
        let role = msg.role.as_deref().unwrap_or("unknown");
        if role != "user" && role != "assistant" {
            continue;
        }
        let content_text = msg.content.as_ref().and_then(extract_message_text);
        let Some(content_text) = content_text else {
            continue;
        };
        if content_text.trim().is_empty() {
            continue;
        }
        push_message(&mut messages, role, &content_text);
        if let Some(ts) = entry.timestamp.clone() {
            if started_at.is_none() {
                started_at = Some(ts.clone());
            }
            last_message_at = Some(ts);
        }
    }

    Ok(SessionMessages {
        messages,
        started_at,
        last_message_at,
    })
}

fn read_codex_messages(session_id: &str) -> Result<SessionMessages> {
    let session_file = find_codex_session_file(session_id)
        .ok_or_else(|| anyhow::anyhow!("Codex session file not found"))?;
    let content = fs::read_to_string(&session_file).context("failed to read session file")?;
    let mut messages = Vec::new();
    let mut started_at: Option<String> = None;
    let mut last_message_at: Option<String> = None;

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }

        let entry: CodexEntry = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let Some((role, text)) = extract_codex_message(&entry) else {
            continue;
        };
        let clean_text = if role == "assistant" {
            strip_thinking_blocks(&text)
        } else {
            text
        };
        if clean_text.trim().is_empty() {
            continue;
        }
        push_message(&mut messages, &role, &clean_text);
        if let Some(ts) = extract_codex_timestamp(&entry) {
            if started_at.is_none() {
                started_at = Some(ts.clone());
            }
            last_message_at = Some(ts);
        }
    }

    Ok(SessionMessages {
        messages,
        started_at,
        last_message_at,
    })
}

fn extract_codex_timestamp(entry: &CodexEntry) -> Option<String> {
    if let Some(ts) = entry.timestamp.as_deref() {
        return Some(ts.to_string());
    }
    entry
        .payload
        .as_ref()
        .and_then(|payload| payload.get("timestamp"))
        .and_then(|value| value.as_str())
        .map(|ts| ts.to_string())
}

fn extract_message_text(content_value: &serde_json::Value) -> Option<String> {
    match content_value {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(arr) => {
            let parts: Vec<String> = arr
                .iter()
                .filter_map(|item| {
                    let item_type = item.get("type").and_then(|t| t.as_str());
                    if item_type.is_some() && item_type != Some("text") {
                        return None;
                    }
                    item.get("text")
                        .and_then(|t| t.as_str())
                        .map(|text| text.to_string())
                })
                .filter(|text| !text.trim().is_empty())
                .collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n"))
            }
        }
        serde_json::Value::Object(obj) => {
            if let Some(text) = obj.get("text").and_then(|t| t.as_str()) {
                return Some(text.to_string());
            }
            None
        }
        _ => None,
    }
}

fn push_message(messages: &mut Vec<WebSessionMessage>, role: &str, content: &str) {
    if let Some(last) = messages.last_mut() {
        if last.role == role {
            last.content.push_str("\n\n");
            last.content.push_str(content);
            return;
        }
    }
    messages.push(WebSessionMessage {
        role: role.to_string(),
        content: content.to_string(),
    });
}

/// Save the session index.
fn save_index(index: &SessionIndex) -> Result<()> {
    let path = get_index_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(index)?;
    fs::write(&path, content)?;
    Ok(())
}

/// Get Claude's projects directory.
fn get_claude_projects_dir() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".claude").join("projects")
}

/// Get Codex's projects directory.
fn get_codex_projects_dir() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".codex").join("projects")
}

fn get_codex_sessions_dir() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".codex").join("sessions")
}

/// Convert a path to project folder name (replaces / with -).
fn path_to_project_name(path: &str) -> String {
    path.replace('/', "-")
}

/// Read sessions for the current project, filtered by provider.
fn read_sessions_for_project(provider: Provider) -> Result<Vec<AiSession>> {
    let mut sessions = Vec::new();

    if provider == Provider::Claude || provider == Provider::All {
        sessions.extend(read_provider_sessions(Provider::Claude)?);
    }

    if provider == Provider::Codex || provider == Provider::All {
        sessions.extend(read_provider_sessions(Provider::Codex)?);
    }

    // Sort by last message timestamp descending (most recent first)
    sessions.sort_by(|a, b| {
        let ts_a = a
            .last_message_at
            .as_deref()
            .or(a.timestamp.as_deref())
            .unwrap_or("");
        let ts_b = b
            .last_message_at
            .as_deref()
            .or(b.timestamp.as_deref())
            .unwrap_or("");
        ts_b.cmp(ts_a)
    });

    Ok(sessions)
}

/// Read sessions for a project at a specific path.
fn read_sessions_for_path(provider: Provider, path: &PathBuf) -> Result<Vec<AiSession>> {
    let mut sessions = Vec::new();

    if provider == Provider::Claude || provider == Provider::All {
        sessions.extend(read_provider_sessions_for_path(Provider::Claude, path)?);
    }

    if provider == Provider::Codex || provider == Provider::All {
        sessions.extend(read_provider_sessions_for_path(Provider::Codex, path)?);
    }

    // Sort by last message timestamp descending (most recent first)
    sessions.sort_by(|a, b| {
        let ts_a = a
            .last_message_at
            .as_deref()
            .or(a.timestamp.as_deref())
            .unwrap_or("");
        let ts_b = b
            .last_message_at
            .as_deref()
            .or(b.timestamp.as_deref())
            .unwrap_or("");
        ts_b.cmp(ts_a)
    });

    Ok(sessions)
}

/// Read sessions for a specific provider at a given path.
fn read_provider_sessions_for_path(provider: Provider, path: &PathBuf) -> Result<Vec<AiSession>> {
    if provider == Provider::Codex {
        return read_codex_sessions_for_path(path);
    }

    let path_str = path.to_string_lossy().to_string();
    let project_name = path_to_project_name(&path_str);

    let projects_dir = match provider {
        Provider::Claude => get_claude_projects_dir(),
        Provider::Codex => get_codex_projects_dir(),
        Provider::All => return Ok(vec![]),
    };

    let project_dir = projects_dir.join(&project_name);

    if !project_dir.exists() {
        return Ok(vec![]);
    }

    let mut sessions = Vec::new();

    let entries = fs::read_dir(&project_dir)
        .with_context(|| format!("failed to read {}", project_dir.display()))?;

    for entry in entries {
        let entry = entry?;
        let file_path = entry.path();

        if file_path.extension().map(|e| e == "jsonl").unwrap_or(false) {
            let filename = file_path.file_stem().and_then(|s| s.to_str()).unwrap_or("");

            if filename.starts_with("agent-") {
                continue;
            }

            if let Some(session) = parse_session_file(&file_path, filename, provider) {
                sessions.push(session);
            }
        }
    }

    Ok(sessions)
}

/// Read sessions for a specific provider.
fn read_provider_sessions(provider: Provider) -> Result<Vec<AiSession>> {
    if provider == Provider::Codex {
        let cwd = std::env::current_dir().context("failed to get current directory")?;
        return read_codex_sessions_for_path(&cwd);
    }

    let cwd = std::env::current_dir()?;
    let cwd_str = cwd.to_string_lossy().to_string();
    let project_name = path_to_project_name(&cwd_str);

    let projects_dir = match provider {
        Provider::Claude => get_claude_projects_dir(),
        Provider::Codex => get_codex_projects_dir(),
        Provider::All => return Ok(vec![]), // Should use read_sessions_for_project instead
    };

    let project_dir = projects_dir.join(&project_name);

    if !project_dir.exists() {
        debug!(
            "{:?} project dir not found at {}",
            provider,
            project_dir.display()
        );
        return Ok(vec![]);
    }

    let mut sessions = Vec::new();

    // Read all .jsonl files in the project directory
    let entries = fs::read_dir(&project_dir)
        .with_context(|| format!("failed to read {}", project_dir.display()))?;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        // Only process .jsonl files that look like session IDs (UUID format)
        if path.extension().map(|e| e == "jsonl").unwrap_or(false) {
            let filename = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");

            // Skip agent- prefixed files (subagent sessions)
            if filename.starts_with("agent-") {
                continue;
            }

            // Parse the session file
            if let Some(session) = parse_session_file(&path, filename, provider) {
                sessions.push(session);
            }
        }
    }

    Ok(sessions)
}

/// Parse a session .jsonl file to extract metadata.
fn parse_session_file(path: &PathBuf, session_id: &str, provider: Provider) -> Option<AiSession> {
    if provider == Provider::Codex {
        let (session, _cwd) = parse_codex_session_file(path, session_id)?;
        return Some(session);
    }

    let content = fs::read_to_string(path).ok()?;

    let mut timestamp = None;
    let mut last_message_at = None;
    let mut last_message = None;
    let mut first_message = None;
    let mut error_summary = None;

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }

        if let Ok(entry) = serde_json::from_str::<JsonlEntry>(line) {
            // Get timestamp from first entry
            if timestamp.is_none() {
                timestamp = entry.timestamp.clone();
            }

            if let Some(ref msg) = entry.message {
                let role = msg.role.as_deref();
                if role == Some("user") || role == Some("assistant") {
                    if let Some(ref content) = msg.content {
                        if let Some(text) = extract_message_text(content) {
                            let clean_text = if role == Some("assistant") {
                                strip_thinking_blocks(&text)
                            } else {
                                text
                            };
                            if !clean_text.trim().is_empty() {
                                last_message = Some(clean_text);
                                if let Some(ts) = entry.timestamp.clone() {
                                    last_message_at = Some(ts);
                                }
                            }
                        }
                    }
                }
            }

            // Get first user message as summary
            if first_message.is_none() {
                if let Some(ref msg) = entry.message {
                    if msg.role.as_deref() == Some("user") {
                        if let Some(ref content) = msg.content {
                            first_message = match content {
                                serde_json::Value::String(s) => Some(s.clone()),
                                serde_json::Value::Array(arr) => {
                                    // Content might be array of content blocks
                                    arr.first()
                                        .and_then(|v| v.get("text"))
                                        .and_then(|v| v.as_str())
                                        .map(|s| s.to_string())
                                }
                                _ => None,
                            };
                        }
                    }
                }
            }

            // Capture first error summary (useful when no user message exists)
            if error_summary.is_none() {
                error_summary = extract_error_summary(&entry);
            }
        }
    }

    Some(AiSession {
        session_id: session_id.to_string(),
        provider,
        timestamp,
        last_message_at,
        last_message,
        first_message,
        error_summary,
    })
}

fn parse_codex_session_file(
    path: &PathBuf,
    fallback_id: &str,
) -> Option<(AiSession, Option<PathBuf>)> {
    let content = fs::read_to_string(path).ok()?;

    let mut timestamp = None;
    let mut last_message_at = None;
    let mut last_message = None;
    let mut first_message = None;
    let mut error_summary = None;
    let mut session_id = None;
    let mut cwd = None;

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }

        let entry: CodexEntry = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if timestamp.is_none() {
            timestamp = entry.timestamp.clone();
        }

        if let Some((role, text)) = extract_codex_message(&entry) {
            let clean_text = if role == "assistant" {
                strip_thinking_blocks(&text)
            } else {
                text
            };
            if !clean_text.trim().is_empty() {
                last_message = Some(clean_text);
                if let Some(ts) = extract_codex_timestamp(&entry) {
                    last_message_at = Some(ts);
                }
            }
        }

        if entry.entry_type.as_deref() == Some("session_meta") {
            if let Some(payload) = entry.payload.as_ref() {
                if session_id.is_none() {
                    session_id = payload
                        .get("id")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                }
                if cwd.is_none() {
                    cwd = payload
                        .get("cwd")
                        .and_then(|v| v.as_str())
                        .map(|s| PathBuf::from(s));
                }
                if timestamp.is_none() {
                    timestamp = payload
                        .get("timestamp")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                }
            }
        }

        if first_message.is_none() {
            if let Some(text) = extract_codex_user_message(&entry) {
                first_message = Some(text);
            }
        }

        if error_summary.is_none() {
            if let Some(summary) = extract_codex_error_summary(&entry) {
                error_summary = Some(summary);
            }
        }
    }

    let session = AiSession {
        session_id: session_id.unwrap_or_else(|| fallback_id.to_string()),
        provider: Provider::Codex,
        timestamp,
        last_message_at,
        last_message,
        first_message,
        error_summary,
    };

    Some((session, cwd))
}

fn read_codex_sessions_for_path(path: &PathBuf) -> Result<Vec<AiSession>> {
    let sessions_dir = get_codex_sessions_dir();
    if !sessions_dir.exists() {
        return Ok(vec![]);
    }

    let mut sessions = Vec::new();
    let target = path.to_string_lossy();

    for file_path in collect_codex_session_files(&sessions_dir) {
        let filename = file_path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let Some((session, cwd)) = parse_codex_session_file(&file_path, filename) else {
            continue;
        };

        if let Some(cwd_path) = cwd {
            if cwd_path.to_string_lossy() == target {
                sessions.push(session);
            }
        }
    }

    Ok(sessions)
}

fn collect_codex_session_files(root: &PathBuf) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.clone()];

    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(v) => v,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().map(|e| e == "jsonl").unwrap_or(false) {
                out.push(path);
            }
        }
    }

    out
}

/// Get the most recent session ID for this project.
fn get_most_recent_session_id() -> Result<Option<String>> {
    let sessions = read_sessions_for_project(Provider::All)?;
    Ok(sessions.first().map(|s| s.session_id.clone()))
}

/// Entry for fzf selection
struct FzfSessionEntry {
    display: String,
    session_id: String,
    provider: Provider,
}

/// List all sessions and let user fuzzy-select one to resume.
fn list_sessions(provider: Provider) -> Result<()> {
    // Auto-import any new sessions silently
    auto_import_sessions()?;

    let index = load_index()?;
    let sessions = read_sessions_for_project(provider)?;

    if index.sessions.is_empty() && sessions.is_empty() {
        let provider_name = match provider {
            Provider::Claude => "Claude",
            Provider::Codex => "Codex",
            Provider::All => "AI",
        };
        println!("No {} sessions found for this project.", provider_name);
        println!("\nTip: Run `claude` or `codex` in this directory to start a session,");
        println!("     then use `f ai save <name>` to bookmark it.");
        return Ok(());
    }

    // Build entries for fzf - combine saved metadata with session data
    let mut entries: Vec<FzfSessionEntry> = Vec::new();

    // Process all sessions, enriching with saved names where available
    for session in &sessions {
        // Skip sessions without timestamps or content
        if session.timestamp.is_none()
            && session.last_message_at.is_none()
            && session.last_message.is_none()
            && session.first_message.is_none()
            && session.error_summary.is_none()
        {
            continue;
        }

        let relative_time = session
            .last_message_at
            .as_deref()
            .or(session.timestamp.as_deref())
            .map(format_relative_time)
            .unwrap_or_else(|| "".to_string());

        // Check if this session has a human-assigned name (not auto-generated)
        let saved_name = index
            .sessions
            .iter()
            .find(|(_, s)| s.id == session.session_id)
            .map(|(name, _)| name.as_str())
            .filter(|name| !is_auto_generated_name(name));

        let summary = session
            .last_message
            .as_deref()
            .or(session.first_message.as_deref())
            .or(session.error_summary.as_deref())
            .unwrap_or("");
        let summary_clean = clean_summary(summary);
        let id_short = &session.session_id[..8.min(session.session_id.len())];

        // Add provider indicator when showing all
        let provider_tag = if provider == Provider::All {
            match session.provider {
                Provider::Claude => "claude | ",
                Provider::Codex => "codex | ",
                Provider::All => "",
            }
        } else {
            ""
        };

        let display = if let Some(name) = saved_name {
            // For named sessions, show: [provider] name | time | summary
            format!(
                "{}{} | {} | {}",
                provider_tag,
                name,
                relative_time,
                truncate_str(&summary_clean, 40)
            )
        } else {
            // For other sessions, show: [provider] time | summary
            format!(
                "{}{} | {} | {}",
                provider_tag,
                relative_time,
                truncate_str(&summary_clean, 60),
                id_short
            )
        };

        entries.push(FzfSessionEntry {
            display,
            session_id: session.session_id.clone(),
            provider: session.provider,
        });
    }

    if entries.is_empty() {
        println!("No sessions available.");
        return Ok(());
    }

    // Check for fzf
    if which::which("fzf").is_err() {
        println!("fzf not found – install it for fuzzy selection.");
        println!("\nSessions:");
        for entry in &entries {
            println!("{}", entry.display);
        }
        return Ok(());
    }

    // Run fzf
    if let Some(selected) = run_session_fzf(&entries)? {
        println!(
            "Resuming session {}...",
            &selected.session_id[..8.min(selected.session_id.len())]
        );
        launch_session(&selected.session_id, selected.provider)?;
    }

    Ok(())
}

/// Run fzf and return the selected session entry.
fn run_session_fzf(entries: &[FzfSessionEntry]) -> Result<Option<&FzfSessionEntry>> {
    let mut child = Command::new("fzf")
        .arg("--prompt")
        .arg("ai> ")
        .arg("--ansi")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("failed to spawn fzf")?;

    {
        let stdin = child.stdin.as_mut().context("failed to open fzf stdin")?;
        for entry in entries {
            writeln!(stdin, "{}", entry.display)?;
        }
    }

    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Ok(None);
    }

    let selection = String::from_utf8(output.stdout).context("fzf output was not valid UTF-8")?;
    let selection = selection.trim();

    if selection.is_empty() {
        return Ok(None);
    }

    Ok(entries.iter().find(|e| e.display == selection))
}

/// Launch a session with the appropriate CLI. Returns true if successful, false if failed.
fn launch_session(session_id: &str, provider: Provider) -> Result<bool> {
    let status = match provider {
        Provider::Claude | Provider::All => {
            // Claude uses: claude --resume <session_id> --dangerously-skip-permissions
            Command::new("claude")
                .arg("--resume")
                .arg(session_id)
                .arg("--dangerously-skip-permissions")
                .status()
                .with_context(|| "failed to launch claude")?
        }
        Provider::Codex => {
            // Codex uses: codex resume <session_id> --dangerously-bypass-approvals-and-sandbox
            Command::new("codex")
                .arg("resume")
                .arg(session_id)
                .arg("--dangerously-bypass-approvals-and-sandbox")
                .status()
                .with_context(|| "failed to launch codex")?
        }
    };

    Ok(status.success())
}

/// Quick start: continue last session or create new one with dangerous flags.
pub fn quick_start_session(provider: Provider) -> Result<()> {
    // Auto-import any new sessions silently
    let _ = auto_import_sessions();

    let sessions = read_sessions_for_project(provider)?;

    // Find first session that has actual content (messages)
    let valid_session = sessions
        .iter()
        .find(|s| s.last_message.is_some() || s.first_message.is_some());

    if let Some(sess) = valid_session {
        let launched = launch_session(&sess.session_id, sess.provider)?;
        if !launched {
            // Session not found, start a new one
            new_session(provider)?;
        }
    } else {
        new_session(provider)?;
    }

    Ok(())
}

/// Start a new session with dangerous flags (ignores existing sessions).
fn new_session(provider: Provider) -> Result<()> {
    let status = match provider {
        Provider::Claude | Provider::All => Command::new("claude")
            .arg("--dangerously-skip-permissions")
            .status()
            .with_context(|| "failed to launch claude")?,
        Provider::Codex => Command::new("codex")
            .arg("--yolo")
            .arg("--sandbox")
            .arg("danger-full-access")
            .status()
            .with_context(|| "failed to launch codex")?,
    };

    let name = match provider {
        Provider::Claude | Provider::All => "claude",
        Provider::Codex => "codex",
    };

    if !status.success() {
        bail!("{} exited with status {}", name, status);
    }

    Ok(())
}

/// Copy session history to clipboard.
fn copy_session(session: Option<String>, provider: Provider) -> Result<()> {
    // Auto-import any new sessions silently
    auto_import_sessions()?;

    if session.is_none() && provider != Provider::All {
        return copy_last_session(provider, None);
    }

    // Handle provider shortcuts: "claude" or "codex" -> copy last session for that provider
    if let Some(ref query) = session {
        let q = query.to_lowercase();
        if q == "claude" || q == "c" {
            return copy_last_session(Provider::Claude, None);
        }
        if q == "codex" || q == "x" {
            return copy_last_session(Provider::Codex, None);
        }
    }

    let index = load_index()?;
    let sessions = read_sessions_for_project(provider)?;

    if sessions.is_empty() {
        let provider_name = match provider {
            Provider::Claude => "Claude",
            Provider::Codex => "Codex",
            Provider::All => "AI",
        };
        println!("No {} sessions found for this project.", provider_name);
        return Ok(());
    }

    if session.is_none() && !io::stdin().is_terminal() {
        bail!("no session specified (interactive selection requires a TTY)");
    }

    // Find the session ID and provider
    let (session_id, session_provider) = if let Some(ref query) = session {
        // Try to find by name or ID
        if let Some((_, saved)) = index
            .sessions
            .iter()
            .find(|(name, _)| name.as_str() == query)
        {
            // Find the provider for this session
            let prov = sessions
                .iter()
                .find(|s| s.session_id == saved.id)
                .map(|s| s.provider)
                .unwrap_or(Provider::Claude);
            (saved.id.clone(), prov)
        } else if let Some(s) = sessions
            .iter()
            .find(|s| s.session_id == *query || s.session_id.starts_with(query))
        {
            (s.session_id.clone(), s.provider)
        } else {
            bail!("Session not found: {}", query);
        }
    } else {
        // Show fzf selection
        let mut entries: Vec<FzfSessionEntry> = Vec::new();

        for session in &sessions {
            if session.timestamp.is_none()
                && session.last_message_at.is_none()
                && session.last_message.is_none()
                && session.first_message.is_none()
                && session.error_summary.is_none()
            {
                continue;
            }

            let relative_time = session
                .last_message_at
                .as_deref()
                .or(session.timestamp.as_deref())
                .map(format_relative_time)
                .unwrap_or_else(|| "".to_string());

            let saved_name = index
                .sessions
                .iter()
                .find(|(_, s)| s.id == session.session_id)
                .map(|(name, _)| name.as_str())
                .filter(|name| !is_auto_generated_name(name));

            let summary = session
                .last_message
                .as_deref()
                .or(session.first_message.as_deref())
                .or(session.error_summary.as_deref())
                .unwrap_or("");
            let summary_clean = clean_summary(summary);
            let id_short = &session.session_id[..8.min(session.session_id.len())];

            // Add provider indicator when showing all
            let provider_tag = if provider == Provider::All {
                match session.provider {
                    Provider::Claude => "claude | ",
                    Provider::Codex => "codex | ",
                    Provider::All => "",
                }
            } else {
                ""
            };

            let display = if let Some(name) = saved_name {
                format!(
                    "{}{} | {} | {}",
                    provider_tag,
                    name,
                    relative_time,
                    truncate_str(&summary_clean, 40)
                )
            } else {
                format!(
                    "{}{} | {} | {}",
                    provider_tag,
                    relative_time,
                    truncate_str(&summary_clean, 60),
                    id_short
                )
            };

            entries.push(FzfSessionEntry {
                display,
                session_id: session.session_id.clone(),
                provider: session.provider,
            });
        }

        if entries.is_empty() {
            println!("No sessions available.");
            return Ok(());
        }

        if which::which("fzf").is_err() {
            bail!("fzf not found – install it for fuzzy selection");
        }

        let Some(selected) = run_session_fzf(&entries)? else {
            return Ok(());
        };

        (selected.session_id.clone(), selected.provider)
    };

    // Read and format the session history
    let history = read_session_history(&session_id, session_provider)?;

    // Copy to clipboard
    copy_to_clipboard(&history)?;

    let line_count = history.lines().count();
    println!("Copied session history ({} lines) to clipboard", line_count);

    Ok(())
}

/// Copy the most recent session for a provider directly (no fzf selection).
/// If search query is provided, searches ALL sessions globally for matching content.
fn copy_last_session(provider: Provider, search: Option<String>) -> Result<()> {
    // Auto-import any new sessions silently
    auto_import_sessions()?;

    // If search query provided, search all sessions globally
    if let Some(query) = search {
        return copy_session_by_search(provider, &query);
    }

    let sessions = read_sessions_for_project(provider)?;

    if sessions.is_empty() {
        let provider_name = match provider {
            Provider::Claude => "Claude",
            Provider::Codex => "Codex",
            Provider::All => "AI",
        };
        println!("No {} sessions found for this project.", provider_name);
        return Ok(());
    }

    // sessions are already sorted by most recent first
    let session = &sessions[0];

    // Read and format the session history
    let history = read_session_history(&session.session_id, session.provider)?;

    // Copy to clipboard
    copy_to_clipboard(&history)?;

    let line_count = history.lines().count();
    let id_short = &session.session_id[..8.min(session.session_id.len())];
    println!(
        "Copied session {} ({} lines) to clipboard",
        id_short, line_count
    );

    Ok(())
}

/// Search all sessions globally for content matching the query.
fn copy_session_by_search(provider: Provider, query: &str) -> Result<()> {
    let query_lower = query.to_lowercase();

    // Search Codex sessions
    if provider == Provider::Codex || provider == Provider::All {
        let sessions_dir = get_codex_sessions_dir();
        if sessions_dir.exists() {
            for file_path in collect_codex_session_files(&sessions_dir) {
                // Read raw content and check for query
                if let Ok(content) = fs::read_to_string(&file_path) {
                    if content.to_lowercase().contains(&query_lower) {
                        // Found a match - get session ID and read formatted history
                        let filename = file_path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                        let session_id = filename.split('_').next().unwrap_or(filename);

                        let history = read_session_history(session_id, Provider::Codex)?;
                        copy_to_clipboard(&history)?;

                        let line_count = history.lines().count();
                        let id_short = &session_id[..8.min(session_id.len())];

                        // Try to get project path from session
                        if let Some((_, cwd)) = parse_codex_session_file(&file_path, filename) {
                            if let Some(project_path) = cwd {
                                println!(
                                    "Copied session {} from {} ({} lines) to clipboard",
                                    id_short,
                                    project_path.display(),
                                    line_count
                                );
                                return Ok(());
                            }
                        }

                        println!(
                            "Copied session {} ({} lines) to clipboard",
                            id_short, line_count
                        );
                        return Ok(());
                    }
                }
            }
        }
    }

    // Search Claude sessions
    if provider == Provider::Claude || provider == Provider::All {
        let projects_dir = get_claude_projects_dir();
        if projects_dir.exists() {
            if let Ok(entries) = fs::read_dir(&projects_dir) {
                for entry in entries.flatten() {
                    let project_dir = entry.path();
                    if !project_dir.is_dir() {
                        continue;
                    }
                    if let Ok(files) = fs::read_dir(&project_dir) {
                        for file in files.flatten() {
                            let file_path = file.path();
                            if file_path.extension().map(|e| e == "jsonl").unwrap_or(false) {
                                if let Ok(content) = fs::read_to_string(&file_path) {
                                    if content.to_lowercase().contains(&query_lower) {
                                        let session_id = file_path
                                            .file_stem()
                                            .and_then(|s| s.to_str())
                                            .unwrap_or("");

                                        let history =
                                            read_session_history(session_id, Provider::Claude)?;
                                        copy_to_clipboard(&history)?;

                                        let line_count = history.lines().count();
                                        let id_short = &session_id[..8.min(session_id.len())];
                                        let project_name = project_dir
                                            .file_name()
                                            .and_then(|s| s.to_str())
                                            .unwrap_or("unknown");

                                        println!(
                                            "Copied session {} from {} ({} lines) to clipboard",
                                            id_short, project_name, line_count
                                        );
                                        return Ok(());
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    println!("No session found containing: {}", query);
    Ok(())
}

/// Read full session history from JSONL file and format as conversation.
fn read_session_history(session_id: &str, provider: Provider) -> Result<String> {
    let session_file = if provider == Provider::Codex {
        // Codex stores sessions in ~/.codex/sessions/ with different structure
        find_codex_session_file(session_id)
            .ok_or_else(|| anyhow::anyhow!("Codex session file not found: {}", session_id))?
    } else {
        let cwd = std::env::current_dir()?;
        let cwd_str = cwd.to_string_lossy().to_string();
        let project_folder = path_to_project_name(&cwd_str);
        let projects_dir = get_claude_projects_dir();
        projects_dir
            .join(&project_folder)
            .join(format!("{}.jsonl", session_id))
    };

    if !session_file.exists() {
        bail!("Session file not found: {}", session_file.display());
    }

    let content = fs::read_to_string(&session_file).context("failed to read session file")?;

    let mut history = String::new();

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }

        let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };

        // Try Claude format first (entry.message.role + entry.message.content)
        if let Some(msg) = entry.get("message") {
            let role = msg
                .get("role")
                .and_then(|r| r.as_str())
                .unwrap_or("unknown");
            let role_label = match role {
                "user" => "Human",
                "assistant" => "Assistant",
                _ => role,
            };

            let content_text = extract_content_text(msg.get("content"));
            let cleaned = strip_system_reminders(&content_text);
            if !cleaned.is_empty() && !is_session_boilerplate(&cleaned) {
                history.push_str(&format!("{}: {}\n\n", role_label, cleaned));
            }
            continue;
        }

        // Try Codex format (type: response_item, payload.type: message)
        if entry.get("type").and_then(|t| t.as_str()) == Some("response_item") {
            if let Some(payload) = entry.get("payload") {
                if payload.get("type").and_then(|t| t.as_str()) == Some("message") {
                    let role = payload
                        .get("role")
                        .and_then(|r| r.as_str())
                        .unwrap_or("unknown");
                    let role_label = match role {
                        "user" => "Human",
                        "assistant" => "Assistant",
                        _ => role,
                    };

                    let content_text = extract_content_text(payload.get("content"));
                    let cleaned = strip_system_reminders(&content_text);
                    if !cleaned.is_empty() && !is_session_boilerplate(&cleaned) {
                        history.push_str(&format!("{}: {}\n\n", role_label, cleaned));
                    }
                }
            }
        }
    }

    Ok(history)
}

/// Extract text content from various content formats.
fn extract_content_text(content: Option<&serde_json::Value>) -> String {
    let Some(content) = content else {
        return String::new();
    };

    match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => {
            arr.iter()
                .filter_map(|v| {
                    // Handle text blocks (Claude uses "text", Codex uses "text" in input_text type)
                    v.get("text")
                        .and_then(|t| t.as_str())
                        .map(|s| s.to_string())
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        _ => String::new(),
    }
}

/// Strip <system-reminder>...</system-reminder> blocks from text.
fn strip_system_reminders(text: &str) -> String {
    let mut result = text.to_string();
    while let Some(start) = result.find("<system-reminder>") {
        if let Some(end) = result[start..].find("</system-reminder>") {
            let end_pos = start + end + "</system-reminder>".len();
            result = format!("{}{}", &result[..start], &result[end_pos..]);
        } else {
            // Unclosed tag - remove from start to end
            result = result[..start].to_string();
            break;
        }
    }
    result.trim().to_string()
}

/// Check if content is boilerplate that should be skipped.
fn is_session_boilerplate(text: &str) -> bool {
    let trimmed = text.trim();

    // === Codex boilerplate ===
    // Skip agents.md instructions
    if trimmed.starts_with("# AGENTS.md instructions")
        || trimmed.starts_with("# agents.md instructions")
    {
        return true;
    }
    // Skip environment context
    if trimmed.starts_with("<environment_context>") {
        return true;
    }
    // Skip instructions blocks
    if trimmed.starts_with("<INSTRUCTIONS>") {
        return true;
    }
    // Skip permissions instructions (Codex system context)
    if trimmed.contains("<permissions instructions>") {
        return true;
    }
    // Skip developer role messages with system instructions
    if trimmed.starts_with("developer:") {
        return true;
    }
    // Skip short status messages (likely action summaries)
    if trimmed.len() < 50 && !trimmed.contains(' ') {
        return true;
    }
    // Skip skill usage announcements
    if trimmed.starts_with("Using ") && trimmed.contains("skill") {
        return true;
    }

    // === Claude boilerplate ===
    // Skip system reminders
    if trimmed.starts_with("<system-reminder>") {
        return true;
    }
    // Skip messages that are only system reminders
    if trimmed.contains("<system-reminder>")
        && !trimmed.contains("Human:")
        && !trimmed.contains("Assistant:")
    {
        // Check if the non-reminder content is minimal
        let without_reminders = trimmed
            .split("<system-reminder>")
            .next()
            .unwrap_or("")
            .trim();
        if without_reminders.is_empty() {
            return true;
        }
    }

    false
}

/// Copy last prompt and response from a session to clipboard.
fn copy_context(
    session: Option<String>,
    provider: Provider,
    count: usize,
    path: Option<String>,
) -> Result<()> {
    // Auto-import any new sessions silently
    auto_import_sessions()?;

    // Treat "-" as None (trigger fuzzy search)
    let mut session = session.filter(|s| s != "-");
    let mut path = path;

    // Allow `f ai context .` to mean "use current path" instead of a session ID.
    if path.is_none() {
        if let Some(ref candidate) = session {
            let candidate_path = PathBuf::from(candidate);
            if candidate == "." || candidate == ".." || candidate_path.exists() {
                path = Some(candidate.clone());
                session = None;
            }
        }
    }

    // Determine project path
    let project_path = if let Some(ref p) = path {
        PathBuf::from(p)
    } else {
        std::env::current_dir()?
    };

    let index = load_index()?;
    let sessions = read_sessions_for_path(provider, &project_path)?;

    if sessions.is_empty() {
        let provider_name = match provider {
            Provider::Claude => "Claude",
            Provider::Codex => "Codex",
            Provider::All => "AI",
        };
        println!("No {} sessions found for this project.", provider_name);
        return Ok(());
    }

    // Find the session ID and provider
    let (session_id, session_provider) = if let Some(ref query) = session {
        // Try to find by name or ID
        if let Some((_, saved)) = index
            .sessions
            .iter()
            .find(|(name, _)| name.as_str() == query)
        {
            let prov = sessions
                .iter()
                .find(|s| s.session_id == saved.id)
                .map(|s| s.provider)
                .unwrap_or(Provider::Claude);
            (saved.id.clone(), prov)
        } else if let Some(s) = sessions
            .iter()
            .find(|s| s.session_id == *query || s.session_id.starts_with(query))
        {
            (s.session_id.clone(), s.provider)
        } else {
            bail!("Session not found: {}", query);
        }
    } else {
        // Show fzf selection
        let mut entries: Vec<FzfSessionEntry> = Vec::new();

        for session in &sessions {
            if session.timestamp.is_none()
                && session.last_message_at.is_none()
                && session.last_message.is_none()
                && session.first_message.is_none()
                && session.error_summary.is_none()
            {
                continue;
            }

            let relative_time = session
                .last_message_at
                .as_deref()
                .or(session.timestamp.as_deref())
                .map(format_relative_time)
                .unwrap_or_else(|| "".to_string());

            let saved_name = index
                .sessions
                .iter()
                .find(|(_, s)| s.id == session.session_id)
                .map(|(name, _)| name.as_str())
                .filter(|name| !is_auto_generated_name(name));

            let summary = session
                .last_message
                .as_deref()
                .or(session.first_message.as_deref())
                .or(session.error_summary.as_deref())
                .unwrap_or("");
            let summary_clean = clean_summary(summary);
            let id_short = &session.session_id[..8.min(session.session_id.len())];

            let provider_tag = if provider == Provider::All {
                match session.provider {
                    Provider::Claude => "claude | ",
                    Provider::Codex => "codex | ",
                    Provider::All => "",
                }
            } else {
                ""
            };

            let display = if let Some(name) = saved_name {
                format!(
                    "{}{} | {} | {}",
                    provider_tag,
                    name,
                    relative_time,
                    truncate_str(&summary_clean, 40)
                )
            } else {
                format!(
                    "{}{} | {} | {}",
                    provider_tag,
                    relative_time,
                    truncate_str(&summary_clean, 60),
                    id_short
                )
            };

            entries.push(FzfSessionEntry {
                display,
                session_id: session.session_id.clone(),
                provider: session.provider,
            });
        }

        if entries.is_empty() {
            println!("No sessions available.");
            return Ok(());
        }

        if which::which("fzf").is_err() {
            bail!("fzf not found – install it for fuzzy selection");
        }

        let Some(selected) = run_session_fzf(&entries)? else {
            return Ok(());
        };

        (selected.session_id.clone(), selected.provider)
    };

    // Read the last N exchanges
    let context = read_last_context(&session_id, session_provider, count, &project_path)?;

    // Copy to clipboard
    copy_to_clipboard(&context)?;

    let exchange_word = if count == 1 { "exchange" } else { "exchanges" };
    let line_count = context.lines().count();
    println!(
        "Copied last {} {} ({} lines) to clipboard",
        count, exchange_word, line_count
    );

    Ok(())
}

/// Read last N user prompts and assistant responses from a session.
fn read_last_context(
    session_id: &str,
    provider: Provider,
    count: usize,
    project_path: &PathBuf,
) -> Result<String> {
    if provider == Provider::Codex {
        let session_file = find_codex_session_file(session_id).ok_or_else(|| {
            anyhow::anyhow!("Session file not found for Codex session {}", session_id)
        })?;
        return read_codex_last_context(&session_file, count);
    }

    let path_str = project_path.to_string_lossy().to_string();
    let project_folder = path_to_project_name(&path_str);

    let projects_dir = match provider {
        Provider::Claude | Provider::All => get_claude_projects_dir(),
        Provider::Codex => get_codex_projects_dir(),
    };

    let session_file = projects_dir
        .join(&project_folder)
        .join(format!("{}.jsonl", session_id));

    if !session_file.exists() {
        bail!("Session file not found: {}", session_file.display());
    }

    let content = fs::read_to_string(&session_file).context("failed to read session file")?;

    // Collect all exchanges (user + assistant pairs)
    let mut exchanges: Vec<(String, String)> = Vec::new();
    let mut current_user: Option<String> = None;

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }

        if let Ok(entry) = serde_json::from_str::<JsonlEntry>(line) {
            if let Some(ref msg) = entry.message {
                let role = msg.role.as_deref().unwrap_or("unknown");

                let content_text = if let Some(ref content) = msg.content {
                    match content {
                        serde_json::Value::String(s) => s.clone(),
                        serde_json::Value::Array(arr) => arr
                            .iter()
                            .filter_map(|v| {
                                if let Some(text) = v.get("text").and_then(|t| t.as_str()) {
                                    Some(text.to_string())
                                } else {
                                    None
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                        _ => continue,
                    }
                } else {
                    continue;
                };

                if content_text.is_empty() {
                    continue;
                }

                match role {
                    "user" => {
                        current_user = Some(content_text);
                    }
                    "assistant" => {
                        let clean_text = strip_thinking_blocks(&content_text);
                        if clean_text.trim().is_empty() {
                            continue;
                        }
                        if let Some(user_msg) = current_user.take() {
                            exchanges.push((user_msg, clean_text));
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    if exchanges.is_empty() {
        bail!("No exchanges found in session");
    }

    // Take the last N exchanges
    let start = exchanges.len().saturating_sub(count);
    let last_exchanges = &exchanges[start..];

    // Format the context
    let mut context = String::new();

    for (user_msg, assistant_msg) in last_exchanges {
        context.push_str("Human: ");
        context.push_str(user_msg);
        context.push_str("\n\n");
        context.push_str("Assistant: ");
        context.push_str(assistant_msg);
        context.push_str("\n\n");
    }

    // Remove trailing newlines
    while context.ends_with('\n') {
        context.pop();
    }
    context.push('\n');

    Ok(context)
}

/// Copy text to system clipboard.
fn copy_to_clipboard(text: &str) -> Result<()> {
    if std::env::var("FLOW_NO_CLIPBOARD").is_ok() || !std::io::stdin().is_terminal() {
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        let mut child = Command::new("pbcopy")
            .stdin(Stdio::piped())
            .spawn()
            .context("failed to spawn pbcopy")?;

        if let Some(stdin) = child.stdin.as_mut() {
            stdin.write_all(text.as_bytes())?;
        }

        child.wait()?;
    }

    #[cfg(target_os = "linux")]
    {
        // Try xclip first, then xsel
        let result = Command::new("xclip")
            .arg("-selection")
            .arg("clipboard")
            .stdin(Stdio::piped())
            .spawn();

        let mut child = match result {
            Ok(c) => c,
            Err(_) => Command::new("xsel")
                .arg("--clipboard")
                .arg("--input")
                .stdin(Stdio::piped())
                .spawn()
                .context("failed to spawn xclip or xsel")?,
        };

        if let Some(stdin) = child.stdin.as_mut() {
            stdin.write_all(text.as_bytes())?;
        }

        child.wait()?;
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        bail!("clipboard not supported on this platform");
    }

    Ok(())
}

/// Strip <thinking> blocks from content (internal Claude processing).
fn strip_thinking_blocks(s: &str) -> String {
    let mut remaining = s;
    let mut out = String::new();

    loop {
        let Some(start) = remaining.find("<thinking>") else {
            out.push_str(remaining);
            break;
        };

        out.push_str(&remaining[..start]);
        let after_start = &remaining[start + "<thinking>".len()..];

        let Some(end) = after_start.find("</thinking>") else {
            break;
        };

        remaining = &after_start[end + "</thinking>".len()..];
    }

    out
}

fn truncate_str(s: &str, max: usize) -> String {
    let first_line = s.lines().next().unwrap_or(s);

    if first_line.chars().count() <= max {
        first_line.to_string()
    } else {
        let take_len = max.saturating_sub(3);
        let truncated: String = first_line.chars().take(take_len).collect();
        format!("{}...", truncated)
    }
}

/// Format timestamp as relative time (e.g., "3 days ago", "2 hours ago").
fn format_relative_time(ts: &str) -> String {
    // Parse ISO 8601 timestamp: "2025-12-09T19:21:15.562Z"
    let parsed = chrono::DateTime::parse_from_rfc3339(ts).or_else(|_| {
        // Try without timezone
        chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%S%.fZ")
            .map(|dt| dt.and_utc().fixed_offset())
    });

    let Ok(dt) = parsed else {
        return "unknown".to_string();
    };

    let now = chrono::Utc::now();
    let duration = now.signed_duration_since(dt);

    let seconds = duration.num_seconds();
    if seconds < 0 {
        return "just now".to_string();
    }

    let minutes = duration.num_minutes();
    let hours = duration.num_hours();
    let days = duration.num_days();
    let weeks = days / 7;

    if seconds < 60 {
        "just now".to_string()
    } else if minutes < 60 {
        format!("{}m ago", minutes)
    } else if hours < 24 {
        format!("{}h ago", hours)
    } else if days == 1 {
        "yesterday".to_string()
    } else if days < 7 {
        format!("{}d ago", days)
    } else if weeks < 4 {
        format!("{}w ago", weeks)
    } else {
        // Show date for older sessions
        dt.format("%b %d").to_string()
    }
}

/// Check if a session name looks auto-generated (from import).
fn is_auto_generated_name(name: &str) -> bool {
    // Auto-generated names start with date like "20251215-" or "unknown-session"
    name.starts_with("202") && name.chars().nth(8) == Some('-')
        || name.starts_with("unknown-session")
}

fn extract_error_summary(entry: &JsonlEntry) -> Option<String> {
    let entry_type = entry.entry_type.as_deref();
    let subtype = entry.subtype.as_deref();
    let level = entry.level.as_deref();

    let is_error = level == Some("error")
        || entry_type == Some("error")
        || subtype.map(|s| s.contains("error")).unwrap_or(false)
        || entry.error.is_some();

    if !is_error {
        return None;
    }

    let mut summary = if let Some(sub) = subtype {
        format!("error: {}", sub)
    } else if let Some(kind) = entry_type {
        format!("error: {}", kind)
    } else {
        "error".to_string()
    };

    if let Some(err) = &entry.error {
        let msg = err
            .get("message")
            .and_then(|v| v.as_str())
            .or_else(|| err.get("error").and_then(|v| v.as_str()));
        if let Some(msg) = msg {
            summary = format!("{}: {}", summary, msg);
        }
    }

    Some(summary)
}

fn extract_codex_user_message(entry: &CodexEntry) -> Option<String> {
    let entry_type = entry.entry_type.as_deref();

    if entry_type == Some("response_item") {
        let payload = entry.payload.as_ref()?;
        if payload.get("type").and_then(|v| v.as_str()) != Some("message") {
            return None;
        }
        if payload.get("role").and_then(|v| v.as_str()) != Some("user") {
            return None;
        }
        return extract_codex_content_text(payload.get("content")?);
    }

    if entry_type == Some("event_msg") {
        let payload = entry.payload.as_ref()?;
        let payload_type = payload.get("type").and_then(|v| v.as_str());
        if payload_type == Some("user_message") {
            return payload
                .get("message")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
        }
    }

    if entry_type == Some("message") && entry.role.as_deref() == Some("user") {
        if let Some(content) = entry.content.as_ref() {
            return extract_codex_content_text(content);
        }
    }

    None
}

fn extract_codex_error_summary(entry: &CodexEntry) -> Option<String> {
    let entry_type = entry.entry_type.as_deref();
    let payload = entry.payload.as_ref();

    let is_error = entry_type == Some("error")
        || payload
            .and_then(|p| p.get("type").and_then(|v| v.as_str()))
            .map(|t| t.contains("error"))
            .unwrap_or(false);

    if !is_error {
        return None;
    }

    let mut summary = if let Some(t) = entry_type {
        format!("error: {}", t)
    } else {
        "error".to_string()
    };

    if let Some(p) = payload {
        if let Some(msg) = p.get("message").and_then(|v| v.as_str()) {
            summary = format!("{}: {}", summary, msg);
        }
    }

    Some(summary)
}

fn extract_codex_content_text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(arr) => {
            let mut parts = Vec::new();
            for item in arr {
                if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                    parts.push(text.to_string());
                    continue;
                }
                if let Some(text) = item.get("input_text").and_then(|v| v.as_str()) {
                    parts.push(text.to_string());
                    continue;
                }
                if let Some(text) = item.get("output_text").and_then(|v| v.as_str()) {
                    parts.push(text.to_string());
                    continue;
                }
            }
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n"))
            }
        }
        _ => None,
    }
}

/// Clean up a summary string - remove noise, paths, special chars.
fn clean_summary(s: &str) -> String {
    // Take first meaningful line (skip empty lines and lines starting with special chars)
    let meaningful_line = s
        .lines()
        .map(|l| l.trim())
        .find(|l| {
            !l.is_empty()
                && !l.starts_with('~')
                && !l.starts_with('/')
                && !l.starts_with('>')
                && !l.starts_with('❯')
                && !l.starts_with('$')
                && !l.starts_with('#')
                && !l.starts_with("Error:")
                && !l.starts_with("<INSTRUCTIONS>")
                && !l.starts_with("## Skills")
        })
        .or_else(|| s.lines().find(|l| !l.trim().is_empty()))
        .unwrap_or(s);

    // Clean up the line
    meaningful_line.trim().replace('\t', " ").replace("  ", " ")
}

const GEMINI_API_URL: &str = "https://generativelanguage.googleapis.com/v1beta/models";
const DEFAULT_GEMINI_MODEL: &str = "gemini-1.5-flash";
const DEFAULT_SUMMARY_AGE_MINUTES: i64 = 45;
const DEFAULT_SUMMARY_MAX_CHARS: usize = 12_000;
const DEFAULT_HANDOFF_MAX_CHARS: usize = 6_000;

fn get_session_summaries_path(project_path: &PathBuf) -> PathBuf {
    project_path
        .join(".ai")
        .join("internal")
        .join("session-summaries.json")
}

fn load_session_summaries(project_path: &PathBuf) -> Result<SessionSummaries> {
    let path = get_session_summaries_path(project_path);
    if !path.exists() {
        return Ok(SessionSummaries::default());
    }
    let content =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&content).context("failed to parse session-summaries.json")
}

fn save_session_summaries(project_path: &PathBuf, summaries: &SessionSummaries) -> Result<()> {
    let path = get_session_summaries_path(project_path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(summaries)?;
    fs::write(&path, content)?;
    Ok(())
}

fn summary_key(session: &CrossProjectSession) -> String {
    let provider = match session.provider {
        Provider::Claude => "claude",
        Provider::Codex => "codex",
        Provider::All => "ai",
    };
    format!("{}:{}", provider, session.session_id)
}

fn get_summary_cache_entry<'a>(
    cache: &'a mut HashMap<PathBuf, SummaryCacheEntry>,
    project_path: &PathBuf,
) -> Result<&'a mut SummaryCacheEntry> {
    if !cache.contains_key(project_path) {
        let store = load_session_summaries(project_path)?;
        cache.insert(
            project_path.clone(),
            SummaryCacheEntry {
                store,
                dirty: false,
            },
        );
    }
    Ok(cache.get_mut(project_path).expect("cache entry must exist"))
}

fn summary_age_minutes() -> i64 {
    std::env::var("FLOW_SESSIONS_SUMMARY_AGE_MINUTES")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(DEFAULT_SUMMARY_AGE_MINUTES)
}

fn summary_max_chars() -> usize {
    std::env::var("FLOW_SESSIONS_SUMMARY_MAX_CHARS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(DEFAULT_SUMMARY_MAX_CHARS)
}

fn handoff_max_chars() -> usize {
    std::env::var("FLOW_SESSIONS_HANDOFF_MAX_CHARS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(DEFAULT_HANDOFF_MAX_CHARS)
}

fn gemini_model() -> String {
    std::env::var("GEMINI_MODEL").unwrap_or_else(|_| DEFAULT_GEMINI_MODEL.to_string())
}

fn get_gemini_api_key() -> Result<String> {
    if let Ok(key) = std::env::var("GEMINI_API_KEY") {
        if !key.trim().is_empty() {
            return Ok(key);
        }
    }
    if let Ok(key) = std::env::var("GOOGLE_API_KEY") {
        if !key.trim().is_empty() {
            return Ok(key);
        }
    }

    if let Ok(Some(key)) = crate::env::get_personal_env_var("GEMINI_API_KEY") {
        if !key.trim().is_empty() {
            return Ok(key);
        }
    }
    if let Ok(Some(key)) = crate::env::get_personal_env_var("GOOGLE_API_KEY") {
        if !key.trim().is_empty() {
            return Ok(key);
        }
    }

    bail!("Missing GEMINI_API_KEY/GOOGLE_API_KEY (set env var or add to personal env)")
}

fn truncate_for_summary(context: &str) -> String {
    let max_chars = summary_max_chars();
    if context.chars().count() <= max_chars {
        return context.to_string();
    }
    let start = context.chars().count().saturating_sub(max_chars);
    context.chars().skip(start).collect()
}

fn truncate_for_handoff(context: &str) -> String {
    let max_chars = handoff_max_chars();
    if context.chars().count() <= max_chars {
        return context.to_string();
    }
    let start = context.chars().count().saturating_sub(max_chars);
    context.chars().skip(start).collect()
}

fn should_summarize(last_ts: &str) -> bool {
    let Ok(ts) = chrono::DateTime::parse_from_rfc3339(last_ts) else {
        return false;
    };
    let age = chrono::Utc::now().signed_duration_since(ts);
    age.num_minutes() >= summary_age_minutes()
}

fn summarize_session_with_gemini(context: &str) -> Result<SessionSummary> {
    let api_key = get_gemini_api_key()?;
    let model = gemini_model();

    let prompt = format!(
        "Summarize this coding session. Return JSON only with fields:\n\
summary: short 1-2 sentence summary (<= 220 chars), no boilerplate\n\
chapters: array of 3-8 items, each with title (3-8 words) and summary (1-2 sentences)\n\
\nSession:\n{}",
        truncate_for_summary(context)
    );

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("failed to create HTTP client")?;

    let url = format!(
        "{}/{}:generateContent?key={}",
        GEMINI_API_URL, model, api_key
    );
    let payload = json!({
        "contents": [
            {
                "role": "user",
                "parts": [
                    { "text": prompt }
                ]
            }
        ],
        "generationConfig": {
            "temperature": 0.2,
            "maxOutputTokens": 700,
            "responseMimeType": "application/json"
        }
    });

    let resp = client
        .post(&url)
        .json(&payload)
        .send()
        .context("failed to call Gemini API")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        bail!("Gemini API error {}: {}", status, text);
    }

    let parsed: GeminiResponse = resp.json().context("failed to parse Gemini response")?;
    let content = parsed
        .candidates
        .get(0)
        .and_then(|c| c.content.parts.get(0))
        .and_then(|p| p.text.as_deref())
        .unwrap_or("")
        .trim();

    if content.is_empty() {
        bail!("Gemini returned empty summary");
    }

    let summary_payload = parse_summary_response(content)?;

    Ok(SessionSummary {
        summary: summary_payload.summary,
        chapters: summary_payload.chapters,
        session_last_timestamp: None,
        model,
        updated_at: chrono::Utc::now().to_rfc3339(),
    })
}

fn summarize_handoff_with_gemini(context: &str) -> Result<String> {
    let api_key = get_gemini_api_key()?;
    let model = gemini_model();

    let prompt = format!(
        "Create a concise handoff for another coding agent. Plain text only.\n\
Include these sections:\n\
- Goal\n\
- Current state\n\
- Key files/paths\n\
- Pending tasks / next steps\n\
- Gotchas / blockers\n\
Keep it brief (<= 12 lines). No preamble.\n\
\nSession:\n{}",
        truncate_for_handoff(context)
    );

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("failed to create HTTP client")?;

    let url = format!(
        "{}/{}:generateContent?key={}",
        GEMINI_API_URL, model, api_key
    );
    let payload = json!({
        "contents": [
            {
                "role": "user",
                "parts": [
                    { "text": prompt }
                ]
            }
        ],
        "generationConfig": {
            "temperature": 0.2,
            "maxOutputTokens": 600,
            "responseMimeType": "text/plain"
        }
    });

    let resp = client
        .post(&url)
        .json(&payload)
        .send()
        .context("failed to call Gemini API")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        bail!("Gemini API error {}: {}", status, text);
    }

    let parsed: GeminiResponse = resp.json().context("failed to parse Gemini response")?;
    let content = parsed
        .candidates
        .get(0)
        .and_then(|c| c.content.parts.get(0))
        .and_then(|p| p.text.as_deref())
        .unwrap_or("")
        .trim();

    if content.is_empty() {
        bail!("Gemini returned empty handoff");
    }

    Ok(content.to_string())
}

fn parse_summary_response(content: &str) -> Result<SessionSummaryResponse> {
    if let Ok(parsed) = serde_json::from_str::<SessionSummaryResponse>(content) {
        return Ok(parsed);
    }

    let json_blob = extract_json_object(content)
        .ok_or_else(|| anyhow::anyhow!("summary response was not valid JSON"))?;
    serde_json::from_str(&json_blob).context("failed to parse summary JSON")
}

fn extract_json_object(s: &str) -> Option<String> {
    let start = s.find('{')?;
    let end = s.rfind('}')?;
    if end <= start {
        return None;
    }
    Some(s[start..=end].to_string())
}

#[derive(Debug, Deserialize)]
struct GeminiResponse {
    candidates: Vec<GeminiCandidate>,
}

#[derive(Debug, Deserialize)]
struct GeminiCandidate {
    content: GeminiContent,
}

#[derive(Debug, Deserialize)]
struct GeminiContent {
    parts: Vec<GeminiPart>,
}

#[derive(Debug, Deserialize)]
struct GeminiPart {
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SessionSummaryResponse {
    summary: String,
    chapters: Vec<SessionChapter>,
}

fn get_display_summary(
    session: &CrossProjectSession,
    cache: &mut HashMap<PathBuf, SummaryCacheEntry>,
) -> Result<Option<String>> {
    let key = summary_key(session);
    let entry = get_summary_cache_entry(cache, &session.project_path)?;
    if let Some(summary) = entry.store.summaries.get(&key) {
        if !summary.summary.trim().is_empty() {
            return Ok(Some(summary.summary.clone()));
        }
    }
    Ok(None)
}

/// Return provider:session_id for the most recent session in the project.
pub fn get_latest_session_ref_for_path(project_path: &PathBuf) -> Result<Option<String>> {
    let sessions = read_sessions_for_path(Provider::All, project_path)?;
    let Some(session) = sessions.first() else {
        return Ok(None);
    };
    let provider = match session.provider {
        Provider::Claude => "claude",
        Provider::Codex => "codex",
        Provider::All => "ai",
    };
    Ok(Some(format!("{}:{}", provider, session.session_id)))
}

/// Return full message history for the latest session matching a path.
pub fn get_latest_session_history_for_path(
    project_path: &PathBuf,
) -> Result<Option<SessionHistory>> {
    let sessions = read_sessions_for_path(Provider::All, project_path)?;
    let Some(session) = sessions.first() else {
        return Ok(None);
    };
    let session_messages =
        read_session_messages_for_path(project_path, &session.session_id, session.provider)?;
    let provider = match session.provider {
        Provider::Claude => "claude",
        Provider::Codex => "codex",
        Provider::All => "unknown",
    };

    let started_at = session_messages
        .started_at
        .clone()
        .or_else(|| session.timestamp.clone());
    let last_message_at = session_messages
        .last_message_at
        .clone()
        .or_else(|| session.last_message_at.clone())
        .or_else(|| started_at.clone());

    Ok(Some(SessionHistory {
        session_id: session.session_id.clone(),
        provider: provider.to_string(),
        started_at,
        last_message_at,
        messages: session_messages.messages,
    }))
}

fn maybe_update_summary(
    session: &CrossProjectSession,
    cache: &mut HashMap<PathBuf, SummaryCacheEntry>,
) -> Result<()> {
    let Some(last_ts) = get_session_last_timestamp_for_session(session)? else {
        return Ok(());
    };

    if !should_summarize(&last_ts) {
        return Ok(());
    }

    let key = summary_key(session);
    let entry = get_summary_cache_entry(cache, &session.project_path)?;
    if let Some(existing) = entry.store.summaries.get(&key) {
        if existing.session_last_timestamp.as_deref() == Some(last_ts.as_str()) {
            return Ok(());
        }
    }

    let (context, context_last_ts) = read_cross_project_context(session, None, None)?;
    if context.trim().is_empty() {
        return Ok(());
    }

    let mut summary = summarize_session_with_gemini(&context)?;
    summary.session_last_timestamp = Some(context_last_ts.unwrap_or(last_ts));

    entry.store.summaries.insert(key, summary);
    entry.dirty = true;

    Ok(())
}

fn save_summary_cache(cache: &mut HashMap<PathBuf, SummaryCacheEntry>) -> Result<()> {
    for (project_path, entry) in cache.iter_mut() {
        if entry.dirty {
            save_session_summaries(project_path, &entry.store)?;
            entry.dirty = false;
        }
    }
    Ok(())
}

fn get_session_last_timestamp_for_session(session: &CrossProjectSession) -> Result<Option<String>> {
    if session.provider == Provider::Codex {
        let session_file = session
            .session_path
            .clone()
            .or_else(|| find_codex_session_file(&session.session_id));
        let Some(session_file) = session_file else {
            return Ok(None);
        };
        return get_codex_last_timestamp(&session_file);
    }

    get_session_last_timestamp_for_path(
        &session.session_id,
        session.provider,
        &session.project_path,
    )
}

/// Resume a session by name or ID.
fn resume_session(session: Option<String>, provider: Provider) -> Result<()> {
    let index = load_index()?;
    let sessions = read_sessions_for_project(provider)?;

    let (session_id, session_provider) = match session {
        Some(s) => {
            // Check if it's a saved name
            if let Some(saved) = index.sessions.get(&s) {
                // Find the provider for this session
                let prov = sessions
                    .iter()
                    .find(|sess| sess.session_id == saved.id)
                    .map(|sess| sess.provider)
                    .unwrap_or(Provider::Claude);
                (saved.id.clone(), prov)
            } else if s.len() >= 8 {
                // Might be a session ID or prefix
                if let Some(sess) = sessions.iter().find(|sess| sess.session_id.starts_with(&s)) {
                    (sess.session_id.clone(), sess.provider)
                } else {
                    // Assume it's a full ID for claude by default
                    (s, Provider::Claude)
                }
            } else {
                // Try numeric index (1-based)
                if let Ok(idx) = s.parse::<usize>() {
                    if idx > 0 && idx <= sessions.len() {
                        let sess = &sessions[idx - 1];
                        (sess.session_id.clone(), sess.provider)
                    } else {
                        bail!("Session index {} out of range", idx);
                    }
                } else {
                    bail!("Session '{}' not found", s);
                }
            }
        }
        None => {
            // Resume most recent
            let sess = sessions
                .first()
                .ok_or_else(|| anyhow::anyhow!("No sessions found for this project"))?;
            (sess.session_id.clone(), sess.provider)
        }
    };

    println!(
        "Resuming session {}...",
        &session_id[..8.min(session_id.len())]
    );
    launch_session(&session_id, session_provider)?;

    Ok(())
}

/// Save a session with a name.
fn save_session(name: &str, id: Option<String>) -> Result<()> {
    let session_id = match id {
        Some(id) => id,
        None => get_most_recent_session_id()?
            .ok_or_else(|| anyhow::anyhow!("No sessions found. Run claude first."))?,
    };

    let mut index = load_index()?;

    // Check if name already exists
    if index.sessions.contains_key(name) {
        bail!(
            "Session name '{}' already exists. Use a different name or remove it first.",
            name
        );
    }

    let saved = SavedSession {
        id: session_id.clone(),
        provider: "claude".to_string(), // Default to claude for manually saved sessions
        description: None,
        saved_at: chrono::Utc::now().to_rfc3339(),
        last_resumed: None,
    };

    index.sessions.insert(name.to_string(), saved);
    save_index(&index)?;

    println!("Saved session as '{}'", name);
    println!("  ID: {}", &session_id[..8.min(session_id.len())]);
    println!("\nResume with: f ai resume {}", name);

    Ok(())
}

/// Open or create notes for a session.
fn open_notes(session: &str) -> Result<()> {
    let index = load_index()?;

    // Find the session ID
    let session_id = if let Some(saved) = index.sessions.get(session) {
        saved.id.clone()
    } else {
        // Might be a direct ID
        session.to_string()
    };

    let notes_dir = get_notes_dir()?;
    fs::create_dir_all(&notes_dir)?;

    let note_file = notes_dir.join(format!("{}.md", session));

    // Create the file if it doesn't exist
    if !note_file.exists() {
        let template = format!(
            "# Session: {}\n\nSession ID: {}\n\n## Notes\n\n",
            session,
            &session_id[..8.min(session_id.len())]
        );
        fs::write(&note_file, template)?;
    }

    // Open in $EDITOR
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vim".to_string());
    let status = Command::new(&editor)
        .arg(&note_file)
        .status()
        .with_context(|| format!("failed to open editor: {}", editor))?;

    if !status.success() {
        bail!("editor exited with status {}", status);
    }

    Ok(())
}

/// Remove a saved session from tracking.
fn remove_session(session: &str) -> Result<()> {
    let mut index = load_index()?;

    if index.sessions.remove(session).is_some() {
        save_index(&index)?;
        println!("Removed session '{}'", session);

        // Also remove notes if they exist
        let notes_dir = get_notes_dir()?;
        let note_file = notes_dir.join(format!("{}.md", session));
        if note_file.exists() {
            fs::remove_file(&note_file)?;
            println!("Removed notes file");
        }
    } else {
        bail!("Session '{}' not found in saved sessions", session);
    }

    Ok(())
}

/// Initialize the .ai folder structure.
fn init_ai_folder() -> Result<()> {
    let ai_dir = std::env::current_dir()?.join(".ai");
    let internal_dir = ai_dir.join("internal");
    let sessions_dir = internal_dir.join("sessions").join("claude");
    let notes_dir = sessions_dir.join("notes");

    fs::create_dir_all(&notes_dir)?;

    // Create empty index.json if it doesn't exist
    let index_path = sessions_dir.join("index.json");
    if !index_path.exists() {
        let index = SessionIndex::default();
        let content = serde_json::to_string_pretty(&index)?;
        fs::write(&index_path, content)?;
    }

    println!("Initialized .ai folder structure:");
    println!("  .ai/internal/sessions/claude/index.json");
    println!("  .ai/internal/sessions/claude/notes/");

    Ok(())
}

/// Ensure .ai/internal is in the project's .gitignore to prevent session leaks.
fn ensure_gitignore() -> Result<()> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    let gitignore_path = cwd.join(".gitignore");

    if gitignore_path.exists() {
        let content = fs::read_to_string(&gitignore_path).unwrap_or_default();
        // Check if .ai/internal is already ignored
        let already_ignored = content.lines().any(|line| {
            let trimmed = line.trim();
            trimmed == ".ai/internal"
                || trimmed == ".ai/internal/"
                || trimmed == "/.ai/internal"
                || trimmed == "/.ai/internal/"
        });

        if !already_ignored {
            // Append .ai/internal to gitignore
            let mut file = fs::OpenOptions::new().append(true).open(&gitignore_path)?;
            // Add newline if file doesn't end with one
            if !content.ends_with('\n') && !content.is_empty() {
                writeln!(file)?;
            }
            writeln!(file, ".ai/internal/")?;
        }
    } else {
        // Create .gitignore with .ai/internal
        fs::write(&gitignore_path, ".ai/internal/\n")?;
    }

    Ok(())
}

/// Silently auto-import any new Claude sessions (called by list_sessions).
fn auto_import_sessions() -> Result<()> {
    // Ensure .ai is in .gitignore to prevent session leaks
    let _ = ensure_gitignore();

    // Silently ensure .ai folder exists
    let sessions_dir = get_ai_sessions_dir()?;
    if !sessions_dir.exists() {
        fs::create_dir_all(&sessions_dir)?;
        let index_path = sessions_dir.join("index.json");
        fs::write(&index_path, "{\"sessions\":{}}")?;
    }

    let sessions = read_sessions_for_project(Provider::Claude)?;
    if sessions.is_empty() {
        return Ok(());
    }

    let mut index = load_index()?;
    let mut changed = false;

    for session in &sessions {
        // Skip if already imported
        if index.sessions.values().any(|s| s.id == session.session_id) {
            continue;
        }

        let name = generate_session_name(session, &index);
        let provider_str = match session.provider {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
            Provider::All => "claude",
        };
        let saved = SavedSession {
            id: session.session_id.clone(),
            provider: provider_str.to_string(),
            description: session
                .first_message
                .as_ref()
                .or(session.error_summary.as_ref())
                .map(|m| {
                    if m.len() > 100 {
                        let end = floor_char_boundary(m, 97);
                        format!("{}...", &m[..end])
                    } else {
                        m.clone()
                    }
                }),
            saved_at: chrono::Utc::now().to_rfc3339(),
            last_resumed: None,
        };

        index.sessions.insert(name, saved);
        changed = true;
    }

    if changed {
        save_index(&index)?;
    }

    Ok(())
}

/// Import all existing Claude sessions for this project.
fn import_sessions() -> Result<()> {
    // Ensure .ai folder exists
    init_ai_folder()?;
    println!();

    let sessions = read_sessions_for_project(Provider::Claude)?;

    if sessions.is_empty() {
        println!("No Claude sessions found for this project.");
        return Ok(());
    }

    let mut index = load_index()?;
    let mut imported = 0;
    let mut skipped = 0;

    for session in &sessions {
        // Check if already imported
        if index.sessions.values().any(|s| s.id == session.session_id) {
            skipped += 1;
            continue;
        }

        // Generate a name from timestamp and first few words of first message
        let name = generate_session_name(session, &index);

        let provider_str = match session.provider {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
            Provider::All => "claude",
        };
        let saved = SavedSession {
            id: session.session_id.clone(),
            provider: provider_str.to_string(),
            description: session
                .first_message
                .as_ref()
                .or(session.error_summary.as_ref())
                .map(|m| {
                    if m.len() > 100 {
                        let end = floor_char_boundary(m, 97);
                        format!("{}...", &m[..end])
                    } else {
                        m.clone()
                    }
                }),
            saved_at: chrono::Utc::now().to_rfc3339(),
            last_resumed: None,
        };

        index.sessions.insert(name.clone(), saved);
        imported += 1;

        let id_short = &session.session_id[..8.min(session.session_id.len())];
        println!("  Imported: {} ({})", name, id_short);
    }

    save_index(&index)?;

    println!();
    println!(
        "Imported {} sessions, skipped {} (already exists)",
        imported, skipped
    );

    Ok(())
}

/// Generate a unique name for a session based on its content.
fn generate_session_name(session: &AiSession, index: &SessionIndex) -> String {
    // Try to create a name from date + first words of message
    let date_part = session
        .timestamp
        .as_deref()
        .map(|ts| ts[..10].replace('-', "")) // "20251209"
        .unwrap_or_else(|| "unknown".to_string());

    let words_part = session
        .first_message
        .as_deref()
        .or(session.error_summary.as_deref())
        .map(|msg| {
            // Extract first few meaningful words
            let words: Vec<&str> = msg
                .split_whitespace()
                .filter(|w| w.len() > 2 && !w.starts_with('/') && !w.starts_with('~'))
                .take(3)
                .collect();

            if words.is_empty() {
                "session".to_string()
            } else {
                words
                    .join("-")
                    .to_lowercase()
                    .chars()
                    .filter(|c| c.is_alphanumeric() || *c == '-')
                    .take(20)
                    .collect()
            }
        })
        .unwrap_or_else(|| "session".to_string());

    let base_name = format!("{}-{}", date_part, words_part);

    // Ensure uniqueness
    if !index.sessions.contains_key(&base_name) {
        return base_name;
    }

    // Add suffix if name exists
    for i in 2..100 {
        let name = format!("{}-{}", base_name, i);
        if !index.sessions.contains_key(&name) {
            return name;
        }
    }

    // Fallback to UUID prefix
    format!("{}-{}", base_name, &session.session_id[..8])
}

// ============================================================================
// Cross-project session search (f sessions)
// ============================================================================

use crate::cli::SessionsOpts;

/// Session with project info for cross-project display.
#[derive(Debug, Clone)]
struct CrossProjectSession {
    session_id: String,
    provider: Provider,
    project_path: PathBuf,
    project_name: String,
    timestamp: Option<String>,
    first_message: Option<String>,
    error_summary: Option<String>,
    session_path: Option<PathBuf>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct SessionSummaries {
    summaries: HashMap<String, SessionSummary>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct SessionSummary {
    summary: String,
    chapters: Vec<SessionChapter>,
    session_last_timestamp: Option<String>,
    model: String,
    updated_at: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct SessionChapter {
    title: String,
    summary: String,
}

struct SummaryCacheEntry {
    store: SessionSummaries,
    dirty: bool,
}

/// Consumed checkpoint tracking - stored in target project's .ai folder.
#[derive(Debug, Serialize, Deserialize, Default)]
struct ConsumedCheckpoints {
    /// Map of source project path -> last consumed timestamp
    consumed: HashMap<String, ConsumedEntry>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct ConsumedEntry {
    /// Last consumed timestamp from that project
    last_timestamp: String,
    /// When we consumed it
    consumed_at: String,
    /// Session ID we consumed from
    session_id: String,
}

/// Run cross-project session search.
pub fn run_sessions(opts: &SessionsOpts) -> Result<()> {
    let provider = match opts.provider.to_lowercase().as_str() {
        "claude" => Provider::Claude,
        "codex" => Provider::Codex,
        _ => Provider::All,
    };

    let sessions = scan_all_project_sessions(provider)?;
    let mut summary_cache: HashMap<PathBuf, SummaryCacheEntry> = HashMap::new();
    let summarize_enabled = opts.summarize && get_gemini_api_key().is_ok();

    if sessions.is_empty() {
        println!("No AI sessions found across projects.");
        return Ok(());
    }

    if opts.summarize && !summarize_enabled {
        println!("GEMINI_API_KEY/GOOGLE_API_KEY not set; skipping session summaries.");
    }

    if summarize_enabled {
        for session in &sessions {
            let _ = maybe_update_summary(session, &mut summary_cache);
        }
        let _ = save_summary_cache(&mut summary_cache);
    }

    if opts.list {
        // Just list, don't fuzzy search
        println!("AI Sessions across projects:\n");
        for session in &sessions {
            let relative_time = session
                .timestamp
                .as_deref()
                .map(format_relative_time)
                .unwrap_or_else(|| "unknown".to_string());
            let summary = get_display_summary(session, &mut summary_cache)?
                .or_else(|| {
                    session
                        .first_message
                        .as_deref()
                        .or(session.error_summary.as_deref())
                        .map(|s| s.to_string())
                })
                .map(|s| truncate_str(&clean_summary(&s), 50))
                .unwrap_or_default();
            let provider_tag = match session.provider {
                Provider::Claude => "claude",
                Provider::Codex => "codex",
                Provider::All => "ai",
            };
            println!(
                "{} | {} | {} | {}",
                session.project_name, provider_tag, relative_time, summary
            );
        }
        return Ok(());
    }

    // Build fzf entries
    let entries: Vec<(String, &CrossProjectSession)> = sessions
        .iter()
        .filter(|s| s.timestamp.is_some() || s.first_message.is_some() || s.error_summary.is_some())
        .map(|session| {
            let relative_time = session
                .timestamp
                .as_deref()
                .map(format_relative_time)
                .unwrap_or_else(|| "".to_string());
            let summary = get_display_summary(session, &mut summary_cache)
                .unwrap_or(None)
                .or_else(|| {
                    session
                        .first_message
                        .as_deref()
                        .or(session.error_summary.as_deref())
                        .map(|s| s.to_string())
                })
                .map(|s| truncate_str(&clean_summary(&s), 40))
                .unwrap_or_default();
            let provider_tag = match session.provider {
                Provider::Claude => "claude",
                Provider::Codex => "codex",
                Provider::All => "",
            };
            let display = format!(
                "{} | {} | {} | {}",
                session.project_name, provider_tag, relative_time, summary
            );
            (display, session)
        })
        .collect();

    if entries.is_empty() {
        println!("No sessions with content found.");
        return Ok(());
    }

    // Check for fzf
    if which::which("fzf").is_err() {
        println!("fzf not found – install it for fuzzy selection.");
        println!("\nSessions:");
        for (display, _) in &entries {
            println!("{}", display);
        }
        return Ok(());
    }

    // Run fzf
    let mut child = Command::new("fzf")
        .arg("--prompt")
        .arg("sessions> ")
        .arg("--ansi")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("failed to spawn fzf")?;

    {
        let stdin = child.stdin.as_mut().context("failed to open fzf stdin")?;
        for (display, _) in &entries {
            writeln!(stdin, "{}", display)?;
        }
    }

    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Ok(());
    }

    let selection = String::from_utf8(output.stdout).context("fzf output was not valid UTF-8")?;
    let selection = selection.trim();

    if selection.is_empty() {
        return Ok(());
    }

    // Find selected session
    let Some((_, session)) = entries.iter().find(|(d, _)| d == selection) else {
        bail!("Session not found");
    };

    // Get context since last consumed checkpoint (or full if --full)
    let context = get_cross_project_context(session, opts.count, opts.full)?;

    if context.is_empty() {
        if opts.full {
            println!("No context found in session.");
        } else {
            println!("No new context since last consumption. Use --full for entire session.");
        }
        return Ok(());
    }

    let output = if opts.handoff {
        summarize_handoff_with_gemini(&context)?
    } else {
        context
    };

    // Copy to clipboard
    copy_to_clipboard(&output)?;

    let explains = if opts.handoff {
        "handoff summary"
    } else {
        "context"
    };

    let line_count = output.lines().count();
    println!(
        "Copied {} from {} ({} lines) to clipboard",
        explains, session.project_name, line_count
    );

    // Save consumed checkpoint
    save_consumed_checkpoint(session)?;

    Ok(())
}

/// Scan all projects for AI sessions.
fn scan_all_project_sessions(provider: Provider) -> Result<Vec<CrossProjectSession>> {
    let mut all_sessions = Vec::new();

    // Scan Claude projects
    if provider == Provider::Claude || provider == Provider::All {
        let claude_dir = get_claude_projects_dir();
        if claude_dir.exists() {
            if let Ok(entries) = fs::read_dir(&claude_dir) {
                for entry in entries.flatten() {
                    let project_folder = entry.path();
                    if project_folder.is_dir() {
                        let project_name = extract_project_name(&project_folder);
                        let project_path = folder_to_path(&project_folder);

                        if let Ok(sessions) =
                            scan_project_sessions(&project_folder, Provider::Claude)
                        {
                            for session in sessions {
                                all_sessions.push(CrossProjectSession {
                                    session_id: session.session_id,
                                    provider: Provider::Claude,
                                    project_path: project_path.clone(),
                                    project_name: project_name.clone(),
                                    timestamp: session.timestamp,
                                    first_message: session.first_message,
                                    error_summary: session.error_summary,
                                    session_path: None,
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    // Scan Codex sessions (new format)
    if provider == Provider::Codex || provider == Provider::All {
        let codex_dir = get_codex_sessions_dir();
        if codex_dir.exists() {
            for file_path in collect_codex_session_files(&codex_dir) {
                let filename = file_path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                let Some((session, cwd)) = parse_codex_session_file(&file_path, filename) else {
                    continue;
                };
                let Some(project_path) = cwd else {
                    continue;
                };
                let project_name = project_path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown")
                    .to_string();

                all_sessions.push(CrossProjectSession {
                    session_id: session.session_id,
                    provider: Provider::Codex,
                    project_path,
                    project_name,
                    timestamp: session.timestamp,
                    first_message: session.first_message,
                    error_summary: session.error_summary,
                    session_path: Some(file_path),
                });
            }
        } else {
            // Fallback to legacy Codex projects layout
            let codex_dir = get_codex_projects_dir();
            if codex_dir.exists() {
                if let Ok(entries) = fs::read_dir(&codex_dir) {
                    for entry in entries.flatten() {
                        let project_folder = entry.path();
                        if project_folder.is_dir() {
                            let project_name = extract_project_name(&project_folder);
                            let project_path = folder_to_path(&project_folder);

                            if let Ok(sessions) =
                                scan_project_sessions(&project_folder, Provider::Codex)
                            {
                                for session in sessions {
                                    all_sessions.push(CrossProjectSession {
                                        session_id: session.session_id,
                                        provider: Provider::Codex,
                                        project_path: project_path.clone(),
                                        project_name: project_name.clone(),
                                        timestamp: session.timestamp,
                                        first_message: session.first_message,
                                        error_summary: session.error_summary,
                                        session_path: None,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Sort by timestamp descending (most recent first)
    all_sessions.sort_by(|a, b| {
        let ts_a = a.timestamp.as_deref().unwrap_or("");
        let ts_b = b.timestamp.as_deref().unwrap_or("");
        ts_b.cmp(ts_a)
    });

    Ok(all_sessions)
}

/// Scan a project folder for sessions.
fn scan_project_sessions(project_folder: &PathBuf, provider: Provider) -> Result<Vec<AiSession>> {
    let mut sessions = Vec::new();

    let entries = fs::read_dir(project_folder)
        .with_context(|| format!("failed to read {}", project_folder.display()))?;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().map(|e| e == "jsonl").unwrap_or(false) {
            let filename = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");

            if filename.starts_with("agent-") {
                continue;
            }

            if let Some(session) = parse_session_file(&path, filename, provider) {
                sessions.push(session);
            }
        }
    }

    // Sort by timestamp descending
    sessions.sort_by(|a, b| {
        let ts_a = a.timestamp.as_deref().unwrap_or("");
        let ts_b = b.timestamp.as_deref().unwrap_or("");
        ts_b.cmp(ts_a)
    });

    Ok(sessions)
}

/// Extract a friendly project name from the folder name.
fn extract_project_name(folder: &PathBuf) -> String {
    folder
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| {
            // The folder name is path with / replaced by -
            // Extract just the last component as project name
            s.rsplit('-').next().unwrap_or(s).to_string()
        })
        .unwrap_or_else(|| "unknown".to_string())
}

/// Convert folder name back to approximate path.
fn folder_to_path(folder: &PathBuf) -> PathBuf {
    let name = folder.file_name().and_then(|s| s.to_str()).unwrap_or("");
    // Folder name is path with / replaced by -
    // This is a heuristic - convert leading - to /
    PathBuf::from(name.replacen('-', "/", name.matches('-').count()))
}

/// Get context from a cross-project session since last consumed checkpoint.
fn get_cross_project_context(
    session: &CrossProjectSession,
    count: Option<usize>,
    full: bool,
) -> Result<String> {
    // If full mode, ignore checkpoints
    let since_ts = if full {
        None
    } else {
        // Load consumed checkpoints for current project
        let cwd = std::env::current_dir()?;
        let consumed = load_consumed_checkpoints(&cwd)?;
        let source_key = session.project_path.to_string_lossy().to_string();
        consumed
            .consumed
            .get(&source_key)
            .map(|e| e.last_timestamp.clone())
    };

    // Read context since checkpoint (or full if since_ts is None)
    let (context, _last_ts) = read_cross_project_context(session, since_ts.as_deref(), count)?;

    Ok(context)
}

/// Read context from a cross-project session.
fn read_cross_project_context(
    session: &CrossProjectSession,
    since_ts: Option<&str>,
    max_count: Option<usize>,
) -> Result<(String, Option<String>)> {
    if session.provider == Provider::Codex {
        let session_file = session
            .session_path
            .clone()
            .or_else(|| find_codex_session_file(&session.session_id));
        let Some(session_file) = session_file else {
            bail!(
                "Session file not found for Codex session {}",
                session.session_id
            );
        };
        return read_codex_cross_project_context(session, &session_file, since_ts, max_count);
    }

    let projects_dir = match session.provider {
        Provider::Claude | Provider::All => get_claude_projects_dir(),
        Provider::Codex => get_codex_projects_dir(),
    };

    let project_folder = session.project_path.to_string_lossy().replace('/', "-");
    let session_file = projects_dir
        .join(&project_folder)
        .join(format!("{}.jsonl", session.session_id));

    if !session_file.exists() {
        bail!("Session file not found: {}", session_file.display());
    }

    let content = fs::read_to_string(&session_file).context("failed to read session file")?;

    // Collect exchanges after the checkpoint timestamp
    let mut exchanges: Vec<(String, String, String)> = Vec::new();
    let mut current_user: Option<String> = None;
    let mut current_ts: Option<String> = None;
    let mut last_ts: Option<String> = None;

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }

        if let Ok(entry) = serde_json::from_str::<JsonlEntry>(line) {
            let entry_ts = entry.timestamp.clone();

            // Skip entries before checkpoint
            if let (Some(since), Some(ts)) = (since_ts, &entry_ts) {
                if ts.as_str() <= since {
                    continue;
                }
            }

            if let Some(ref msg) = entry.message {
                let role = msg.role.as_deref().unwrap_or("unknown");

                let content_text = if let Some(ref content) = msg.content {
                    match content {
                        serde_json::Value::String(s) => s.clone(),
                        serde_json::Value::Array(arr) => arr
                            .iter()
                            .filter_map(|v| {
                                if let Some(text) = v.get("text").and_then(|t| t.as_str()) {
                                    Some(text.to_string())
                                } else {
                                    None
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                        _ => continue,
                    }
                } else {
                    continue;
                };

                if content_text.is_empty() {
                    continue;
                }

                match role {
                    "user" => {
                        current_user = Some(content_text);
                        current_ts = entry_ts.clone();
                    }
                    "assistant" => {
                        let clean_text = strip_thinking_blocks(&content_text);
                        if clean_text.trim().is_empty() {
                            continue;
                        }
                        if let Some(user_msg) = current_user.take() {
                            let ts = current_ts.take().or(entry_ts.clone()).unwrap_or_default();
                            exchanges.push((user_msg, clean_text, ts.clone()));
                            last_ts = Some(ts);
                        }
                    }
                    _ => {}
                }
            }

            if entry_ts.is_some() {
                last_ts = entry_ts;
            }
        }
    }

    if exchanges.is_empty() {
        return Ok((String::new(), last_ts));
    }

    // Limit exchanges if count specified
    let exchanges_to_use = if let Some(count) = max_count {
        let start = exchanges.len().saturating_sub(count);
        &exchanges[start..]
    } else {
        &exchanges[..]
    };

    // Format the context with project info
    let mut context = format!(
        "=== Context from {} ({}) ===\n\n",
        session.project_name,
        match session.provider {
            Provider::Claude => "Claude Code",
            Provider::Codex => "Codex",
            Provider::All => "AI",
        }
    );

    for (user_msg, assistant_msg, _ts) in exchanges_to_use {
        context.push_str("H: ");
        context.push_str(user_msg);
        context.push_str("\n\n");
        context.push_str("A: ");
        context.push_str(assistant_msg);
        context.push_str("\n\n");
    }

    context.push_str("=== End Context ===\n");

    Ok((context, last_ts))
}

fn find_codex_session_file(session_id: &str) -> Option<PathBuf> {
    let root = get_codex_sessions_dir();
    if !root.exists() {
        return None;
    }

    for path in collect_codex_session_files(&root) {
        let filename = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if filename.contains(session_id) {
            return Some(path);
        }
    }

    None
}

fn read_codex_cross_project_context(
    session: &CrossProjectSession,
    session_file: &PathBuf,
    since_ts: Option<&str>,
    max_count: Option<usize>,
) -> Result<(String, Option<String>)> {
    let (exchanges, last_ts) = read_codex_exchanges(session_file, since_ts)?;

    if exchanges.is_empty() {
        return Ok((String::new(), last_ts));
    }

    let exchanges_to_use = if let Some(count) = max_count {
        let start = exchanges.len().saturating_sub(count);
        &exchanges[start..]
    } else {
        &exchanges[..]
    };

    let mut context = format!(
        "=== Context from {} ({}) ===\n\n",
        session.project_name,
        match session.provider {
            Provider::Claude => "Claude Code",
            Provider::Codex => "Codex",
            Provider::All => "AI",
        }
    );

    for (user_msg, assistant_msg, _ts) in exchanges_to_use {
        context.push_str("H: ");
        context.push_str(user_msg);
        context.push_str("\n\n");
        context.push_str("A: ");
        context.push_str(assistant_msg);
        context.push_str("\n\n");
    }

    context.push_str("=== End Context ===\n");

    Ok((context, last_ts))
}

/// Get consumed checkpoints file path.
fn get_consumed_checkpoints_path(project_path: &PathBuf) -> PathBuf {
    project_path
        .join(".ai")
        .join("internal")
        .join("consumed-checkpoints.json")
}

/// Load consumed checkpoints for a project.
fn load_consumed_checkpoints(project_path: &PathBuf) -> Result<ConsumedCheckpoints> {
    let path = get_consumed_checkpoints_path(project_path);
    if !path.exists() {
        return Ok(ConsumedCheckpoints::default());
    }
    let content =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&content).context("failed to parse consumed-checkpoints.json")
}

/// Save consumed checkpoint after copying context.
fn save_consumed_checkpoint(session: &CrossProjectSession) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let path = get_consumed_checkpoints_path(&cwd);

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut checkpoints = load_consumed_checkpoints(&cwd).unwrap_or_default();

    // Get the last timestamp from this session
    let last_ts = get_session_last_timestamp_for_path(
        &session.session_id,
        session.provider,
        &session.project_path,
    )?
    .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());

    let source_key = session.project_path.to_string_lossy().to_string();
    checkpoints.consumed.insert(
        source_key,
        ConsumedEntry {
            last_timestamp: last_ts,
            consumed_at: chrono::Utc::now().to_rfc3339(),
            session_id: session.session_id.clone(),
        },
    );

    let content = serde_json::to_string_pretty(&checkpoints)?;
    fs::write(&path, content)?;

    Ok(())
}

/// Get the last timestamp from a session file (for a specific project path).
fn get_session_last_timestamp_for_path(
    session_id: &str,
    provider: Provider,
    project_path: &PathBuf,
) -> Result<Option<String>> {
    if provider == Provider::Codex {
        let session_file = find_codex_session_file(session_id);
        let Some(session_file) = session_file else {
            return Ok(None);
        };
        return get_codex_last_timestamp(&session_file);
    }

    let projects_dir = match provider {
        Provider::Claude | Provider::All => get_claude_projects_dir(),
        Provider::Codex => get_codex_projects_dir(),
    };

    let project_folder = project_path.to_string_lossy().replace('/', "-");
    let session_file = projects_dir
        .join(&project_folder)
        .join(format!("{}.jsonl", session_id));

    if !session_file.exists() {
        return Ok(None);
    }

    let content = fs::read_to_string(&session_file).context("failed to read session file")?;

    let mut last_ts: Option<String> = None;
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<JsonlEntry>(line) {
            if let Some(ts) = entry.timestamp {
                last_ts = Some(ts);
            }
        }
    }

    Ok(last_ts)
}
