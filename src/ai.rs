//! AI session management for Claude Code, Codex, and Cursor integration.
//!
//! Tracks and manages AI coding sessions per project, allowing users to:
//! - List sessions for the current project (Claude, Codex, or both)
//! - Save/bookmark sessions with names
//! - Resume sessions
//! - Add notes to sessions
//! - Copy session history to clipboard

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::env;
use std::fs;
use std::fs::OpenOptions;
use std::hash::{Hash, Hasher};
use std::io::{self, BufRead, BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use regex::Regex;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use toml::Value as TomlValue;
use tracing::debug;

use crate::cli::{
    AiAction, CodexDaemonAction, CodexMemoryAction, CodexRuntimeAction, CodexSkillEvalAction,
    CodexSkillSourceAction, ProviderAiAction,
};
use crate::commit::configured_codex_bin_for_workdir;
use crate::{
    codex_memory, codex_text, codexd, config, project_snapshot, repo_capsule, url_inspect,
};
use crate::{codex_runtime, codex_skill_eval};

/// AI provider type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Claude,
    Codex,
    Cursor,
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
    /// Which provider (claude, codex, cursor)
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
struct CursorEntry {
    role: Option<String>,
    message: Option<SessionMessage>,
}

#[derive(Debug, Deserialize)]
struct SessionMessage {
    role: Option<String>,
    content: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CodexRecoverRow {
    pub(crate) id: String,
    pub(crate) updated_at: i64,
    pub(crate) cwd: String,
    pub(crate) title: Option<String>,
    pub(crate) first_user_message: Option<String>,
    pub(crate) git_branch: Option<String>,
}

#[derive(Debug, Serialize)]
struct CodexRecoverCandidate {
    id: String,
    updated_at: String,
    updated_at_unix: i64,
    cwd: String,
    git_branch: Option<String>,
    title: Option<String>,
    first_user_message: Option<String>,
}

#[derive(Debug, Serialize)]
struct CodexRecoverOutput {
    target_path: String,
    exact_cwd: bool,
    query: Option<String>,
    recommended_route: String,
    summary: String,
    candidates: Vec<CodexRecoverCandidate>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct CodexResolvedReference {
    name: String,
    source: String,
    matched: String,
    command: Option<String>,
    output: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct CodexOpenPlan {
    action: String,
    route: String,
    reason: String,
    target_path: String,
    launch_path: String,
    query: Option<String>,
    session_id: Option<String>,
    prompt: Option<String>,
    references: Vec<CodexResolvedReference>,
    runtime_state_path: Option<String>,
    runtime_skills: Vec<String>,
    prompt_context_budget_chars: usize,
    max_resolved_references: usize,
    prompt_chars: usize,
    injected_context_chars: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CodexSessionReferenceRequest {
    session_hint: String,
    count: usize,
    user_request: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LinearUrlReference {
    url: String,
    workspace_slug: String,
    resource_kind: LinearUrlKind,
    resource_value: String,
    view: Option<String>,
    title_hint: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinearUrlKind {
    Issue,
    Project,
}

const CODEX_QUERY_CACHE_VERSION: u32 = 1;
const CODEX_QUERY_CACHE_ENV_DISABLE: &str = "FLOW_DISABLE_CODEX_QUERY_CACHE";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct CodexStateDbStamp {
    path: String,
    len: u64,
    modified_unix_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CodexQueryCacheEntry {
    version: u32,
    stamp: CodexStateDbStamp,
    rows: Vec<CodexRecoverRow>,
}

/// Run a provider-specific action (for top-level `f codex` / `f claude` commands).
pub fn run_provider(provider: Provider, action: Option<ProviderAiAction>) -> Result<()> {
    if provider == Provider::Cursor {
        match action {
            None | Some(ProviderAiAction::List) => list_sessions(Provider::Cursor)?,
            Some(ProviderAiAction::LatestId { path }) => {
                print_latest_session_id(Provider::Cursor, path)?
            }
            Some(ProviderAiAction::Connect { .. }) => {
                bail!("connect is only supported for Codex sessions; use `f codex connect ...`");
            }
            Some(ProviderAiAction::Copy { session }) => copy_session(session, Provider::Cursor)?,
            Some(ProviderAiAction::Context {
                session,
                count,
                path,
            }) => copy_context(session, Provider::Cursor, count, path)?,
            Some(ProviderAiAction::Show {
                session,
                path,
                count,
                full,
            }) => show_session(session, Provider::Cursor, count, path, full)?,
            Some(ProviderAiAction::Runtime { .. }) => {
                bail!(
                    "runtime helpers are only supported for Codex sessions; use `f codex runtime ...`"
                );
            }
            Some(ProviderAiAction::Doctor { .. }) => {
                bail!("doctor is only supported for Codex sessions; use `f codex doctor`");
            }
            Some(ProviderAiAction::TouchLaunch { .. }) => {
                bail!(
                    "touch-launch is only supported for Codex sessions; use `f codex touch-launch`"
                );
            }
            Some(ProviderAiAction::EnableGlobal { .. }) => {
                bail!(
                    "global Codex enablement is only supported for Codex sessions; use `f codex enable-global`"
                );
            }
            Some(ProviderAiAction::Daemon { .. }) => {
                bail!("daemon is only supported for Codex sessions; use `f codex daemon ...`");
            }
            Some(ProviderAiAction::Memory { .. }) => {
                bail!("memory is only supported for Codex sessions; use `f codex memory ...`");
            }
            Some(ProviderAiAction::SkillEval { .. }) => {
                bail!(
                    "skill-eval is only supported for Codex sessions; use `f codex skill-eval ...`"
                );
            }
            Some(ProviderAiAction::SkillSource { .. }) => {
                bail!(
                    "skill-source is only supported for Codex sessions; use `f codex skill-source ...`"
                );
            }
            Some(ProviderAiAction::Sessions)
            | Some(ProviderAiAction::Continue { .. })
            | Some(ProviderAiAction::New)
            | Some(ProviderAiAction::Open { .. })
            | Some(ProviderAiAction::Resolve { .. })
            | Some(ProviderAiAction::Resume { .. })
            | Some(ProviderAiAction::Find { .. })
            | Some(ProviderAiAction::FindAndCopy { .. }) => {
                bail!(
                    "Cursor transcripts are readable only; use `f cursor list`, `f cursor copy`, or `f cursor context`"
                );
            }
            Some(ProviderAiAction::Recover { .. }) => {
                bail!("recover is only supported for Codex sessions; use `f ai codex recover ...`");
            }
        }
        return Ok(());
    }

    match action {
        None => quick_start_session(provider)?,
        Some(ProviderAiAction::List) => list_sessions(provider)?,
        Some(ProviderAiAction::LatestId { path }) => print_latest_session_id(provider, path)?,
        Some(ProviderAiAction::Sessions) => provider_sessions(provider)?,
        Some(ProviderAiAction::Continue { session, path }) => {
            continue_session(session, path, provider)?
        }
        Some(ProviderAiAction::New) => new_session(provider)?,
        Some(ProviderAiAction::Connect {
            path,
            exact_cwd,
            json,
            query,
        }) => connect_codex_session(path, query, exact_cwd, json, provider)?,
        Some(ProviderAiAction::Open {
            path,
            exact_cwd,
            query,
        }) => open_codex_session(path, query, exact_cwd, provider)?,
        Some(ProviderAiAction::Daemon { action }) => codex_daemon_command(action, provider)?,
        Some(ProviderAiAction::Memory { action }) => codex_memory_command(action, provider)?,
        Some(ProviderAiAction::SkillEval { action }) => codex_skill_eval_command(action, provider)?,
        Some(ProviderAiAction::SkillSource { action }) => {
            codex_skill_source_command(action, provider)?
        }
        Some(ProviderAiAction::Doctor {
            path,
            assert_runtime,
            assert_schedule,
            assert_learning,
            assert_autonomous,
            json,
        }) => codex_doctor(
            path,
            assert_runtime,
            assert_schedule,
            assert_learning,
            assert_autonomous,
            json,
            provider,
        )?,
        Some(ProviderAiAction::TouchLaunch { mode, cwd }) => {
            codex_touch_launch(mode, cwd, provider)?
        }
        Some(ProviderAiAction::EnableGlobal {
            dry_run,
            install_launchd,
            start_daemon,
            sync_skills,
            full,
            minutes,
            limit,
            max_targets,
            within_hours,
        }) => codex_enable_global(
            dry_run,
            install_launchd,
            start_daemon,
            sync_skills,
            full,
            minutes,
            limit,
            max_targets,
            within_hours,
            provider,
        )?,
        Some(ProviderAiAction::Resolve {
            path,
            exact_cwd,
            json,
            query,
        }) => resolve_codex_input(path, query, exact_cwd, json, provider)?,
        Some(ProviderAiAction::Runtime { action }) => codex_runtime_command(action, provider)?,
        Some(ProviderAiAction::Resume { session, path }) => {
            resume_session(session, path, provider)?
        }
        Some(ProviderAiAction::Find {
            path,
            exact_cwd,
            query,
        }) => find_codex_session(path, query, exact_cwd, provider)?,
        Some(ProviderAiAction::FindAndCopy {
            path,
            exact_cwd,
            query,
        }) => find_and_copy_codex_session(path, query, exact_cwd, provider)?,
        Some(ProviderAiAction::Copy { session }) => copy_session(session, provider)?,
        Some(ProviderAiAction::Context {
            session,
            count,
            path,
        }) => copy_context(session, provider, count, path)?,
        Some(ProviderAiAction::Show {
            session,
            path,
            count,
            full,
        }) => show_session(session, provider, count, path, full)?,
        Some(ProviderAiAction::Recover {
            path,
            exact_cwd,
            limit,
            json,
            summary_only,
            query,
        }) => recover_codex_sessions(path, query, exact_cwd, limit, json, summary_only, provider)?,
    }
    Ok(())
}

/// Run the ai subcommand.
pub fn run(action: Option<AiAction>) -> Result<()> {
    let action = action.unwrap_or(AiAction::List);

    match action {
        AiAction::List => list_sessions(Provider::All)?,
        AiAction::Cursor { action } => run_provider(Provider::Cursor, action)?,
        AiAction::Claude { action } => match action {
            None => quick_start_session(Provider::Claude)?,
            Some(ProviderAiAction::List) => list_sessions(Provider::Claude)?,
            Some(ProviderAiAction::LatestId { path }) => {
                print_latest_session_id(Provider::Claude, path)?
            }
            Some(ProviderAiAction::Sessions) => provider_sessions(Provider::Claude)?,
            Some(ProviderAiAction::Continue { session, path }) => {
                continue_session(session, path, Provider::Claude)?
            }
            Some(ProviderAiAction::New) => new_session(Provider::Claude)?,
            Some(ProviderAiAction::Connect { .. }) => {
                bail!("connect is only supported for Codex sessions; use `f codex connect ...`");
            }
            Some(ProviderAiAction::Open { .. }) | Some(ProviderAiAction::Resolve { .. }) => {
                bail!("open/resolve is only supported for Codex sessions; use `f codex ...`");
            }
            Some(ProviderAiAction::Runtime { .. }) => {
                bail!(
                    "runtime helpers are only supported for Codex sessions; use `f codex runtime ...`"
                );
            }
            Some(ProviderAiAction::Doctor { .. }) => {
                bail!("doctor is only supported for Codex sessions; use `f codex doctor`");
            }
            Some(ProviderAiAction::TouchLaunch { .. }) => {
                bail!(
                    "touch-launch is only supported for Codex sessions; use `f codex touch-launch`"
                );
            }
            Some(ProviderAiAction::EnableGlobal { .. }) => {
                bail!(
                    "global Codex enablement is only supported for Codex sessions; use `f codex enable-global`"
                );
            }
            Some(ProviderAiAction::Daemon { .. }) => {
                bail!("daemon is only supported for Codex sessions; use `f codex daemon ...`");
            }
            Some(ProviderAiAction::Memory { .. }) => {
                bail!("memory is only supported for Codex sessions; use `f codex memory ...`");
            }
            Some(ProviderAiAction::SkillEval { .. }) => {
                bail!(
                    "skill-eval is only supported for Codex sessions; use `f codex skill-eval ...`"
                );
            }
            Some(ProviderAiAction::SkillSource { .. }) => {
                bail!(
                    "skill-source is only supported for Codex sessions; use `f codex skill-source ...`"
                );
            }
            Some(ProviderAiAction::Resume { session, path }) => {
                resume_session(session, path, Provider::Claude)?
            }
            Some(ProviderAiAction::Find {
                path,
                exact_cwd,
                query,
            }) => find_codex_session(path, query, exact_cwd, Provider::Claude)?,
            Some(ProviderAiAction::FindAndCopy {
                path,
                exact_cwd,
                query,
            }) => find_and_copy_codex_session(path, query, exact_cwd, Provider::Claude)?,
            Some(ProviderAiAction::Copy { session }) => copy_session(session, Provider::Claude)?,
            Some(ProviderAiAction::Context {
                session,
                count,
                path,
            }) => copy_context(session, Provider::Claude, count, path)?,
            Some(ProviderAiAction::Show {
                session,
                path,
                count,
                full,
            }) => show_session(session, Provider::Claude, count, path, full)?,
            Some(ProviderAiAction::Recover {
                path,
                exact_cwd,
                limit,
                json,
                summary_only,
                query,
            }) => recover_codex_sessions(
                path,
                query,
                exact_cwd,
                limit,
                json,
                summary_only,
                Provider::Claude,
            )?,
        },
        AiAction::Codex { action } => match action {
            None => quick_start_session(Provider::Codex)?,
            Some(ProviderAiAction::List) => list_sessions(Provider::Codex)?,
            Some(ProviderAiAction::LatestId { path }) => {
                print_latest_session_id(Provider::Codex, path)?
            }
            Some(ProviderAiAction::Sessions) => provider_sessions(Provider::Codex)?,
            Some(ProviderAiAction::Continue { session, path }) => {
                continue_session(session, path, Provider::Codex)?
            }
            Some(ProviderAiAction::New) => new_session(Provider::Codex)?,
            Some(ProviderAiAction::Connect {
                path,
                exact_cwd,
                json,
                query,
            }) => connect_codex_session(path, query, exact_cwd, json, Provider::Codex)?,
            Some(ProviderAiAction::Open {
                path,
                exact_cwd,
                query,
            }) => open_codex_session(path, query, exact_cwd, Provider::Codex)?,
            Some(ProviderAiAction::Daemon { action }) => {
                codex_daemon_command(action, Provider::Codex)?
            }
            Some(ProviderAiAction::Memory { action }) => {
                codex_memory_command(action, Provider::Codex)?
            }
            Some(ProviderAiAction::SkillEval { action }) => {
                codex_skill_eval_command(action, Provider::Codex)?
            }
            Some(ProviderAiAction::SkillSource { action }) => {
                codex_skill_source_command(action, Provider::Codex)?
            }
            Some(ProviderAiAction::Doctor {
                path,
                assert_runtime,
                assert_schedule,
                assert_learning,
                assert_autonomous,
                json,
            }) => codex_doctor(
                path,
                assert_runtime,
                assert_schedule,
                assert_learning,
                assert_autonomous,
                json,
                Provider::Codex,
            )?,
            Some(ProviderAiAction::TouchLaunch { mode, cwd }) => {
                codex_touch_launch(mode, cwd, Provider::Codex)?
            }
            Some(ProviderAiAction::EnableGlobal {
                dry_run,
                install_launchd,
                start_daemon,
                sync_skills,
                full,
                minutes,
                limit,
                max_targets,
                within_hours,
            }) => codex_enable_global(
                dry_run,
                install_launchd,
                start_daemon,
                sync_skills,
                full,
                minutes,
                limit,
                max_targets,
                within_hours,
                Provider::Codex,
            )?,
            Some(ProviderAiAction::Resolve {
                path,
                exact_cwd,
                json,
                query,
            }) => resolve_codex_input(path, query, exact_cwd, json, Provider::Codex)?,
            Some(ProviderAiAction::Runtime { action }) => {
                codex_runtime_command(action, Provider::Codex)?
            }
            Some(ProviderAiAction::Resume { session, path }) => {
                resume_session(session, path, Provider::Codex)?
            }
            Some(ProviderAiAction::Find {
                path,
                exact_cwd,
                query,
            }) => find_codex_session(path, query, exact_cwd, Provider::Codex)?,
            Some(ProviderAiAction::FindAndCopy {
                path,
                exact_cwd,
                query,
            }) => find_and_copy_codex_session(path, query, exact_cwd, Provider::Codex)?,
            Some(ProviderAiAction::Copy { session }) => copy_session(session, Provider::Codex)?,
            Some(ProviderAiAction::Context {
                session,
                count,
                path,
            }) => copy_context(session, Provider::Codex, count, path)?,
            Some(ProviderAiAction::Show {
                session,
                path,
                count,
                full,
            }) => show_session(session, Provider::Codex, count, path, full)?,
            Some(ProviderAiAction::Recover {
                path,
                exact_cwd,
                limit,
                json,
                summary_only,
                query,
            }) => recover_codex_sessions(
                path,
                query,
                exact_cwd,
                limit,
                json,
                summary_only,
                Provider::Codex,
            )?,
        },
        AiAction::Everruns(opts) => crate::ai_everruns::run(opts)?,
        AiAction::Resume { session, path } => resume_session(session, path, Provider::All)?,
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

fn for_each_nonempty_jsonl_line(path: &Path, mut on_line: impl FnMut(&str)) -> Result<()> {
    let file =
        fs::File::open(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut line = String::with_capacity(1024);

    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim_end_matches(['\n', '\r']);
        if line.trim().is_empty() {
            continue;
        }
        on_line(line);
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

    // Get sessions for Claude, Codex, and Cursor
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
            Provider::Cursor => "Cursor",
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
    let since_ts = checkpoints
        .last_commit
        .as_ref()
        .and_then(|c| c.last_entry_timestamp.clone());
    get_sessions_for_gitedit_between(project_path, since_ts.as_deref(), None)
}

/// Get structured AI session data for GitEdit/myflow sync in a strict time window.
/// Includes exchanges where `since_ts < exchange_ts <= until_ts` (when bounds are provided).
pub fn get_sessions_for_gitedit_between(
    project_path: &PathBuf,
    since_ts: Option<&str>,
    until_ts: Option<&str>,
) -> Result<Vec<GitEditSessionData>> {
    let sessions = read_sessions_for_path(Provider::All, project_path)?;

    if sessions.is_empty() {
        return Ok(vec![]);
    }

    let mut result = Vec::new();

    for session in sessions {
        let provider_name = match session.provider {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
            Provider::Cursor => "cursor",
            Provider::All => "unknown",
        };

        // Get full exchanges (not summarized)
        let exchanges = get_session_exchanges_since(
            &session.session_id,
            session.provider,
            since_ts,
            until_ts,
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
    until_ts: Option<&str>,
    project_path: &PathBuf,
) -> Result<Vec<GitEditExchange>> {
    if provider == Provider::Codex {
        let session_file = find_codex_session_file(session_id);
        if let Some(session_file) = session_file {
            let (exchanges, _) = read_codex_exchanges(&session_file, since_ts, until_ts)?;
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
    if provider == Provider::Cursor {
        let session_file = find_cursor_session_file(session_id);
        if let Some(session_file) = session_file {
            let (exchanges, _) = read_cursor_exchanges(&session_file, since_ts, until_ts)?;
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

    let window = parse_timestamp_window(since_ts, until_ts);

    let mut exchanges: Vec<GitEditExchange> = Vec::new();
    let mut current_user: Option<String> = None;
    let mut current_ts: Option<String> = None;

    for_each_nonempty_jsonl_line(&session_file, |line| {
        if let Ok(entry) = crate::json_parse::parse_json_line::<JsonlEntry>(line) {
            let entry_ts = entry.timestamp.clone();

            // In bounded mode, require a timestamp and enforce window.
            if since_ts.is_some() || until_ts.is_some() {
                let Some(ref ts) = entry_ts else {
                    return;
                };
                if !timestamp_in_window_cached(ts, &window) {
                    return;
                }
            }

            if let Some(ref msg) = entry.message {
                let role = msg.role.as_deref().unwrap_or("unknown");

                let Some(content_text) = msg.content.as_ref().and_then(extract_message_text) else {
                    return;
                };
                let Some(clean_text) = normalize_session_message(role, &content_text) else {
                    return;
                };

                match role {
                    "user" => {
                        current_user = Some(clean_text);
                        current_ts = entry_ts.clone();
                    }
                    "assistant" => {
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
    })?;

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
    if provider == Provider::Cursor {
        let session_file = find_cursor_session_file(session_id);
        let Some(session_file) = session_file else {
            return Ok(None);
        };
        return get_cursor_last_timestamp(&session_file);
    }

    let path_str = project_path.to_string_lossy().to_string();
    let project_folder = path_to_project_name(&path_str);

    let projects_dir = match provider {
        Provider::Claude | Provider::All => get_claude_projects_dir(),
        Provider::Codex => get_codex_projects_dir(),
        Provider::Cursor => get_cursor_projects_dir(),
    };

    let session_file = projects_dir
        .join(&project_folder)
        .join(format!("{}.jsonl", session_id));

    if !session_file.exists() {
        return Ok(None);
    }

    let mut last_ts: Option<String> = None;
    for_each_nonempty_jsonl_line(&session_file, |line| {
        if let Ok(entry) = crate::json_parse::parse_json_line::<JsonlEntry>(line) {
            if let Some(ts) = entry.timestamp {
                last_ts = Some(ts);
            }
        }
    })?;

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
    if provider == Provider::Cursor {
        let session_file = find_cursor_session_file(session_id).ok_or_else(|| {
            anyhow::anyhow!("Session file not found for Cursor session {}", session_id)
        })?;
        let (exchanges, last_ts) = read_cursor_exchanges(&session_file, since_ts, None)?;

        if exchanges.is_empty() {
            return Ok((String::new(), last_ts));
        }

        const MAX_EXCHANGES: usize = 5;
        const MAX_USER_CHARS: usize = 500;
        const MAX_ASSIST_CHARS: usize = 300;

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
        if total_exchanges > MAX_EXCHANGES {
            context.push_str(&format!("[+{} earlier]\n", total_exchanges - MAX_EXCHANGES));
        }

        for (user_msg, assistant_msg, _ts) in &exchanges_to_use {
            let user_intent = extract_intent(user_msg, MAX_USER_CHARS);
            let assist_summary = extract_intent(assistant_msg, MAX_ASSIST_CHARS);
            context.push_str(">");
            context.push_str(&user_intent);
            context.push('\n');
            context.push_str(&assist_summary);
            context.push_str("\n\n");
        }

        return Ok((context.trim().to_string(), last_ts));
    }

    let path_str = project_path.to_string_lossy().to_string();
    let project_folder = path_to_project_name(&path_str);

    let projects_dir = match provider {
        Provider::Claude | Provider::All => get_claude_projects_dir(),
        Provider::Codex => get_codex_projects_dir(),
        Provider::Cursor => get_cursor_projects_dir(),
    };

    let session_file = projects_dir
        .join(&project_folder)
        .join(format!("{}.jsonl", session_id));

    if !session_file.exists() {
        bail!("Session file not found: {}", session_file.display());
    }

    // Collect exchanges after the checkpoint timestamp
    let mut exchanges: Vec<(String, String, String)> = Vec::new(); // (user_msg, assistant_msg, timestamp)
    let mut current_user: Option<String> = None;
    let mut current_ts: Option<String> = None;
    let mut last_ts: Option<String> = None;

    for_each_nonempty_jsonl_line(&session_file, |line| {
        if let Ok(entry) = crate::json_parse::parse_json_line::<JsonlEntry>(line) {
            let entry_ts = entry.timestamp.clone();

            // Skip entries before checkpoint
            if let (Some(since), Some(ts)) = (since_ts, &entry_ts) {
                if ts.as_str() <= since {
                    return;
                }
            }

            if let Some(ref msg) = entry.message {
                let role = msg.role.as_deref().unwrap_or("unknown");

                let Some(content_text) = msg.content.as_ref().and_then(extract_message_text) else {
                    return;
                };
                let Some(clean_text) = normalize_session_message(role, &content_text) else {
                    return;
                };

                match role {
                    "user" => {
                        current_user = Some(clean_text);
                        current_ts = entry_ts.clone();
                    }
                    "assistant" => {
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
    })?;

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
    let (exchanges, last_ts) = read_codex_exchanges(session_file, since_ts, None)?;

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
    let (exchanges, _last_ts) = read_codex_exchanges(session_file, None, None)?;

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

pub(crate) fn read_codex_memory_exchanges(
    session_id: &str,
    max_count: usize,
) -> Result<Vec<(String, String)>> {
    let session_file = find_codex_session_file(session_id)
        .ok_or_else(|| anyhow::anyhow!("Codex session file not found: {}", session_id))?;
    let (exchanges, _last_ts) = read_codex_exchanges(&session_file, None, None)?;
    if exchanges.is_empty() || max_count == 0 {
        return Ok(Vec::new());
    }

    let start = exchanges.len().saturating_sub(max_count);
    Ok(exchanges[start..]
        .iter()
        .filter_map(|(user, assistant, _)| {
            let user = normalize_session_message("user", user)?;
            let assistant = normalize_session_message("assistant", assistant)?;
            Some((user, assistant))
        })
        .collect())
}

fn read_cursor_last_context(session_file: &PathBuf, count: usize) -> Result<String> {
    let (exchanges, _last_ts) = read_cursor_exchanges(session_file, None, None)?;

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
    until_ts: Option<&str>,
) -> Result<(Vec<(String, String, String)>, Option<String>)> {
    let window = parse_timestamp_window(since_ts, until_ts);
    let mut exchanges: Vec<(String, String, String)> = Vec::new();
    let mut current_user: Option<String> = None;
    let mut current_ts: Option<String> = None;
    let mut last_ts: Option<String> = None;

    for_each_nonempty_jsonl_line(session_file, |line| {
        let entry: CodexEntry = match crate::json_parse::parse_json_line(line) {
            Ok(v) => v,
            Err(_) => return,
        };

        let entry_ts = entry.timestamp.clone();
        if since_ts.is_some() || until_ts.is_some() {
            let Some(ts) = entry_ts.as_deref() else {
                return;
            };
            if !timestamp_in_window_cached(ts, &window) {
                return;
            }
        }

        if let Some((role, text)) = extract_codex_message(&entry) {
            match role.as_str() {
                "user" => {
                    current_user = Some(text);
                    current_ts = entry_ts.clone();
                }
                "assistant" => {
                    if let Some(user_msg) = current_user.take() {
                        let ts = current_ts.take().or(entry_ts.clone()).unwrap_or_default();
                        exchanges.push((user_msg, text, ts.clone()));
                        last_ts = Some(ts);
                    }
                }
                _ => {}
            }
        }

        if entry_ts.is_some() {
            last_ts = entry_ts;
        }
    })?;

    Ok((exchanges, last_ts))
}

fn read_cursor_exchanges(
    session_file: &PathBuf,
    since_ts: Option<&str>,
    until_ts: Option<&str>,
) -> Result<(Vec<(String, String, String)>, Option<String>)> {
    let session_ts = get_cursor_last_timestamp(session_file)?;
    if since_ts.is_some() || until_ts.is_some() {
        let window = parse_timestamp_window(since_ts, until_ts);
        if session_ts
            .as_deref()
            .map(|ts| !timestamp_in_window_cached(ts, &window))
            .unwrap_or(false)
        {
            return Ok((Vec::new(), session_ts));
        }
    }

    let mut exchanges: Vec<(String, String, String)> = Vec::new();
    let mut current_user: Option<String> = None;

    for_each_nonempty_jsonl_line(session_file, |line| {
        let entry: CursorEntry = match crate::json_parse::parse_json_line(line) {
            Ok(v) => v,
            Err(_) => return,
        };

        let Some((role, text)) = extract_cursor_message(&entry) else {
            return;
        };

        match role.as_str() {
            "user" => {
                current_user = Some(text);
            }
            "assistant" => {
                if let Some(user_msg) = current_user.take() {
                    let ts = session_ts.clone().unwrap_or_default();
                    exchanges.push((user_msg, text, ts));
                }
            }
            _ => {}
        }
    })?;

    Ok((exchanges, session_ts))
}

fn parse_timestamp_for_compare(ts: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%S%.fZ")
                .map(|dt| dt.and_utc())
        })
        .ok()
}

struct TimestampWindow<'a> {
    since_raw: Option<&'a str>,
    until_raw: Option<&'a str>,
    since_dt: Option<chrono::DateTime<chrono::Utc>>,
    until_dt: Option<chrono::DateTime<chrono::Utc>>,
}

fn parse_timestamp_window<'a>(
    since_ts: Option<&'a str>,
    until_ts: Option<&'a str>,
) -> TimestampWindow<'a> {
    TimestampWindow {
        since_raw: since_ts,
        until_raw: until_ts,
        since_dt: since_ts.and_then(parse_timestamp_for_compare),
        until_dt: until_ts.and_then(parse_timestamp_for_compare),
    }
}

fn timestamp_in_window_cached(ts: &str, window: &TimestampWindow<'_>) -> bool {
    let ts_dt = parse_timestamp_for_compare(ts);

    if let Some(entry_dt) = ts_dt {
        if let Some(lower) = window.since_dt {
            if entry_dt <= lower {
                return false;
            }
        } else if let Some(lower_raw) = window.since_raw {
            if ts <= lower_raw {
                return false;
            }
        }

        if let Some(upper) = window.until_dt {
            if entry_dt > upper {
                return false;
            }
        } else if let Some(upper_raw) = window.until_raw {
            if ts > upper_raw {
                return false;
            }
        }
        return true;
    }

    if let Some(lower_raw) = window.since_raw {
        if ts <= lower_raw {
            return false;
        }
    }
    if let Some(upper_raw) = window.until_raw {
        if ts > upper_raw {
            return false;
        }
    }
    true
}

fn get_codex_last_timestamp(session_file: &PathBuf) -> Result<Option<String>> {
    let mut last_ts: Option<String> = None;

    for_each_nonempty_jsonl_line(session_file, |line| {
        let entry: CodexEntry = match crate::json_parse::parse_json_line(line) {
            Ok(v) => v,
            Err(_) => return,
        };

        if let Some(ts) = entry.timestamp {
            last_ts = Some(ts);
            return;
        }

        if let Some(payload_ts) = entry
            .payload
            .as_ref()
            .and_then(|p| p.get("timestamp"))
            .and_then(|v| v.as_str())
        {
            last_ts = Some(payload_ts.to_string());
        }
    })?;

    Ok(last_ts)
}

fn get_cursor_last_timestamp(session_file: &PathBuf) -> Result<Option<String>> {
    Ok(get_cursor_file_timestamp(session_file))
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
        let clean_text = normalize_session_message(&role, &text)?;
        return Some((role, clean_text));
    }

    if entry_type == Some("event_msg") {
        let payload = entry.payload.as_ref()?;
        let payload_type = payload.get("type").and_then(|v| v.as_str());
        if payload_type == Some("user_message") {
            let text = payload.get("message").and_then(|v| v.as_str())?;
            let clean_text = normalize_session_message("user", text)?;
            return Some(("user".to_string(), clean_text));
        }
        if payload_type == Some("agent_message") {
            let text = payload.get("message").and_then(|v| v.as_str())?;
            let clean_text = normalize_session_message("assistant", text)?;
            return Some(("assistant".to_string(), clean_text));
        }
    }

    if entry_type == Some("message") {
        let role = entry.role.as_deref()?.to_string();
        let content = entry.content.as_ref()?;
        let text = extract_codex_content_text(content)?;
        let clean_text = normalize_session_message(&role, &text)?;
        return Some((role, clean_text));
    }

    None
}

fn normalize_cursor_role(role: &str) -> &str {
    match role {
        "assistant" | "assistanlft" => "assistant",
        "user" => "user",
        other => other,
    }
}

fn extract_cursor_message(entry: &CursorEntry) -> Option<(String, String)> {
    let role = normalize_cursor_role(entry.role.as_deref()?);
    if role != "user" && role != "assistant" {
        return None;
    }

    let message = entry.message.as_ref()?;
    let content = message.content.as_ref()?;
    let text = extract_message_text(content)?;
    let clean_text = normalize_session_message(role, &text)?;
    Some((role.to_string(), clean_text))
}

/// Get recent AI session context for the current project.
/// Used by commit workflow to provide context for code review.
/// Returns the last N exchanges from the most recent sessions.
pub fn get_recent_session_context(max_exchanges: usize) -> Result<Option<String>> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;

    // Get sessions for Claude, Codex, and Cursor
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
                    Provider::Cursor => "Cursor",
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
            Provider::Cursor => "cursor",
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
        Provider::Cursor => read_cursor_messages(session_id),
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

    let mut messages = Vec::new();
    let mut started_at: Option<String> = None;
    let mut last_message_at: Option<String> = None;

    for_each_nonempty_jsonl_line(&session_file, |line| {
        let Ok(entry) = crate::json_parse::parse_json_line::<JsonlEntry>(line) else {
            return;
        };
        let Some(ref msg) = entry.message else {
            return;
        };
        let role = msg.role.as_deref().unwrap_or("unknown");
        if role != "user" && role != "assistant" {
            return;
        }
        let content_text = msg.content.as_ref().and_then(extract_message_text);
        let Some(content_text) = content_text else {
            return;
        };
        let Some(clean_text) = normalize_session_message(role, &content_text) else {
            return;
        };
        push_message(&mut messages, role, &clean_text);
        if let Some(ts) = entry.timestamp.clone() {
            if started_at.is_none() {
                started_at = Some(ts.clone());
            }
            last_message_at = Some(ts);
        }
    })?;

    Ok(SessionMessages {
        messages,
        started_at,
        last_message_at,
    })
}

fn read_codex_messages(session_id: &str) -> Result<SessionMessages> {
    let session_file = find_codex_session_file(session_id)
        .ok_or_else(|| anyhow::anyhow!("Codex session file not found"))?;
    let mut messages = Vec::new();
    let mut started_at: Option<String> = None;
    let mut last_message_at: Option<String> = None;

    for_each_nonempty_jsonl_line(&session_file, |line| {
        let entry: CodexEntry = match crate::json_parse::parse_json_line(line) {
            Ok(v) => v,
            Err(_) => return,
        };

        let Some((role, text)) = extract_codex_message(&entry) else {
            return;
        };
        push_message(&mut messages, &role, &text);
        if let Some(ts) = extract_codex_timestamp(&entry) {
            if started_at.is_none() {
                started_at = Some(ts.clone());
            }
            last_message_at = Some(ts);
        }
    })?;

    Ok(SessionMessages {
        messages,
        started_at,
        last_message_at,
    })
}

fn read_cursor_messages(session_id: &str) -> Result<SessionMessages> {
    let session_file = find_cursor_session_file(session_id)
        .ok_or_else(|| anyhow::anyhow!("Cursor session file not found"))?;
    let mut messages = Vec::new();
    let mut started_at = get_cursor_file_timestamp(&session_file);
    let mut last_message_at = started_at.clone();

    for_each_nonempty_jsonl_line(&session_file, |line| {
        let entry: CursorEntry = match crate::json_parse::parse_json_line(line) {
            Ok(v) => v,
            Err(_) => return,
        };

        let Some((role, text)) = extract_cursor_message(&entry) else {
            return;
        };
        push_message(&mut messages, &role, &text);
    })?;

    if started_at.is_none() && !messages.is_empty() {
        started_at = Some(chrono::Utc::now().to_rfc3339());
        last_message_at = started_at.clone();
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

fn strip_tagged_block(text: &str, open_tag: &str, close_tag: &str) -> String {
    let mut result = text.to_string();
    while let Some(start) = result.find(open_tag) {
        if let Some(end) = result[start..].find(close_tag) {
            let end_pos = start + end + close_tag.len();
            result = format!("{}{}", &result[..start], &result[end_pos..]);
        } else {
            result = result[..start].to_string();
            break;
        }
    }
    result
}

fn truncate_before_heading(text: &str, heading: &str) -> String {
    let mut offset = 0usize;
    for line in text.lines() {
        if line.trim_start().starts_with(heading) {
            return text[..offset].trim().to_string();
        }
        offset += line.len();
        if offset < text.len() {
            offset += 1;
        }
    }
    text.trim().to_string()
}

fn collapse_blank_lines(text: &str) -> String {
    let mut out = String::new();
    let mut saw_blank = false;

    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim().is_empty() {
            if saw_blank || out.is_empty() {
                continue;
            }
            saw_blank = true;
            out.push('\n');
            continue;
        }

        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(trimmed);
        out.push('\n');
        saw_blank = false;
    }

    out.trim().to_string()
}

fn strip_known_transcript_scaffolding(role: &str, text: &str) -> String {
    let mut cleaned = strip_system_reminders(text);

    cleaned = strip_tagged_block(&cleaned, "<environment_context>", "</environment_context>");
    cleaned = strip_tagged_block(
        &cleaned,
        "<permissions instructions>",
        "</permissions instructions>",
    );
    cleaned = strip_tagged_block(&cleaned, "<collaboration_mode>", "</collaboration_mode>");

    let trimmed = cleaned.trim_start();
    if trimmed.starts_with("# AGENTS.md instructions for ")
        || trimmed.starts_with("# agents.md instructions for ")
    {
        return String::new();
    }

    cleaned = truncate_before_heading(&cleaned, "Workflow context:");
    cleaned = truncate_before_heading(&cleaned, "Start by checking:");
    cleaned = truncate_before_heading(&cleaned, "Designer stack notes:");

    if role == "assistant" {
        let trimmed = cleaned.trim_start();
        if trimmed.starts_with("Using `")
            && (trimmed.contains("workflow")
                || trimmed.contains("dispatch")
                || trimmed.contains("because this is"))
        {
            return String::new();
        }
    }

    collapse_blank_lines(&cleaned)
}

fn normalize_session_message(role: &str, text: &str) -> Option<String> {
    if role != "user" && role != "assistant" {
        return None;
    }

    let cleaned = if role == "assistant" {
        strip_thinking_blocks(text)
    } else {
        text.to_string()
    };
    let cleaned = strip_known_transcript_scaffolding(role, &cleaned);
    let cleaned = cleaned.trim();

    if cleaned.is_empty() || is_session_boilerplate(cleaned) {
        return None;
    }

    Some(cleaned.to_string())
}

fn get_cursor_file_timestamp(path: &Path) -> Option<String> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    Some(DateTime::<Utc>::from(modified).to_rfc3339())
}

fn push_message(messages: &mut Vec<WebSessionMessage>, role: &str, content: &str) {
    if let Some(last) = messages.last_mut() {
        if last.role == role {
            if last.content.trim() == content.trim() {
                return;
            }
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

fn get_cursor_projects_dir() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".cursor").join("projects")
}

/// Convert a path to project folder name (replaces / with -).
fn path_to_project_name(path: &str) -> String {
    path.replace('/', "-")
}

fn path_to_cursor_project_key(path: &Path) -> String {
    path.to_string_lossy()
        .trim_start_matches('/')
        .replace('/', "-")
}

fn cursor_project_key_matches_path(project_key: &str, path: &Path) -> bool {
    let prefix = path_to_cursor_project_key(path);
    project_key == prefix
        || project_key
            .strip_prefix(&prefix)
            .map(|rest| rest.starts_with('-'))
            .unwrap_or(false)
}

fn decode_cursor_project_path(project_key: &str) -> Option<PathBuf> {
    let mut segments = project_key.split('-');
    let root = segments.next()?;
    let second = segments.next()?;
    let mut current = PathBuf::from("/").join(root).join(second);
    if !current.exists() {
        return None;
    }

    let remaining: Vec<String> = segments.map(|segment| segment.to_string()).collect();
    let mut index = 0usize;

    while index < remaining.len() {
        let entries = fs::read_dir(&current).ok()?;
        let mut best_match: Option<(usize, PathBuf)> = None;

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            let Some(name) = entry.file_name().to_str().map(|value| value.to_string()) else {
                continue;
            };
            let name_segments: Vec<&str> = name.split('-').collect();
            if name_segments.len() > remaining.len().saturating_sub(index) {
                continue;
            }

            let matches = name_segments
                .iter()
                .zip(remaining[index..].iter())
                .all(|(expected, actual)| *expected == actual);
            if !matches {
                continue;
            }

            let consumed = name_segments.len();
            let should_replace = best_match
                .as_ref()
                .map(|(best_consumed, _)| consumed > *best_consumed)
                .unwrap_or(true);
            if should_replace {
                best_match = Some((consumed, path));
            }
        }

        let Some((consumed, next_path)) = best_match else {
            return None;
        };
        current = next_path;
        index += consumed;
    }

    Some(current)
}

fn collect_cursor_project_session_files(project_dir: &Path) -> Vec<PathBuf> {
    let transcripts_dir = project_dir.join("agent-transcripts");
    if !transcripts_dir.exists() {
        return Vec::new();
    }

    let mut files = Vec::new();
    let Ok(entries) = fs::read_dir(&transcripts_dir) else {
        return files;
    };

    for entry in entries.flatten() {
        let session_dir = entry.path();
        if !session_dir.is_dir() {
            continue;
        }

        let Ok(session_entries) = fs::read_dir(&session_dir) else {
            continue;
        };
        for session_entry in session_entries.flatten() {
            let file_path = session_entry.path();
            if file_path
                .extension()
                .map(|ext| ext == "jsonl")
                .unwrap_or(false)
            {
                files.push(file_path);
            }
        }
    }

    files
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

    if provider == Provider::Cursor || provider == Provider::All {
        sessions.extend(read_provider_sessions(Provider::Cursor)?);
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

fn resolve_session_target_path(path: Option<&str>) -> Result<PathBuf> {
    match path.map(str::trim).filter(|value| !value.is_empty()) {
        Some(raw) => {
            let expanded = PathBuf::from(shellexpand::tilde(raw).to_string());
            let resolved = if expanded.is_absolute() {
                expanded
            } else {
                env::current_dir()?.join(expanded)
            };
            Ok(resolved.canonicalize().unwrap_or(resolved))
        }
        None => {
            let resolved = env::current_dir().context("failed to get current directory")?;
            Ok(resolved.canonicalize().unwrap_or(resolved))
        }
    }
}

fn read_sessions_for_target(provider: Provider, path: Option<&str>) -> Result<Vec<AiSession>> {
    let target = resolve_session_target_path(path)?;
    read_sessions_for_path(provider, &target)
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

    if provider == Provider::Cursor || provider == Provider::All {
        sessions.extend(read_provider_sessions_for_path(Provider::Cursor, path)?);
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
    if provider == Provider::Cursor {
        return read_cursor_sessions_for_path(path);
    }

    let path_str = path.to_string_lossy().to_string();
    let project_name = path_to_project_name(&path_str);

    let projects_dir = match provider {
        Provider::Claude => get_claude_projects_dir(),
        Provider::Codex => get_codex_projects_dir(),
        Provider::Cursor => get_cursor_projects_dir(),
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
    if provider == Provider::Cursor {
        let cwd = std::env::current_dir().context("failed to get current directory")?;
        return read_cursor_sessions_for_path(&cwd);
    }

    let cwd = std::env::current_dir()?;
    let cwd_str = cwd.to_string_lossy().to_string();
    let project_name = path_to_project_name(&cwd_str);

    let projects_dir = match provider {
        Provider::Claude => get_claude_projects_dir(),
        Provider::Codex => get_codex_projects_dir(),
        Provider::Cursor => get_cursor_projects_dir(),
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
    if provider == Provider::Cursor {
        return parse_cursor_session_file(path, session_id);
    }

    let mut timestamp = None;
    let mut last_message_at = None;
    let mut last_message = None;
    let mut first_message = None;
    let mut error_summary = None;

    for_each_nonempty_jsonl_line(path, |line| {
        if let Ok(entry) = crate::json_parse::parse_json_line::<JsonlEntry>(line) {
            // Get timestamp from first entry
            if timestamp.is_none() {
                timestamp = entry.timestamp.clone();
            }

            if let Some(ref msg) = entry.message {
                let role = msg.role.as_deref();
                if role == Some("user") || role == Some("assistant") {
                    if let Some(ref content) = msg.content {
                        if let Some(text) = extract_message_text(content) {
                            if let Some(clean_text) =
                                normalize_session_message(role.unwrap_or("unknown"), &text)
                            {
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
                            first_message = extract_message_text(content)
                                .and_then(|text| normalize_session_message("user", &text));
                        }
                    }
                }
            }

            // Capture first error summary (useful when no user message exists)
            if error_summary.is_none() {
                error_summary = extract_error_summary(&entry);
            }
        }
    })
    .ok()?;

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
    let mut timestamp = None;
    let mut last_message_at = None;
    let mut last_message = None;
    let mut first_message = None;
    let mut error_summary = None;
    let mut session_id = None;
    let mut cwd = None;

    for_each_nonempty_jsonl_line(path, |line| {
        let entry: CodexEntry = match crate::json_parse::parse_json_line(line) {
            Ok(v) => v,
            Err(_) => return,
        };

        if timestamp.is_none() {
            timestamp = entry.timestamp.clone();
        }

        if let Some((_role, text)) = extract_codex_message(&entry) {
            if !text.trim().is_empty() {
                last_message = Some(text);
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
    })
    .ok()?;

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

fn parse_cursor_session_file(path: &PathBuf, fallback_id: &str) -> Option<AiSession> {
    let timestamp = get_cursor_file_timestamp(path);
    let mut last_message = None;
    let mut first_message = None;

    for_each_nonempty_jsonl_line(path, |line| {
        let entry: CursorEntry = match crate::json_parse::parse_json_line(line) {
            Ok(v) => v,
            Err(_) => return,
        };

        let Some((role, text)) = extract_cursor_message(&entry) else {
            return;
        };
        last_message = Some(text.clone());
        if first_message.is_none() && role == "user" {
            first_message = Some(text);
        }
    })
    .ok()?;

    Some(AiSession {
        session_id: fallback_id.to_string(),
        provider: Provider::Cursor,
        timestamp: timestamp.clone(),
        last_message_at: timestamp,
        last_message,
        first_message,
        error_summary: None,
    })
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

fn read_cursor_sessions_for_path(path: &PathBuf) -> Result<Vec<AiSession>> {
    let projects_dir = get_cursor_projects_dir();
    if !projects_dir.exists() {
        return Ok(vec![]);
    }

    let mut sessions = Vec::new();
    let entries = fs::read_dir(&projects_dir)
        .with_context(|| format!("failed to read {}", projects_dir.display()))?;

    for entry in entries.flatten() {
        let project_dir = entry.path();
        if !project_dir.is_dir() {
            continue;
        }

        let Some(project_key) = project_dir.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !cursor_project_key_matches_path(project_key, path) {
            continue;
        }

        for file_path in collect_cursor_project_session_files(&project_dir) {
            let filename = file_path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if let Some(session) = parse_cursor_session_file(&file_path, filename) {
                sessions.push(session);
            }
        }
    }

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

fn codex_session_id_from_path(path: &Path) -> Option<String> {
    let filename = path.file_stem()?.to_str()?;
    Some(filename.split('_').next().unwrap_or(filename).to_string())
}

fn cursor_session_id_from_path(path: &Path) -> Option<String> {
    path.file_stem()
        .and_then(|name| name.to_str())
        .map(str::to_string)
}

fn resolve_explicit_native_session(query: &str, provider: Provider) -> Option<(String, Provider)> {
    if matches!(provider, Provider::Codex | Provider::All) {
        if let Some(path) = find_codex_session_file(query) {
            if let Some(session_id) = codex_session_id_from_path(&path) {
                return Some((session_id, Provider::Codex));
            }
        }
    }

    if matches!(provider, Provider::Cursor | Provider::All) {
        if let Some(path) = find_cursor_session_file(query) {
            if let Some(session_id) = cursor_session_id_from_path(&path) {
                return Some((session_id, Provider::Cursor));
            }
        }
    }

    None
}

fn resolve_session_selection(
    query: &str,
    sessions: &[AiSession],
    index: &SessionIndex,
    provider: Provider,
) -> Result<(String, Provider)> {
    if let Some((_, saved)) = index
        .sessions
        .iter()
        .find(|(name, _)| name.as_str() == query)
    {
        if let Some(session) = sessions.iter().find(|s| s.session_id == saved.id) {
            return Ok((saved.id.clone(), session.provider));
        }
        if let Some((session_id, session_provider)) =
            resolve_explicit_native_session(&saved.id, provider)
        {
            return Ok((session_id, session_provider));
        }
        return Ok((saved.id.clone(), Provider::Claude));
    }

    if let Some(session) = sessions
        .iter()
        .find(|s| s.session_id == *query || s.session_id.starts_with(query))
    {
        return Ok((session.session_id.clone(), session.provider));
    }

    if let Some((session_id, session_provider)) = resolve_explicit_native_session(query, provider) {
        return Ok((session_id, session_provider));
    }

    bail!("Session not found: {}", query);
}

/// Get the most recent session ID for this project.
fn get_most_recent_session_id() -> Result<Option<String>> {
    let sessions = read_sessions_for_project(Provider::All)?;
    Ok(sessions.first().map(|s| s.session_id.clone()))
}

fn format_session_ref(session: &AiSession, include_provider: bool) -> String {
    if !include_provider {
        return session.session_id.clone();
    }

    let provider = match session.provider {
        Provider::Claude => "claude",
        Provider::Codex => "codex",
        Provider::Cursor => "cursor",
        Provider::All => "ai",
    };
    format!("{provider}:{}", session.session_id)
}

fn print_latest_session_id(provider: Provider, path: Option<String>) -> Result<()> {
    let target = resolve_session_target_path(path.as_deref())?;
    if provider == Provider::Codex {
        let rows = read_recent_codex_threads(&target, false, 1, None)?;
        let Some(row) = rows.first() else {
            bail!("No Codex sessions found for {}", target.display());
        };
        println!("{}", row.id);
        return Ok(());
    }

    let sessions = read_sessions_for_path(provider, &target)?;
    let Some(session) = sessions.first() else {
        let provider_name = match provider {
            Provider::Claude => "Claude",
            Provider::Codex => "Codex",
            Provider::Cursor => "Cursor",
            Provider::All => "AI",
        };
        bail!("No {provider_name} sessions found for {}", target.display());
    };

    println!("{}", format_session_ref(session, false));
    Ok(())
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
            Provider::Cursor => "Cursor",
            Provider::All => "AI",
        };
        println!("No {} sessions found for this project.", provider_name);
        if provider == Provider::Cursor {
            println!("\nTip: open this repo in Cursor and use its agent to create transcripts.");
        } else {
            println!("\nTip: Run `claude` or `codex` in this directory to start a session,");
            println!("     then use `f ai save <name>` to bookmark it.");
        }
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
                Provider::Cursor => "cursor | ",
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
        if selected.provider == Provider::Cursor {
            let history = read_session_history(&selected.session_id, selected.provider)?;
            copy_to_clipboard(&history)?;
            let line_count = history.lines().count();
            println!(
                "Copied Cursor session {} ({} lines) to clipboard",
                &selected.session_id[..8.min(selected.session_id.len())],
                line_count
            );
            return Ok(());
        }
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
    launch_session_for_target(session_id, provider, None, None, None)
}

fn launch_session_for_target(
    session_id: &str,
    provider: Provider,
    prompt: Option<&str>,
    target_path: Option<&Path>,
    runtime_state_path: Option<&str>,
) -> Result<bool> {
    let status = match provider {
        Provider::Claude | Provider::All => {
            // Claude uses: claude --resume <session_id> --dangerously-skip-permissions
            let mut command = Command::new("claude");
            command
                .arg("--resume")
                .arg(session_id)
                .arg("--dangerously-skip-permissions");
            if let Some(path) = target_path {
                command.current_dir(path);
            }
            command
                .status()
                .with_context(|| "failed to launch claude")?
        }
        Provider::Codex => {
            // Codex uses: codex resume --dangerously-bypass-approvals-and-sandbox <session_id> [prompt]
            let workdir = target_path
                .map(Path::to_path_buf)
                .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            let mut command = Command::new(configured_codex_bin_for_workdir(&workdir));
            command.arg("resume");
            if let Some(path) = target_path {
                command.current_dir(path);
            }
            apply_codex_trust_overrides_for(&mut command, target_path);
            apply_codex_runtime_state_to_command(&mut command, runtime_state_path);
            command
                .arg("--dangerously-bypass-approvals-and-sandbox")
                .arg(session_id);
            if let Some(prompt) = prompt.map(str::trim).filter(|value| !value.is_empty()) {
                command.arg(prompt);
            }
            command.status().with_context(|| "failed to launch codex")?
        }
        Provider::Cursor => {
            bail!(
                "Cursor transcripts are readable only; use `f cursor list`, `f cursor copy`, or `f cursor context`"
            );
        }
    };

    Ok(status.success())
}

fn launch_claude_continue() -> Result<bool> {
    let status = Command::new("claude")
        .arg("--continue")
        .arg("--dangerously-skip-permissions")
        .status()
        .with_context(|| "failed to launch claude --continue")?;
    Ok(status.success())
}

fn launch_claude_resume_picker() -> Result<bool> {
    let status = Command::new("claude")
        .arg("--resume")
        .arg("--dangerously-skip-permissions")
        .status()
        .with_context(|| "failed to launch claude --resume")?;
    Ok(status.success())
}

fn detect_git_root(path: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .arg("rev-parse")
        .arg("--show-toplevel")
        .current_dir(path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return None;
    }

    Some(PathBuf::from(trimmed))
}

fn codex_trusted_paths() -> Vec<PathBuf> {
    env::current_dir()
        .ok()
        .map(|path| codex_trusted_paths_for(&path))
        .unwrap_or_default()
}

fn codex_trusted_paths_for(seed: &Path) -> Vec<PathBuf> {
    let mut paths = BTreeSet::new();
    let raw_cwd = seed.to_path_buf();
    paths.insert(raw_cwd.clone());
    if let Some(raw_git_root) = detect_git_root(&raw_cwd) {
        paths.insert(raw_git_root);
    }

    if let Ok(canonical_cwd) = raw_cwd.canonicalize() {
        paths.insert(canonical_cwd.clone());
        if let Some(canonical_git_root) = detect_git_root(&canonical_cwd) {
            paths.insert(canonical_git_root);
        }
    }
    paths.into_iter().collect()
}

fn codex_projects_override(paths: &[PathBuf]) -> Option<String> {
    if paths.is_empty() {
        return None;
    }

    let projects = paths
        .iter()
        .map(|path| {
            let escaped = path
                .display()
                .to_string()
                .replace('\\', "\\\\")
                .replace('"', "\\\"");
            format!("\"{escaped}\"={{ trust_level=\"trusted\" }}")
        })
        .collect::<Vec<_>>()
        .join(", ");

    Some(format!("projects={{ {projects} }}"))
}

fn apply_codex_trust_overrides(command: &mut Command) {
    if let Some(override_value) = codex_projects_override(&codex_trusted_paths()) {
        command.arg("--config").arg(override_value);
    }
}

fn apply_codex_trust_overrides_for(command: &mut Command, target_path: Option<&Path>) {
    let paths = target_path
        .map(codex_trusted_paths_for)
        .unwrap_or_else(codex_trusted_paths);
    if let Some(override_value) = codex_projects_override(&paths) {
        command.arg("--config").arg(override_value);
    }
}

fn apply_codex_runtime_state_to_command(command: &mut Command, runtime_state_path: Option<&str>) {
    if let Some(path) = runtime_state_path
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        command.env("FLOW_CODEX_RUNTIME_STATE", path);
    }
}

fn codex_runtime_transport_enabled(target_path: &Path) -> bool {
    if let Ok(value) = env::var("FLOW_CODEX_RUNTIME_TRANSPORT") {
        let normalized = value.trim().to_ascii_lowercase();
        if matches!(normalized.as_str(), "1" | "true" | "yes" | "on") {
            return true;
        }
    }

    let bin = configured_codex_bin_for_workdir(target_path);
    Path::new(&bin)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(bin.as_str())
        .contains("codex-flow-wrapper")
}

fn launch_codex_resume_picker() -> Result<bool> {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut command = Command::new(configured_codex_bin_for_workdir(&cwd));
    command
        .arg("resume")
        .arg("--dangerously-bypass-approvals-and-sandbox");
    apply_codex_trust_overrides(&mut command);
    let status = command
        .status()
        .with_context(|| "failed to launch codex resume")?;
    Ok(status.success())
}

fn launch_codex_continue_last_for_target(target_path: Option<&Path>) -> Result<bool> {
    let workdir = target_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let mut command = Command::new(configured_codex_bin_for_workdir(&workdir));
    command.arg("resume");
    if let Some(path) = target_path {
        command.current_dir(path);
    }
    apply_codex_trust_overrides_for(&mut command, target_path);
    command
        .arg("--last")
        .arg("--dangerously-bypass-approvals-and-sandbox");
    let status = command
        .status()
        .with_context(|| "failed to launch codex resume --last")?;
    Ok(status.success())
}

fn provider_name(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "claude",
        Provider::Codex => "codex",
        Provider::Cursor => "cursor",
        Provider::All => "ai",
    }
}

fn ensure_provider_tty(provider: Provider, action: &str) -> Result<()> {
    if io::stdin().is_terminal() && io::stdout().is_terminal() {
        return Ok(());
    }

    bail!(
        "{} {} requires an interactive terminal (TTY); run this in a terminal tab (e.g. Zed/Ghostty)",
        provider_name(provider),
        action
    );
}

fn provider_sessions(provider: Provider) -> Result<()> {
    if provider == Provider::All {
        bail!("sessions requires a specific provider (claude or codex)");
    }
    ensure_provider_tty(provider, "sessions")?;

    let launched = match provider {
        Provider::Claude => launch_claude_resume_picker()?,
        Provider::Codex => launch_codex_resume_picker()?,
        Provider::Cursor => false,
        Provider::All => false,
    };

    if launched {
        Ok(())
    } else {
        bail!("failed to open {} session picker", provider_name(provider))
    }
}

fn continue_session(
    session: Option<String>,
    path: Option<String>,
    provider: Provider,
) -> Result<()> {
    if session.is_some() {
        return resume_session(session, path, provider);
    }
    if provider == Provider::All {
        bail!("continue requires a specific provider (claude or codex)");
    }
    ensure_provider_tty(provider, "continue")?;

    if path
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_some()
    {
        let target = resolve_session_target_path(path.as_deref())?;
        let sessions = read_sessions_for_target(provider, path.as_deref())?;
        let sess = sessions.first().ok_or_else(|| {
            anyhow::anyhow!(
                "No {} sessions found for {}",
                provider_name(provider),
                target.display()
            )
        })?;
        println!(
            "Resuming session {} from {}...",
            &sess.session_id[..8.min(sess.session_id.len())],
            target.display()
        );
        if launch_session_for_target(&sess.session_id, sess.provider, None, Some(&target), None)? {
            return Ok(());
        }
        bail!(
            "failed to continue {} session {} for {}",
            provider_name(sess.provider),
            sess.session_id,
            target.display()
        );
    }

    let launched = match provider {
        Provider::Claude => launch_claude_continue()?,
        Provider::Codex => launch_codex_continue_last_for_target(None)?,
        Provider::Cursor => false,
        Provider::All => false,
    };

    if launched {
        Ok(())
    } else {
        bail!("failed to continue {} session", provider_name(provider))
    }
}

/// Quick start: continue last session or create new one with dangerous flags.
pub fn quick_start_session(provider: Provider) -> Result<()> {
    if provider == Provider::Codex {
        let launched = launch_codex_continue_last_for_target(None)?;
        if !launched {
            new_session(provider)?;
        }
        return Ok(());
    }

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
    new_session_for_target(provider, None, None, None)
}

fn new_session_for_target(
    provider: Provider,
    prompt: Option<&str>,
    target_path: Option<&Path>,
    runtime_state_path: Option<&str>,
) -> Result<()> {
    let status = match provider {
        Provider::Claude | Provider::All => {
            let mut command = Command::new("claude");
            command.arg("--dangerously-skip-permissions");
            if let Some(path) = target_path {
                command.current_dir(path);
            }
            command
                .status()
                .with_context(|| "failed to launch claude")?
        }
        Provider::Codex => {
            let workdir = target_path
                .map(Path::to_path_buf)
                .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            let mut command = Command::new(configured_codex_bin_for_workdir(&workdir));
            if let Some(path) = target_path {
                command.current_dir(path);
            }
            apply_codex_trust_overrides_for(&mut command, target_path);
            apply_codex_runtime_state_to_command(&mut command, runtime_state_path);
            command
                .arg("--yolo")
                .arg("--sandbox")
                .arg("danger-full-access");
            if let Some(prompt) = prompt.map(str::trim).filter(|value| !value.is_empty()) {
                command.arg(prompt);
            }
            command.status().with_context(|| "failed to launch codex")?
        }
        Provider::Cursor => {
            bail!(
                "Cursor transcripts are readable only; use `f cursor list`, `f cursor copy`, or `f cursor context`"
            );
        }
    };

    let name = match provider {
        Provider::Claude | Provider::All => "claude",
        Provider::Codex => "codex",
        Provider::Cursor => "cursor",
    };

    if !status.success() {
        bail!("{} exited with status {}", name, status);
    }

    Ok(())
}

fn find_codex_session(
    path: Option<String>,
    query: Vec<String>,
    exact_cwd: bool,
    provider: Provider,
) -> Result<()> {
    let selected = find_best_codex_session_match(path, query, exact_cwd, provider, "find", true)?;
    resume_session(Some(selected.id.clone()), None, Provider::Codex)
}

fn find_and_copy_codex_session(
    path: Option<String>,
    query: Vec<String>,
    exact_cwd: bool,
    provider: Provider,
) -> Result<()> {
    let selected =
        find_best_codex_session_match(path, query, exact_cwd, provider, "findAndCopy", false)?;
    copy_session_history_to_clipboard(&selected.id, Provider::Codex)?;
    println!(
        "Session {} found and copied to clipboard",
        truncate_recover_id(&selected.id)
    );
    Ok(())
}

fn find_best_codex_session_match(
    path: Option<String>,
    query: Vec<String>,
    exact_cwd: bool,
    provider: Provider,
    action_name: &str,
    verbose: bool,
) -> Result<CodexRecoverRow> {
    if provider != Provider::Codex {
        bail!(
            "{} is only supported for Codex sessions; use `f ai codex {} ...`",
            action_name,
            action_name
        );
    }

    let query_text = normalize_recover_query(&query).ok_or_else(|| {
        anyhow::anyhow!(
            "{} requires a query, for example: `f ai codex {} \"make plan to get designer\"`",
            action_name,
            action_name
        )
    })?;
    let target_path = path
        .map(|value| canonicalize_recover_path(Some(value)))
        .transpose()?;
    let rows = search_codex_threads_for_find(target_path.as_deref(), exact_cwd, &query_text, 5)?;
    let selected = rows.first().ok_or_else(|| match target_path.as_ref() {
        Some(target_path) => anyhow::anyhow!(
            "No matching Codex sessions found for {:?} under {}",
            query_text,
            target_path.display()
        ),
        None => anyhow::anyhow!("No matching Codex sessions found for {:?}", query_text),
    })?;

    if verbose {
        println!(
            "Matched Codex session {} | {} | {}",
            truncate_recover_id(&selected.id),
            format_unix_ts(selected.updated_at),
            selected.cwd
        );
        if let Some(first) = selected.first_user_message.as_deref() {
            println!("Prompt: {}", truncate_recover_text(first));
        } else if let Some(title) = selected.title.as_deref() {
            println!("Title: {}", truncate_recover_text(title));
        }
    }

    Ok(selected.clone())
}

fn recover_codex_sessions(
    path: Option<String>,
    query: Vec<String>,
    exact_cwd: bool,
    limit: usize,
    json_output: bool,
    summary_only: bool,
    provider: Provider,
) -> Result<()> {
    if provider != Provider::Codex {
        bail!("recover is only supported for Codex sessions; use `f ai codex recover ...`");
    }

    let query_text = normalize_recover_query(&query);
    let requested_target_path = canonicalize_recover_path(path)?;
    let explicit_session_hint = query_text.as_deref().and_then(extract_codex_session_hint);
    let (target_path, rows) = if let Some(session_hint) = explicit_session_hint.as_deref() {
        let rows = read_codex_threads_by_session_hint(session_hint, limit.max(1))?;
        if let Some(first) = rows.first() {
            (canonicalize_recover_path(Some(first.cwd.clone()))?, rows)
        } else {
            (
                requested_target_path.clone(),
                read_recent_codex_threads(
                    &requested_target_path,
                    exact_cwd,
                    limit.max(1),
                    query_text.as_deref(),
                )?,
            )
        }
    } else {
        (
            requested_target_path.clone(),
            read_recent_codex_threads(
                &requested_target_path,
                exact_cwd,
                limit.max(1),
                query_text.as_deref(),
            )?,
        )
    };
    let output = build_recover_output(&target_path, exact_cwd, query_text, rows);

    if summary_only {
        println!("{}", output.summary);
        return Ok(());
    }

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&output).context("failed to encode recovery JSON")?
        );
        return Ok(());
    }

    print_recover_output(&output);
    Ok(())
}

fn canonicalize_recover_path(path: Option<String>) -> Result<PathBuf> {
    let raw = path.unwrap_or_else(|| ".".to_string());
    let expanded = shellexpand::tilde(&raw).to_string();
    let candidate = PathBuf::from(expanded);
    let absolute = if candidate.is_absolute() {
        candidate
    } else {
        env::current_dir()
            .context("failed to determine current directory")?
            .join(candidate)
    };
    Ok(absolute.canonicalize().unwrap_or(absolute))
}

fn normalize_recover_query(parts: &[String]) -> Option<String> {
    let text = parts.join(" ").trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

fn recover_query_tokens(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(|part| {
            part.trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '-' && ch != '_')
                .to_ascii_lowercase()
        })
        .filter(|part| !part.is_empty())
        .collect()
}

fn looks_like_git_sha(token: &str) -> bool {
    (7..=40).contains(&token.len()) && token.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn looks_like_codex_session_token(token: &str) -> bool {
    if token.len() < 8 || token.len() > 36 || !token.contains('-') {
        return false;
    }

    let mut hex_chars = 0usize;
    for ch in token.chars() {
        if ch == '-' {
            continue;
        }
        if !ch.is_ascii_hexdigit() {
            return false;
        }
        hex_chars += 1;
    }

    if hex_chars < 8 {
        return false;
    }

    if token.len() == 36 {
        let segments: Vec<_> = token.split('-').collect();
        if segments.len() != 5 {
            return false;
        }
        let expected = [8usize, 4, 4, 4, 12];
        return segments
            .iter()
            .zip(expected)
            .all(|(segment, expected_len)| segment.len() == expected_len);
    }

    true
}

fn extract_codex_session_hint(query: &str) -> Option<String> {
    recover_query_tokens(query)
        .into_iter()
        .find(|token| !looks_like_git_sha(token) && looks_like_codex_session_token(token))
}

fn extract_codex_session_reference_request(
    query_text: &str,
    normalized_query: &str,
) -> Option<CodexSessionReferenceRequest> {
    if starts_with_codex_session_lookup_only_phrase(normalized_query) {
        return None;
    }
    let session_hint = extract_codex_session_hint(normalized_query)?;
    let user_request = extract_codex_session_reference_user_request(query_text, &session_hint)?;
    let count = extract_codex_session_reference_count(query_text, &session_hint);
    Some(CodexSessionReferenceRequest {
        session_hint,
        count,
        user_request,
    })
}

fn starts_with_codex_session_lookup_only_phrase(query: &str) -> bool {
    [
        "open ",
        "resume ",
        "continue ",
        "connect ",
        "find ",
        "copy ",
        "show ",
    ]
    .iter()
    .any(|prefix| query.starts_with(prefix))
}

fn extract_codex_session_reference_user_request(
    query_text: &str,
    session_hint: &str,
) -> Option<String> {
    let query_lower = query_text.to_ascii_lowercase();
    let hint_lower = session_hint.to_ascii_lowercase();
    let start = query_lower.find(&hint_lower)?;
    let after_hint = query_text.get(start + session_hint.len()..)?.trim_start();
    let remainder = strip_codex_session_window_prefix(after_hint)
        .trim_start_matches(|ch: char| ch.is_whitespace() || matches!(ch, ',' | ';' | ':' | '-'))
        .trim();
    if remainder.is_empty() {
        None
    } else {
        Some(remainder.to_string())
    }
}

fn extract_codex_session_reference_count(query_text: &str, session_hint: &str) -> usize {
    let query_lower = query_text.to_ascii_lowercase();
    let hint_lower = session_hint.to_ascii_lowercase();
    let after_hint = query_lower
        .find(&hint_lower)
        .and_then(|start| query_text.get(start + session_hint.len()..))
        .unwrap_or(query_text);
    let captures = codex_session_window_regex().captures(after_hint);
    captures
        .and_then(|caps| caps.get(1))
        .and_then(|value| value.as_str().parse::<usize>().ok())
        .map(|value| value.clamp(1, 50))
        .unwrap_or(12)
}

fn strip_codex_session_window_prefix(value: &str) -> &str {
    if let Some(matched) = codex_session_window_regex().find(value) {
        &value[matched.end()..]
    } else {
        value
    }
}

fn codex_session_window_regex() -> &'static Regex {
    static WINDOW_RE: OnceLock<Regex> = OnceLock::new();
    WINDOW_RE.get_or_init(|| {
        Regex::new(r"(?i)^\s*(?:last|past)\s+(\d{1,3})\s+(?:messages?|exchanges?|turns?)\b")
            .expect("valid session window regex")
    })
}

fn resolve_builtin_codex_session_reference(
    request: &CodexSessionReferenceRequest,
) -> Result<CodexResolvedReference> {
    let row = read_codex_threads_by_session_hint(&request.session_hint, 1)?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("No Codex session found for {}", request.session_hint))?;
    let excerpt = read_last_context(
        &row.id,
        Provider::Codex,
        request.count,
        &PathBuf::from(&row.cwd),
    )?;
    Ok(CodexResolvedReference {
        name: "codex-session".to_string(),
        source: "session".to_string(),
        matched: row.id.clone(),
        command: None,
        output: render_codex_session_reference(&row, request.count, &excerpt),
    })
}

fn render_codex_session_reference(row: &CodexRecoverRow, count: usize, excerpt: &str) -> String {
    let mut lines = vec![
        format!("- Codex session: {}", row.id),
        format!("- Repo cwd: {}", row.cwd),
        format!("- Updated: {}", format_unix_ts(row.updated_at)),
        format!("- Included excerpt: last {} exchanges", count),
    ];
    if let Some(title) = row.title.as_deref() {
        lines.push(format!("- Title: {}", truncate_recover_text(title)));
    }
    if let Some(first) = row.first_user_message.as_deref() {
        lines.push(format!(
            "- First user message: {}",
            truncate_recover_text(first)
        ));
    }
    lines.push("Recent transcript excerpt:".to_string());
    lines.extend(excerpt.lines().map(str::to_string));
    compact_codex_context_block(&lines.join("\n"), 32, 3200)
}

fn codex_sqlite_home() -> Result<PathBuf> {
    if let Some(path) = env::var_os("CODEX_SQLITE_HOME") {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = env::var_os("CODEX_HOME") {
        return Ok(PathBuf::from(path));
    }
    let home = dirs::home_dir().context("failed to resolve home directory")?;
    Ok(home.join(".codex"))
}

fn latest_codex_state_db() -> Result<PathBuf> {
    let sqlite_home = codex_sqlite_home()?;
    let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = fs::read_dir(&sqlite_home)
        .with_context(|| format!("failed to read {}", sqlite_home.display()))?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let path = entry.path();
            let file_name = path.file_name()?.to_str()?;
            if !file_name.starts_with("state_") || !file_name.ends_with(".sqlite") {
                return None;
            }
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, path))
        })
        .collect();

    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    candidates
        .into_iter()
        .map(|(_, path)| path)
        .next()
        .context("no Codex state_*.sqlite database found")
}

fn codex_query_cache_disabled() -> bool {
    matches!(
        env::var(CODEX_QUERY_CACHE_ENV_DISABLE)
            .ok()
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

fn codex_query_cache_root() -> Result<PathBuf> {
    Ok(config::ensure_global_state_dir()?
        .join("codex")
        .join("query-cache"))
}

fn codex_query_cache_entry_count() -> usize {
    let Ok(root) = codex_query_cache_root() else {
        return 0;
    };
    fs::read_dir(root)
        .ok()
        .into_iter()
        .flat_map(|entries| entries.flatten())
        .filter(|entry| {
            entry.path().extension().and_then(|value| value.to_str()) == Some("msgpack")
        })
        .count()
}

fn codex_query_cache_store() -> &'static Mutex<HashMap<PathBuf, CodexQueryCacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, CodexQueryCacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0)
}

fn codex_state_db_stamp(path: &Path) -> Result<CodexStateDbStamp> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("failed to stat Codex state db {}", path.display()))?;
    let modified = metadata
        .modified()
        .unwrap_or(SystemTime::UNIX_EPOCH)
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    Ok(CodexStateDbStamp {
        path: path.display().to_string(),
        len: metadata.len(),
        modified_unix_secs: modified,
    })
}

fn codex_query_cache_path(
    stamp: &CodexStateDbStamp,
    scope: &str,
    key_material: &str,
) -> Result<PathBuf> {
    let hash_input = format!("{}\n{}\n{}", stamp.path, scope, key_material);
    let hash = blake3::hash(hash_input.as_bytes()).to_hex();
    Ok(codex_query_cache_root()?.join(format!("{hash}.msgpack")))
}

fn read_codex_query_cache(path: &Path, stamp: &CodexStateDbStamp) -> Option<Vec<CodexRecoverRow>> {
    if codex_query_cache_disabled() {
        return None;
    }

    if let Ok(cache) = codex_query_cache_store().lock()
        && let Some(entry) = cache.get(path)
        && entry.version == CODEX_QUERY_CACHE_VERSION
        && entry.stamp == *stamp
    {
        return Some(entry.rows.clone());
    }

    let bytes = fs::read(path).ok()?;
    let entry = rmp_serde::from_slice::<CodexQueryCacheEntry>(&bytes).ok()?;
    if entry.version != CODEX_QUERY_CACHE_VERSION || entry.stamp != *stamp {
        return None;
    }

    if let Ok(mut cache) = codex_query_cache_store().lock() {
        cache.insert(path.to_path_buf(), entry.clone());
    }
    Some(entry.rows)
}

fn write_codex_query_cache(path: &Path, entry: &CodexQueryCacheEntry) -> Result<()> {
    if codex_query_cache_disabled() {
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create Codex query cache dir {}",
                parent.display()
            )
        })?;
    }

    let bytes = rmp_serde::to_vec(entry).context("failed to encode Codex query cache")?;
    let tmp_path = path.with_extension(format!(
        "msgpack.tmp.{}.{}",
        std::process::id(),
        unix_now_secs()
    ));
    fs::write(&tmp_path, bytes)
        .with_context(|| format!("failed to write Codex query cache {}", tmp_path.display()))?;
    if let Err(err) = fs::rename(&tmp_path, path) {
        if path.exists() {
            let _ = fs::remove_file(path);
            fs::rename(&tmp_path, path).with_context(|| {
                format!("failed to finalize Codex query cache {}", path.display())
            })?;
        } else {
            return Err(err).with_context(|| {
                format!("failed to finalize Codex query cache {}", path.display())
            });
        }
    }

    if let Ok(mut cache) = codex_query_cache_store().lock() {
        cache.insert(path.to_path_buf(), entry.clone());
    }

    Ok(())
}

fn with_codex_query_cache<F>(
    db_path: &Path,
    scope: &str,
    key_material: &str,
    query: F,
) -> Result<Vec<CodexRecoverRow>>
where
    F: FnOnce(&Connection) -> Result<Vec<CodexRecoverRow>>,
{
    let stamp = codex_state_db_stamp(db_path)?;
    let cache_path = codex_query_cache_path(&stamp, scope, key_material)?;
    if let Some(rows) = read_codex_query_cache(&cache_path, &stamp) {
        return Ok(rows);
    }

    let conn = Connection::open(db_path)
        .with_context(|| format!("failed to open {}", db_path.display()))?;
    let rows = query(&conn)?;
    let entry = CodexQueryCacheEntry {
        version: CODEX_QUERY_CACHE_VERSION,
        stamp,
        rows: rows.clone(),
    };
    if let Err(err) = write_codex_query_cache(&cache_path, &entry) {
        debug!(path = %cache_path.display(), error = %err, "failed to write codex query cache");
    }
    Ok(rows)
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn read_recent_codex_threads(
    target_path: &Path,
    exact_cwd: bool,
    limit: usize,
    query: Option<&str>,
) -> Result<Vec<CodexRecoverRow>> {
    match codexd::query_recent(target_path, exact_cwd, limit, query) {
        Ok(rows) => Ok(rows),
        Err(err) => {
            debug!(error = %err, "codexd recent query failed; falling back to local query");
            read_recent_codex_threads_local(target_path, exact_cwd, limit, query)
        }
    }
}

pub(crate) fn read_recent_codex_threads_local(
    target_path: &Path,
    exact_cwd: bool,
    limit: usize,
    query: Option<&str>,
) -> Result<Vec<CodexRecoverRow>> {
    let db_path = latest_codex_state_db()?;

    let target = target_path.to_string_lossy().to_string();
    let like_target = format!("{}/%", escape_like(&target));
    let fetch_limit = (limit.max(3) * 12).min(120);
    let cache_key = format!("target={target}\nexact={exact_cwd}\nfetch_limit={fetch_limit}");

    let sql_exact = r#"
select
  id,
  updated_at,
  cwd,
  title,
  first_user_message,
  git_branch
from threads
where archived = 0
  and cwd = ?1
order by updated_at desc
limit ?2
"#;

    let sql_tree = r#"
select
  id,
  updated_at,
  cwd,
  title,
  first_user_message,
  git_branch
from threads
where archived = 0
  and (cwd = ?1 or cwd like ?2 escape '\')
order by updated_at desc
limit ?3
"#;

    let mut rows = with_codex_query_cache(&db_path, "recent", &cache_key, |conn| {
        if exact_cwd {
            let mut stmt = conn
                .prepare(sql_exact)
                .context("failed to prepare exact recover query")?;
            let iter = stmt.query_map(params![target, fetch_limit as i64], |row| {
                Ok(CodexRecoverRow {
                    id: row.get(0)?,
                    updated_at: row.get(1)?,
                    cwd: row.get(2)?,
                    title: row.get(3)?,
                    first_user_message: row.get(4)?,
                    git_branch: row.get(5)?,
                })
            })?;
            Ok(iter.collect::<rusqlite::Result<Vec<_>>>()?)
        } else {
            let mut stmt = conn
                .prepare(sql_tree)
                .context("failed to prepare subtree recover query")?;
            let iter = stmt.query_map(params![target, like_target, fetch_limit as i64], |row| {
                Ok(CodexRecoverRow {
                    id: row.get(0)?,
                    updated_at: row.get(1)?,
                    cwd: row.get(2)?,
                    title: row.get(3)?,
                    first_user_message: row.get(4)?,
                    git_branch: row.get(5)?,
                })
            })?;
            Ok(iter.collect::<rusqlite::Result<Vec<_>>>()?)
        }
    })?;

    rank_recover_rows(&mut rows, query);
    rows.truncate(limit.max(1));
    Ok(rows)
}

fn read_codex_threads_by_session_hint(
    session_hint: &str,
    limit: usize,
) -> Result<Vec<CodexRecoverRow>> {
    match codexd::query_session_hint(session_hint, limit) {
        Ok(rows) => Ok(rows),
        Err(err) => {
            debug!(
                error = %err,
                "codexd session hint query failed; falling back to local query"
            );
            read_codex_threads_by_session_hint_local(session_hint, limit)
        }
    }
}

pub(crate) fn read_codex_threads_by_session_hint_local(
    session_hint: &str,
    limit: usize,
) -> Result<Vec<CodexRecoverRow>> {
    let db_path = latest_codex_state_db()?;
    let normalized_hint = session_hint.trim().to_ascii_lowercase();
    if normalized_hint.is_empty() {
        return Ok(vec![]);
    }
    let cache_key = format!("hint={normalized_hint}\nlimit={}", limit.max(1));

    let sql = r#"
select
  id,
  updated_at,
  cwd,
  title,
  first_user_message,
  git_branch
from threads
where archived = 0
  and (lower(id) = ?1 or lower(id) like ?2 escape '\')
order by
  case when lower(id) = ?1 then 0 else 1 end,
  updated_at desc
limit ?3
"#;

    let prefix_like = format!("{}%", escape_like(&normalized_hint));
    with_codex_query_cache(&db_path, "session-hint", &cache_key, |conn| {
        let mut stmt = conn
            .prepare(sql)
            .context("failed to prepare explicit session recover query")?;
        let iter = stmt.query_map(
            params![normalized_hint, prefix_like, limit.max(1) as i64],
            |row| {
                Ok(CodexRecoverRow {
                    id: row.get(0)?,
                    updated_at: row.get(1)?,
                    cwd: row.get(2)?,
                    title: row.get(3)?,
                    first_user_message: row.get(4)?,
                    git_branch: row.get(5)?,
                })
            },
        )?;
        Ok(iter.collect::<rusqlite::Result<Vec<_>>>()?)
    })
}

fn search_codex_threads_for_find(
    target_path: Option<&Path>,
    exact_cwd: bool,
    query: &str,
    limit: usize,
) -> Result<Vec<CodexRecoverRow>> {
    match codexd::query_find(target_path, exact_cwd, query, limit) {
        Ok(rows) => Ok(rows),
        Err(err) => {
            debug!(error = %err, "codexd find query failed; falling back to local query");
            search_codex_threads_for_find_local(target_path, exact_cwd, query, limit)
        }
    }
}

pub(crate) fn search_codex_threads_for_find_local(
    target_path: Option<&Path>,
    exact_cwd: bool,
    query: &str,
    limit: usize,
) -> Result<Vec<CodexRecoverRow>> {
    let normalized_query = query.trim().to_lowercase();
    if normalized_query.is_empty() {
        return Ok(vec![]);
    }

    if let Some(session_hint) = extract_codex_session_hint(&normalized_query) {
        let rows = read_codex_threads_by_session_hint_local(&session_hint, limit.max(1))?;
        if !rows.is_empty() {
            return Ok(rows);
        }
    }

    let db_path = latest_codex_state_db()?;

    let mut sql = String::from(
        r#"
select
  id,
  updated_at,
  cwd,
  title,
  first_user_message,
  git_branch
from threads
where archived = 0
"#,
    );
    let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if let Some(target_path) = target_path {
        let target = target_path.to_string_lossy().to_string();
        if exact_cwd {
            sql.push_str("  and cwd = ?\n");
            params_vec.push(Box::new(target));
        } else {
            sql.push_str("  and (cwd = ? or cwd like ? escape '\\')\n");
            params_vec.push(Box::new(target.clone()));
            params_vec.push(Box::new(format!("{}/%", escape_like(&target))));
        }
    }

    let search_terms = codex_find_search_terms(&normalized_query);
    let mut clauses = Vec::new();
    for term in search_terms {
        let pattern = format!("%{}%", escape_like(&term));
        for column in ["id", "first_user_message", "title", "git_branch", "cwd"] {
            clauses.push(format!("lower(coalesce({column}, '')) like ? escape '\\'"));
            params_vec.push(Box::new(pattern.clone()));
        }
    }
    if !clauses.is_empty() {
        sql.push_str("  and (");
        sql.push_str(&clauses.join(" or "));
        sql.push_str(")\n");
    }

    sql.push_str("order by updated_at desc\nlimit ?\n");
    let fetch_limit = (limit.max(5) * 20).min(200);
    params_vec.push(Box::new(fetch_limit as i64));
    let scope_target = target_path
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    let cache_key = format!(
        "query={normalized_query}\nexact={exact_cwd}\ntarget={scope_target}\nfetch_limit={fetch_limit}"
    );
    let mut rows = with_codex_query_cache(&db_path, "find", &cache_key, |conn| {
        let params_refs: Vec<&dyn rusqlite::ToSql> =
            params_vec.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn
            .prepare(&sql)
            .context("failed to prepare Codex find query")?;
        let iter = stmt.query_map(params_refs.as_slice(), |row| {
            Ok(CodexRecoverRow {
                id: row.get(0)?,
                updated_at: row.get(1)?,
                cwd: row.get(2)?,
                title: row.get(3)?,
                first_user_message: row.get(4)?,
                git_branch: row.get(5)?,
            })
        })?;
        Ok(iter.collect::<rusqlite::Result<Vec<_>>>()?)
    })?;
    rank_recover_rows(&mut rows, Some(&normalized_query));
    rows.truncate(limit.max(1));
    Ok(rows)
}

fn codex_find_search_terms(query: &str) -> Vec<String> {
    let normalized = query.trim().to_lowercase();
    if normalized.is_empty() {
        return vec![];
    }

    let mut terms = vec![normalized.clone()];
    let mut seen = BTreeSet::from([normalized]);
    for token in tokenize_recover_query(query) {
        if token.len() <= 2 {
            continue;
        }
        if seen.insert(token.clone()) {
            terms.push(token);
        }
    }
    terms
}

fn tokenize_recover_query(query: &str) -> Vec<String> {
    query
        .split(|ch: char| {
            !ch.is_ascii_alphanumeric() && ch != '/' && ch != '-' && ch != '_' && ch != '#'
        })
        .filter(|part| !part.is_empty())
        .map(|part| part.to_lowercase())
        .filter(|part| part.len() > 1)
        .collect()
}

fn rank_recover_rows(rows: &mut Vec<CodexRecoverRow>, query: Option<&str>) {
    let normalized_query = query.map(|q| q.to_lowercase()).unwrap_or_default();
    let tokens = tokenize_recover_query(&normalized_query);

    rows.sort_by(|a, b| {
        let score_a = recover_row_score(a, &normalized_query, &tokens);
        let score_b = recover_row_score(b, &normalized_query, &tokens);
        score_b
            .cmp(&score_a)
            .then_with(|| b.updated_at.cmp(&a.updated_at))
            .then_with(|| a.cwd.cmp(&b.cwd))
    });

    if !tokens.is_empty()
        && rows
            .iter()
            .all(|row| recover_row_score(row, &normalized_query, &tokens) == 0)
    {
        rows.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    }
}

fn recover_row_score(row: &CodexRecoverRow, normalized_query: &str, tokens: &[String]) -> i64 {
    if tokens.is_empty() && normalized_query.is_empty() {
        return 0;
    }

    let id = row.id.to_lowercase();
    let cwd = row.cwd.to_lowercase();
    let branch = row.git_branch.clone().unwrap_or_default().to_lowercase();
    let title = row.title.clone().unwrap_or_default().to_lowercase();
    let first = row
        .first_user_message
        .clone()
        .unwrap_or_default()
        .to_lowercase();

    let mut score = 0i64;

    if !normalized_query.is_empty() {
        if id == normalized_query {
            score += 600;
        } else if id.starts_with(normalized_query) {
            score += 500;
        } else if id.contains(normalized_query) {
            score += 300;
        }
        if first.contains(normalized_query) {
            score += 120;
        }
        if title.contains(normalized_query) {
            score += 90;
        }
        if branch.contains(normalized_query) {
            score += 70;
        }
        if cwd.contains(normalized_query) {
            score += 60;
        }
    }

    for token in tokens {
        if id.starts_with(token) {
            score += 90;
        } else if id.contains(token) {
            score += 60;
        }
        if first.contains(token) {
            score += 18;
        }
        if title.contains(token) {
            score += 14;
        }
        if branch.contains(token) {
            score += 12;
        }
        if cwd.contains(token) {
            score += 8;
        }
    }

    score
}

fn build_recover_output(
    target_path: &Path,
    exact_cwd: bool,
    query: Option<String>,
    rows: Vec<CodexRecoverRow>,
) -> CodexRecoverOutput {
    let candidates: Vec<CodexRecoverCandidate> = rows
        .into_iter()
        .map(|row| CodexRecoverCandidate {
            id: row.id,
            updated_at: format_unix_ts(row.updated_at),
            updated_at_unix: row.updated_at,
            cwd: row.cwd,
            git_branch: row.git_branch.filter(|value| !value.trim().is_empty()),
            title: row.title.filter(|value| !value.trim().is_empty()),
            first_user_message: row
                .first_user_message
                .filter(|value| !value.trim().is_empty()),
        })
        .collect();

    let recommended_route = infer_recover_route(
        target_path,
        query.as_deref().unwrap_or_default(),
        &candidates,
    );
    let summary = build_recover_summary(target_path, exact_cwd, &recommended_route, &candidates);

    CodexRecoverOutput {
        target_path: target_path.to_string_lossy().to_string(),
        exact_cwd,
        query,
        recommended_route,
        summary,
        candidates,
    }
}

fn infer_recover_route(
    target_path: &Path,
    _query: &str,
    candidates: &[CodexRecoverCandidate],
) -> String {
    if let Some(candidate) = candidates.first() {
        let candidate_cwd = Path::new(&candidate.cwd);
        if candidate_cwd != target_path {
            return format!(
                "cd {} && f ai codex resume {}",
                shell_escape_path(candidate_cwd),
                candidate.id
            );
        }
        return format!("f ai codex resume {}", candidate.id);
    }

    "f ai codex new".to_string()
}

fn shell_escape_path(path: &Path) -> String {
    let display = path.to_string_lossy();
    if display
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || "/-._~".contains(ch))
    {
        return display.to_string();
    }

    format!("'{}'", display.replace('\'', "'\"'\"'"))
}

fn build_recover_summary(
    target_path: &Path,
    exact_cwd: bool,
    recommended_route: &str,
    candidates: &[CodexRecoverCandidate],
) -> String {
    let mut lines = Vec::new();
    let mode = if exact_cwd { "exact cwd" } else { "repo-tree" };
    lines.push(format!(
        "Recovered recent Codex context for {} ({mode} lookup).",
        target_path.display()
    ));

    if candidates.is_empty() {
        lines.push("No recent matching Codex sessions found.".to_string());
        lines.push(format!("Recommended route: {}", recommended_route));
        return lines.join("\n");
    }

    for candidate in candidates.iter().take(3) {
        let message = candidate
            .first_user_message
            .as_deref()
            .or(candidate.title.as_deref())
            .map(truncate_recover_text)
            .unwrap_or_else(|| "(no stored prompt text)".to_string());
        let branch = candidate
            .git_branch
            .as_deref()
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_string());
        lines.push(format!(
            "- {} | {} | {} | {} | {}",
            truncate_recover_id(&candidate.id),
            candidate.updated_at,
            branch,
            candidate.cwd,
            message
        ));
    }

    lines.push(format!("Recommended route: {}", recommended_route));
    lines.join("\n")
}

fn truncate_recover_id(value: &str) -> String {
    value.chars().take(8).collect()
}

fn truncate_recover_text(value: &str) -> String {
    let clean = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if clean.chars().count() <= 110 {
        return clean;
    }
    let truncated: String = clean.chars().take(107).collect();
    format!("{truncated}...")
}

fn format_unix_ts(ts: i64) -> String {
    DateTime::<Utc>::from_timestamp(ts, 0)
        .map(|value| value.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| ts.to_string())
}

fn print_recover_output(output: &CodexRecoverOutput) {
    println!("Target path: {}", output.target_path);
    println!(
        "Search mode: {}",
        if output.exact_cwd {
            "exact cwd"
        } else {
            "repo-tree"
        }
    );
    if let Some(query) = output.query.as_deref() {
        println!("Query: {}", query);
    }
    println!("Recommended route: {}", output.recommended_route);
    println!();
    if output.candidates.is_empty() {
        println!("No recent matching Codex sessions found.");
        return;
    }
    println!("Recent sessions:");
    for candidate in &output.candidates {
        println!(
            "- {} | {} | {}",
            truncate_recover_id(&candidate.id),
            candidate.updated_at,
            candidate.cwd
        );
        if let Some(branch) = candidate.git_branch.as_deref() {
            println!("  branch: {}", branch);
        }
        if let Some(first) = candidate.first_user_message.as_deref() {
            println!("  first: {}", truncate_recover_text(first));
        } else if let Some(title) = candidate.title.as_deref() {
            println!("  title: {}", truncate_recover_text(title));
        }
    }
    println!();
    println!("Summary:");
    println!("{}", output.summary);
}

fn open_codex_session(
    path: Option<String>,
    query: Vec<String>,
    exact_cwd: bool,
    provider: Provider,
) -> Result<()> {
    if provider != Provider::Codex {
        bail!("open is only supported for Codex sessions; use `f codex open ...`");
    }
    ensure_provider_tty(Provider::Codex, "open")?;

    let plan = build_codex_open_plan(path, query, exact_cwd)?;
    record_codex_open_plan(&plan, "open");
    execute_codex_open_plan(&plan)
}

fn connect_codex_session(
    path: Option<String>,
    query: Vec<String>,
    exact_cwd: bool,
    json_output: bool,
    provider: Provider,
) -> Result<()> {
    if provider != Provider::Codex {
        bail!("connect is only supported for Codex sessions; use `f codex connect ...`");
    }

    let target_path = resolve_codex_connect_target_path(path)?;
    let query_text = query.join(" ").trim().to_string();
    let normalized_query = query_text.to_ascii_lowercase();
    let resolved = if query_text.is_empty() {
        read_recent_codex_threads(&target_path, exact_cwd, 1, None)?
            .into_iter()
            .next()
            .map(|row| (row, "latest recent session".to_string()))
    } else {
        resolve_codex_session_lookup(&target_path, exact_cwd, &query_text, &normalized_query)?
    };

    let Some((row, reason)) = resolved else {
        if query_text.is_empty() {
            bail!("No Codex sessions found for {}", target_path.display());
        }
        bail!(
            "{}",
            build_codex_open_no_match_message(&target_path, exact_cwd, &query_text)?
        );
    };

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "id": row.id,
                "cwd": row.cwd,
                "updatedAtUnix": row.updated_at,
                "title": row.title,
                "firstUserMessage": row.first_user_message,
                "gitBranch": row.git_branch,
                "reason": reason,
                "targetPath": target_path.display().to_string(),
                "exactCwd": exact_cwd,
                "query": if query_text.is_empty() { None::<String> } else { Some(query_text) },
            }))
            .context("failed to encode codex connect JSON")?
        );
        return Ok(());
    }

    ensure_provider_tty(Provider::Codex, "connect")?;
    let launch_path = PathBuf::from(&row.cwd);
    println!(
        "Resuming session {} from {}...",
        &row.id[..8.min(row.id.len())],
        launch_path.display()
    );
    if launch_session_for_target(&row.id, Provider::Codex, None, Some(&launch_path), None)? {
        return Ok(());
    }

    bail!(
        "failed to connect to codex session {} for {}",
        row.id,
        launch_path.display()
    )
}

fn resolve_codex_input(
    path: Option<String>,
    query: Vec<String>,
    exact_cwd: bool,
    json_output: bool,
    provider: Provider,
) -> Result<()> {
    if provider != Provider::Codex {
        bail!("resolve is only supported for Codex sessions; use `f codex resolve ...`");
    }

    let (query, json_output) = normalize_codex_resolve_args(query, json_output);
    let plan = build_codex_open_plan(path, query, exact_cwd)?;
    record_codex_open_plan(&plan, "resolve");
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&plan).context("failed to encode Codex resolve JSON")?
        );
        return Ok(());
    }

    print_codex_open_plan(&plan);
    Ok(())
}

const DEFAULT_GLOBAL_CODEX_WRAPPER_BIN: &str = "~/code/flow/scripts/codex-flow-wrapper";
const DEFAULT_GLOBAL_CODEX_HOME_SESSION_PATH: &str = "~/repos/openai/codex";
const DEFAULT_GLOBAL_CODEX_SKILL_SOURCE_NAME: &str = "vercel-labs-skills";
const DEFAULT_GLOBAL_CODEX_SKILL_SOURCE_PATH: &str = "~/repos/vercel-labs/skills";
const DEFAULT_GLOBAL_CODEX_PROMPT_BUDGET: usize = 1200;
const DEFAULT_GLOBAL_CODEX_MAX_REFERENCES: usize = 2;
const CODEX_SKILL_EVAL_LAUNCHD_LABEL: &str = "dev.nikiv.flow-codex-skill-eval";

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodexSkillEvalScheduleStatus {
    Unsupported,
    NotInstalled,
    PlistOnly,
    Loaded,
}

impl CodexSkillEvalScheduleStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Unsupported => "unsupported",
            Self::NotInstalled => "not-installed",
            Self::PlistOnly => "plist-only",
            Self::Loaded => "loaded",
        }
    }

    fn ready(self) -> bool {
        matches!(self, Self::Loaded)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexDoctorSnapshot {
    target: String,
    codex_bin: String,
    codexd: String,
    codexd_socket: String,
    memory_state: String,
    memory_root: String,
    memory_db_path: String,
    memory_events_indexed: usize,
    memory_facts_indexed: usize,
    runtime_transport: String,
    runtime_skills: String,
    auto_resolve_references: bool,
    home_session_path: String,
    prompt_context_budget_chars: usize,
    max_resolved_references: usize,
    reference_resolvers: usize,
    query_cache: String,
    query_cache_entries_on_disk: usize,
    skill_eval_events_on_disk: usize,
    skill_eval_outcomes_on_disk: usize,
    skill_scorecard_samples: usize,
    skill_scorecard_entries: usize,
    skill_scorecard_top: Option<String>,
    external_skill_candidates: usize,
    runtime_state_files: usize,
    runtime_state_files_for_target: usize,
    skill_eval_schedule: String,
    learning_state: String,
    runtime_ready: bool,
    schedule_ready: bool,
    learning_ready: bool,
    warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexSkillsDashboardResponse {
    pub doctor: CodexDoctorSnapshot,
    pub skills: codex_runtime::CodexSkillsDashboardSnapshot,
}

fn codex_skill_eval_launchd_plist_path() -> PathBuf {
    config::expand_path(&format!(
        "~/Library/LaunchAgents/{}.plist",
        CODEX_SKILL_EVAL_LAUNCHD_LABEL
    ))
}

fn codex_skill_eval_launchd_status() -> CodexSkillEvalScheduleStatus {
    #[cfg(not(target_os = "macos"))]
    {
        CodexSkillEvalScheduleStatus::Unsupported
    }

    #[cfg(target_os = "macos")]
    {
        let plist = codex_skill_eval_launchd_plist_path();
        if !plist.exists() {
            return CodexSkillEvalScheduleStatus::NotInstalled;
        }

        let uid = unsafe { libc::geteuid() };
        let domain = format!("gui/{uid}/{CODEX_SKILL_EVAL_LAUNCHD_LABEL}");
        match Command::new("launchctl").arg("print").arg(&domain).output() {
            Ok(output) if output.status.success() => CodexSkillEvalScheduleStatus::Loaded,
            _ => CodexSkillEvalScheduleStatus::PlistOnly,
        }
    }
}

fn collect_codex_doctor_snapshot(target_path: &Path) -> Result<CodexDoctorSnapshot> {
    let codex_cfg = load_codex_config_for_path(target_path);
    let runtime_transport_enabled = codex_runtime_transport_enabled(target_path);
    let runtime_states = codex_runtime::load_runtime_states()?;
    let active_runtime_states = runtime_states
        .iter()
        .filter(|state| state.target_path == target_path.display().to_string())
        .count();
    let codex_bin = configured_codex_bin_for_workdir(target_path);
    let codexd_socket = codexd::socket_path()?;
    let codexd_running = codexd::is_running();
    let memory_stats = codex_memory::stats().ok();
    let skill_eval_events = codex_skill_eval::event_count();
    let skill_eval_outcomes = codex_skill_eval::outcome_count();
    let schedule_status = codex_skill_eval_launchd_status();
    let scorecard = codex_skill_eval::load_scorecard(target_path)?;
    let (skill_scorecard_samples, skill_scorecard_entries, skill_scorecard_top) = scorecard
        .as_ref()
        .map(|value| {
            (
                value.samples,
                value.skills.len(),
                value
                    .skills
                    .first()
                    .map(|top| format!("{} ({:.2})", top.name, top.score)),
            )
        })
        .unwrap_or((0, 0, None));
    let discovered_skills = codex_runtime::discover_external_skills(target_path, &codex_cfg)?;

    let runtime_skills_state =
        if codex_cfg.runtime_skills.unwrap_or(false) && runtime_transport_enabled {
            "enabled"
        } else if codex_cfg.runtime_skills.unwrap_or(false) {
            "configured-but-inactive"
        } else {
            "disabled"
        };
    let runtime_ready = runtime_transport_enabled
        && runtime_skills_state == "enabled"
        && codex_cfg.auto_resolve_references.unwrap_or(true);
    let learning_state = if skill_scorecard_entries > 0 && skill_eval_outcomes > 0 {
        "grounded"
    } else if skill_scorecard_entries > 0 {
        "affinity-only"
    } else if skill_eval_events > 0 || skill_eval_outcomes > 0 {
        "warming-up"
    } else {
        "dormant"
    };
    let learning_ready =
        skill_eval_events > 0 && skill_eval_outcomes > 0 && skill_scorecard_entries > 0;

    let mut warnings = Vec::new();
    if !runtime_transport_enabled {
        warnings.push(
            "wrapper transport is disabled; Flow is launching plain `codex`, so runtime skills never activate"
                .to_string(),
        );
    }
    if runtime_skills_state == "disabled" {
        warnings.push("runtime skills are disabled in config".to_string());
    }
    if !schedule_status.ready() {
        warnings.push(
            "scheduled skill-eval refresh is not loaded; scorecards will only update when you run cron manually"
                .to_string(),
        );
    }
    if skill_eval_events == 0 {
        warnings.push("no Codex route events recorded yet".to_string());
    }
    if skill_eval_outcomes == 0 {
        warnings.push(
            "no grounded outcome events recorded yet; scorecards are still affinity-only"
                .to_string(),
        );
    }
    if memory_stats.is_none() {
        warnings.push(
            "codex memory mirror is unavailable; recent memory and durable sync will stay local-only"
                .to_string(),
        );
    }

    let (memory_state, memory_root, memory_db_path, memory_events_indexed, memory_facts_indexed) =
        if let Some(stats) = memory_stats {
            (
                "ready".to_string(),
                stats.root_dir,
                stats.db_path,
                stats.total_events,
                stats.total_facts,
            )
        } else {
            (
                "unavailable".to_string(),
                codex_memory::root_dir().display().to_string(),
                codex_memory::db_path().display().to_string(),
                0,
                0,
            )
        };

    Ok(CodexDoctorSnapshot {
        target: target_path.display().to_string(),
        codex_bin,
        codexd: if codexd_running {
            "running".to_string()
        } else {
            "stopped".to_string()
        },
        codexd_socket: codexd_socket.display().to_string(),
        memory_state,
        memory_root,
        memory_db_path,
        memory_events_indexed,
        memory_facts_indexed,
        runtime_transport: if runtime_transport_enabled {
            "enabled".to_string()
        } else {
            "disabled".to_string()
        },
        runtime_skills: runtime_skills_state.to_string(),
        auto_resolve_references: codex_cfg.auto_resolve_references.unwrap_or(true),
        home_session_path: codex_cfg
            .home_session_path
            .as_deref()
            .map(config::expand_path)
            .unwrap_or_else(default_codex_connect_path)
            .display()
            .to_string(),
        prompt_context_budget_chars: effective_prompt_context_budget_chars(&codex_cfg, false),
        max_resolved_references: effective_max_resolved_references(&codex_cfg),
        reference_resolvers: codex_cfg.reference_resolvers.len(),
        query_cache: if codex_query_cache_disabled() {
            "disabled".to_string()
        } else {
            "enabled".to_string()
        },
        query_cache_entries_on_disk: codex_query_cache_entry_count(),
        skill_eval_events_on_disk: skill_eval_events,
        skill_eval_outcomes_on_disk: skill_eval_outcomes,
        skill_scorecard_samples,
        skill_scorecard_entries,
        skill_scorecard_top,
        external_skill_candidates: discovered_skills.len(),
        runtime_state_files: runtime_states.len(),
        runtime_state_files_for_target: active_runtime_states,
        skill_eval_schedule: schedule_status.as_str().to_string(),
        learning_state: learning_state.to_string(),
        runtime_ready,
        schedule_ready: schedule_status.ready(),
        learning_ready,
        warnings,
    })
}

pub fn codex_skills_dashboard_snapshot(
    target_path: &Path,
    recent_limit: usize,
) -> Result<CodexSkillsDashboardResponse> {
    let codex_cfg = load_codex_config_for_path(target_path);
    Ok(CodexSkillsDashboardResponse {
        doctor: collect_codex_doctor_snapshot(target_path)?,
        skills: codex_runtime::dashboard_snapshot(target_path, &codex_cfg, recent_limit)?,
    })
}

fn print_codex_doctor(snapshot: &CodexDoctorSnapshot) {
    println!("# codex doctor");
    println!("target: {}", snapshot.target);
    println!("codex_bin: {}", snapshot.codex_bin);
    println!("codexd: {}", snapshot.codexd);
    println!("codexd_socket: {}", snapshot.codexd_socket);
    println!("memory_state: {}", snapshot.memory_state);
    println!("memory_root: {}", snapshot.memory_root);
    println!("memory_db_path: {}", snapshot.memory_db_path);
    println!("memory_events_indexed: {}", snapshot.memory_events_indexed);
    println!("memory_facts_indexed: {}", snapshot.memory_facts_indexed);
    println!("runtime_transport: {}", snapshot.runtime_transport);
    println!("runtime_skills: {}", snapshot.runtime_skills);
    println!(
        "auto_resolve_references: {}",
        snapshot.auto_resolve_references
    );
    println!("home_session_path: {}", snapshot.home_session_path);
    println!(
        "prompt_context_budget_chars: {}",
        snapshot.prompt_context_budget_chars
    );
    println!(
        "max_resolved_references: {}",
        snapshot.max_resolved_references
    );
    println!("reference_resolvers: {}", snapshot.reference_resolvers);
    println!("query_cache: {}", snapshot.query_cache);
    println!(
        "query_cache_entries_on_disk: {}",
        snapshot.query_cache_entries_on_disk
    );
    println!(
        "skill_eval_events_on_disk: {}",
        snapshot.skill_eval_events_on_disk
    );
    println!(
        "skill_eval_outcomes_on_disk: {}",
        snapshot.skill_eval_outcomes_on_disk
    );
    println!(
        "skill_scorecard_samples: {}",
        snapshot.skill_scorecard_samples
    );
    println!(
        "skill_scorecard_entries: {}",
        snapshot.skill_scorecard_entries
    );
    if let Some(top) = &snapshot.skill_scorecard_top {
        println!("skill_scorecard_top: {}", top);
    }
    println!(
        "external_skill_candidates: {}",
        snapshot.external_skill_candidates
    );
    println!("runtime_state_files: {}", snapshot.runtime_state_files);
    println!(
        "runtime_state_files_for_target: {}",
        snapshot.runtime_state_files_for_target
    );
    println!("skill_eval_schedule: {}", snapshot.skill_eval_schedule);
    println!("learning_state: {}", snapshot.learning_state);
    println!("runtime_ready: {}", snapshot.runtime_ready);
    println!("schedule_ready: {}", snapshot.schedule_ready);
    println!("learning_ready: {}", snapshot.learning_ready);
    if !snapshot.warnings.is_empty() {
        println!("warnings: {}", snapshot.warnings.len());
        for warning in &snapshot.warnings {
            println!("- {}", warning);
        }
    }
}

fn assert_codex_doctor(
    snapshot: &CodexDoctorSnapshot,
    assert_runtime: bool,
    assert_schedule: bool,
    assert_learning: bool,
    assert_autonomous: bool,
) -> Result<()> {
    let mut failures = Vec::new();
    let require_runtime = assert_runtime || assert_autonomous;
    let require_schedule = assert_schedule || assert_autonomous;
    let require_learning = assert_learning || assert_autonomous;

    if require_runtime {
        if snapshot.runtime_transport != "enabled" {
            failures.push(
                "runtime transport is disabled; set [options].codex_bin to the Flow wrapper"
                    .to_string(),
            );
        }
        if snapshot.runtime_skills != "enabled" {
            failures.push(
                "runtime skills are not active; enable [codex].runtime_skills and use the Flow wrapper"
                    .to_string(),
            );
        }
        if !snapshot.auto_resolve_references {
            failures.push("auto_resolve_references is disabled".to_string());
        }
    }

    if require_schedule && !snapshot.schedule_ready {
        failures.push(format!(
            "scheduled skill-eval refresh is {}; install/load the launchd agent",
            snapshot.skill_eval_schedule
        ));
    }

    if require_learning {
        if snapshot.skill_eval_events_on_disk == 0 {
            failures.push("no Codex route events recorded yet".to_string());
        }
        if snapshot.skill_scorecard_entries == 0 {
            failures.push("no skill scorecard entries built yet".to_string());
        }
        if snapshot.skill_eval_outcomes_on_disk == 0 {
            failures.push(
                "no grounded skill outcome events recorded yet; the system is still affinity-only"
                    .to_string(),
            );
        }
    }

    if failures.is_empty() {
        return Ok(());
    }

    bail!(
        "codex doctor assertion failed:\n- {}\nnext: run `f codex enable-global --full`, then exercise `f codex open ...` or `f ai codex new` through Flow until outcomes appear",
        failures.join("\n- ")
    )
}

fn parse_global_flow_toml(path: &Path) -> Result<toml::value::Table> {
    if !path.exists() {
        return Ok(toml::value::Table::new());
    }

    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    if content.trim().is_empty() {
        return Ok(toml::value::Table::new());
    }

    let value: TomlValue =
        toml::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))?;
    value
        .as_table()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("global flow config must be a TOML table"))
}

fn ensure_toml_table<'a>(
    root: &'a mut toml::value::Table,
    key: &str,
) -> Result<&'a mut toml::value::Table> {
    let needs_insert = !matches!(root.get(key), Some(TomlValue::Table(_)));
    if needs_insert {
        if root.contains_key(key) {
            bail!("expected [{}] to be a table in global flow config", key);
        }
        root.insert(key.to_string(), TomlValue::Table(toml::value::Table::new()));
    }
    root.get_mut(key)
        .and_then(TomlValue::as_table_mut)
        .ok_or_else(|| anyhow::anyhow!("expected [{}] to be a table in global flow config", key))
}

fn write_string_atomically(path: &Path, content: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("missing parent for {}", path.display()))?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".{}.tmp-{}-{}",
        path.file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("flow.toml"),
        std::process::id(),
        unix_now_secs()
    ));
    fs::write(&temp, content).with_context(|| format!("failed to write {}", temp.display()))?;
    fs::rename(&temp, path).with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

fn upsert_global_codex_config(path: &Path) -> Result<(String, bool, bool, bool)> {
    let mut root = parse_global_flow_toml(path)?;
    let created = !path.exists();
    let wrapper_path = config::expand_path(DEFAULT_GLOBAL_CODEX_WRAPPER_BIN);
    if !wrapper_path.exists() {
        bail!(
            "Flow Codex wrapper is missing at {}; build or sync Flow first",
            wrapper_path.display()
        );
    }

    let codex = ensure_toml_table(&mut root, "codex")?;
    codex.insert("runtime_skills".to_string(), TomlValue::Boolean(true));
    codex.insert(
        "auto_resolve_references".to_string(),
        TomlValue::Boolean(true),
    );
    codex
        .entry("home_session_path".to_string())
        .or_insert_with(|| TomlValue::String(DEFAULT_GLOBAL_CODEX_HOME_SESSION_PATH.to_string()));
    codex
        .entry("prompt_context_budget_chars".to_string())
        .or_insert_with(|| TomlValue::Integer(DEFAULT_GLOBAL_CODEX_PROMPT_BUDGET as i64));
    codex
        .entry("max_resolved_references".to_string())
        .or_insert_with(|| TomlValue::Integer(DEFAULT_GLOBAL_CODEX_MAX_REFERENCES as i64));

    let skill_source_root = config::expand_path(DEFAULT_GLOBAL_CODEX_SKILL_SOURCE_PATH);
    let skill_source_available = skill_source_root.exists();
    let mut skill_source_added = false;
    if skill_source_available {
        let entry = codex
            .entry("skill_source".to_string())
            .or_insert_with(|| TomlValue::Array(Vec::new()));
        let array = entry
            .as_array_mut()
            .ok_or_else(|| anyhow::anyhow!("[codex].skill_source must be an array"))?;
        let exists = array.iter().any(|value| {
            let Some(table) = value.as_table() else {
                return false;
            };
            table
                .get("name")
                .and_then(TomlValue::as_str)
                .map(|name| name == DEFAULT_GLOBAL_CODEX_SKILL_SOURCE_NAME)
                .unwrap_or(false)
                || table
                    .get("path")
                    .and_then(TomlValue::as_str)
                    .map(|value| config::expand_path(value) == skill_source_root)
                    .unwrap_or(false)
        });
        if !exists {
            let mut source = toml::value::Table::new();
            source.insert(
                "name".to_string(),
                TomlValue::String(DEFAULT_GLOBAL_CODEX_SKILL_SOURCE_NAME.to_string()),
            );
            source.insert(
                "path".to_string(),
                TomlValue::String(DEFAULT_GLOBAL_CODEX_SKILL_SOURCE_PATH.to_string()),
            );
            source.insert("enabled".to_string(), TomlValue::Boolean(true));
            array.push(TomlValue::Table(source));
            skill_source_added = true;
        }
    }

    let options = ensure_toml_table(&mut root, "options")?;
    options.insert(
        "codex_bin".to_string(),
        TomlValue::String(DEFAULT_GLOBAL_CODEX_WRAPPER_BIN.to_string()),
    );

    let rendered = toml::to_string_pretty(&TomlValue::Table(root))
        .context("failed to render global flow config")?;
    Ok((
        rendered,
        created,
        skill_source_added,
        skill_source_available,
    ))
}

fn install_codex_skill_eval_launchd(
    current_exe: &Path,
    minutes: usize,
    limit: usize,
    max_targets: usize,
    within_hours: u64,
    dry_run: bool,
) -> Result<String> {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("scripts")
        .join("codex-skill-eval-launchd.py");
    let mut command = Command::new("python3");
    command
        .arg(script)
        .arg("install")
        .arg("--minutes")
        .arg(minutes.to_string())
        .arg("--limit")
        .arg(limit.to_string())
        .arg("--max-targets")
        .arg(max_targets.to_string())
        .arg("--within-hours")
        .arg(within_hours.to_string());
    if dry_run {
        command.arg("--dry-run");
    }
    command.env("FLOW_CODEX_SKILL_EVAL_F_BIN", current_exe);
    let output = command
        .output()
        .context("failed to run codex skill-eval launchd installer")?;
    if !output.status.success() {
        bail!(
            "codex skill-eval launchd install failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn codex_enable_global(
    dry_run: bool,
    install_launchd: bool,
    start_daemon: bool,
    sync_skills: bool,
    full: bool,
    minutes: usize,
    limit: usize,
    max_targets: usize,
    within_hours: u64,
    provider: Provider,
) -> Result<()> {
    if provider != Provider::Codex {
        bail!("enable-global is only supported for Codex sessions; use `f codex enable-global`");
    }

    let install_launchd = install_launchd || full;
    let start_daemon = start_daemon || full;
    let sync_skills = sync_skills || full;
    let config_path = config::default_config_path();
    let (rendered, created, skill_source_added, skill_source_available) =
        upsert_global_codex_config(&config_path)?;

    if dry_run {
        println!("# codex enable-global");
        println!("config_path: {}", config_path.display());
        println!("config_created: {}", created);
        println!("skill_source_available: {}", skill_source_available);
        println!("skill_source_added: {}", skill_source_added);
        if install_launchd {
            let preview = install_codex_skill_eval_launchd(
                &env::current_exe().context("failed to resolve current flow executable")?,
                minutes,
                limit,
                max_targets,
                within_hours,
                true,
            )?;
            println!();
            println!("{}", preview);
        }
        println!();
        print!("{}", rendered);
        return Ok(());
    }

    let global_dir = config::ensure_global_config_dir()?;
    write_string_atomically(&config_path, &rendered)?;
    println!("Updated global Flow config: {}", config_path.display());
    if created {
        println!("Created {}", global_dir.display());
    }
    println!(
        "Enabled global Codex wrapper/runtime transport via {}",
        DEFAULT_GLOBAL_CODEX_WRAPPER_BIN
    );
    if skill_source_available {
        if skill_source_added {
            println!(
                "Registered external skill source: {}",
                DEFAULT_GLOBAL_CODEX_SKILL_SOURCE_PATH
            );
        } else {
            println!(
                "External skill source already configured: {}",
                DEFAULT_GLOBAL_CODEX_SKILL_SOURCE_PATH
            );
        }
    }

    if install_launchd {
        let launchd_output = install_codex_skill_eval_launchd(
            &env::current_exe().context("failed to resolve current flow executable")?,
            minutes,
            limit,
            max_targets,
            within_hours,
            false,
        )?;
        if !launchd_output.is_empty() {
            println!("{}", launchd_output);
        }
    }

    if start_daemon {
        codexd::start()?;
    }

    if sync_skills {
        let target_path = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let codex_cfg = load_codex_config_for_path(&target_path);
        let installed = codex_runtime::sync_external_skills(&target_path, &codex_cfg, &[], false)?;
        println!(
            "Synced {} external Codex skill(s) into ~/.codex/skills.",
            installed
        );
    }

    let verify_target = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let snapshot = collect_codex_doctor_snapshot(&verify_target)?;
    assert_codex_doctor(&snapshot, true, install_launchd, false, false)?;
    println!();
    print_codex_doctor(&snapshot);
    Ok(())
}

fn codex_doctor(
    path: Option<String>,
    assert_runtime: bool,
    assert_schedule: bool,
    assert_learning: bool,
    assert_autonomous: bool,
    json_output: bool,
    provider: Provider,
) -> Result<()> {
    if provider != Provider::Codex {
        bail!("doctor is only supported for Codex sessions; use `f codex doctor`");
    }

    let target_path = resolve_session_target_path(path.as_deref())?;
    let snapshot = collect_codex_doctor_snapshot(&target_path)?;
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&snapshot)
                .context("failed to encode codex doctor JSON")?
        );
    } else {
        print_codex_doctor(&snapshot);
    }
    assert_codex_doctor(
        &snapshot,
        assert_runtime,
        assert_schedule,
        assert_learning,
        assert_autonomous,
    )?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CodexQuickLaunchEvent {
    version: u8,
    launch_id: String,
    recorded_at_unix: u64,
    mode: String,
    cwd: String,
    daemon: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CodexQuickLaunchHydration {
    version: u8,
    launch_id: String,
    hydrated_at_unix: u64,
    target_path: String,
    session_id: String,
    query: String,
    prompt_recorded_at_unix: u64,
}

fn codex_quick_launch_log_path() -> Result<PathBuf> {
    Ok(config::ensure_global_state_dir()?
        .join("codex")
        .join("quick-launches.jsonl"))
}

fn codex_quick_launch_hydrations_path() -> Result<PathBuf> {
    Ok(config::ensure_global_state_dir()?
        .join("codex")
        .join("quick-launches-hydrated.jsonl"))
}

fn log_codex_quick_launch_event(event: &CodexQuickLaunchEvent) -> Result<()> {
    let path = codex_quick_launch_log_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    serde_json::to_writer(&mut file, event).context("failed to encode quick launch event")?;
    file.write_all(b"\n")
        .context("failed to terminate quick launch event")?;
    Ok(())
}

fn log_codex_quick_launch_hydration(hydration: &CodexQuickLaunchHydration) -> Result<()> {
    let path = codex_quick_launch_hydrations_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    serde_json::to_writer(&mut file, hydration)
        .context("failed to encode quick launch hydration")?;
    file.write_all(b"\n")
        .context("failed to terminate quick launch hydration")?;
    Ok(())
}

fn load_recent_codex_quick_launches(limit: usize) -> Result<Vec<CodexQuickLaunchEvent>> {
    let path = codex_quick_launch_log_path()?;
    if !path.exists() || limit == 0 {
        return Ok(Vec::new());
    }
    let raw =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut launches = raw
        .lines()
        .rev()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return None;
            }
            serde_json::from_str::<CodexQuickLaunchEvent>(trimmed).ok()
        })
        .take(limit)
        .collect::<Vec<_>>();
    launches.sort_by_key(|launch| launch.recorded_at_unix);
    Ok(launches)
}

fn load_hydrated_codex_quick_launch_ids() -> Result<BTreeSet<String>> {
    let path = codex_quick_launch_hydrations_path()?;
    if !path.exists() {
        return Ok(BTreeSet::new());
    }
    let raw =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(raw
        .lines()
        .filter_map(|line| serde_json::from_str::<CodexQuickLaunchHydration>(line.trim()).ok())
        .map(|hydration| hydration.launch_id)
        .collect())
}

fn parse_rfc3339_to_unix(value: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .and_then(|dt| u64::try_from(dt.timestamp()).ok())
}

fn read_codex_first_user_message_since(
    session_file: &PathBuf,
    since_unix: u64,
) -> Result<Option<(String, u64)>> {
    let mut first: Option<(String, u64)> = None;
    for_each_nonempty_jsonl_line(session_file, |line| {
        let entry: CodexEntry = match crate::json_parse::parse_json_line(line) {
            Ok(v) => v,
            Err(_) => return,
        };
        let Some((role, text)) = extract_codex_message(&entry) else {
            return;
        };
        if role != "user" || text.trim().is_empty() {
            return;
        }
        let Some(cleaned) = codex_text::sanitize_codex_query_text(&text) else {
            return;
        };
        let Some(ts) =
            extract_codex_timestamp(&entry).and_then(|value| parse_rfc3339_to_unix(&value))
        else {
            return;
        };
        if ts < since_unix {
            return;
        }
        if first
            .as_ref()
            .map(|(_, current)| ts < *current)
            .unwrap_or(true)
        {
            first = Some((cleaned, ts));
        }
    })?;
    Ok(first)
}

fn hydrate_codex_quick_launch(
    launch: &CodexQuickLaunchEvent,
) -> Result<Option<CodexQuickLaunchHydration>> {
    let target_path = PathBuf::from(&launch.cwd);
    if !target_path.exists() {
        return Ok(None);
    }

    let mut candidates = read_recent_codex_threads_local(&target_path, true, 8, None)?;
    if candidates.is_empty() {
        candidates = read_recent_codex_threads_local(&target_path, false, 8, None)?;
    }
    if candidates.is_empty() {
        return Ok(None);
    }

    let since_unix = launch.recorded_at_unix.saturating_sub(1);
    let mut best: Option<(u64, String, String)> = None;
    for candidate in candidates {
        let Some(session_file) = find_codex_session_file(&candidate.id) else {
            continue;
        };
        let Some((query, prompt_recorded_at_unix)) =
            read_codex_first_user_message_since(&session_file, since_unix)?
        else {
            continue;
        };
        let replace = best
            .as_ref()
            .map(|(best_ts, _, _)| prompt_recorded_at_unix < *best_ts)
            .unwrap_or(true);
        if replace {
            best = Some((prompt_recorded_at_unix, candidate.id, query));
        }
    }

    let Some((prompt_recorded_at_unix, session_id, query)) = best else {
        return Ok(None);
    };

    Ok(Some(CodexQuickLaunchHydration {
        version: 1,
        launch_id: launch.launch_id.clone(),
        hydrated_at_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_secs())
            .unwrap_or(0),
        target_path: target_path.display().to_string(),
        session_id,
        query,
        prompt_recorded_at_unix,
    }))
}

fn reconcile_pending_codex_quick_launches(limit: usize) -> Result<usize> {
    let launches = load_recent_codex_quick_launches(limit)?;
    if launches.is_empty() {
        return Ok(0);
    }

    let hydrated_ids = load_hydrated_codex_quick_launch_ids()?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0);
    let mut reconciled = 0usize;

    for launch in launches {
        if hydrated_ids.contains(&launch.launch_id) {
            continue;
        }
        if now.saturating_sub(launch.recorded_at_unix) < 2 {
            continue;
        }
        let Some(hydration) = hydrate_codex_quick_launch(&launch)? else {
            continue;
        };

        let event = codex_skill_eval::CodexSkillEvalEvent {
            version: 1,
            recorded_at_unix: hydration.prompt_recorded_at_unix,
            mode: "quick-launch".to_string(),
            action: if launch.mode == "new" {
                "new".to_string()
            } else {
                "resume".to_string()
            },
            route: "quick-launch-hydrated".to_string(),
            target_path: hydration.target_path.clone(),
            launch_path: hydration.target_path.clone(),
            query: hydration.query.clone(),
            session_id: Some(hydration.session_id.clone()),
            runtime_token: None,
            runtime_skills: Vec::new(),
            prompt_context_budget_chars: 0,
            prompt_chars: hydration.query.chars().count(),
            injected_context_chars: 0,
            reference_count: 0,
        };
        let _ = codex_skill_eval::log_event(&event);
        let _ = log_codex_quick_launch_hydration(&hydration);
        reconciled += 1;
    }

    Ok(reconciled)
}

fn codex_touch_launch(mode: String, cwd: Option<String>, provider: Provider) -> Result<()> {
    if provider != Provider::Codex {
        bail!("touch-launch is only supported for Codex sessions; use `f codex touch-launch`");
    }

    let cwd_path = resolve_session_target_path(cwd.as_deref())?;
    let daemon = if codexd::ensure_running().is_ok() {
        "running"
    } else {
        "unavailable"
    };
    let recorded_at_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    mode.hash(&mut hasher);
    cwd_path.hash(&mut hasher);
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0)
        .hash(&mut hasher);
    let event = CodexQuickLaunchEvent {
        version: 1,
        launch_id: format!("{:016x}", hasher.finish()),
        recorded_at_unix,
        mode,
        cwd: cwd_path.display().to_string(),
        daemon: daemon.to_string(),
    };
    let _ = log_codex_quick_launch_event(&event);
    let _ = reconcile_pending_codex_quick_launches(48);
    Ok(())
}

fn codex_daemon_command(action: Option<CodexDaemonAction>, provider: Provider) -> Result<()> {
    if provider != Provider::Codex {
        bail!("daemon is only supported for Codex sessions; use `f codex daemon ...`");
    }

    match action.unwrap_or(CodexDaemonAction::Status) {
        CodexDaemonAction::Start => codexd::start(),
        CodexDaemonAction::Stop => codexd::stop(),
        CodexDaemonAction::Restart => {
            codexd::stop().ok();
            std::thread::sleep(Duration::from_millis(300));
            codexd::start()
        }
        CodexDaemonAction::Status => codexd::status(),
        CodexDaemonAction::Serve { socket } => codexd::serve(socket.as_deref()),
        CodexDaemonAction::Ping => codexd::ping(),
    }
}

fn codex_memory_command(action: Option<CodexMemoryAction>, provider: Provider) -> Result<()> {
    if provider != Provider::Codex {
        bail!("memory is only supported for Codex sessions; use `f codex memory ...`");
    }

    match action.unwrap_or(CodexMemoryAction::Status { json: false }) {
        CodexMemoryAction::Status { json } => {
            let stats = codex_memory::stats()?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&stats)
                        .context("failed to encode codex memory status JSON")?
                );
            } else {
                println!("# codex memory");
                println!("root: {}", stats.root_dir);
                println!("db_path: {}", stats.db_path);
                println!("events_indexed: {}", stats.total_events);
                println!("facts_indexed: {}", stats.total_facts);
                println!("skill_eval_events: {}", stats.skill_eval_events);
                println!("skill_eval_outcomes: {}", stats.skill_eval_outcomes);
                if let Some(latest) = stats.latest_recorded_at_unix {
                    println!("latest_recorded_at_unix: {}", latest);
                }
            }
            Ok(())
        }
        CodexMemoryAction::Sync { limit, json } => {
            let _ = reconcile_pending_codex_quick_launches(limit.max(64));
            let summary = codex_memory::sync_from_skill_eval_logs(limit)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&summary)
                        .context("failed to encode codex memory sync JSON")?
                );
            } else {
                println!("# codex memory sync");
                println!("total_considered: {}", summary.total_considered);
                println!("inserted: {}", summary.inserted);
                println!("skipped: {}", summary.skipped);
            }
            Ok(())
        }
        CodexMemoryAction::Query {
            path,
            limit,
            json,
            query,
        } => {
            let query_text = query.join(" ").trim().to_string();
            if query_text.is_empty() {
                bail!("codex memory query requires a search string");
            }
            let target_path = resolve_session_target_path(path.as_deref())?;
            let result = codex_memory::query_repo_facts(&target_path, &query_text, limit)?
                .ok_or_else(|| anyhow::anyhow!("no codex memory facts matched {:?}", query_text))?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&result)
                        .context("failed to encode codex memory query JSON")?
                );
            } else {
                println!("{}", result.rendered);
            }
            Ok(())
        }
        CodexMemoryAction::Recent { path, limit, json } => {
            let _ = reconcile_pending_codex_quick_launches(limit.max(64));
            let _ = codex_memory::sync_from_skill_eval_logs(limit.max(200));
            let target_path = path
                .as_deref()
                .map(|value| resolve_session_target_path(Some(value)))
                .transpose()?;
            let rows = codex_memory::recent(target_path.as_deref(), limit)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&rows)
                        .context("failed to encode codex memory recent JSON")?
                );
            } else if rows.is_empty() {
                println!("No codex memory rows recorded.");
            } else {
                println!("# codex memory recent");
                for row in rows {
                    let subject = row
                        .query
                        .as_deref()
                        .filter(|value| !value.trim().is_empty())
                        .map(|value| truncate_message(value, 96))
                        .or_else(|| row.route.clone())
                        .unwrap_or_else(|| "(no query)".to_string());
                    println!(
                        "- {} | {} | {}",
                        row.event_kind, row.recorded_at_unix, subject
                    );
                }
            }
            Ok(())
        }
    }
}

fn codex_skill_eval_command(
    action: Option<CodexSkillEvalAction>,
    provider: Provider,
) -> Result<()> {
    if provider != Provider::Codex {
        bail!("skill-eval is only supported for Codex sessions; use `f codex skill-eval ...`");
    }

    match action.unwrap_or(CodexSkillEvalAction::Show {
        path: None,
        json: false,
    }) {
        CodexSkillEvalAction::Run { path, limit, json } => {
            let _ = reconcile_pending_codex_quick_launches(limit.max(48));
            let target_path = resolve_session_target_path(path.as_deref())?;
            let scorecard = codex_skill_eval::rebuild_scorecard(&target_path, limit)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&scorecard)
                        .context("failed to encode codex skill-eval JSON")?
                );
            } else {
                println!("{}", codex_skill_eval::format_scorecard(&scorecard));
            }
            Ok(())
        }
        CodexSkillEvalAction::Show { path, json } => {
            let _ = reconcile_pending_codex_quick_launches(64);
            let target_path = resolve_session_target_path(path.as_deref())?;
            let scorecard = codex_skill_eval::load_scorecard(&target_path)?
                .unwrap_or(codex_skill_eval::rebuild_scorecard(&target_path, 200)?);
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&scorecard)
                        .context("failed to encode codex skill-eval JSON")?
                );
            } else {
                println!("{}", codex_skill_eval::format_scorecard(&scorecard));
            }
            Ok(())
        }
        CodexSkillEvalAction::Events { path, limit, json } => {
            let _ = reconcile_pending_codex_quick_launches(limit.max(48));
            let target_path = path
                .as_deref()
                .map(|value| resolve_session_target_path(Some(value)))
                .transpose()?;
            let events = codex_skill_eval::load_events(target_path.as_deref(), limit)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&events)
                        .context("failed to encode codex skill-eval events JSON")?
                );
            } else if events.is_empty() {
                println!("No codex skill-eval events recorded.");
            } else {
                println!("# codex skill-eval events");
                for event in events {
                    println!(
                        "- {} | {} | {} | skills {}",
                        event.mode,
                        event.route,
                        event.target_path,
                        if event.runtime_skills.is_empty() {
                            "(none)".to_string()
                        } else {
                            event.runtime_skills.join(", ")
                        }
                    );
                }
            }
            Ok(())
        }
        CodexSkillEvalAction::Cron {
            limit,
            max_targets,
            within_hours,
            json,
        } => {
            let reconciled = reconcile_pending_codex_quick_launches(limit.max(64))?;
            let memory_sync = codex_memory::sync_from_skill_eval_logs(limit.max(200))?;
            let targets = codex_skill_eval::recent_targets(limit, max_targets, within_hours)?;
            let mut capsule_sync_count = 0usize;
            let mut scorecards = Vec::new();
            for target in targets {
                if codex_memory::sync_repo_capsule_for_path(&target).is_ok() {
                    capsule_sync_count += 1;
                }
                scorecards.push(codex_skill_eval::rebuild_scorecard(&target, limit)?);
            }
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "reconciledQuickLaunches": reconciled,
                        "memorySync": memory_sync,
                        "capsulesSynced": capsule_sync_count,
                        "scorecards": scorecards,
                    }))
                    .context("failed to encode codex skill-eval cron JSON")?
                );
            } else if scorecards.is_empty() {
                println!(
                    "No recent Codex skill-eval targets found. Reconciled {} fast launch(es), indexed {} memory event(s), synced {} repo capsule(s).",
                    reconciled, memory_sync.inserted, capsule_sync_count
                );
            } else {
                println!("# codex skill-eval cron");
                println!("reconciled fast launches: {}", reconciled);
                println!("memory inserted: {}", memory_sync.inserted);
                println!("repo capsules synced: {}", capsule_sync_count);
                for scorecard in scorecards {
                    let top = scorecard
                        .skills
                        .first()
                        .map(|skill| format!("{} ({:.2})", skill.name, skill.score))
                        .unwrap_or_else(|| "none".to_string());
                    println!(
                        "- {} | samples {} | top {}",
                        scorecard.target_path, scorecard.samples, top
                    );
                }
            }
            Ok(())
        }
    }
}

fn codex_skill_source_command(
    action: Option<CodexSkillSourceAction>,
    provider: Provider,
) -> Result<()> {
    if provider != Provider::Codex {
        bail!("skill-source is only supported for Codex sessions; use `f codex skill-source ...`");
    }

    match action.unwrap_or(CodexSkillSourceAction::List {
        path: None,
        json: false,
    }) {
        CodexSkillSourceAction::List { path, json } => {
            let target_path = resolve_session_target_path(path.as_deref())?;
            let codex_cfg = load_codex_config_for_path(&target_path);
            let skills = codex_runtime::discover_external_skills(&target_path, &codex_cfg)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&skills)
                        .context("failed to encode codex skill-source JSON")?
                );
            } else {
                println!("{}", codex_runtime::format_external_skills(&skills));
            }
            Ok(())
        }
        CodexSkillSourceAction::Sync {
            path,
            skills,
            force,
        } => {
            let target_path = resolve_session_target_path(path.as_deref())?;
            let codex_cfg = load_codex_config_for_path(&target_path);
            let installed =
                codex_runtime::sync_external_skills(&target_path, &codex_cfg, &skills, force)?;
            println!(
                "Synced {} external Codex skill(s) into ~/.codex/skills.",
                installed
            );
            Ok(())
        }
    }
}

fn codex_runtime_command(action: Option<CodexRuntimeAction>, provider: Provider) -> Result<()> {
    if provider != Provider::Codex {
        bail!("runtime helpers are only supported for Codex sessions; use `f codex runtime ...`");
    }

    match action.unwrap_or(CodexRuntimeAction::Show) {
        CodexRuntimeAction::Show => {
            let states = codex_runtime::load_runtime_states()?;
            println!("{}", codex_runtime::format_runtime_states(&states));
        }
        CodexRuntimeAction::Clear => {
            let removed = codex_runtime::clear_runtime_states()?;
            println!(
                "Cleared {} Flow-managed Codex runtime state file(s).",
                removed
            );
        }
        CodexRuntimeAction::WritePlan {
            title,
            stem,
            dir,
            source_session,
        } => {
            let path = codex_runtime::write_plan_from_stdin(
                title.as_deref(),
                stem.as_deref(),
                dir.as_deref(),
                source_session.as_deref(),
            )?;
            println!("{}", path.display());
        }
    }

    Ok(())
}

fn normalize_codex_resolve_args(query: Vec<String>, json_output: bool) -> (Vec<String>, bool) {
    if json_output {
        return (query, true);
    }

    let mut normalized = query;
    let mut resolved_json = false;
    while matches!(normalized.last().map(String::as_str), Some("--json")) {
        normalized.pop();
        resolved_json = true;
    }

    (normalized, resolved_json)
}

fn build_codex_open_plan(
    path: Option<String>,
    query: Vec<String>,
    exact_cwd: bool,
) -> Result<CodexOpenPlan> {
    let target_path = resolve_session_target_path(path.as_deref())?;
    let query_text = normalize_recover_query(&query);
    let codex_cfg = load_codex_config_for_path(&target_path);
    let auto_resolve_references = codex_cfg.auto_resolve_references.unwrap_or(true);
    let max_resolved_references = effective_max_resolved_references(&codex_cfg);
    let runtime_skills_enabled =
        codex_cfg.runtime_skills.unwrap_or(false) && codex_runtime_transport_enabled(&target_path);
    let default_prompt_budget = effective_prompt_context_budget_chars(&codex_cfg, false);

    let Some(query_text) = query_text else {
        let prompt = None;
        return Ok(finalize_codex_open_plan(CodexOpenPlan {
            action: "new".to_string(),
            route: "new-empty".to_string(),
            reason: "no query provided".to_string(),
            target_path: target_path.display().to_string(),
            launch_path: target_path.display().to_string(),
            query: None,
            session_id: None,
            prompt,
            references: Vec::new(),
            runtime_state_path: None,
            runtime_skills: Vec::new(),
            prompt_context_budget_chars: default_prompt_budget,
            max_resolved_references,
            prompt_chars: 0,
            injected_context_chars: 0,
        }));
    };

    let normalized_query = query_text.to_ascii_lowercase();

    if let Some(request) = extract_codex_session_reference_request(&query_text, &normalized_query) {
        let mut references = vec![resolve_builtin_codex_session_reference(&request)?];
        if auto_resolve_references {
            let extra_references = resolve_codex_references(
                &target_path,
                &request.user_request,
                &codex_cfg.reference_resolvers,
            )?;
            for reference in extra_references {
                if !references
                    .iter()
                    .any(|existing| existing.matched == reference.matched)
                {
                    references.push(reference);
                }
            }
        }
        let runtime = codex_runtime::prepare_runtime_activation(
            &target_path,
            &request.user_request,
            runtime_skills_enabled,
            &codex_cfg,
        )?;
        let prompt_budget = effective_prompt_context_budget_chars(&codex_cfg, true);
        let prompt = build_codex_prompt_with_runtime(
            &request.user_request,
            &references,
            runtime.as_ref(),
            max_resolved_references,
            prompt_budget,
        );
        return Ok(finalize_codex_open_plan(CodexOpenPlan {
            action: "new".to_string(),
            route: "reference-new".to_string(),
            reason: "start a new session with resolved Codex session context".to_string(),
            target_path: target_path.display().to_string(),
            launch_path: target_path.display().to_string(),
            query: Some(query_text),
            session_id: None,
            prompt,
            references,
            runtime_state_path: runtime
                .as_ref()
                .map(|value| value.state_path.display().to_string()),
            runtime_skills: runtime_skill_names(runtime.as_ref()),
            prompt_context_budget_chars: prompt_budget,
            max_resolved_references,
            prompt_chars: 0,
            injected_context_chars: 0,
        }));
    }

    if looks_like_recovery_prompt(&normalized_query) {
        return build_codex_recovery_plan(
            &target_path,
            exact_cwd,
            &query_text,
            runtime_skills_enabled,
            default_prompt_budget,
            max_resolved_references,
        );
    }

    if let Some((session, reason)) =
        resolve_codex_session_lookup(&target_path, exact_cwd, &query_text, &normalized_query)?
    {
        return Ok(finalize_codex_open_plan(CodexOpenPlan {
            action: "resume".to_string(),
            route: "resume-existing".to_string(),
            reason,
            target_path: target_path.display().to_string(),
            launch_path: session.cwd.clone(),
            query: Some(query_text),
            session_id: Some(session.id),
            prompt: None,
            references: Vec::new(),
            runtime_state_path: None,
            runtime_skills: Vec::new(),
            prompt_context_budget_chars: default_prompt_budget,
            max_resolved_references,
            prompt_chars: 0,
            injected_context_chars: 0,
        }));
    }

    if looks_like_session_lookup_query(&normalized_query) {
        bail!(
            "{}",
            build_codex_open_no_match_message(&target_path, exact_cwd, &query_text)?
        );
    }

    let references = if auto_resolve_references {
        resolve_codex_references(&target_path, &query_text, &codex_cfg.reference_resolvers)?
    } else {
        Vec::new()
    };
    let runtime = codex_runtime::prepare_runtime_activation(
        &target_path,
        &query_text,
        runtime_skills_enabled,
        &codex_cfg,
    )?;
    let prompt_budget =
        effective_prompt_context_budget_chars(&codex_cfg, has_session_reference(&references));
    let prompt = build_codex_prompt_with_runtime(
        &query_text,
        &references,
        runtime.as_ref(),
        max_resolved_references,
        prompt_budget,
    );

    Ok(finalize_codex_open_plan(CodexOpenPlan {
        action: "new".to_string(),
        route: if references.is_empty() {
            "new-plain".to_string()
        } else {
            "new-with-context".to_string()
        },
        reason: if references.is_empty() {
            "start a new session from the current query".to_string()
        } else {
            "start a new session with compact resolved context".to_string()
        },
        target_path: target_path.display().to_string(),
        launch_path: target_path.display().to_string(),
        query: Some(query_text),
        session_id: None,
        prompt,
        references,
        runtime_state_path: runtime
            .as_ref()
            .map(|value| value.state_path.display().to_string()),
        runtime_skills: runtime_skill_names(runtime.as_ref()),
        prompt_context_budget_chars: prompt_budget,
        max_resolved_references,
        prompt_chars: 0,
        injected_context_chars: 0,
    }))
}

fn execute_codex_open_plan(plan: &CodexOpenPlan) -> Result<()> {
    let launch_path = PathBuf::from(&plan.launch_path);
    match plan.action.as_str() {
        "resume" => {
            let session_id = plan
                .session_id
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("missing session id for resume plan"))?;
            println!(
                "Opening Codex session {} in {}...",
                truncate_recover_id(session_id),
                launch_path.display()
            );
            if launch_session_for_target(
                session_id,
                Provider::Codex,
                plan.prompt.as_deref(),
                Some(&launch_path),
                plan.runtime_state_path.as_deref(),
            )? {
                Ok(())
            } else {
                bail!("failed to resume codex session {}", session_id);
            }
        }
        "new" | "recover-new" => {
            println!("Starting Codex in {}...", launch_path.display());
            new_session_for_target(
                Provider::Codex,
                plan.prompt.as_deref(),
                Some(&launch_path),
                plan.runtime_state_path.as_deref(),
            )
        }
        other => bail!("unsupported codex open action: {}", other),
    }
}

fn print_codex_open_plan(plan: &CodexOpenPlan) {
    println!("# codex resolve");
    println!("action: {}", plan.action);
    println!("route: {}", plan.route);
    println!("reason: {}", plan.reason);
    println!("target: {}", plan.target_path);
    println!("launch: {}", plan.launch_path);
    println!(
        "budget: {} chars, {} reference(s)",
        plan.prompt_context_budget_chars, plan.max_resolved_references
    );
    if let Some(session_id) = plan.session_id.as_deref() {
        println!("session: {}", truncate_recover_id(session_id));
    }
    if !plan.references.is_empty() {
        println!("references:");
        for reference in &plan.references {
            println!(
                "- {} [{}] {}",
                reference.name, reference.source, reference.matched
            );
        }
    }
    if !plan.runtime_skills.is_empty() {
        println!("runtime:");
        for skill in &plan.runtime_skills {
            println!("- {}", skill);
        }
        if let Some(path) = plan.runtime_state_path.as_deref() {
            println!("runtime_state: {}", path);
        }
    }
    if let Some(prompt) = plan.prompt.as_deref() {
        println!("prompt_chars: {}", plan.prompt_chars);
        println!("injected_context_chars: {}", plan.injected_context_chars);
        println!("prompt:");
        println!("{}", compact_codex_context_block(prompt, 12, 900));
    }
}

fn record_codex_open_plan(plan: &CodexOpenPlan, mode: &str) {
    let Some(query) = plan
        .query
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return;
    };

    let event = codex_skill_eval::CodexSkillEvalEvent {
        version: 1,
        recorded_at_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_secs())
            .unwrap_or(0),
        mode: mode.to_string(),
        action: plan.action.clone(),
        route: plan.route.clone(),
        target_path: plan.target_path.clone(),
        launch_path: plan.launch_path.clone(),
        query: query.to_string(),
        session_id: plan.session_id.clone(),
        runtime_token: plan.runtime_state_path.as_deref().and_then(|path| {
            Path::new(path)
                .file_stem()
                .and_then(|value| value.to_str())
                .map(|value| value.to_string())
        }),
        runtime_skills: plan.runtime_skills.clone(),
        prompt_context_budget_chars: plan.prompt_context_budget_chars,
        prompt_chars: plan.prompt_chars,
        injected_context_chars: plan.injected_context_chars,
        reference_count: plan.references.len(),
    };

    let _ = codex_skill_eval::log_event(&event);
}

fn load_codex_config_for_path(target_path: &Path) -> config::CodexConfig {
    let mut resolved = config::CodexConfig::default();

    let global_path = config::default_config_path();
    if global_path.exists()
        && let Ok(cfg) = config::load(&global_path)
        && let Some(codex_cfg) = cfg.codex
    {
        resolved.merge(codex_cfg);
    }

    if let Some(local_path) = project_snapshot::find_flow_toml_upwards(target_path)
        && local_path != global_path
        && let Ok(cfg) = config::load(&local_path)
        && let Some(codex_cfg) = cfg.codex
    {
        resolved.merge(codex_cfg);
    }

    resolved
}

fn default_codex_connect_path() -> PathBuf {
    let seed = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let cfg = load_codex_config_for_path(&seed);
    if let Some(path) = cfg
        .home_session_path
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return config::expand_path(path);
    }

    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("~"))
        .join("repos")
        .join("openai")
        .join("codex")
}

fn resolve_codex_connect_target_path(path: Option<String>) -> Result<PathBuf> {
    match path
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(value) => resolve_session_target_path(Some(value)),
        None => Ok(default_codex_connect_path()),
    }
}

fn looks_like_recovery_prompt(normalized_query: &str) -> bool {
    normalized_query.contains("see this convo")
        || normalized_query.contains("what was i doing")
        || normalized_query.contains("recover recent context")
        || normalized_query.contains("recover context")
        || (normalized_query.contains("continue the")
            && (normalized_query.contains(" work")
                || normalized_query.contains(" session")
                || normalized_query.contains(" convo")
                || normalized_query.contains(" conversation")))
}

fn looks_like_session_lookup_query(normalized_query: &str) -> bool {
    extract_codex_session_hint(normalized_query).is_some()
        || looks_like_directional_session_query(normalized_query)
        || parse_ordinal_index(normalized_query).is_some()
        || looks_like_latest_query(normalized_query)
        || (contains_lookup_subject(normalized_query)
            && starts_with_session_control_phrase(normalized_query))
}

fn looks_like_directional_session_query(query: &str) -> bool {
    let has_direction = find_word_boundary(query, "after").is_some()
        || find_word_boundary(query, "before").is_some();
    has_direction && (contains_lookup_subject(query) || starts_with_session_control_phrase(query))
}

fn contains_lookup_subject(query: &str) -> bool {
    [
        "session",
        "sessions",
        "conversation",
        "conversations",
        "convo",
        "convos",
    ]
    .iter()
    .any(|value| query.split_whitespace().any(|word| word == *value))
}

fn starts_with_session_control_phrase(query: &str) -> bool {
    [
        "open ",
        "resume ",
        "continue ",
        "connect ",
        "find ",
        "recover ",
        "show ",
        "see ",
        "copy ",
        "summarize ",
        "what was i doing",
    ]
    .iter()
    .any(|prefix| query.starts_with(prefix))
}

fn resolve_codex_session_lookup(
    target_path: &Path,
    exact_cwd: bool,
    query_text: &str,
    normalized_query: &str,
) -> Result<Option<(CodexRecoverRow, String)>> {
    if let Some(session_hint) = extract_codex_session_hint(normalized_query) {
        let rows = read_codex_threads_by_session_hint(&session_hint, 1)?;
        if let Some(row) = rows.into_iter().next() {
            return Ok(Some((
                row,
                format!("explicit session id/prefix `{}`", session_hint),
            )));
        }
    }

    if let Some((row, reason)) =
        resolve_directional_session_lookup(target_path, exact_cwd, normalized_query)?
    {
        return Ok(Some((row, reason)));
    }

    if let Some(index) = parse_ordinal_index(normalized_query) {
        let rows = read_recent_codex_threads(target_path, exact_cwd, index + 1, None)?;
        if let Some(row) = rows.into_iter().nth(index) {
            return Ok(Some((row, format!("ordinal session match #{}", index + 1))));
        }
    }

    if looks_like_latest_query(normalized_query) {
        let rows = read_recent_codex_threads(target_path, exact_cwd, 1, None)?;
        if let Some(row) = rows.into_iter().next() {
            return Ok(Some((row, "latest recent session".to_string())));
        }
    }

    if looks_like_session_lookup_query(normalized_query) {
        let rows = search_codex_threads_for_find(Some(target_path), exact_cwd, query_text, 1)?;
        if let Some(row) = rows.into_iter().next() {
            return Ok(Some((row, "matched session search query".to_string())));
        }
    }

    Ok(None)
}

fn resolve_directional_session_lookup(
    target_path: &Path,
    exact_cwd: bool,
    normalized_query: &str,
) -> Result<Option<(CodexRecoverRow, String)>> {
    if !looks_like_directional_session_query(normalized_query) {
        return Ok(None);
    }
    let Some((direction, anchor_text)) = split_directional_query(normalized_query) else {
        return Ok(None);
    };
    let recent_rows = read_recent_codex_threads(target_path, exact_cwd, 50, None)?;
    if recent_rows.is_empty() {
        return Ok(None);
    }

    let anchor = if let Some(index) = parse_ordinal_index(&anchor_text) {
        recent_rows.get(index).cloned()
    } else if anchor_text.is_empty() || looks_like_latest_query(&anchor_text) {
        recent_rows.first().cloned()
    } else if let Some(session_hint) = extract_codex_session_hint(&anchor_text) {
        read_codex_threads_by_session_hint(&session_hint, 1)?
            .into_iter()
            .next()
    } else {
        search_codex_threads_for_find(Some(target_path), exact_cwd, &anchor_text, 1)?
            .into_iter()
            .next()
    };

    let Some(anchor) = anchor else {
        return Ok(None);
    };
    let Some(anchor_index) = recent_rows.iter().position(|row| row.id == anchor.id) else {
        return Ok(None);
    };
    let selected = if direction == "after" {
        recent_rows.get(anchor_index + 1).cloned()
    } else {
        anchor_index
            .checked_sub(1)
            .and_then(|index| recent_rows.get(index).cloned())
    };

    Ok(selected.map(|row| {
        (
            row,
            format!("{} session relative to `{}`", direction, anchor_text.trim()),
        )
    }))
}

fn split_directional_query(query: &str) -> Option<(String, String)> {
    for direction in ["after", "before"] {
        if let Some(index) = find_word_boundary(query, direction) {
            let anchor = query[index + direction.len()..].trim().to_string();
            return Some((direction.to_string(), anchor));
        }
    }
    None
}

fn find_word_boundary(text: &str, needle: &str) -> Option<usize> {
    let haystack = text.as_bytes();
    let needle_bytes = needle.as_bytes();
    let last = haystack.len().checked_sub(needle_bytes.len())?;
    for start in 0..=last {
        if &haystack[start..start + needle_bytes.len()] != needle_bytes {
            continue;
        }
        let before_ok = start == 0 || !haystack[start - 1].is_ascii_alphanumeric();
        let after_index = start + needle_bytes.len();
        let after_ok =
            after_index >= haystack.len() || !haystack[after_index].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return Some(start);
        }
    }
    None
}

fn parse_ordinal_index(query: &str) -> Option<usize> {
    let filtered = strip_codex_control_words(query);
    if filtered.len() == 1 {
        if let Ok(value) = filtered[0].parse::<usize>() {
            if value > 0 {
                return Some(value - 1);
            }
        }
        let ordinal = match filtered[0].as_str() {
            "1st" | "first" | "one" => Some(0),
            "2nd" | "second" | "two" => Some(1),
            "3rd" | "third" | "three" => Some(2),
            "4th" | "fourth" | "four" => Some(3),
            "5th" | "fifth" | "five" => Some(4),
            "6th" | "sixth" | "six" => Some(5),
            "7th" | "seventh" | "seven" => Some(6),
            "8th" | "eighth" | "eight" => Some(7),
            "9th" | "ninth" | "nine" => Some(8),
            "10th" | "tenth" | "ten" => Some(9),
            _ => None,
        };
        if ordinal.is_some() {
            return ordinal;
        }
    }
    None
}

fn looks_like_latest_query(query: &str) -> bool {
    let filtered = strip_codex_control_words(query);
    filtered.is_empty()
        && (query.contains("most recent")
            || query.contains("latest")
            || query.contains("newest")
            || query.contains("last"))
}

fn strip_codex_control_words(query: &str) -> Vec<String> {
    query
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(|part| part.to_ascii_lowercase())
        .filter(|part| {
            !matches!(
                part.as_str(),
                "connect"
                    | "open"
                    | "resume"
                    | "continue"
                    | "session"
                    | "sessions"
                    | "conversation"
                    | "conversations"
                    | "convo"
                    | "convos"
                    | "after"
                    | "before"
                    | "most"
                    | "recent"
                    | "latest"
                    | "newest"
                    | "last"
                    | "active"
                    | "the"
                    | "a"
                    | "an"
                    | "to"
                    | "from"
                    | "for"
                    | "please"
            )
        })
        .collect()
}

fn build_codex_recovery_plan(
    target_path: &Path,
    exact_cwd: bool,
    query_text: &str,
    runtime_skills_enabled: bool,
    prompt_context_budget_chars: usize,
    max_resolved_references: usize,
) -> Result<CodexOpenPlan> {
    let rows = read_recent_codex_threads(target_path, exact_cwd, 3, Some(query_text))?;
    let output = build_recover_output(target_path, exact_cwd, Some(query_text.to_string()), rows);
    let launch_path = output
        .candidates
        .first()
        .map(|value| value.cwd.clone())
        .unwrap_or_else(|| target_path.display().to_string());

    if output.candidates.is_empty() {
        bail!("{}", output.summary);
    }

    let recovery_prompt = build_recovery_prompt(query_text, &output, prompt_context_budget_chars);
    let codex_cfg = load_codex_config_for_path(target_path);
    let runtime = codex_runtime::prepare_runtime_activation(
        target_path,
        query_text,
        runtime_skills_enabled,
        &codex_cfg,
    )?;
    let prompt = runtime
        .as_ref()
        .map(|value| value.inject_into_prompt(&recovery_prompt))
        .or(Some(recovery_prompt));
    Ok(finalize_codex_open_plan(CodexOpenPlan {
        action: "recover-new".to_string(),
        route: "recover-new".to_string(),
        reason: "explicit recovery prompt".to_string(),
        target_path: target_path.display().to_string(),
        launch_path,
        query: Some(query_text.to_string()),
        session_id: None,
        prompt,
        references: Vec::new(),
        runtime_state_path: runtime
            .as_ref()
            .map(|value| value.state_path.display().to_string()),
        runtime_skills: runtime_skill_names(runtime.as_ref()),
        prompt_context_budget_chars,
        max_resolved_references,
        prompt_chars: 0,
        injected_context_chars: 0,
    }))
}

fn build_recovery_prompt(
    query_text: &str,
    output: &CodexRecoverOutput,
    max_chars: usize,
) -> String {
    let mut lines = vec!["Recovered recent Codex context:".to_string()];
    for candidate in output.candidates.iter().take(2) {
        let preview = candidate
            .first_user_message
            .as_deref()
            .or(candidate.title.as_deref())
            .map(truncate_recover_text)
            .unwrap_or_else(|| "(no stored prompt text)".to_string());
        lines.push(format!(
            "- {} | {} | {} | {}",
            truncate_recover_id(&candidate.id),
            candidate.updated_at,
            candidate.cwd,
            preview
        ));
    }
    lines.push(String::new());
    lines.push("User request:".to_string());
    lines.push(query_text.trim().to_string());
    compact_codex_context_block(&lines.join("\n"), 10, max_chars)
}

fn build_codex_open_no_match_message(
    target_path: &Path,
    exact_cwd: bool,
    query_text: &str,
) -> Result<String> {
    let output = build_recover_output(
        target_path,
        exact_cwd,
        Some(query_text.to_string()),
        read_recent_codex_threads(target_path, exact_cwd, 5, None)?,
    );
    Ok(format!(
        "No Codex session matched {:?}.\n{}",
        query_text, output.summary
    ))
}

fn resolve_codex_references(
    target_path: &Path,
    query_text: &str,
    resolvers: &[config::CodexReferenceResolverConfig],
) -> Result<Vec<CodexResolvedReference>> {
    let candidates = extract_reference_candidates(query_text);
    let mut matches = Vec::new();

    for resolver in resolvers {
        if let Some(reference) =
            resolve_external_reference(target_path, query_text, &candidates, resolver)?
        {
            matches.push(reference);
        }
        if matches.len() >= 2 {
            return Ok(matches);
        }
    }

    let remaining = 2usize.saturating_sub(matches.len());
    if remaining > 0 {
        for reference in
            resolve_builtin_repo_references(target_path, query_text, &candidates, remaining)?
        {
            if !matches
                .iter()
                .any(|value| value.matched == reference.matched)
            {
                matches.push(reference);
            }
            if matches.len() >= 2 {
                return Ok(matches);
            }
        }
    }

    if let Some(reference) = resolve_builtin_linear_reference(query_text, &candidates)
        && !matches
            .iter()
            .any(|value| value.matched == reference.matched)
    {
        matches.push(reference);
    }

    if let Some(reference) = resolve_builtin_url_reference(target_path, &candidates, &matches)
        && !matches
            .iter()
            .any(|value| value.matched == reference.matched)
    {
        matches.push(reference);
    }

    Ok(matches)
}

fn resolve_builtin_repo_references(
    target_path: &Path,
    query_text: &str,
    candidates: &[String],
    limit: usize,
) -> Result<Vec<CodexResolvedReference>> {
    let references =
        repo_capsule::resolve_reference_candidates(target_path, query_text, candidates, limit)?;
    Ok(references
        .into_iter()
        .map(|reference| {
            let memory_context =
                codex_memory::query_repo_facts(Path::new(&reference.repo_root), query_text, 4)
                    .ok()
                    .flatten()
                    .map(|result| compact_codex_context_block(&result.rendered, 8, 700));
            let output = if let Some(memory) = memory_context {
                format!("{}\n{}", reference.output, memory)
            } else {
                reference.output
            };
            CodexResolvedReference {
                name: "repo".to_string(),
                source: "repo".to_string(),
                matched: reference.matched,
                command: None,
                output,
            }
        })
        .collect())
}

fn resolve_external_reference(
    target_path: &Path,
    query_text: &str,
    candidates: &[String],
    resolver: &config::CodexReferenceResolverConfig,
) -> Result<Option<CodexResolvedReference>> {
    for candidate in candidates {
        if !resolver
            .matches
            .iter()
            .any(|pattern| wildcard_match(pattern, candidate))
        {
            continue;
        }

        let command_text = render_reference_resolver_command(
            &resolver.command,
            candidate,
            query_text,
            target_path,
        );
        let args = shell_words::split(&command_text)
            .with_context(|| format!("invalid resolver command: {}", command_text))?;
        let Some((program, rest)) = args.split_first() else {
            bail!("empty resolver command for {}", resolver.name);
        };
        let output = Command::new(program)
            .args(rest)
            .current_dir(target_path)
            .output()
            .with_context(|| format!("failed to run resolver {}", resolver.name))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            bail!(
                "resolver {} failed for {}: {}",
                resolver.name,
                candidate,
                if stderr.is_empty() {
                    format!("exit status {}", output.status)
                } else {
                    stderr
                }
            );
        }
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if stdout.is_empty() {
            bail!(
                "resolver {} returned empty output for {}",
                resolver.name,
                candidate
            );
        }

        return Ok(Some(CodexResolvedReference {
            name: resolver
                .inject_as
                .clone()
                .unwrap_or_else(|| resolver.name.clone()),
            source: "resolver".to_string(),
            matched: candidate.clone(),
            command: Some(command_text),
            output: compact_codex_context_block(&stdout, 12, 1200),
        }));
    }

    Ok(None)
}

fn render_reference_resolver_command(
    template: &str,
    matched: &str,
    query_text: &str,
    target_path: &Path,
) -> String {
    template
        .replace("{{ref}}", &shell_words::quote(matched))
        .replace("{{query}}", &shell_words::quote(query_text))
        .replace(
            "{{cwd}}",
            &shell_words::quote(&target_path.display().to_string()),
        )
}

fn resolve_builtin_linear_reference(
    query_text: &str,
    candidates: &[String],
) -> Option<CodexResolvedReference> {
    for candidate in candidates {
        if let Some(reference) = parse_linear_url_reference(candidate) {
            return Some(CodexResolvedReference {
                name: "linear".to_string(),
                source: "builtin".to_string(),
                matched: candidate.clone(),
                command: None,
                output: render_linear_url_reference(&reference),
            });
        }
    }
    let _ = query_text;
    None
}

fn resolve_builtin_url_reference(
    target_path: &Path,
    candidates: &[String],
    existing: &[CodexResolvedReference],
) -> Option<CodexResolvedReference> {
    for candidate in candidates {
        if !looks_like_http_url(candidate) {
            continue;
        }
        if existing.iter().any(|value| value.matched == *candidate) {
            continue;
        }
        let Ok(output) = url_inspect::inspect_compact(candidate, target_path) else {
            continue;
        };
        return Some(CodexResolvedReference {
            name: "url".to_string(),
            source: "builtin".to_string(),
            matched: candidate.clone(),
            command: None,
            output: compact_codex_context_block(&output, 10, 900),
        });
    }
    None
}

fn extract_reference_candidates(query_text: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut candidates = Vec::new();

    let trimmed = trim_reference_token(query_text);
    if !trimmed.is_empty() && seen.insert(trimmed.to_string()) {
        candidates.push(trimmed.to_string());
    }

    for token in query_text.split_whitespace() {
        let trimmed = trim_reference_token(token);
        if trimmed.is_empty() {
            continue;
        }
        if seen.insert(trimmed.to_string()) {
            candidates.push(trimmed.to_string());
        }
    }

    candidates
}

fn trim_reference_token(value: &str) -> &str {
    value.trim_matches(|ch: char| {
        matches!(
            ch,
            '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | ',' | '.' | ';'
        )
    })
}

fn looks_like_http_url(value: &str) -> bool {
    let trimmed = trim_reference_token(value);
    trimmed.starts_with("https://") || trimmed.starts_with("http://")
}

fn wildcard_match(pattern: &str, candidate: &str) -> bool {
    let pattern = pattern.to_ascii_lowercase();
    let candidate = candidate.to_ascii_lowercase();
    if !pattern.contains('*') {
        return pattern == candidate;
    }

    let mut remainder = candidate.as_str();
    let mut anchored = true;
    for segment in pattern.split('*') {
        if segment.is_empty() {
            anchored = false;
            continue;
        }
        if anchored {
            let Some(stripped) = remainder.strip_prefix(segment) else {
                return false;
            };
            remainder = stripped;
        } else if let Some(index) = remainder.find(segment) {
            remainder = &remainder[index + segment.len()..];
        } else {
            return false;
        }
        anchored = false;
    }

    pattern.ends_with('*') || remainder.is_empty()
}

fn parse_linear_url_reference(value: &str) -> Option<LinearUrlReference> {
    let trimmed = trim_reference_token(value);
    let relative = trimmed.strip_prefix("https://linear.app/")?;
    let relative = relative
        .split(['?', '#'])
        .next()
        .unwrap_or(relative)
        .trim_matches('/');
    let segments: Vec<_> = relative
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.len() < 3 {
        return None;
    }

    let workspace_slug = segments[0].to_string();
    match segments[1] {
        "issue" => Some(LinearUrlReference {
            url: trimmed.to_string(),
            workspace_slug,
            resource_kind: LinearUrlKind::Issue,
            resource_value: segments[2].to_string(),
            view: None,
            title_hint: segments[2].to_string(),
        }),
        "project" => {
            let project_slug = segments[2].to_string();
            let title_hint = humanize_linear_slug(&project_slug);
            Some(LinearUrlReference {
                url: trimmed.to_string(),
                workspace_slug,
                resource_kind: LinearUrlKind::Project,
                resource_value: project_slug,
                view: segments.get(3).map(|value| (*value).to_string()),
                title_hint,
            })
        }
        _ => None,
    }
}

fn humanize_linear_slug(value: &str) -> String {
    let mut parts: Vec<_> = value.split('-').filter(|part| !part.is_empty()).collect();
    if parts
        .last()
        .is_some_and(|part| part.len() >= 8 && part.chars().all(|ch| ch.is_ascii_hexdigit()))
    {
        parts.pop();
    }
    if parts.is_empty() {
        value.to_string()
    } else {
        parts.join(" ")
    }
}

fn render_linear_url_reference(reference: &LinearUrlReference) -> String {
    let mut lines = vec![format!("- Linear URL: {}", reference.url)];
    lines.push(format!("- Linear workspace: {}", reference.workspace_slug));
    match reference.resource_kind {
        LinearUrlKind::Issue => {
            lines.push(format!("- Linear issue: {}", reference.resource_value));
        }
        LinearUrlKind::Project => {
            lines.push(format!(
                "- Linear project slug: {}",
                reference.resource_value
            ));
            lines.push(format!("- Linear project hint: {}", reference.title_hint));
            if let Some(view) = reference.view.as_deref() {
                lines.push(format!("- Linear project view: {}", view));
            }
        }
    }
    compact_codex_context_block(&lines.join("\n"), 8, 700)
}

fn build_codex_prompt(
    query_text: &str,
    references: &[CodexResolvedReference],
    max_resolved_references: usize,
    max_chars: usize,
) -> Option<String> {
    let trimmed_query = query_text.trim();
    if references.is_empty() {
        if trimmed_query.is_empty() {
            return None;
        }
        return Some(trimmed_query.to_string());
    }

    let mut lines = vec!["Resolved context:".to_string()];
    let selected: Vec<_> = references.iter().take(max_resolved_references).collect();
    for (index, reference) in selected.iter().enumerate() {
        let current_chars = lines.iter().map(|line| line.chars().count()).sum::<usize>();
        let query_reserve = if trimmed_query.is_empty() {
            0
        } else {
            trimmed_query.chars().count() + "User request:".chars().count() + 8
        };
        let remaining = max_chars.saturating_sub(current_chars + query_reserve);
        if remaining < 80 {
            break;
        }
        let refs_left = selected.len().saturating_sub(index).max(1);
        let per_ref_budget = (remaining / refs_left).clamp(120, max_chars.max(120));
        lines.push(format!("[{}]", reference.name));
        lines.push(compact_codex_context_block(
            &reference.output,
            8,
            per_ref_budget,
        ));
    }
    if !trimmed_query.is_empty() {
        lines.push(String::new());
        lines.push("User request:".to_string());
        lines.push(trimmed_query.to_string());
    }
    let (max_lines, max_chars) = if has_session_reference(references) {
        (24, max_chars)
    } else {
        (14, max_chars)
    };
    Some(compact_codex_context_block(
        &lines.join("\n"),
        max_lines,
        max_chars,
    ))
}

fn build_codex_prompt_with_runtime(
    query_text: &str,
    references: &[CodexResolvedReference],
    runtime: Option<&codex_runtime::CodexRuntimeActivation>,
    max_resolved_references: usize,
    max_chars: usize,
) -> Option<String> {
    let prompt = build_codex_prompt(query_text, references, max_resolved_references, max_chars)?;
    Some(
        runtime
            .map(|value| value.inject_into_prompt(&prompt))
            .unwrap_or(prompt),
    )
}

fn has_session_reference(references: &[CodexResolvedReference]) -> bool {
    references
        .iter()
        .any(|reference| reference.source == "session")
}

fn effective_max_resolved_references(codex_cfg: &config::CodexConfig) -> usize {
    codex_cfg.max_resolved_references.unwrap_or(2).clamp(1, 6)
}

fn effective_prompt_context_budget_chars(
    codex_cfg: &config::CodexConfig,
    has_session_reference: bool,
) -> usize {
    codex_cfg
        .prompt_context_budget_chars
        .unwrap_or(if has_session_reference { 2200 } else { 1200 })
        .clamp(300, 12_000)
}

fn finalize_codex_open_plan(mut plan: CodexOpenPlan) -> CodexOpenPlan {
    plan.prompt_chars = plan
        .prompt
        .as_deref()
        .map(|value| value.chars().count())
        .unwrap_or(0);
    let query_chars = plan
        .query
        .as_deref()
        .map(str::trim)
        .map(|value| value.chars().count())
        .unwrap_or(0);
    plan.injected_context_chars = plan.prompt_chars.saturating_sub(query_chars);
    plan
}

fn runtime_skill_names(runtime: Option<&codex_runtime::CodexRuntimeActivation>) -> Vec<String> {
    runtime
        .map(|value| {
            value
                .skills
                .iter()
                .map(|skill| {
                    skill
                        .original_name
                        .clone()
                        .unwrap_or_else(|| skill.name.clone())
                })
                .collect()
        })
        .unwrap_or_default()
}

fn compact_codex_context_block(value: &str, max_lines: usize, max_chars: usize) -> String {
    let mut lines = Vec::new();
    let mut chars = 0usize;
    for line in value
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
    {
        let line_chars = line.chars().count();
        if lines.len() >= max_lines || chars + line_chars > max_chars {
            break;
        }
        lines.push(line.to_string());
        chars += line_chars;
    }
    let mut out = lines.join("\n");
    if out.chars().count() > max_chars {
        out = out
            .chars()
            .take(max_chars.saturating_sub(1))
            .collect::<String>()
            + "…";
    }
    out
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
        if q == "cursor" || q == "u" {
            return copy_last_session(Provider::Cursor, None);
        }
    }

    let index = load_index()?;
    let sessions = read_sessions_for_project(provider)?;

    if sessions.is_empty() && session.is_none() {
        let provider_name = match provider {
            Provider::Claude => "Claude",
            Provider::Codex => "Codex",
            Provider::Cursor => "Cursor",
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
        resolve_session_selection(query, &sessions, &index, provider)?
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
                    Provider::Cursor => "cursor | ",
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

fn copy_session_history_to_clipboard(session_id: &str, provider: Provider) -> Result<usize> {
    let history = read_session_history(session_id, provider)?;
    copy_to_clipboard(&history)?;
    Ok(history.lines().count())
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
            Provider::Cursor => "Cursor",
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

    // Search Cursor sessions
    if provider == Provider::Cursor || provider == Provider::All {
        let projects_dir = get_cursor_projects_dir();
        if projects_dir.exists() {
            if let Ok(entries) = fs::read_dir(&projects_dir) {
                for entry in entries.flatten() {
                    let project_dir = entry.path();
                    if !project_dir.is_dir() {
                        continue;
                    }
                    for file_path in collect_cursor_project_session_files(&project_dir) {
                        if let Ok(content) = fs::read_to_string(&file_path) {
                            if content.to_lowercase().contains(&query_lower) {
                                let session_id =
                                    file_path.file_stem().and_then(|s| s.to_str()).unwrap_or("");

                                let history = read_session_history(session_id, Provider::Cursor)?;
                                copy_to_clipboard(&history)?;

                                let line_count = history.lines().count();
                                let id_short = &session_id[..8.min(session_id.len())];
                                let project_name = project_dir
                                    .file_name()
                                    .and_then(|s| s.to_str())
                                    .and_then(decode_cursor_project_path)
                                    .and_then(|path| {
                                        path.file_name()
                                            .and_then(|name| name.to_str())
                                            .map(str::to_string)
                                    })
                                    .unwrap_or_else(|| {
                                        project_dir
                                            .file_name()
                                            .and_then(|s| s.to_str())
                                            .unwrap_or("unknown")
                                            .to_string()
                                    });

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

fn append_history_message(
    history: &mut String,
    last_entry: &mut Option<(String, String)>,
    role: &str,
    content: &str,
) {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return;
    }

    let role_label = match role {
        "user" => "Human",
        "assistant" => "Assistant",
        _ => return,
    };

    let content_key = trimmed.to_string();
    if let Some((last_role, last_content)) = last_entry.as_ref() {
        if last_role == role_label && last_content == &content_key {
            return;
        }
    }

    history.push_str(role_label);
    history.push_str(": ");
    history.push_str(trimmed);
    history.push_str("\n\n");
    *last_entry = Some((role_label.to_string(), content_key));
}

/// Read full session history from JSONL file and format as conversation.
fn read_session_history(session_id: &str, provider: Provider) -> Result<String> {
    let session_file = if provider == Provider::Codex {
        // Codex stores sessions in ~/.codex/sessions/ with different structure
        find_codex_session_file(session_id)
            .ok_or_else(|| anyhow::anyhow!("Codex session file not found: {}", session_id))?
    } else if provider == Provider::Cursor {
        find_cursor_session_file(session_id)
            .ok_or_else(|| anyhow::anyhow!("Cursor session file not found: {}", session_id))?
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

    let mut history = String::new();
    let mut last_entry: Option<(String, String)> = None;

    for_each_nonempty_jsonl_line(&session_file, |line| {
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
            return;
        };

        // Cursor format (top-level role + nested message.content)
        if let Some(role) = entry
            .get("role")
            .and_then(|r| r.as_str())
            .map(normalize_cursor_role)
        {
            let content_text = extract_content_text(
                entry
                    .get("message")
                    .and_then(|message| message.get("content")),
            );
            if let Some(cleaned) = normalize_session_message(role, &content_text) {
                append_history_message(&mut history, &mut last_entry, role, &cleaned);
            }
            return;
        }

        // Try Claude format first (entry.message.role + entry.message.content)
        if let Some(msg) = entry.get("message") {
            let role = msg
                .get("role")
                .and_then(|r| r.as_str())
                .unwrap_or("unknown");
            let content_text = extract_content_text(msg.get("content"));
            if let Some(cleaned) = normalize_session_message(role, &content_text) {
                append_history_message(&mut history, &mut last_entry, role, &cleaned);
            }
            return;
        }

        // Try Codex format (type: response_item, payload.type: message)
        if entry.get("type").and_then(|t| t.as_str()) == Some("response_item") {
            if let Some(payload) = entry.get("payload") {
                if payload.get("type").and_then(|t| t.as_str()) == Some("message") {
                    let role = payload
                        .get("role")
                        .and_then(|r| r.as_str())
                        .unwrap_or("unknown");
                    let content_text = payload
                        .get("content")
                        .and_then(extract_codex_content_text)
                        .unwrap_or_default();
                    if let Some(cleaned) = normalize_session_message(role, &content_text) {
                        append_history_message(&mut history, &mut last_entry, role, &cleaned);
                    }
                }
            }
        }
    })?;

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

    if sessions.is_empty() && session.is_none() {
        let provider_name = match provider {
            Provider::Claude => "Claude",
            Provider::Codex => "Codex",
            Provider::Cursor => "Cursor",
            Provider::All => "AI",
        };
        println!("No {} sessions found for this project.", provider_name);
        return Ok(());
    }

    // Find the session ID and provider
    let (session_id, session_provider) = if let Some(ref query) = session {
        resolve_session_selection(query, &sessions, &index, provider)?
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
                    Provider::Cursor => "cursor | ",
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

/// Print a cleaned session excerpt to stdout.
fn show_session(
    session: Option<String>,
    provider: Provider,
    count: usize,
    path: Option<String>,
    full: bool,
) -> Result<()> {
    auto_import_sessions()?;

    let mut session = session.filter(|value| value != "-");
    let mut path = path;

    if path.is_none() {
        if let Some(ref candidate) = session {
            let candidate_path = PathBuf::from(candidate);
            if candidate == "." || candidate == ".." || candidate_path.exists() {
                path = Some(candidate.clone());
                session = None;
            }
        }
    }

    let project_path = if let Some(ref p) = path {
        PathBuf::from(p)
    } else {
        std::env::current_dir()?
    };

    let index = load_index()?;
    let sessions = read_sessions_for_path(provider, &project_path)?;

    let (session_id, session_provider) = if let Some(ref query) = session {
        resolve_session_selection(query, &sessions, &index, provider)?
    } else {
        let latest = sessions.first().ok_or_else(|| {
            let provider_name = match provider {
                Provider::Claude => "Claude",
                Provider::Codex => "Codex",
                Provider::Cursor => "Cursor",
                Provider::All => "AI",
            };
            anyhow::anyhow!(
                "No {provider_name} sessions found for {}",
                project_path.display()
            )
        })?;
        (latest.session_id.clone(), latest.provider)
    };

    let output = if full {
        read_session_history(&session_id, session_provider)?
    } else {
        read_last_context(&session_id, session_provider, count.max(1), &project_path)?
    };

    print!("{}", output);
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
    if provider == Provider::Cursor {
        let session_file = find_cursor_session_file(session_id).ok_or_else(|| {
            anyhow::anyhow!("Session file not found for Cursor session {}", session_id)
        })?;
        return read_cursor_last_context(&session_file, count);
    }

    let path_str = project_path.to_string_lossy().to_string();
    let project_folder = path_to_project_name(&path_str);

    let projects_dir = match provider {
        Provider::Claude | Provider::All => get_claude_projects_dir(),
        Provider::Codex => get_codex_projects_dir(),
        Provider::Cursor => get_cursor_projects_dir(),
    };

    let session_file = projects_dir
        .join(&project_folder)
        .join(format!("{}.jsonl", session_id));

    if !session_file.exists() {
        bail!("Session file not found: {}", session_file.display());
    }

    // Collect only the trailing `count` exchanges to bound memory usage for large sessions.
    let keep = count.max(1);
    let mut exchanges: VecDeque<(String, String)> = VecDeque::with_capacity(keep.min(64));
    let mut current_user: Option<String> = None;

    for_each_nonempty_jsonl_line(&session_file, |line| {
        if let Ok(entry) = crate::json_parse::parse_json_line::<JsonlEntry>(line) {
            if let Some(ref msg) = entry.message {
                let role = msg.role.as_deref().unwrap_or("unknown");

                let Some(content_text) = msg.content.as_ref().and_then(extract_message_text) else {
                    return;
                };
                let Some(clean_text) = normalize_session_message(role, &content_text) else {
                    return;
                };

                match role {
                    "user" => {
                        current_user = Some(clean_text);
                    }
                    "assistant" => {
                        if let Some(user_msg) = current_user.take() {
                            if exchanges.len() == keep {
                                exchanges.pop_front();
                            }
                            exchanges.push_back((user_msg, clean_text));
                        }
                    }
                    _ => {}
                }
            }
        }
    })?;

    if exchanges.is_empty() {
        bail!("No exchanges found in session");
    }

    // Format the context
    let mut context = String::new();

    for (user_msg, assistant_msg) in exchanges {
        context.push_str("Human: ");
        context.push_str(&user_msg);
        context.push_str("\n\n");
        context.push_str("Assistant: ");
        context.push_str(&assistant_msg);
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
    if std::env::var("FLOW_NO_CLIPBOARD").is_ok() {
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

        let status = child.wait()?;
        if !status.success() {
            bail!("pbcopy exited with status {}", status);
        }
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

        let status = child.wait()?;
        if !status.success() {
            bail!("clipboard command exited with status {}", status);
        }
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
        let text = extract_codex_content_text(payload.get("content")?)?;
        return normalize_session_message("user", &text);
    }

    if entry_type == Some("event_msg") {
        let payload = entry.payload.as_ref()?;
        let payload_type = payload.get("type").and_then(|v| v.as_str());
        if payload_type == Some("user_message") {
            return payload
                .get("message")
                .and_then(|v| v.as_str())
                .and_then(|s| normalize_session_message("user", s));
        }
    }

    if entry_type == Some("message") && entry.role.as_deref() == Some("user") {
        if let Some(content) = entry.content.as_ref() {
            let text = extract_codex_content_text(content)?;
            return normalize_session_message("user", &text);
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
        Provider::Cursor => "cursor",
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

    let client = crate::http_client::blocking_with_timeout(Duration::from_secs(30))
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

    let client = crate::http_client::blocking_with_timeout(Duration::from_secs(30))
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
    Ok(sessions
        .first()
        .map(|session| format_session_ref(session, true)))
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
        Provider::Cursor => "cursor",
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
fn resume_session(session: Option<String>, path: Option<String>, provider: Provider) -> Result<()> {
    let index = load_index()?;
    let sessions = read_sessions_for_target(provider, path.as_deref())?;
    let explicit_session_requested = session.is_some();
    let default_provider = if provider == Provider::All {
        Provider::Claude
    } else {
        provider
    };

    let (session_id, session_provider) = match session {
        Some(s) => {
            // Check if it's a saved name
            if let Some(saved) = index.sessions.get(&s) {
                // Find the provider for this session
                let prov = sessions
                    .iter()
                    .find(|sess| sess.session_id == saved.id)
                    .map(|sess| sess.provider)
                    .unwrap_or(default_provider);
                (saved.id.clone(), prov)
            } else if s.len() >= 8 {
                // Might be a session ID or prefix
                if let Some(sess) = sessions.iter().find(|sess| sess.session_id.starts_with(&s)) {
                    (sess.session_id.clone(), sess.provider)
                } else {
                    // Assume it's a full ID for requested provider.
                    (s, default_provider)
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

    let has_tty = io::stdin().is_terminal() && io::stdout().is_terminal();
    if !has_tty {
        match session_provider {
            Provider::Codex => {
                bail!(
                    "codex resume requires an interactive terminal (TTY); run this in a terminal tab (e.g. Zed/Ghostty)"
                );
            }
            Provider::Claude => {
                bail!(
                    "claude resume requires an interactive terminal (TTY); run this in a terminal tab (e.g. Zed/Ghostty)"
                );
            }
            Provider::Cursor => {
                bail!(
                    "cursor transcripts are readable only; use `f cursor list`, `f cursor copy`, or `f cursor context`"
                );
            }
            Provider::All => {}
        }
    }

    if session_provider == Provider::Cursor {
        bail!(
            "cursor transcripts are readable only; use `f cursor list`, `f cursor copy`, or `f cursor context`"
        );
    }

    println!(
        "Resuming session {}...",
        &session_id[..8.min(session_id.len())]
    );
    let launched = launch_session(&session_id, session_provider)?;
    if launched {
        return Ok(());
    }

    // Claude occasionally cannot reopen older local transcript IDs.
    // For explicit IDs, do not auto-fallback to --continue because that can
    // open a different conversation and hide the failure.
    if session_provider == Provider::Claude {
        eprintln!(
            "Claude could not resume session {}.",
            &session_id[..8.min(session_id.len())]
        );
        if explicit_session_requested {
            bail!(
                "failed to resume exact claude session {}. refusing fallback to `claude --continue` to avoid opening the wrong session",
                session_id
            );
        }
        if !has_tty {
            bail!(
                "failed to resume claude session {} (non-interactive shell; fallback continue unavailable)",
                session_id
            );
        }
        eprintln!("Falling back to `claude --continue` in this directory...");
        let continued = launch_claude_continue()?;
        if continued {
            return Ok(());
        }
        bail!(
            "failed to resume claude session {} and fallback `claude --continue` also failed",
            session_id
        );
    }

    bail!(
        "failed to resume {} session {}",
        provider_name(session_provider),
        session_id
    );
}

/// Save a session with a name.
fn save_session(name: &str, id: Option<String>) -> Result<()> {
    let session_id = match id {
        Some(id) => id,
        None => get_most_recent_session_id()?
            .ok_or_else(|| anyhow::anyhow!("No sessions found. Start an AI session first."))?,
    };

    let mut index = load_index()?;

    // Check if name already exists
    if index.sessions.contains_key(name) {
        bail!(
            "Session name '{}' already exists. Use a different name or remove it first.",
            name
        );
    }

    let session_provider = read_sessions_for_project(Provider::All)?
        .into_iter()
        .find(|session| session.session_id == session_id)
        .map(|session| session.provider)
        .unwrap_or(Provider::Claude);

    let saved = SavedSession {
        id: session_id.clone(),
        provider: provider_name(session_provider).to_string(),
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
            Provider::Cursor => "cursor",
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
            Provider::Cursor => "cursor",
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
        "cursor" => Provider::Cursor,
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
                Provider::Cursor => "cursor",
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
                Provider::Cursor => "cursor",
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

    // Scan Cursor agent transcripts.
    if provider == Provider::Cursor || provider == Provider::All {
        let cursor_dir = get_cursor_projects_dir();
        if cursor_dir.exists() {
            if let Ok(entries) = fs::read_dir(&cursor_dir) {
                for entry in entries.flatten() {
                    let project_dir = entry.path();
                    if !project_dir.is_dir() {
                        continue;
                    }

                    let Some(project_key) = project_dir.file_name().and_then(|name| name.to_str())
                    else {
                        continue;
                    };
                    let Some(project_path) = decode_cursor_project_path(project_key) else {
                        continue;
                    };
                    let project_name = project_path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or(project_key)
                        .to_string();

                    for file_path in collect_cursor_project_session_files(&project_dir) {
                        let filename = file_path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                        let Some(session) = parse_cursor_session_file(&file_path, filename) else {
                            continue;
                        };

                        all_sessions.push(CrossProjectSession {
                            session_id: session.session_id,
                            provider: Provider::Cursor,
                            project_path: project_path.clone(),
                            project_name: project_name.clone(),
                            timestamp: session.timestamp,
                            first_message: session.first_message,
                            error_summary: session.error_summary,
                            session_path: Some(file_path),
                        });
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
    if session.provider == Provider::Cursor {
        let session_file = session
            .session_path
            .clone()
            .or_else(|| find_cursor_session_file(&session.session_id));
        let Some(session_file) = session_file else {
            bail!(
                "Session file not found for Cursor session {}",
                session.session_id
            );
        };
        return read_cursor_cross_project_context(session, &session_file, since_ts, max_count);
    }

    let projects_dir = match session.provider {
        Provider::Claude | Provider::All => get_claude_projects_dir(),
        Provider::Codex => get_codex_projects_dir(),
        Provider::Cursor => get_cursor_projects_dir(),
    };

    let project_folder = session.project_path.to_string_lossy().replace('/', "-");
    let session_file = projects_dir
        .join(&project_folder)
        .join(format!("{}.jsonl", session.session_id));

    if !session_file.exists() {
        bail!("Session file not found: {}", session_file.display());
    }

    // Collect exchanges after the checkpoint timestamp
    let mut exchanges: Vec<(String, String, String)> = Vec::new();
    let mut current_user: Option<String> = None;
    let mut current_ts: Option<String> = None;
    let mut last_ts: Option<String> = None;

    for_each_nonempty_jsonl_line(&session_file, |line| {
        if let Ok(entry) = crate::json_parse::parse_json_line::<JsonlEntry>(line) {
            let entry_ts = entry.timestamp.clone();

            // Skip entries before checkpoint
            if let (Some(since), Some(ts)) = (since_ts, &entry_ts) {
                if ts.as_str() <= since {
                    return;
                }
            }

            if let Some(ref msg) = entry.message {
                let role = msg.role.as_deref().unwrap_or("unknown");

                let Some(content_text) = msg.content.as_ref().and_then(extract_message_text) else {
                    return;
                };
                let Some(clean_text) = normalize_session_message(role, &content_text) else {
                    return;
                };

                match role {
                    "user" => {
                        current_user = Some(clean_text);
                        current_ts = entry_ts.clone();
                    }
                    "assistant" => {
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
    })?;

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
            Provider::Cursor => "Cursor",
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

    let mut stack = vec![root];
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
                let filename = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
                if filename.contains(session_id) {
                    return Some(path);
                }
            }
        }
    }

    None
}

fn find_cursor_session_file(session_id: &str) -> Option<PathBuf> {
    let root = get_cursor_projects_dir();
    if !root.exists() {
        return None;
    }

    let entries = fs::read_dir(&root).ok()?;
    for entry in entries.flatten() {
        let project_dir = entry.path();
        if !project_dir.is_dir() {
            continue;
        }

        for file_path in collect_cursor_project_session_files(&project_dir) {
            let filename = file_path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if filename.contains(session_id) {
                return Some(file_path);
            }
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
    let (exchanges, last_ts) = read_codex_exchanges(session_file, since_ts, None)?;

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
            Provider::Cursor => "Cursor",
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

fn read_cursor_cross_project_context(
    session: &CrossProjectSession,
    session_file: &PathBuf,
    since_ts: Option<&str>,
    max_count: Option<usize>,
) -> Result<(String, Option<String>)> {
    let (exchanges, last_ts) = read_cursor_exchanges(session_file, since_ts, None)?;

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
            Provider::Cursor => "Cursor",
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
    if provider == Provider::Cursor {
        let session_file = find_cursor_session_file(session_id);
        let Some(session_file) = session_file else {
            return Ok(None);
        };
        return get_cursor_last_timestamp(&session_file);
    }

    let projects_dir = match provider {
        Provider::Claude | Provider::All => get_claude_projects_dir(),
        Provider::Codex => get_codex_projects_dir(),
        Provider::Cursor => get_cursor_projects_dir(),
    };

    let project_folder = project_path.to_string_lossy().replace('/', "-");
    let session_file = projects_dir
        .join(&project_folder)
        .join(format!("{}.jsonl", session_id));

    if !session_file.exists() {
        return Ok(None);
    }

    let mut last_ts: Option<String> = None;
    for_each_nonempty_jsonl_line(&session_file, |line| {
        if let Ok(entry) = crate::json_parse::parse_json_line::<JsonlEntry>(line) {
            if let Some(ts) = entry.timestamp {
                last_ts = Some(ts);
            }
        }
    })?;

    Ok(last_ts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn decode_cursor_project_path_handles_hyphenated_components() {
        let root = tempfile::Builder::new()
            .prefix("cursorproject")
            .tempdir_in("/tmp")
            .expect("tempdir");
        let repo_path = root
            .path()
            .join("review")
            .join("nikiv-designer-dev-deploy")
            .join("ide")
            .join("designer");
        fs::create_dir_all(&repo_path).expect("create repo path");

        let project_key = format!(
            "tmp-{}-review-nikiv-designer-dev-deploy-ide-designer",
            root.path()
                .file_name()
                .and_then(|name| name.to_str())
                .expect("tempdir name")
        );

        let decoded = decode_cursor_project_path(&project_key).expect("decoded path");
        assert_eq!(decoded, repo_path);
    }

    #[test]
    fn parse_cursor_session_file_extracts_messages() {
        let root = tempdir().expect("tempdir");
        let session_file = root.path().join("cursor-session.jsonl");
        fs::write(
            &session_file,
            concat!(
                "{\"role\":\"user\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"hello cursor\"}]}}\n",
                "{\"role\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"world\"}]}}\n"
            ),
        )
        .expect("write session file");

        let session =
            parse_cursor_session_file(&session_file, "cursor-session").expect("parsed session");
        assert_eq!(session.session_id, "cursor-session");
        assert_eq!(session.provider, Provider::Cursor);
        assert_eq!(session.first_message.as_deref(), Some("hello cursor"));
        assert_eq!(session.last_message.as_deref(), Some("world"));
        assert!(session.timestamp.is_some());
        assert_eq!(session.last_message_at, session.timestamp);
    }

    #[test]
    fn normalize_session_message_strips_setup_scaffolding() {
        let workflow_text = concat!(
            "ai sidebar improvements\n\n",
            "Workflow context:\n",
            "- Repo: ~/code/example-project\n",
            "- Review branch: review/example-feature\n",
            "\nStart by checking:\n1. flow status\n"
        );
        assert_eq!(
            normalize_session_message("user", workflow_text).as_deref(),
            Some("ai sidebar improvements")
        );

        let agents_text = concat!(
            "# AGENTS.md instructions for /tmp/repo\n\n",
            "<INSTRUCTIONS>\n",
            "Do important things.\n",
            "</INSTRUCTIONS>"
        );
        assert_eq!(normalize_session_message("user", agents_text), None);

        let assistant_setup = "Using `example-dispatch`, then `example-workflow` because this is a stacked review workspace.";
        assert_eq!(
            normalize_session_message("assistant", assistant_setup),
            None
        );
    }

    #[test]
    fn normalize_codex_resolve_args_accepts_trailing_json_flag() {
        let (query, json_output) = normalize_codex_resolve_args(
            vec![
                "https://developers.cloudflare.com/changelog/post/2026-03-10-br-crawl-endpoint/"
                    .to_string(),
                "--json".to_string(),
            ],
            false,
        );

        assert!(json_output);
        assert_eq!(
            query,
            vec![
                "https://developers.cloudflare.com/changelog/post/2026-03-10-br-crawl-endpoint/"
                    .to_string()
            ]
        );
    }

    #[test]
    fn read_codex_first_user_message_since_prefers_first_post_launch_turn() {
        let root = tempdir().expect("tempdir");
        let session_file = root.path().join("codex.jsonl");
        fs::write(
            &session_file,
            concat!(
                "{\"type\":\"response_item\",\"timestamp\":\"2026-03-16T10:00:00Z\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"old prompt\"}]}}\n",
                "{\"type\":\"response_item\",\"timestamp\":\"2026-03-16T10:00:01Z\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"old answer\"}]}}\n",
                "{\"type\":\"response_item\",\"timestamp\":\"2026-03-16T10:05:00Z\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"new prompt after launch\"}]}}\n",
                "{\"type\":\"response_item\",\"timestamp\":\"2026-03-16T10:05:02Z\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"new answer\"}]}}\n"
            ),
        )
        .expect("write session file");

        let since_unix = parse_rfc3339_to_unix("2026-03-16T10:05:00Z").expect("parse timestamp");
        let first = read_codex_first_user_message_since(&session_file, since_unix)
            .expect("read")
            .expect("first post-launch prompt");
        assert_eq!(first.0, "new prompt after launch");
        assert_eq!(first.1, since_unix);
    }

    #[test]
    fn read_codex_first_user_message_since_skips_contextual_scaffolding() {
        let root = tempdir().expect("tempdir");
        let session_file = root.path().join("codex.jsonl");
        fs::write(
            &session_file,
            concat!(
                "{\"type\":\"response_item\",\"timestamp\":\"2026-03-16T10:05:00Z\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"# AGENTS.md instructions for /tmp\\n\\n<INSTRUCTIONS>\\nbody\\n</INSTRUCTIONS>\"}]}}\n",
                "{\"type\":\"response_item\",\"timestamp\":\"2026-03-16T10:05:01Z\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"<environment_context>\\n<cwd>/tmp</cwd>\\n</environment_context>\"}]}}\n",
                "{\"type\":\"response_item\",\"timestamp\":\"2026-03-16T10:05:02Z\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"write plan for rollout\"}]}}\n"
            ),
        )
        .expect("write session file");

        let since_unix = parse_rfc3339_to_unix("2026-03-16T10:05:00Z").expect("parse timestamp");
        let first = read_codex_first_user_message_since(&session_file, since_unix)
            .expect("read")
            .expect("first real prompt");
        assert_eq!(first.0, "write plan for rollout");
        assert_eq!(first.1, since_unix + 2);
    }

    #[test]
    fn append_history_message_skips_consecutive_duplicates() {
        let mut history = String::new();
        let mut last_entry = None;

        append_history_message(&mut history, &mut last_entry, "user", "same");
        append_history_message(&mut history, &mut last_entry, "user", "same");
        append_history_message(&mut history, &mut last_entry, "assistant", "reply");
        append_history_message(&mut history, &mut last_entry, "assistant", "reply");

        assert_eq!(history, "Human: same\n\nAssistant: reply\n\n");
    }

    #[test]
    fn codex_find_search_terms_keep_phrase_and_meaningful_tokens() {
        assert_eq!(
            codex_find_search_terms("make plan to get designer"),
            vec![
                "make plan to get designer".to_string(),
                "make".to_string(),
                "plan".to_string(),
                "get".to_string(),
                "designer".to_string(),
            ]
        );
    }

    #[test]
    fn rank_recover_rows_prefers_matching_session_id_prefix() {
        let mut rows = vec![
            CodexRecoverRow {
                id: "019caaaa-0000-7000-8000-aaaaaaaaaaaa".to_string(),
                updated_at: 10,
                cwd: "/tmp/repo".to_string(),
                title: Some("one remaining unrelated issue".to_string()),
                first_user_message: Some("npm run lint still fails".to_string()),
                git_branch: Some("main".to_string()),
            },
            CodexRecoverRow {
                id: "019cdcff-0b3a-7a80-b22b-5ac4ff076eff".to_string(),
                updated_at: 5,
                cwd: "/tmp/other".to_string(),
                title: Some("something else".to_string()),
                first_user_message: Some("different prompt".to_string()),
                git_branch: Some("feature".to_string()),
            },
        ];

        rank_recover_rows(&mut rows, Some("019cdcff"));

        assert_eq!(rows[0].id, "019cdcff-0b3a-7a80-b22b-5ac4ff076eff");
    }

    #[test]
    fn extract_codex_session_hint_prefers_uuid_like_token() {
        assert_eq!(
            extract_codex_session_hint(
                "see 019cdcff-0b3a-7a80-b22b-5ac4ff076eff for work done on that"
            ),
            Some("019cdcff-0b3a-7a80-b22b-5ac4ff076eff".to_string())
        );
    }

    #[test]
    fn extract_codex_session_hint_ignores_git_sha_like_token() {
        assert_eq!(
            extract_codex_session_hint("see 3a4c62bfd29335a0170397b028a440c49858f1f5 for that"),
            None
        );
    }

    #[test]
    fn extract_codex_session_reference_request_parses_count_and_followup() {
        let request = extract_codex_session_reference_request(
            "see 019ce6ce-c77a-7d52-838e-c01f8820f6b8 last 20 messages, research react hot reload",
            "see 019ce6ce-c77a-7d52-838e-c01f8820f6b8 last 20 messages, research react hot reload",
        )
        .expect("expected session reference request");

        assert_eq!(request.session_hint, "019ce6ce-c77a-7d52-838e-c01f8820f6b8");
        assert_eq!(request.count, 20);
        assert_eq!(request.user_request, "research react hot reload");
    }

    #[test]
    fn extract_codex_session_reference_request_requires_followup_work() {
        assert!(
            extract_codex_session_reference_request(
                "see 019ce6ce-c77a-7d52-838e-c01f8820f6b8 last 20 messages",
                "see 019ce6ce-c77a-7d52-838e-c01f8820f6b8 last 20 messages",
            )
            .is_none()
        );
    }

    #[test]
    fn extract_codex_session_reference_request_does_not_steal_resume_queries() {
        assert!(
            extract_codex_session_reference_request(
                "resume 019ce6ce-c77a-7d52-838e-c01f8820f6b8",
                "resume 019ce6ce-c77a-7d52-838e-c01f8820f6b8",
            )
            .is_none()
        );
    }

    #[test]
    fn infer_recover_route_changes_directory_for_cross_repo_candidate() {
        let output = build_recover_output(
            Path::new("/tmp/current"),
            false,
            Some("019cdcff-0b3a-7a80-b22b-5ac4ff076eff".to_string()),
            vec![CodexRecoverRow {
                id: "019cdcff-0b3a-7a80-b22b-5ac4ff076eff".to_string(),
                updated_at: 5,
                cwd: "/tmp/other".to_string(),
                title: Some("something else".to_string()),
                first_user_message: Some("different prompt".to_string()),
                git_branch: Some("feature".to_string()),
            }],
        );

        assert_eq!(
            output.recommended_route,
            "cd /tmp/other && f ai codex resume 019cdcff-0b3a-7a80-b22b-5ac4ff076eff"
        );
    }

    #[test]
    fn session_lookup_detection_stays_conservative_for_general_session_work() {
        assert!(!looks_like_session_lookup_query(
            "improve session support in flow"
        ));
        assert!(!looks_like_session_lookup_query(
            "conversation summary pipeline cleanup"
        ));
        assert!(!looks_like_session_lookup_query(
            "write plan after reading https://github.com/openai/codex"
        ));
    }

    #[test]
    fn session_lookup_detection_accepts_explicit_control_prompts() {
        assert!(looks_like_session_lookup_query("resume session"));
        assert!(looks_like_session_lookup_query("show conversation"));
        assert!(looks_like_session_lookup_query("latest"));
        assert!(looks_like_session_lookup_query("after latest"));
    }

    #[test]
    fn wildcard_match_handles_linear_style_patterns() {
        assert!(wildcard_match(
            "https://linear.app/*/project/*",
            "https://linear.app/example-workspace/project/example-project-v1-1234567890ab/overview"
        ));
        assert!(wildcard_match(
            "https://linear.app/*/issue/*",
            "https://linear.app/example-workspace/issue/EX-123/test-title"
        ));
        assert!(!wildcard_match(
            "https://linear.app/*/issue/*",
            "https://github.com/openai/codex"
        ));
    }

    fn sample_codex_doctor_snapshot() -> CodexDoctorSnapshot {
        CodexDoctorSnapshot {
            target: "/tmp/repo".to_string(),
            codex_bin: "codex-flow-wrapper".to_string(),
            codexd: "running".to_string(),
            codexd_socket: "/tmp/codexd.sock".to_string(),
            memory_state: "ready".to_string(),
            memory_root: "/tmp/jazz2/codex-memory".to_string(),
            memory_db_path: "/tmp/jazz2/codex-memory/memory.sqlite".to_string(),
            memory_events_indexed: 9,
            memory_facts_indexed: 12,
            runtime_transport: "enabled".to_string(),
            runtime_skills: "enabled".to_string(),
            auto_resolve_references: true,
            home_session_path: "/tmp/home".to_string(),
            prompt_context_budget_chars: 1200,
            max_resolved_references: 2,
            reference_resolvers: 0,
            query_cache: "enabled".to_string(),
            query_cache_entries_on_disk: 4,
            skill_eval_events_on_disk: 6,
            skill_eval_outcomes_on_disk: 3,
            skill_scorecard_samples: 6,
            skill_scorecard_entries: 2,
            skill_scorecard_top: Some("plan_write (0.91)".to_string()),
            external_skill_candidates: 1,
            runtime_state_files: 2,
            runtime_state_files_for_target: 1,
            skill_eval_schedule: "loaded".to_string(),
            learning_state: "grounded".to_string(),
            runtime_ready: true,
            schedule_ready: true,
            learning_ready: true,
            warnings: Vec::new(),
        }
    }

    #[test]
    fn codex_doctor_assert_autonomous_accepts_grounded_snapshot() {
        let snapshot = sample_codex_doctor_snapshot();
        assert!(assert_codex_doctor(&snapshot, false, false, false, true).is_ok());
    }

    #[test]
    fn codex_doctor_assert_learning_requires_grounded_outcomes() {
        let mut snapshot = sample_codex_doctor_snapshot();
        snapshot.skill_eval_outcomes_on_disk = 0;
        snapshot.learning_ready = false;
        snapshot.learning_state = "affinity-only".to_string();

        let err = assert_codex_doctor(&snapshot, false, false, true, false)
            .expect_err("learning assertion should fail without outcomes");
        let message = format!("{err:#}");
        assert!(message.contains("no grounded skill outcome events recorded yet"));
    }

    #[test]
    fn parse_linear_url_reference_extracts_project_shape() {
        let reference = parse_linear_url_reference(
            "https://linear.app/example-workspace/project/example-project-v1-1234567890ab/overview",
        )
        .expect("linear project url should parse");

        assert_eq!(reference.workspace_slug, "example-workspace");
        assert_eq!(reference.resource_kind, LinearUrlKind::Project);
        assert_eq!(reference.resource_value, "example-project-v1-1234567890ab");
        assert_eq!(reference.view.as_deref(), Some("overview"));
        assert_eq!(reference.title_hint, "example project v1");
    }

    #[test]
    fn build_codex_prompt_keeps_plain_query_plain() {
        assert_eq!(
            build_codex_prompt("improve codex open perf", &[], 2, 1200).as_deref(),
            Some("improve codex open perf")
        );
    }

    #[test]
    fn build_codex_prompt_respects_shared_context_budget() {
        let references = vec![
            CodexResolvedReference {
                name: "docs".to_string(),
                source: "resolver".to_string(),
                matched: "one".to_string(),
                command: None,
                output: "A".repeat(500),
            },
            CodexResolvedReference {
                name: "issue".to_string(),
                source: "resolver".to_string(),
                matched: "two".to_string(),
                command: None,
                output: "B".repeat(500),
            },
        ];

        let prompt =
            build_codex_prompt("summarize", &references, 2, 260).expect("prompt should exist");

        assert!(prompt.chars().count() <= 260);
        assert!(prompt.contains("User request:"));
    }

    #[test]
    fn format_session_ref_respects_provider_prefix_flag() {
        let session = AiSession {
            session_id: "019ce791-7e05-7e51-b2b7-610dc7172e5c".to_string(),
            provider: Provider::Codex,
            timestamp: None,
            last_message_at: None,
            last_message: None,
            first_message: None,
            error_summary: None,
        };

        assert_eq!(
            format_session_ref(&session, false),
            "019ce791-7e05-7e51-b2b7-610dc7172e5c"
        );
        assert_eq!(
            format_session_ref(&session, true),
            "codex:019ce791-7e05-7e51-b2b7-610dc7172e5c"
        );
    }
}
