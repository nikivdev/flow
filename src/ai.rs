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
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
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
    /// First user message (as summary)
    first_message: Option<String>,
}

/// Entry from a session .jsonl file (we only parse what we need)
#[derive(Debug, Deserialize)]
struct JsonlEntry {
    timestamp: Option<String>,
    message: Option<SessionMessage>,
}

#[derive(Debug, Deserialize)]
struct SessionMessage {
    role: Option<String>,
    content: Option<serde_json::Value>,
}

/// Run the ai subcommand.
pub fn run(action: Option<AiAction>) -> Result<()> {
    let action = action.unwrap_or(AiAction::List);

    match action {
        AiAction::List => list_sessions(Provider::All)?,
        AiAction::Claude { action } => {
            match action.unwrap_or(ProviderAiAction::List) {
                ProviderAiAction::List => list_sessions(Provider::Claude)?,
                ProviderAiAction::Resume { session } => resume_session(session, Provider::Claude)?,
                ProviderAiAction::Copy { session } => copy_session(session, Provider::Claude)?,
                ProviderAiAction::Context { session } => copy_context(session, Provider::Claude)?,
            }
        }
        AiAction::Codex { action } => {
            match action.unwrap_or(ProviderAiAction::List) {
                ProviderAiAction::List => list_sessions(Provider::Codex)?,
                ProviderAiAction::Resume { session } => resume_session(session, Provider::Codex)?,
                ProviderAiAction::Copy { session } => copy_session(session, Provider::Codex)?,
                ProviderAiAction::Context { session } => copy_context(session, Provider::Codex)?,
            }
        }
        AiAction::Resume { session } => resume_session(session, Provider::All)?,
        AiAction::Save { name, id } => save_session(&name, id)?,
        AiAction::Notes { session } => open_notes(&session)?,
        AiAction::Remove { session } => remove_session(&session)?,
        AiAction::Init => init_ai_folder()?,
        AiAction::Import => import_sessions()?,
        AiAction::Copy { session } => copy_session(session, Provider::All)?,
        AiAction::Context { session } => copy_context(session, Provider::All)?,
    }

    Ok(())
}

/// Get the .ai/sessions/claude directory for the current project.
fn get_ai_sessions_dir() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    Ok(cwd.join(".ai").join("sessions").join("claude"))
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
    let content = fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&content).context("failed to parse index.json")
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

    // Sort by timestamp descending (most recent first)
    sessions.sort_by(|a, b| {
        let ts_a = a.timestamp.as_deref().unwrap_or("");
        let ts_b = b.timestamp.as_deref().unwrap_or("");
        ts_b.cmp(ts_a)
    });

    Ok(sessions)
}

/// Read sessions for a specific provider.
fn read_provider_sessions(provider: Provider) -> Result<Vec<AiSession>> {
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
        debug!("{:?} project dir not found at {}", provider, project_dir.display());
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
            let filename = path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("");

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
    let content = fs::read_to_string(path).ok()?;

    let mut timestamp = None;
    let mut first_message = None;

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }

        if let Ok(entry) = serde_json::from_str::<JsonlEntry>(line) {
            // Get timestamp from first entry
            if timestamp.is_none() {
                timestamp = entry.timestamp.clone();
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

            // Once we have both, we can stop
            if timestamp.is_some() && first_message.is_some() {
                break;
            }
        }
    }

    Some(AiSession {
        session_id: session_id.to_string(),
        provider,
        timestamp,
        first_message,
    })
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
        if session.timestamp.is_none() && session.first_message.is_none() {
            continue;
        }

        let relative_time = session.timestamp.as_deref()
            .map(format_relative_time)
            .unwrap_or_else(|| "".to_string());

        // Check if this session has a human-assigned name (not auto-generated)
        let saved_name = index.sessions.iter()
            .find(|(_, s)| s.id == session.session_id)
            .map(|(name, _)| name.as_str())
            .filter(|name| !is_auto_generated_name(name));

        let summary = session.first_message.as_deref().unwrap_or("");
        let summary_clean = clean_summary(summary);

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
            format!("{}{} | {} | {}", provider_tag, name, relative_time, truncate_str(&summary_clean, 40))
        } else {
            // For other sessions, show: [provider] time | summary
            format!("{}{} | {}", provider_tag, relative_time, truncate_str(&summary_clean, 60))
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
        println!("Resuming session {}...", &selected.session_id[..8.min(selected.session_id.len())]);
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

    let selection = String::from_utf8(output.stdout)
        .context("fzf output was not valid UTF-8")?;
    let selection = selection.trim();

    if selection.is_empty() {
        return Ok(None);
    }

    Ok(entries.iter().find(|e| e.display == selection))
}

/// Launch a session with the appropriate CLI.
fn launch_session(session_id: &str, provider: Provider) -> Result<()> {
    let (cmd, name) = match provider {
        Provider::Claude => ("claude", "claude"),
        Provider::Codex => ("codex", "codex"),
        Provider::All => ("claude", "claude"), // Default to claude
    };

    let status = Command::new(cmd)
        .arg("--resume")
        .arg(session_id)
        .arg("--dangerously-skip-permissions")
        .status()
        .with_context(|| format!("failed to launch {}", name))?;

    if !status.success() {
        bail!("{} exited with status {}", name, status);
    }

    Ok(())
}

/// Copy session history to clipboard.
fn copy_session(session: Option<String>, provider: Provider) -> Result<()> {
    // Auto-import any new sessions silently
    auto_import_sessions()?;

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

    // Find the session ID and provider
    let (session_id, session_provider) = if let Some(ref query) = session {
        // Try to find by name or ID
        if let Some((_, saved)) = index.sessions.iter().find(|(name, _)| name.as_str() == query) {
            // Find the provider for this session
            let prov = sessions.iter()
                .find(|s| s.session_id == saved.id)
                .map(|s| s.provider)
                .unwrap_or(Provider::Claude);
            (saved.id.clone(), prov)
        } else if let Some(s) = sessions.iter().find(|s| s.session_id == *query || s.session_id.starts_with(query)) {
            (s.session_id.clone(), s.provider)
        } else {
            bail!("Session not found: {}", query);
        }
    } else {
        // Show fzf selection
        let mut entries: Vec<FzfSessionEntry> = Vec::new();

        for session in &sessions {
            if session.timestamp.is_none() && session.first_message.is_none() {
                continue;
            }

            let relative_time = session.timestamp.as_deref()
                .map(format_relative_time)
                .unwrap_or_else(|| "".to_string());

            let saved_name = index.sessions.iter()
                .find(|(_, s)| s.id == session.session_id)
                .map(|(name, _)| name.as_str())
                .filter(|name| !is_auto_generated_name(name));

            let summary = session.first_message.as_deref().unwrap_or("");
            let summary_clean = clean_summary(summary);

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
                format!("{}{} | {} | {}", provider_tag, name, relative_time, truncate_str(&summary_clean, 40))
            } else {
                format!("{}{} | {}", provider_tag, relative_time, truncate_str(&summary_clean, 60))
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

/// Read full session history from JSONL file and format as conversation.
fn read_session_history(session_id: &str, provider: Provider) -> Result<String> {
    let cwd = std::env::current_dir()?;
    let cwd_str = cwd.to_string_lossy().to_string();
    let project_folder = path_to_project_name(&cwd_str);

    let projects_dir = match provider {
        Provider::Claude | Provider::All => get_claude_projects_dir(),
        Provider::Codex => get_codex_projects_dir(),
    };

    let session_file = projects_dir.join(&project_folder).join(format!("{}.jsonl", session_id));

    if !session_file.exists() {
        bail!("Session file not found: {}", session_file.display());
    }

    let content = fs::read_to_string(&session_file)
        .context("failed to read session file")?;

    let mut history = String::new();

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }

        if let Ok(entry) = serde_json::from_str::<JsonlEntry>(line) {
            if let Some(ref msg) = entry.message {
                let role = msg.role.as_deref().unwrap_or("unknown");

                // Format role header
                let role_label = match role {
                    "user" => "Human",
                    "assistant" => "Assistant",
                    _ => role,
                };

                // Extract content text
                let content_text = if let Some(ref content) = msg.content {
                    match content {
                        serde_json::Value::String(s) => s.clone(),
                        serde_json::Value::Array(arr) => {
                            // Content might be array of content blocks
                            arr.iter()
                                .filter_map(|v| {
                                    // Handle text blocks
                                    if let Some(text) = v.get("text").and_then(|t| t.as_str()) {
                                        Some(text.to_string())
                                    } else {
                                        None
                                    }
                                })
                                .collect::<Vec<_>>()
                                .join("\n")
                        }
                        _ => continue,
                    }
                } else {
                    continue;
                };

                if !content_text.is_empty() {
                    history.push_str(&format!("{}: {}\n\n", role_label, content_text));
                }
            }
        }
    }

    Ok(history)
}

/// Copy last prompt and response from a session to clipboard.
fn copy_context(session: Option<String>, provider: Provider) -> Result<()> {
    // Auto-import any new sessions silently
    auto_import_sessions()?;

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

    // Find the session ID and provider
    let (session_id, session_provider) = if let Some(ref query) = session {
        // Try to find by name or ID
        if let Some((_, saved)) = index.sessions.iter().find(|(name, _)| name.as_str() == query) {
            let prov = sessions.iter()
                .find(|s| s.session_id == saved.id)
                .map(|s| s.provider)
                .unwrap_or(Provider::Claude);
            (saved.id.clone(), prov)
        } else if let Some(s) = sessions.iter().find(|s| s.session_id == *query || s.session_id.starts_with(query)) {
            (s.session_id.clone(), s.provider)
        } else {
            bail!("Session not found: {}", query);
        }
    } else {
        // Show fzf selection
        let mut entries: Vec<FzfSessionEntry> = Vec::new();

        for session in &sessions {
            if session.timestamp.is_none() && session.first_message.is_none() {
                continue;
            }

            let relative_time = session.timestamp.as_deref()
                .map(format_relative_time)
                .unwrap_or_else(|| "".to_string());

            let saved_name = index.sessions.iter()
                .find(|(_, s)| s.id == session.session_id)
                .map(|(name, _)| name.as_str())
                .filter(|name| !is_auto_generated_name(name));

            let summary = session.first_message.as_deref().unwrap_or("");
            let summary_clean = clean_summary(summary);

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
                format!("{}{} | {} | {}", provider_tag, name, relative_time, truncate_str(&summary_clean, 40))
            } else {
                format!("{}{} | {}", provider_tag, relative_time, truncate_str(&summary_clean, 60))
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

    // Read the last context (last user prompt + assistant response)
    let context = read_last_context(&session_id, session_provider)?;

    // Copy to clipboard
    copy_to_clipboard(&context)?;

    let line_count = context.lines().count();
    println!("Copied last context ({} lines) to clipboard", line_count);

    Ok(())
}

/// Read last user prompt and assistant response from a session.
fn read_last_context(session_id: &str, provider: Provider) -> Result<String> {
    let cwd = std::env::current_dir()?;
    let cwd_str = cwd.to_string_lossy().to_string();
    let project_folder = path_to_project_name(&cwd_str);

    let projects_dir = match provider {
        Provider::Claude | Provider::All => get_claude_projects_dir(),
        Provider::Codex => get_codex_projects_dir(),
    };

    let session_file = projects_dir.join(&project_folder).join(format!("{}.jsonl", session_id));

    if !session_file.exists() {
        bail!("Session file not found: {}", session_file.display());
    }

    let content = fs::read_to_string(&session_file)
        .context("failed to read session file")?;

    // Collect all messages, keeping track of the last user and assistant messages
    let mut last_user: Option<String> = None;
    let mut last_assistant: Option<String> = None;

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
                        serde_json::Value::Array(arr) => {
                            arr.iter()
                                .filter_map(|v| {
                                    if let Some(text) = v.get("text").and_then(|t| t.as_str()) {
                                        Some(text.to_string())
                                    } else {
                                        None
                                    }
                                })
                                .collect::<Vec<_>>()
                                .join("\n")
                        }
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
                        last_user = Some(content_text);
                        // Reset assistant when we see a new user message
                        last_assistant = None;
                    }
                    "assistant" => {
                        last_assistant = Some(content_text);
                    }
                    _ => {}
                }
            }
        }
    }

    // Format the context
    let mut context = String::new();

    if let Some(user_msg) = last_user {
        context.push_str("Human: ");
        context.push_str(&user_msg);
        context.push_str("\n\n");
    }

    if let Some(assistant_msg) = last_assistant {
        context.push_str("Assistant: ");
        context.push_str(&assistant_msg);
        context.push('\n');
    }

    if context.is_empty() {
        bail!("No messages found in session");
    }

    Ok(context)
}

/// Copy text to system clipboard.
fn copy_to_clipboard(text: &str) -> Result<()> {
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
            Err(_) => {
                Command::new("xsel")
                    .arg("--clipboard")
                    .arg("--input")
                    .stdin(Stdio::piped())
                    .spawn()
                    .context("failed to spawn xclip or xsel")?
            }
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

/// Truncate a string to max chars, adding ellipsis if needed.
fn truncate_str(s: &str, max: usize) -> String {
    // Handle newlines - take first line only
    let first_line = s.lines().next().unwrap_or(s);

    if first_line.chars().count() <= max {
        first_line.to_string()
    } else {
        let truncated: String = first_line.chars().take(max - 1).collect();
        format!("{}…", truncated)
    }
}

/// Format timestamp as relative time (e.g., "3 days ago", "2 hours ago").
fn format_relative_time(ts: &str) -> String {
    // Parse ISO 8601 timestamp: "2025-12-09T19:21:15.562Z"
    let parsed = chrono::DateTime::parse_from_rfc3339(ts)
        .or_else(|_| {
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
    name.starts_with("202") && name.chars().nth(8) == Some('-') ||
    name.starts_with("unknown-session")
}

/// Clean up a summary string - remove noise, paths, special chars.
fn clean_summary(s: &str) -> String {
    // Take first meaningful line (skip empty lines and lines starting with special chars)
    let meaningful_line = s.lines()
        .map(|l| l.trim())
        .find(|l| {
            !l.is_empty() &&
            !l.starts_with('~') &&
            !l.starts_with('/') &&
            !l.starts_with('>') &&
            !l.starts_with('❯') &&
            !l.starts_with('$') &&
            !l.starts_with('#') &&
            !l.starts_with("Error:")
        })
        .or_else(|| s.lines().find(|l| !l.trim().is_empty()))
        .unwrap_or(s);

    // Clean up the line
    meaningful_line
        .trim()
        .replace('\t', " ")
        .replace("  ", " ")
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
                let prov = sessions.iter()
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
            let sess = sessions.first()
                .ok_or_else(|| anyhow::anyhow!("No sessions found for this project"))?;
            (sess.session_id.clone(), sess.provider)
        }
    };

    println!("Resuming session {}...", &session_id[..8.min(session_id.len())]);
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
        bail!("Session name '{}' already exists. Use a different name or remove it first.", name);
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
    let sessions_dir = ai_dir.join("sessions").join("claude");
    let notes_dir = sessions_dir.join("notes");

    fs::create_dir_all(&notes_dir)?;

    // Create empty index.json if it doesn't exist
    let index_path = sessions_dir.join("index.json");
    if !index_path.exists() {
        let index = SessionIndex::default();
        let content = serde_json::to_string_pretty(&index)?;
        fs::write(&index_path, content)?;
    }

    // Create .gitignore in .ai
    let gitignore_path = ai_dir.join(".gitignore");
    if !gitignore_path.exists() {
        fs::write(&gitignore_path, "# Ignore session notes (personal)\nsessions/claude/notes/\n")?;
    }

    println!("Initialized .ai folder structure:");
    println!("  .ai/");
    println!("  .ai/sessions/claude/index.json");
    println!("  .ai/sessions/claude/notes/");
    println!("  .ai/.gitignore");

    Ok(())
}

/// Ensure .ai is in the project's .gitignore to prevent session leaks.
fn ensure_gitignore() -> Result<()> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    let gitignore_path = cwd.join(".gitignore");

    if gitignore_path.exists() {
        let content = fs::read_to_string(&gitignore_path).unwrap_or_default();
        // Check if .ai is already ignored (as a line by itself or with trailing slash)
        let already_ignored = content.lines().any(|line| {
            let trimmed = line.trim();
            trimmed == ".ai" || trimmed == ".ai/" || trimmed == "/.ai" || trimmed == "/.ai/"
        });

        if !already_ignored {
            // Append .ai to gitignore
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(&gitignore_path)?;
            // Add newline if file doesn't end with one
            if !content.ends_with('\n') && !content.is_empty() {
                writeln!(file)?;
            }
            writeln!(file, ".ai/")?;
        }
    } else {
        // Create .gitignore with .ai
        fs::write(&gitignore_path, ".ai/\n")?;
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
            description: session.first_message.as_ref().map(|m| {
                if m.len() > 100 {
                    format!("{}...", &m[..97])
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
            description: session.first_message.as_ref().map(|m| {
                if m.len() > 100 {
                    format!("{}...", &m[..97])
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
    println!("Imported {} sessions, skipped {} (already exists)", imported, skipped);

    Ok(())
}

/// Generate a unique name for a session based on its content.
fn generate_session_name(session: &AiSession, index: &SessionIndex) -> String {
    // Try to create a name from date + first words of message
    let date_part = session.timestamp.as_deref()
        .map(|ts| ts[..10].replace('-', ""))  // "20251209"
        .unwrap_or_else(|| "unknown".to_string());

    let words_part = session.first_message.as_deref()
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
                words.join("-")
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
